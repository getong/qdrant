use std::path::{Path, PathBuf};

use bitvec::prelude::BitVec;
use quantization::EncodedVectors;

use crate::data_types::vectors::VectorElementType;
use crate::entry::entry_point::OperationResult;
use crate::types::{PointOffsetType, ScoreType};
use crate::vector_storage::quantized::quantized_vectors_base::QuantizedVectors;
use crate::vector_storage::{RawScorer, ScoredPointOffset};

pub const QUANTIZED_DATA_PATH: &str = "quantized.data";
pub const QUANTIZED_META_PATH: &str = "quantized.meta.json";

pub struct ScalarQuantizedRawScorer<'a, TEncodedQuery, TEncodedVectors>
where
    TEncodedVectors: quantization::EncodedVectors<TEncodedQuery>,
{
    pub query: TEncodedQuery,
    pub deleted: &'a BitVec,
    pub quantized_data: &'a TEncodedVectors,
}

impl<TEncodedQuery, TEncodedVectors> RawScorer
    for ScalarQuantizedRawScorer<'_, TEncodedQuery, TEncodedVectors>
where
    TEncodedVectors: quantization::EncodedVectors<TEncodedQuery>,
{
    fn score_points(&self, points: &[PointOffsetType], scores: &mut [ScoredPointOffset]) -> usize {
        let mut size: usize = 0;
        for point_id in points.iter().copied() {
            if self.deleted[point_id as usize] {
                continue;
            }
            scores[size] = ScoredPointOffset {
                idx: point_id,
                score: self.quantized_data.score_point(&self.query, point_id),
            };
            size += 1;
            if size == scores.len() {
                return size;
            }
        }
        size
    }

    fn check_point(&self, point: PointOffsetType) -> bool {
        (point as usize) < self.deleted.len() && !self.deleted[point as usize]
    }

    fn score_point(&self, point: PointOffsetType) -> ScoreType {
        self.quantized_data.score_point(&self.query, point)
    }

    fn score_internal(&self, point_a: PointOffsetType, point_b: PointOffsetType) -> ScoreType {
        self.quantized_data.score_internal(point_a, point_b)
    }
}

pub struct ScalarQuantizedVectors<TStorage: quantization::EncodedStorage + Send + Sync> {
    storage: quantization::EncodedVectorsU8<TStorage>,
}

impl<TStorage: quantization::EncodedStorage + Send + Sync> ScalarQuantizedVectors<TStorage> {
    pub fn new(storage: quantization::EncodedVectorsU8<TStorage>) -> Self {
        Self { storage }
    }
}

impl<TStorage> QuantizedVectors for ScalarQuantizedVectors<TStorage>
where
    TStorage: quantization::EncodedStorage + Send + Sync,
{
    fn raw_scorer<'a>(
        &'a self,
        query: &[VectorElementType],
        deleted: &'a BitVec,
    ) -> Box<dyn RawScorer + 'a> {
        let query = self.storage.encode_query(query);
        Box::new(ScalarQuantizedRawScorer {
            query,
            deleted,
            quantized_data: &self.storage,
        })
    }

    fn save_to(&self, path: &Path) -> OperationResult<()> {
        let data_path = path.join(QUANTIZED_DATA_PATH);
        let meta_path = path.join(QUANTIZED_META_PATH);
        self.storage.save(&data_path, &meta_path)?;
        Ok(())
    }

    fn files(&self) -> Vec<PathBuf> {
        vec![QUANTIZED_DATA_PATH.into(), QUANTIZED_META_PATH.into()]
    }
}
