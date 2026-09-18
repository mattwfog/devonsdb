//! Deterministic level assignment and canonical scalar scoring adapters
//! (`docs/HNSW.md` §2.3, §7, §11 ruling 7).
//!
//! Quantized vector element encodings are specified by `docs/FORMAT.md`.
//!
//! This module deliberately stops at one non-null vector's value bytes. Node
//! group validity bitmaps, payload runs, and catalog wiring remain the storage
//! layer's responsibility.

use std::cmp::Ordering;

use devondb_types::{
    DevonError, DevonResult,
    logical_type::{LogicalType, VectorEncoding},
};

use super::types::{
    HnswConfig, HnswMetric, MAX_LEVEL, NavigationEncoding, NavigationScorer, supports_index,
};
use crate::vector_encoding::{decode_b1_signs, decode_f16, decode_i8, rotate_b1};

const F32_ELEMENT_BYTES: usize = size_of::<f32>();
const F16_ELEMENT_BYTES: usize = size_of::<u16>();
const I8_METADATA_BYTES: usize = 2 * size_of::<f32>();

/// Derives one node's maximum HNSW level from the persistent configuration.
///
/// Levels are never stored. This applies the exact SplitMix64 step from
/// `docs/FORMAT.md` and the geometric promotion rule from `docs/HNSW.md`
/// §2.3, including the fixed [`MAX_LEVEL`] cap.
pub fn level_for_node(config: &HnswConfig, node_offset: u64) -> DevonResult<u8> {
    config.validate()?;
    let mut generator = SplitMix64::new(config.level_seed ^ node_offset);
    let mut level = 0;
    while level < MAX_LEVEL && generator.next().is_multiple_of(u64::from(config.m)) {
        level += 1;
    }
    Ok(level)
}

/// Compares scored nodes in the binding deterministic HNSW order.
///
/// Distance uses IEEE-754 total ordering, including signed zero and NaNs;
/// equal distances are ordered by node offset.
#[must_use]
pub fn compare_distance_then_offset(
    left_distance: f32,
    left_node_offset: u64,
    right_distance: f32,
    right_node_offset: u64,
) -> Ordering {
    left_distance
        .total_cmp(&right_distance)
        .then_with(|| left_node_offset.cmp(&right_node_offset))
}

/// Prepared canonical-scalar scorer for HNSW topology construction.
///
/// The scorer owns the original query. For b1 navigation it additionally
/// owns the query rotated once with the column's catalog seed, so scoring a
/// candidate never rebuilds the rotation schedule. SIMD dispatch is
/// intentionally absent from this construction-path adapter.
#[derive(Debug, Clone)]
pub struct ConstructionScorer {
    query: Box<[f32]>,
    metric: HnswMetric,
    adapter: SlotAdapter,
}

impl ConstructionScorer {
    /// Prepares a scorer for a validated index configuration and vector column.
    ///
    /// Unsupported first-wave combinations, including b1+L2 and b1 without a
    /// rescore run, return [`DevonError::InvalidArgument`]. A disagreement
    /// between the root's navigation encoding and the catalog column is
    /// corruption, as required by `docs/HNSW.md` §7.3.
    pub fn new(config: &HnswConfig, column_type: &LogicalType, query: &[f32]) -> DevonResult<Self> {
        config.validate()?;
        if !supports_index(column_type, config.metric) {
            return Err(invalid_argument(format!(
                "unsupported HNSW index scoring combination: {column_type} with {:?}",
                config.metric
            )));
        }
        validate_navigation_encoding(config, column_type)?;
        validate_query(column_type, query)?;

        let adapter = adapter_for_column(column_type, query)?;
        Ok(Self {
            query: query.into(),
            metric: config.metric,
            adapter,
        })
    }

    fn scalar_distance(&self, candidate: &[f32]) -> DevonResult<f32> {
        if candidate.len() != self.query.len() {
            return Err(invalid_argument(format!(
                "HNSW rescore vector dimension {} does not match query dimension {}",
                candidate.len(),
                self.query.len()
            )));
        }
        Ok(scalar_distance(&self.query, candidate, self.metric))
    }
}

impl NavigationScorer for ConstructionScorer {
    fn navigation_distance(&self, slot: &[u8]) -> DevonResult<f32> {
        match &self.adapter {
            SlotAdapter::F32 => {
                let candidate = decode_f32_slot(slot, self.query.len())?;
                self.scalar_distance(&candidate)
            }
            SlotAdapter::F16 => {
                validate_slot_len(
                    "f16",
                    slot.len(),
                    exact_slot_len(self.query.len(), F16_ELEMENT_BYTES, 0)?,
                )?;
                let candidate = decode_f16(slot)
                    .map_err(|error| corrupt(format!("invalid f16 HNSW slot: {error}")))?;
                self.scalar_distance(&candidate)
            }
            SlotAdapter::I8 => {
                validate_slot_len(
                    "i8",
                    slot.len(),
                    exact_slot_len(self.query.len(), 1, I8_METADATA_BYTES)?,
                )?;
                let candidate = decode_i8(slot)
                    .map_err(|error| corrupt(format!("invalid i8 HNSW slot: {error}")))?;
                self.scalar_distance(&candidate)
            }
            SlotAdapter::B1 {
                rotated_query,
                denominator,
            } => b1_distance(rotated_query, *denominator, slot),
        }
    }

    fn rescore_distance(&self, vector: &[f32]) -> DevonResult<f32> {
        self.scalar_distance(vector)
    }
}

