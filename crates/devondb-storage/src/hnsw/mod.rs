//! Persistent MVCC-aware HNSW (`docs/HNSW.md`, binding).
//!
//! `format` contains the root and layer-directory codecs, `csr` the bounded
//! `RCSR` slot reader, `scoring` level assignment and scalar scoring
//! adapters, `search` bounded deterministic search, and `view` the snapshot
//! index view. Shared vocabulary lives in [`types`].

pub mod csr;
pub mod format;
pub mod index;
pub mod scoring;
pub mod search;
pub mod types;
pub mod view;
