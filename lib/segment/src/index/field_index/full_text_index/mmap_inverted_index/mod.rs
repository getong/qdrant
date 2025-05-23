use std::collections::HashMap;
use std::path::PathBuf;

use bitvec::vec::BitVec;
use common::counter::hardware_counter::HardwareCounterCell;
use common::mmap_hashmap::{MmapHashMap, READ_ENTRY_OVERHEAD};
use common::types::PointOffsetType;
use memory::fadvise::clear_disk_cache;
use memory::madvise::AdviceSetting;
use memory::mmap_ops;
use memory::mmap_type::{MmapBitSlice, MmapSlice};
use mmap_postings::MmapPostings;

use super::compressed_posting::compressed_chunks_reader::ChunkReader;
use super::inverted_index::{InvertedIndex, ParsedQuery};
use super::postings_iterator::intersect_compressed_postings_iterator;
use crate::common::mmap_bitslice_buffered_update_wrapper::MmapBitSliceBufferedUpdateWrapper;
use crate::common::operation_error::{OperationError, OperationResult};
use crate::index::field_index::full_text_index::immutable_inverted_index::ImmutableInvertedIndex;
use crate::index::field_index::full_text_index::inverted_index::TokenId;

mod mmap_postings;

const POSTINGS_FILE: &str = "postings.dat";
const VOCAB_FILE: &str = "vocab.dat";
const POINT_TO_TOKENS_COUNT_FILE: &str = "point_to_tokens_count.dat";
const DELETED_POINTS_FILE: &str = "deleted_points.dat";

pub struct MmapInvertedIndex {
    pub(in crate::index::field_index::full_text_index) path: PathBuf,
    pub(in crate::index::field_index::full_text_index) postings: MmapPostings,
    pub(in crate::index::field_index::full_text_index) vocab: MmapHashMap<str, TokenId>,
    pub(in crate::index::field_index::full_text_index) point_to_tokens_count: MmapSlice<usize>,
    pub(in crate::index::field_index::full_text_index) deleted_points:
        MmapBitSliceBufferedUpdateWrapper,
    /// Number of points which are not deleted
    pub(in crate::index::field_index::full_text_index) active_points_count: usize,
    is_on_disk: bool,
}

impl MmapInvertedIndex {
    pub fn create(path: PathBuf, inverted_index: ImmutableInvertedIndex) -> OperationResult<()> {
        let ImmutableInvertedIndex {
            postings,
            vocab,
            point_to_tokens_count,
            points_count: _,
        } = inverted_index;

        debug_assert_eq!(vocab.len(), postings.len());

        let postings_path = path.join(POSTINGS_FILE);
        let vocab_path = path.join(VOCAB_FILE);
        let point_to_tokens_count_path = path.join(POINT_TO_TOKENS_COUNT_FILE);
        let deleted_points_path = path.join(DELETED_POINTS_FILE);

        MmapPostings::create(postings_path, &postings)?;

        // Currently MmapHashMap maps str -> [u32], but we only need to map str -> u32.
        // TODO: Consider making another mmap structure for this case.
        MmapHashMap::<str, TokenId>::create(
            &vocab_path,
            vocab.iter().map(|(k, v)| (k.as_str(), std::iter::once(*v))),
        )?;

        // Save point_to_tokens_count, separated into a bitslice for None values and a slice for actual values
        //
        // None values are represented as deleted in the bitslice
        let deleted_bitslice: BitVec = point_to_tokens_count
            .iter()
            .map(|count| count.is_none())
            .collect();
        MmapBitSlice::create(&deleted_points_path, &deleted_bitslice)?;

        // The actual values go in the slice
        let point_to_tokens_count_iter = point_to_tokens_count
            .into_iter()
            .map(|count| count.unwrap_or(0));

        MmapSlice::create(&point_to_tokens_count_path, point_to_tokens_count_iter)?;

        Ok(())
    }

    pub fn open(path: PathBuf, populate: bool) -> OperationResult<Self> {
        let postings_path = path.join(POSTINGS_FILE);
        let vocab_path = path.join(VOCAB_FILE);
        let point_to_tokens_count_path = path.join(POINT_TO_TOKENS_COUNT_FILE);
        let deleted_points_path = path.join(DELETED_POINTS_FILE);

        let postings = MmapPostings::open(&postings_path, populate)?;
        let vocab = MmapHashMap::<str, TokenId>::open(&vocab_path, false)?;

        let point_to_tokens_count = unsafe {
            MmapSlice::try_from(mmap_ops::open_write_mmap(
                &point_to_tokens_count_path,
                AdviceSetting::Global,
                populate,
            )?)?
        };

        let deleted =
            mmap_ops::open_write_mmap(&deleted_points_path, AdviceSetting::Global, populate)?;
        let deleted = MmapBitSlice::from(deleted, 0);

        let num_deleted_points = deleted.count_ones();
        let deleted_points = MmapBitSliceBufferedUpdateWrapper::new(deleted);
        let points_count = point_to_tokens_count.len() - num_deleted_points;

        Ok(Self {
            path,
            postings,
            vocab,
            point_to_tokens_count,
            deleted_points,
            active_points_count: points_count,
            is_on_disk: !populate,
        })
    }

    pub(super) fn iter_vocab(&self) -> impl Iterator<Item = (&str, &TokenId)> {
        // unwrap safety: we know that each token points to a token id.
        self.vocab.iter().map(|(k, v)| (k, v.first().unwrap()))
    }

