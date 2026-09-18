//! Thin adapter over the canonical Arrow C Data Interface builder in
//! `devondb-types::arrow` (one builder, one home): the type mapping, buffer
//! layouts, and ownership contract are documented there. This module adapts
//! the engine's `QueryResult` and re-exports the C Data structs for the FFI
//! surface (`include/devondb.h`).

use devondb::QueryResult;

pub use devondb_types::arrow::{ArrowArray, ArrowSchema};

/// Exports a query result as an Arrow C Data Interface struct array: the
/// returned schema has format `+s` and one child per result column, the
/// returned array has `length == rows` and matching child arrays.
///
/// The caller owns both structs; all buffers and strings they point to are
/// producer-owned and freed by calling the structs' release callbacks
/// (top-level only — each release releases its children recursively).
///
/// # Errors
///
/// Returns an error message when a column mixes value types, a column name
/// is not representable as a C string, or a variable-length column exceeds
/// the 32-bit offset limit.
pub fn export(result: &QueryResult) -> Result<(Box<ArrowSchema>, Box<ArrowArray>), String> {
    devondb_types::arrow::export(&result.columns, &result.rows)
}
