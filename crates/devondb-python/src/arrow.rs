//! Arrow PyCapsule Interface export for query results
//! (<https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html>):
//! `QueryResult.__arrow_c_schema__` / `__arrow_c_array__` hand pyarrow,
//! polars, and DuckDB an Arrow C Data Interface struct array (`+s`) whose
//! children are the result columns — no arrow crates involved.
//!
//! This module is only the capsule glue: the canonical builder lives in
//! `devondb-types::arrow` (re-exported as `devondb::arrow`; one builder, one
//! home), where the type mapping and buffer layouts are documented. The
//! export runs over the engine's own `Value` rows kept on `PyQueryResult`,
//! before any `NaturalValue` conversion, so nothing crosses a lossy
//! boundary. The capsule ownership boundary: each capsule owns one top-level
//! struct box, and the capsule destructor calls its release callback if
//! non-NULL and then frees the struct, exactly as the PyCapsule Interface
//! requires.

#![allow(
    unsafe_code,
    reason = "Arrow C Data Interface capsules require raw-pointer marshalling"
)]

use std::ffi::c_void;
use std::ptr::NonNull;

use devondb::arrow::{ArrowArray, ArrowSchema};
use pyo3::ffi;
use pyo3::types::PyCapsule;
use pyo3::{Bound, PyResult, Python};

use crate::PyQueryResult;

/// Implements `QueryResult.__arrow_c_schema__`: a capsule named
/// `"arrow_schema"` owning a freshly exported schema tree. The paired array
/// the builder produces is released immediately — the schema-only entry
/// point has no consumer for it.
pub(crate) fn arrow_c_schema<'py>(
    py: Python<'py>,
    result: &PyQueryResult,
) -> PyResult<Bound<'py, PyCapsule>> {
    let (schema, array) =
        devondb::arrow::export(&result.columns, &result.values).map_err(crate::runtime_error)?;
    release_array(array);
    schema_capsule(py, schema)
}

/// Implements `QueryResult.__arrow_c_array__`: a `(schema, array)` capsule
/// pair named `"arrow_schema"` / `"arrow_array"` owning a fresh export.
pub(crate) fn arrow_c_array<'py>(
    py: Python<'py>,
    result: &PyQueryResult,
) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
    let (schema, array) =
        devondb::arrow::export(&result.columns, &result.values).map_err(crate::runtime_error)?;
    Ok((schema_capsule(py, schema)?, array_capsule(py, array)?))
}

fn schema_capsule<'py>(
    py: Python<'py>,
    schema: Box<ArrowSchema>,
) -> PyResult<Bound<'py, PyCapsule>> {
    let pointer = NonNull::from(Box::leak(schema)).cast::<c_void>();
    // SAFETY: `pointer` owns one Box<ArrowSchema> produced above; the
    // destructor releases it (once — the callback NULLs itself) and frees it.
    unsafe {
        PyCapsule::new_with_pointer_and_destructor(
            py,
            pointer,
            c"arrow_schema",
            Some(schema_capsule_destructor),
        )
    }
}

fn array_capsule<'py>(py: Python<'py>, array: Box<ArrowArray>) -> PyResult<Bound<'py, PyCapsule>> {
    let pointer = NonNull::from(Box::leak(array)).cast::<c_void>();
    // SAFETY: `pointer` owns one Box<ArrowArray> produced above; the
    // destructor releases it (once — the callback NULLs itself) and frees it.
    unsafe {
        PyCapsule::new_with_pointer_and_destructor(
            py,
            pointer,
            c"arrow_array",
            Some(array_capsule_destructor),
        )
    }
}

/// Releases a boxed array that no capsule will own (the schema-only entry
/// point): runs its release callback, which frees every buffer recursively,
/// then frees the top-level struct box.
fn release_array(array: Box<ArrowArray>) {
    let pointer = Box::into_raw(array);
    // SAFETY: `pointer` is a live exported struct whose release callback has
    // not run yet; it is reclaimed here exactly once.
    unsafe {
        if let Some(release) = (*pointer).release {
            release(pointer);
        }
        drop(Box::from_raw(pointer));
    }
}

unsafe extern "C" fn schema_capsule_destructor(capsule: *mut ffi::PyObject) {
    // SAFETY: the capsule was created by `schema_capsule` with this exact
    // name; it runs with the GIL held during capsule finalization.
    let pointer = unsafe { ffi::PyCapsule_GetPointer(capsule, c"arrow_schema".as_ptr()) };
    if pointer.is_null() {
        return;
    }
    let schema = pointer.cast::<ArrowSchema>();
    // SAFETY: the pointer came from Box::leak in `schema_capsule` and is
    // reclaimed here exactly once (capsule destructors run at most once).
    unsafe {
        if let Some(release) = (*schema).release {
            release(schema);
        }
        drop(Box::from_raw(schema));
    }
}

unsafe extern "C" fn array_capsule_destructor(capsule: *mut ffi::PyObject) {
    // SAFETY: the capsule was created by `array_capsule` with this exact
    // name; it runs with the GIL held during capsule finalization.
    let pointer = unsafe { ffi::PyCapsule_GetPointer(capsule, c"arrow_array".as_ptr()) };
    if pointer.is_null() {
        return;
    }
    let array = pointer.cast::<ArrowArray>();
    // SAFETY: the pointer came from Box::leak in `array_capsule` and is
    // reclaimed here exactly once (capsule destructors run at most once).
    unsafe {
        if let Some(release) = (*array).release {
            release(array);
        }
        drop(Box::from_raw(array));
    }
}
