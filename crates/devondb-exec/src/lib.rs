//! Vectorized executor for DevonPlan logical plans.
//!
//! Chunk-at-a-time (~2048 rows) Volcano-style pull execution first;
//! morsel-driven parallelism later. Correctness and format stability come
//! before speed — every operator lands with tests against known-answer data.

pub mod chunk;
pub mod column;
pub mod eval;
pub mod expand;
pub mod expand_rel;
pub mod hnsw;
pub mod join;
pub mod knn;
pub mod operators;
pub mod simd;
pub mod source;
pub mod within;

#[cfg(feature = "fts")]
pub mod fulltext;
