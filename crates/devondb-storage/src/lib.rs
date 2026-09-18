//! devondb storage engine: pager, superblock, write-ahead log, node groups.
//!
//! The on-disk format is specified in `docs/FORMAT.md` and is a versioned,
//! forward-compatible contract — the single most important promise devondb
//! makes (Kuzu broke it on every release). Any change to bytes on disk must
//! update `FORMAT.md` and the golden-file corpus under `tests/golden/` in the
//! same commit.

#[doc(hidden)]
pub mod backend;
pub mod budget;
pub mod bulk;
pub mod catalog;
pub mod compact;
pub mod csr_group;
pub mod free_pages;
pub mod fulltext;
pub mod geo_column;
pub mod hnsw;
pub mod lock;
pub mod node_group;
pub mod node_table;
pub mod overlay;
#[cfg(feature = "pack")]
pub mod pack;
pub mod pager;
pub mod rel_table;
pub mod superblock;
pub mod txn_log;
pub mod vector_encoding;
pub mod wal;
