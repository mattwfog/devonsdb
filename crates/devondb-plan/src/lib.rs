//! DevonPlan IR: the typed, versioned logical plan language of devondb.
//!
//! This is the *real* query language of the engine — the thing the executor
//! executes, bindings emit, the wire protocol carries, and golden tests pin.
//! The natural-language surface compiles *to* DevonPlan; the engine itself
//! never depends on an LLM. Spec: `docs/PLAN_IR.md`. Canonical forms: compact
//! text and JSON, both stable and versioned.

pub mod csv;
pub mod expr;
pub mod ops;
pub mod statement;
pub mod text;
pub mod typing;
pub mod validate;