#[derive(Debug, Clone)]
enum SlotAdapter {
    F32,
    F16,
    I8,
    B1 {
        rotated_query: Box<[f32]>,
        denominator: f64,
    },
}

fn validate_navigation_encoding(config: &HnswConfig, column_type: &LogicalType) -> DevonResult<()> {
    let Some(column_navigation) = NavigationEncoding::of_column(column_type) else {
        return Err(invalid_argument(format!(
            "HNSW scoring requires a vector column, got {column_type}"
        )));
    };
    if column_navigation != config.navigation {
        return Err(corrupt(format!(
            "HNSW root navigation {:?} does not match column navigation {column_navigation:?}",
            config.navigation
        )));
    }
    Ok(())
}

fn validate_query(column_type: &LogicalType, query: &[f32]) -> DevonResult<()> {
    let dimension = column_type
        .vector_dim()
        .ok_or_else(|| invalid_argument("HNSW scoring requires a vector column"))?;
    let dimension = usize::try_from(dimension)
        .map_err(|_| invalid_argument("HNSW vector dimension does not fit usize"))?;
    if query.len() != dimension {
        return Err(invalid_argument(format!(
            "HNSW query dimension {} does not match column dimension {dimension}",
            query.len()
        )));
    }
    if query.iter().any(|value| !value.is_finite()) {
        return Err(invalid_argument("HNSW query contains a non-finite value"));
    }
    Ok(())
}

fn adapter_for_column(column_type: &LogicalType, query: &[f32]) -> DevonResult<SlotAdapter> {
    match column_type {
        LogicalType::Vector { .. } => Ok(SlotAdapter::F32),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::F16,
            ..
        } => Ok(SlotAdapter::F16),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::I8,
            ..
        } => Ok(SlotAdapter::I8),
        LogicalType::VectorEncoded {
            encoding: VectorEncoding::B1 { rotation_seed, .. },
            ..
        } => b1_adapter(query, *rotation_seed),
        _ => Err(invalid_argument(format!(
            "HNSW scoring requires a vector column, got {column_type}"
        ))),
    }
}

fn b1_adapter(query: &[f32], rotation_seed: u64) -> DevonResult<SlotAdapter> {
    let norm_squared: f64 = query.iter().map(|value| f64::from(*value).powi(2)).sum();
    if query.is_empty() || norm_squared == 0.0 {
        return Err(invalid_argument(
            "b1 cosine scoring requires a non-empty query with non-zero norm",
        ));
    }
    let denominator = norm_squared.sqrt() * (query.len() as f64).sqrt();
    Ok(SlotAdapter::B1 {
        rotated_query: rotate_b1(query, rotation_seed).into_boxed_slice(),
        denominator,
    })
}

fn decode_f32_slot(slot: &[u8], dimension: usize) -> DevonResult<Vec<f32>> {
    validate_slot_len(
        "f32",
        slot.len(),
        exact_slot_len(dimension, F32_ELEMENT_BYTES, 0)?,
    )?;
    let candidate: Vec<f32> = slot
        .as_chunks::<F32_ELEMENT_BYTES>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(copy_array(bytes)))
        .collect();
    if candidate.iter().any(|value| !value.is_finite()) {
        return Err(corrupt(
            "f32 HNSW navigation slot contains a non-finite value",
        ));
    }
    Ok(candidate)
}

fn b1_distance(rotated_query: &[f32], denominator: f64, slot: &[u8]) -> DevonResult<f32> {
    let signs = decode_b1_signs(slot, rotated_query.len())
        .map_err(|error| corrupt(format!("invalid b1 HNSW slot: {error}")))?;
    let dot: f64 = rotated_query
        .iter()
        .zip(signs)
        .map(|(query_value, sign)| f64::from(*query_value) * f64::from(sign))
        .sum();
    let similarity = (dot / denominator).clamp(-1.0, 1.0);
    Ok((1.0 - similarity) as f32)
}

fn scalar_distance(query: &[f32], candidate: &[f32], metric: HnswMetric) -> f32 {
    match metric {
        HnswMetric::L2 => query
            .iter()
            .zip(candidate)
            .map(|(left, right)| (left - right) * (left - right))
            .sum::<f32>()
            .sqrt(),
        HnswMetric::Cosine => {
            let (dot, norm_query, norm_candidate) = query.iter().zip(candidate).fold(
                (0.0_f32, 0.0_f32, 0.0_f32),
                |(dot, norm_query, norm_candidate), (left, right)| {
                    (
                        dot + left * right,
                        norm_query + left * left,
                        norm_candidate + right * right,
                    )
                },
            );
            if norm_query == 0.0 || norm_candidate == 0.0 {
                2.0
            } else {
                1.0 - dot / (norm_query.sqrt() * norm_candidate.sqrt())
            }
        }
    }
}

fn exact_slot_len(dimension: usize, element_bytes: usize, metadata: usize) -> DevonResult<usize> {
    dimension
        .checked_mul(element_bytes)
        .and_then(|values| values.checked_add(metadata))
        .ok_or_else(|| invalid_argument("HNSW vector slot length overflows usize"))
}

fn validate_slot_len(encoding: &str, actual: usize, expected: usize) -> DevonResult<()> {
    if actual != expected {
        return Err(corrupt(format!(
            "{encoding} HNSW navigation slot length is {actual}, expected {expected}"
        )));
    }
    Ok(())
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }
}

fn copy_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut array = [0_u8; N];
    array.copy_from_slice(bytes);
    array
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}
