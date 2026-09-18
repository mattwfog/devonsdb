//! Shared HNSW vocabulary (`docs/HNSW.md` §§2-3, §5.1): configuration,
//! encodings, the committed index delta, and the construction scoring
//! interface. Implementations use these types rather than parallel
//! definitions.

use std::collections::BTreeMap;
use std::sync::Arc;

use devondb_types::{
    DevonError, DevonResult,
    logical_type::{B1Rescore, LogicalType, VectorEncoding},
};

/// Distance metric persisted in the HNSW root (`docs/HNSW.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HnswMetric {
    /// Squared-Euclidean ordering.
    L2,
    /// Cosine distance.
    Cosine,
}

impl HnswMetric {
    /// The root-page byte for this metric.
    #[must_use]
    pub fn as_byte(self) -> u8 {
        match self {
            Self::L2 => 0,
            Self::Cosine => 1,
        }
    }

    /// Decodes a root-page metric byte.
    pub fn from_byte(byte: u8) -> DevonResult<Self> {
        match byte {
            0 => Ok(Self::L2),
            1 => Ok(Self::Cosine),
            other => Err(corrupt(format!("unknown HNSW metric byte {other}"))),
        }
    }
}

/// Navigation encoding persisted in the HNSW root (`docs/HNSW.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationEncoding {
    /// Plain f32 vector column.
    F32,
    /// f16-encoded column.
    F16,
    /// i8-encoded column.
    I8,
    /// b1 sign-bit column.
    B1,
}

impl NavigationEncoding {
    /// The root-page byte for this encoding.
    #[must_use]
    pub fn as_byte(self) -> u8 {
        match self {
            Self::F32 => 0,
            Self::F16 => 1,
            Self::I8 => 2,
            Self::B1 => 3,
        }
    }

    /// Decodes a root-page encoding byte.
    pub fn from_byte(byte: u8) -> DevonResult<Self> {
        match byte {
            0 => Ok(Self::F32),
            1 => Ok(Self::F16),
            2 => Ok(Self::I8),
            3 => Ok(Self::B1),
            other => Err(corrupt(format!(
                "unknown HNSW navigation encoding byte {other}"
            ))),
        }
    }

    /// The navigation encoding of a vector column type, or `None` for
    /// non-vector types.
    #[must_use]
    pub fn of_column(ty: &LogicalType) -> Option<Self> {
        match ty {
            LogicalType::Vector { .. } => Some(Self::F32),
            LogicalType::VectorEncoded { encoding, .. } => Some(match encoding {
                VectorEncoding::F16 => Self::F16,
                VectorEncoding::I8 => Self::I8,
                VectorEncoding::B1 { .. } => Self::B1,
            }),
            _ => None,
        }
    }
}

/// Whether an index over `(column type, metric)` is supported: l2 and cosine
/// on f32/f16/i8 navigation; cosine only on b1, which also requires a rescore
/// run. Unsupported combinations use brute force.
#[must_use]
pub fn supports_index(ty: &LogicalType, metric: HnswMetric) -> bool {
    match ty {
        LogicalType::Vector { .. } => true,
        LogicalType::VectorEncoded { encoding, .. } => match encoding {
            VectorEncoding::F16 | VectorEncoding::I8 => true,
            VectorEncoding::B1 { rescore, .. } => {
                metric == HnswMetric::Cosine && *rescore != B1Rescore::None
            }
        },
        _ => false,
    }
}

/// Topology-defining configuration persisted in the HNSW root
/// (`docs/HNSW.md` §2.2). `ef_search` is request policy, not part of this
/// persistent configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswConfig {
    /// Upper-layer degree bound; 4..=64.
    pub m: u16,
    /// Layer-0 degree bound; exactly `2 * m`.
    pub m0: u16,
    /// Construction candidate width; `m0..=4096`.
    pub ef_construction: u32,
    /// Level-assignment seed, fixed for the index lifetime.
    pub level_seed: u64,
    /// Persisted metric.
    pub metric: HnswMetric,
    /// Persisted navigation encoding.
    pub navigation: NavigationEncoding,
}

impl HnswConfig {
    /// Default writer policy for a new index (`docs/HNSW.md` §2.2).
    #[must_use]
    pub fn with_defaults(
        level_seed: u64,
        metric: HnswMetric,
        navigation: NavigationEncoding,
    ) -> Self {
        Self {
            m: 16,
            m0: 32,
            ef_construction: 200,
            level_seed,
            metric,
            navigation,
        }
    }

    /// Validates the §2.2 parameter rules.
    pub fn validate(&self) -> DevonResult<()> {
        if !(4..=64).contains(&self.m) {
            return Err(corrupt(format!(
                "HNSW m is {}, valid range is 4..=64",
                self.m
            )));
        }
        if self.m0 != self.m * 2 {
            return Err(corrupt(format!(
                "HNSW m0 is {}, must be exactly 2*m = {}",
                self.m0,
                self.m * 2
            )));
        }
        if !(u32::from(self.m0)..=4096).contains(&self.ef_construction) {
            return Err(corrupt(format!(
                "HNSW ef_construction is {}, valid range is {}..=4096",
                self.ef_construction, self.m0
            )));
        }
        Ok(())
    }
}