    /// Iterate over posting lists, returning chunk reader for each
    #[inline]
    pub(super) fn iter_postings<'a>(
        &'a self,
        hw_counter: &'a HardwareCounterCell,
    ) -> impl Iterator<Item = Option<ChunkReader<'a>>> {
        self.postings.iter_postings(hw_counter)
    }

    /// Returns whether the point id is valid and active.
    pub fn is_active(&self, point_id: PointOffsetType) -> bool {
        let is_deleted = self.deleted_points.get(point_id as usize).unwrap_or(true);

        !is_deleted
    }

    pub fn files(&self) -> Vec<PathBuf> {
        vec![
            self.path.join(POSTINGS_FILE),
            self.path.join(VOCAB_FILE),
            self.path.join(POINT_TO_TOKENS_COUNT_FILE),
            self.path.join(DELETED_POINTS_FILE),
        ]
    }

    pub fn is_on_disk(&self) -> bool {
        self.is_on_disk
    }

    /// Populate all pages in the mmap.
    /// Block until all pages are populated.
    pub fn populate(&self) -> OperationResult<()> {
        self.postings.populate();
        self.vocab.populate()?;
        self.point_to_tokens_count.populate()?;
        Ok(())
    }

    /// Drop disk cache.
    pub fn clear_cache(&self) -> OperationResult<()> {
        let files = self.files();
        for file in files {
            clear_disk_cache(&file)?;
        }

        Ok(())
    }
}

impl InvertedIndex for MmapInvertedIndex {
    fn get_vocab_mut(&mut self) -> &mut HashMap<String, TokenId> {
        unreachable!("MmapInvertedIndex does not support mutable operations")
    }

    fn index_document(
        &mut self,
        _idx: PointOffsetType,
        _document: super::inverted_index::Document,
        _hw_counter: &HardwareCounterCell,
    ) -> OperationResult<()> {
        Err(OperationError::service_error(
            "Can't add values to mmap immutable text index",
        ))
    }

    fn remove_document(&mut self, idx: PointOffsetType) -> bool {
        let Some(is_deleted) = self.deleted_points.get(idx as usize) else {
            return false; // Never existed
        };

        if is_deleted {
            return false; // Already removed
        }

        self.deleted_points.set(idx as usize, true);
        if let Some(count) = self.point_to_tokens_count.get_mut(idx as usize) {
            *count = 0;

            // `deleted_points`'s length can be larger than `point_to_tokens_count`'s length.
            // Only if the index is within bounds of `point_to_tokens_count`, we decrement the active points count.
            self.active_points_count -= 1;
        }
        true
    }

    fn filter<'a>(
        &'a self,
        query: ParsedQuery,
        hw_counter: &'a HardwareCounterCell,
    ) -> Box<dyn Iterator<Item = PointOffsetType> + 'a> {
        let postings_opt: Option<Vec<_>> = query
            .tokens
            .iter()
            .map(|&token_id| self.postings.get(token_id, hw_counter))
            .collect();
        let Some(posting_readers) = postings_opt else {
            // There are unseen tokens -> no matches
            return Box::new(std::iter::empty());
        };

        if posting_readers.is_empty() {
            // Empty request -> no matches
            return Box::new(std::iter::empty());
        }

        // in case of mmap immutable index, deleted points are still in the postings
        let filter = move |idx| self.is_active(idx);

        intersect_compressed_postings_iterator(posting_readers, filter)
    }

    fn get_posting_len(
        &self,
        token_id: TokenId,
        hw_counter: &HardwareCounterCell,
    ) -> Option<usize> {
        self.postings.get(token_id, hw_counter).map(|p| p.len())
    }

    fn vocab_with_postings_len_iter(&self) -> impl Iterator<Item = (&str, usize)> + '_ {
        let hw_counter = HardwareCounterCell::disposable(); // No propagation needed here because this function is only used for building HNSW index.

        self.iter_vocab().filter_map(move |(token, &token_id)| {
            self.postings
                .get(token_id, &hw_counter)
                .map(|posting| (token, posting.len()))
        })
    }

    fn check_match(
        &self,
        parsed_query: &ParsedQuery,
        point_id: PointOffsetType,
        hw_counter: &HardwareCounterCell,
    ) -> bool {
        // check non-empty query
        if parsed_query.tokens.is_empty() {
            return false;
        }

        // check presence of the document
        if self.values_is_empty(point_id) {
            return false;
        }
        // Check that all tokens are in document
        parsed_query.tokens.iter().all(|query_token| {
            self.postings
                .get(*query_token, hw_counter)
                // unwrap safety: all tokens exist in the vocabulary, otherwise there'd be no query tokens
                .unwrap()
                .contains(point_id)
        })
    }

    fn values_is_empty(&self, point_id: PointOffsetType) -> bool {
        if self.deleted_points.get(point_id as usize).unwrap_or(true) {
            return true;
        }
        self.point_to_tokens_count
            .get(point_id as usize)
            .map(|count| *count == 0)
            // if the point does not exist, it is considered empty
            .unwrap_or(true)
    }

    fn values_count(&self, point_id: PointOffsetType) -> usize {
        if self.deleted_points.get(point_id as usize).unwrap_or(true) {
            return 0;
        }
        self.point_to_tokens_count
            .get(point_id as usize)
            .copied()
            // if the point does not exist, it is considered empty
            .unwrap_or(0)
    }

    fn points_count(&self) -> usize {
        self.active_points_count
    }

    fn get_token_id(&self, token: &str, hw_counter: &HardwareCounterCell) -> Option<TokenId> {
        if self.is_on_disk {
            hw_counter.payload_index_io_read_counter().incr_delta(
                READ_ENTRY_OVERHEAD + size_of::<TokenId>(), // Avoid check overhead and assume token is always read
            );
        }

        self.vocab
            .get(token)
            .ok()
            .flatten()
            .and_then(<[TokenId]>::first)
            .copied()
    }
}
