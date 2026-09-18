//! Core value, schema, and error types shared by every devondb crate.
//!
//! This crate is the dependency root of the workspace: it must stay small,
//! std-only at its core, and free of engine logic. Types that belong here:
//! logical values, the schema model (node/rel tables, columns, `VECTOR(dim)`),
//! and the workspace-wide error type.

pub mod arrow;
pub mod column;
pub mod decimal;
mod error;
pub mod geo_point;
pub mod logical_type;
pub mod schema;
pub mod value;

pub use decimal::Decimal128;
pub use error::{DevonError, DevonResult};
pub use geo_point::GeoPoint;