/// The fixed format/algorithm bound on HNSW levels (`docs/HNSW.md` §2.2).
pub const MAX_LEVEL: u8 = 63;

/// One index's committed, immutable overlay delta (`docs/HNSW.md` §5.1).
///
/// The publisher enforces these invariants: immutable after publication;
/// contiguous coverage only; complete ordered neighbor-list
/// replacements; no duplicate slot; degree and visibility checks before
/// publish; all allocated capacity charged as committed overlay memory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HnswDelta {
    /// Coverage before this delta.
    pub old_covered_rows: u64,
    /// Coverage after this delta; never less than `old_covered_rows`.
    pub new_covered_rows: u64,
    /// Complete ordered neighbor-list replacements keyed by
    /// `(layer, node offset)`.
    pub replacements: BTreeMap<(u8, u64), Arc<[u64]>>,
    /// Entry-point replacement, `(node offset, level)`.
    pub entry: Option<(u64, u8)>,
}

impl HnswDelta {
    /// Estimated resident bytes for overlay budget charging, following the
    /// `Value::approx_bytes` policy of container-estimate plus payload.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        let lists: usize = self
            .replacements
            .values()
            .map(|list| 64 + list.len() * 8)
            .sum();
        64 + lists
    }
}

/// Construction-path scoring interface: canonical scalar kernels only.
/// Implementations MUST NOT dispatch to SIMD paths.
pub trait NavigationScorer {
    /// Distance from the prepared query to one encoded navigation slot.
    fn navigation_distance(&self, slot: &[u8]) -> DevonResult<f32>;

    /// Distance from the original query to one decoded rescore vector.
    fn rescore_distance(&self, vector: &[f32]) -> DevonResult<f32>;
}

/// Read access to one snapshot's HNSW topology (`docs/HNSW.md` §4.2-§4.3).
///
/// Used by search and implemented by the snapshot index view and in-memory
/// unit fixtures. Implementations are immutable views: repeated calls with
/// the same arguments MUST return the same data.
pub trait GraphAccess {
    /// The view's effective entry point, `(node offset, level)`, if any.
    fn entry(&self) -> Option<(u64, u8)>;

    /// Number of layers visible in this view; zero for an empty index.
    fn layer_count(&self) -> u8;

    /// The view's covered-prefix bound: every node this view returns is
    /// below this offset.
    fn covered_rows(&self) -> u64;

    /// Writes the ordered neighbor list of `(layer, node)` into `scratch`
    /// (cleared first; empty when the node has no list at that layer).
    /// Caller-provided scratch keeps per-hop allocation out of the trait;
    /// implementations validate bounds/degree and report `Corrupt`.
    fn neighbors(&self, layer: u8, node: u64, scratch: &mut Vec<u64>) -> DevonResult<()>;
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_and_encoding_bytes_round_trip() {
        for metric in [HnswMetric::L2, HnswMetric::Cosine] {
            assert_eq!(HnswMetric::from_byte(metric.as_byte()).unwrap(), metric);
        }
        assert!(HnswMetric::from_byte(2).is_err());
        for encoding in [
            NavigationEncoding::F32,
            NavigationEncoding::F16,
            NavigationEncoding::I8,
            NavigationEncoding::B1,
        ] {
            assert_eq!(
                NavigationEncoding::from_byte(encoding.as_byte()).unwrap(),
                encoding
            );
        }
        assert!(NavigationEncoding::from_byte(4).is_err());
    }

    #[test]
    fn config_validation_enforces_section_2_2() {
        let valid = HnswConfig::with_defaults(7, HnswMetric::Cosine, NavigationEncoding::F32);
        valid.validate().unwrap();

        let mut bad_m = valid;
        bad_m.m = 3;
        assert!(bad_m.validate().is_err());

        let mut bad_m0 = valid;
        bad_m0.m0 = 33;
        assert!(bad_m0.validate().is_err());

        let mut bad_ef = valid;
        bad_ef.ef_construction = 8;
        assert!(bad_ef.validate().is_err());
    }

    #[test]
    fn first_wave_support_matrix_matches_rule_9() {
        let f32_col = LogicalType::Vector { dim: 4 };
        let b1_rescored = LogicalType::VectorEncoded {
            dim: 4,
            encoding: VectorEncoding::B1 {
                rotation_seed: 1,
                rescore: B1Rescore::F32,
            },
        };
        let b1_bare = LogicalType::VectorEncoded {
            dim: 4,
            encoding: VectorEncoding::B1 {
                rotation_seed: 1,
                rescore: B1Rescore::None,
            },
        };

        assert!(supports_index(&f32_col, HnswMetric::L2));
        assert!(supports_index(&b1_rescored, HnswMetric::Cosine));
        assert!(!supports_index(&b1_rescored, HnswMetric::L2));
        assert!(!supports_index(&b1_bare, HnswMetric::Cosine));
        assert!(!supports_index(&LogicalType::Int64, HnswMetric::L2));
    }

    #[test]
    fn delta_bytes_scale_with_replacement_lists() {
        let mut delta = HnswDelta::default();
        let empty = delta.approx_bytes();
        delta
            .replacements
            .insert((0, 9), Arc::from(vec![1_u64, 2, 3].into_boxed_slice()));
        assert!(delta.approx_bytes() > empty + 3 * 8);
    }
}
