//! devondb — embedded graph database with a stable on-disk format, MVCC
//! snapshot isolation, first-class vectors, and a natural-language query
//! surface over a deterministic logical-plan IR.
//!
//! This crate is the public embedded API: open/create a database, run
//! DevonPlan queries, manage transactions. Architecture: `docs/ARCHITECTURE.md`.

pub mod database;
mod graph;
pub mod introspect;
pub mod txn;

pub use database::{Database, Options, QueryResult};
pub use devondb_plan::ops::Plan;
pub use devondb_plan::statement::{Statement, StatementEnvelope};
pub use devondb_plan::text;
pub use devondb_types::arrow;
pub use devondb_types::schema::{did_you_mean, fold, suggestion_suffix};
pub use devondb_types::{DevonError, DevonResult};
pub use devondb_types::{logical_type::LogicalType, value::Value};
pub use introspect::{ColumnSummary, NodeTableSummary, RelTableSummary, SchemaSummary};
