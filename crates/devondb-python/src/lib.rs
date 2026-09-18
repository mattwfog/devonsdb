//! Python bindings for devondb, exposing the embedded database as the
//! `devondb` Python module (init symbol `PyInit_devondb`; the built
//! `libdevondb_python` artifact is renamed to `devondb.so` on install).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use devondb::text::parser::{Parsed, parse};
use devondb::{Database as EmbeddedDatabase, Options, QueryResult, StatementEnvelope, Value};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyBytes, PyCapsule, PyDateTime, PyDelta, PyDict, PyDictMethods, PyList, PyListMethods,
    PyModule, PyType, PyTzInfo,
};
use serde_json::{Map, Value as JsonValue};

mod arrow;

/// An open embedded devondb database.
///
/// A live handle holds the file's cross-process writer lease
/// (`docs/MULTIPROCESS.md`), so it must be released deterministically:
/// leaving a `with` block or calling `close()` drops the engine handle —
/// Python reference lifetime alone is not the release point.
#[pyclass(name = "Database", module = "devondb")]
struct PyDatabase {
    database: Mutex<Option<EmbeddedDatabase>>,
}

#[pymethods]
impl PyDatabase {
    /// Opens an existing database with default options.
    #[new]
    #[pyo3(signature = (path))]
    fn new(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        py.detach(move || EmbeddedDatabase::open(path))
            .map(Self::from_database)
            .map_err(runtime_error)
    }

    /// Creates a new database with explicit options.
    #[classmethod]
    #[pyo3(signature = (path, *, page_size = 4096, memory_limit = 67_108_864))]
    fn create(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        path: PathBuf,
        page_size: u32,
        memory_limit: usize,
    ) -> PyResult<Self> {
        let options = binding_options(page_size, memory_limit);
        py.detach(move || EmbeddedDatabase::create_with(path, options))
            .map(Self::from_database)
            .map_err(runtime_error)
    }

    /// Opens an existing database with explicit options.
    #[classmethod]
    #[pyo3(signature = (path, *, page_size = 4096, memory_limit = 67_108_864))]
    fn open(
        _cls: &Bound<'_, PyType>,
        py: Python<'_>,
        path: PathBuf,
        page_size: u32,
        memory_limit: usize,
    ) -> PyResult<Self> {
        let options = binding_options(page_size, memory_limit);
        py.detach(move || EmbeddedDatabase::open_with(path, options))
            .map(Self::from_database)
            .map_err(runtime_error)
    }

    /// Parses and executes one DevonPlan statement.
    fn execute(&self, py: Python<'_>, text: &str) -> PyResult<()> {
        self.check_open().map_err(runtime_error)?;
        let statement = statement_from_text(text)?;
        py.detach(|| match self.lock_engine() {
            Ok(mut guard) => match guard.as_mut() {
                Some(database) => database.execute(&statement.stmt).map_err(runtime_error),
                None => Err(closed_error()),
            },
            Err(message) => Err(runtime_error(message)),
        })
    }

    /// Parses and runs one DevonPlan query.
    fn query(&self, py: Python<'_>, text: &str) -> PyResult<PyQueryResult> {
        self.check_open().map_err(runtime_error)?;
        let plan = query_from_text(text)?;
        let result = py.detach(|| match self.lock_engine() {
            Ok(mut guard) => match guard.as_mut() {
                Some(database) => database.run(&plan).map_err(runtime_error),
                None => Err(closed_error()),
            },
            Err(message) => Err(runtime_error(message)),
        })?;
        convert_query_result(result)
    }

    /// Returns the committed schema as nested Python dictionaries and lists.
    fn schema<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let summary = py.detach(|| match self.lock_engine() {
            Ok(guard) => match guard.as_ref() {
                Some(database) => Ok(database.schema_summary()),
                None => Err(closed_error()),
            },
            Err(message) => Err(runtime_error(message)),
        })?;
        let json = serde_json::to_value(summary).map_err(runtime_error)?;
        json_to_python(py, &json)
    }

    /// Materializes committed changes in the main database file.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| match self.lock_engine() {
            Ok(mut guard) => match guard.as_mut() {
                Some(database) => database.checkpoint().map_err(runtime_error),
                None => Err(closed_error()),
            },
            Err(message) => Err(runtime_error(message)),
        })
    }

    /// Closes the database, releasing its writer lease. Idempotent; any
    /// later operation on this handle raises `RuntimeError`.
    fn close(&self, py: Python<'_>) {
        py.detach(|| {
            // `into_inner` here only drops the engine (releasing the writer
            // lease) after a panic poisoned the lock; it never resumes
            // operating on the possibly-torn state, unlike `lock_engine`.
            let mut guard = self.database.lock().unwrap_or_else(PoisonError::into_inner);
            drop(guard.take());
        });
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __exit__(
        &self,
        py: Python<'_>,
        _exc_type: &Bound<'_, PyAny>,
        _exc_value: &Bound<'_, PyAny>,
        _traceback: &Bound<'_, PyAny>,
    ) -> bool {
        self.close(py);
        false
    }
}

impl PyDatabase {
    fn from_database(database: EmbeddedDatabase) -> Self {
        Self {
            database: Mutex::new(Some(database)),
        }
    }

    /// Locks the engine, refusing a poisoned handle: poison means an
    /// earlier operation panicked mid-call, so the handle is rejected
    /// instead of resumed via `PoisonError::into_inner`.
    fn lock_engine(&self) -> Result<MutexGuard<'_, Option<EmbeddedDatabase>>, &'static str> {
        self.database
            .lock()
            .map_err(|_| "engine poisoned by an earlier panic; reopen the database")
    }

    /// The docstring contract — every operation on a closed handle raises
    /// `RuntimeError` — is checked before parsing, so invalid text on a
    /// closed handle never surfaces as a `ValueError`. Kept pyo3-free so
    /// the error mapping is unit-testable without an interpreter.
    fn check_open(&self) -> Result<(), &'static str> {
        self.lock_engine()?.as_ref().ok_or("database is closed")?;
        Ok(())
    }
}

/// Materialized rows and their output column names.
#[pyclass(name = "QueryResult", module = "devondb")]
pub(crate) struct PyQueryResult {
    pub(crate) columns: Vec<String>,
    /// Rows in native-Python form, backing the `rows` getter.
    pub(crate) rows: Vec<Vec<NaturalValue>>,
    /// The engine's own rows, kept lossless (no `NaturalValue` detour) so
    /// the Arrow export in `arrow.rs` sees the exact `Value`s.
    pub(crate) values: Vec<Vec<Value>>,
}

#[pymethods]
impl PyQueryResult {
    /// Output column names in row-value order.
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    /// Materialized rows using native Python values.
    #[getter]
    fn rows<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let rows = PyList::empty(py);
        for row in &self.rows {
            let values = PyList::empty(py);
            for value in row {
                append_natural_value(py, &values, value)?;
            }
            rows.append(values)?;
        }
        Ok(rows)
    }

    /// Exports the result schema as an Arrow PyCapsule Interface capsule
    /// named `"arrow_schema"` (an Arrow C Data Interface struct type whose
    /// children are the result columns).
    fn __arrow_c_schema__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyCapsule>> {
        arrow::arrow_c_schema(py, self)
    }

    /// Exports the result as an Arrow PyCapsule Interface `(schema, array)`
    /// capsule pair named `"arrow_schema"` / `"arrow_array"`, so
    /// `pyarrow.table(result)` and `polars.from_arrow(result)` work.
    /// `requested_schema` is accepted and ignored: the PyCapsule Interface
    /// explicitly permits returning the same schema as if None were passed,
    /// and each devondb column type has exactly one Arrow representation.
    #[pyo3(signature = (requested_schema=None))]
    fn __arrow_c_array__<'py>(
        &self,
        py: Python<'py>,
        requested_schema: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<(Bound<'py, PyCapsule>, Bound<'py, PyCapsule>)> {
        let _ = requested_schema;
        arrow::arrow_c_array(py, self)
    }
}

pub(crate) enum NaturalValue {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    String(String),
    Vector(Vec<f32>),
    /// A GeoPoint surfaces in Python as a `(lat_deg, lng_deg)` tuple.
    GeoPoint(f64, f64),
    /// A Timestamp surfaces as an aware `datetime.datetime` in
    /// `datetime.timezone.utc`: Python's standard temporal type preserves
    /// the engine's UTC epoch-microsecond precision without a float boundary.
    Timestamp(i64),
    /// A Bytes value surfaces as Python's immutable `bytes` type.
    Bytes(Vec<u8>),
    /// A Decimal surfaces as `decimal.Decimal`; the canonical decimal string
    /// crosses the boundary so its digits and scale never pass through float.
    Decimal(String),
    /// A Json value surfaces as the recursively parsed native Python object.
    Json(JsonValue),
}

fn binding_options(page_size: u32, memory_limit: usize) -> Options {
    Options {
        page_size,
        memory_limit,
    }
}

fn statement_from_text(text: &str) -> PyResult<StatementEnvelope> {
    match parse(text).map_err(runtime_error)? {
        Parsed::Statement(statement) => Ok(statement),
        Parsed::Query(_) => Err(PyValueError::new_err("expected a statement, found a query")),
    }
}

fn query_from_text(text: &str) -> PyResult<devondb::Plan> {
    match parse(text).map_err(runtime_error)? {
        Parsed::Query(plan) => Ok(plan),
        Parsed::Statement(_) => Err(PyValueError::new_err("expected a query, found a statement")),
    }
}

fn convert_query_result(result: QueryResult) -> PyResult<PyQueryResult> {
    let mut rows = Vec::with_capacity(result.rows.len());
    let mut values = Vec::with_capacity(result.rows.len());
    for row in result.rows {
        let mut converted = Vec::with_capacity(row.len());
        for value in &row {
            converted.push(natural_value(value).map_err(conversion_error)?);
        }
        rows.push(converted);
        values.push(row);
    }
    Ok(PyQueryResult {
        columns: result.columns,
        rows,
        values,
    })
}

/// Converts one typed engine value to its native-Python form. Matching on
/// the `Value` variant keeps non-finite floats typed end to end — no Debug
/// string, no JSON detour, no per-cell `String` (a unit test asserts the
/// scalar arms do not allocate). Kept pyo3-free so the conversion is
/// unit-testable without an interpreter; the caller wraps the message with
/// `conversion_error`.
fn natural_value(value: &Value) -> Result<NaturalValue, String> {
    match value {
        Value::Null => Ok(NaturalValue::Null),
        Value::Bool(value) => Ok(NaturalValue::Bool(*value)),
        Value::Int64(value) => Ok(NaturalValue::Int64(*value)),
        Value::Float64(value) => Ok(NaturalValue::Float64(*value)),
        Value::String(value) => Ok(NaturalValue::String(value.clone())),
        Value::Vector(values) => Ok(NaturalValue::Vector(values.clone())),
        Value::GeoPoint(point) => Ok(NaturalValue::GeoPoint(point.lat_deg(), point.lng_deg())),
        Value::Timestamp(micros) => Ok(NaturalValue::Timestamp(*micros)),
        Value::Bytes(bytes) => Ok(NaturalValue::Bytes(bytes.clone())),
        Value::Decimal(decimal) => Ok(NaturalValue::Decimal(decimal.to_string())),
        Value::Json(text) => serde_json::from_str(text)
            .map(NaturalValue::Json)
            .map_err(|error| format!("invalid serialized Json text: {error}")),
    }
}

fn append_natural_value(
    py: Python<'_>,
    list: &Bound<'_, PyList>,
    value: &NaturalValue,
) -> PyResult<()> {
    match value {
        NaturalValue::Null => list.append(py.None()),
        NaturalValue::Bool(value) => list.append(*value),
        NaturalValue::Int64(value) => list.append(*value),
        NaturalValue::Float64(value) => list.append(*value),
        NaturalValue::String(value) => list.append(value),
        NaturalValue::Vector(value) => list.append(PyList::new(py, value.iter().copied())?),
        NaturalValue::GeoPoint(lat_deg, lng_deg) => list.append((*lat_deg, *lng_deg)),
        NaturalValue::Timestamp(micros) => list.append(timestamp_to_python(py, *micros)?),
        NaturalValue::Bytes(value) => list.append(PyBytes::new(py, value)),
        NaturalValue::Decimal(value) => list.append(decimal_to_python(py, value)?),
        NaturalValue::Json(value) => list.append(json_to_python(py, value)?),
    }
}

fn timestamp_to_python<'py>(py: Python<'py>, micros: i64) -> PyResult<Bound<'py, PyAny>> {
    const MICROS_PER_SECOND: i64 = 1_000_000;
    const SECONDS_PER_DAY: i64 = 86_400;
    const MICROS_PER_DAY: i64 = MICROS_PER_SECOND * SECONDS_PER_DAY;

    let days = i32::try_from(micros.div_euclid(MICROS_PER_DAY))
        .map_err(|_| conversion_error("Timestamp day count exceeds Python timedelta range"))?;
    let day_micros = micros.rem_euclid(MICROS_PER_DAY);
    let seconds = i32::try_from(day_micros / MICROS_PER_SECOND)
        .map_err(|_| conversion_error("Timestamp second count exceeds Python timedelta range"))?;
    let microseconds = i32::try_from(day_micros % MICROS_PER_SECOND).map_err(|_| {
        conversion_error("Timestamp microsecond count exceeds Python timedelta range")
    })?;
    let utc = PyTzInfo::utc(py)?;
    let epoch = PyDateTime::new(py, 1970, 1, 1, 0, 0, 0, 0, Some(&utc))?;
    let delta = PyDelta::new(py, days, seconds, microseconds, false)?;
    epoch.add(delta)
}

fn decimal_to_python<'py>(py: Python<'py>, value: &str) -> PyResult<Bound<'py, PyAny>> {
    PyModule::import(py, "decimal")?
        .getattr("Decimal")?
        .call1((value,))
}

fn json_to_python<'py>(py: Python<'py>, value: &JsonValue) -> PyResult<Bound<'py, PyAny>> {
    match value {
        JsonValue::Null => Ok(py.None().into_bound(py)),
        JsonValue::Bool(value) => value.into_bound_py_any(py),
        JsonValue::Number(value) => json_number_to_python(py, value),
        JsonValue::String(value) => value.into_bound_py_any(py),
        JsonValue::Array(values) => {
            let list = PyList::empty(py);
            for value in values {
                list.append(json_to_python(py, value)?)?;
            }
            Ok(list.into_any())
        }
        JsonValue::Object(values) => json_object_to_python(py, values),
    }
}

fn json_number_to_python<'py>(
    py: Python<'py>,
    value: &serde_json::Number,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(value) = value.as_i64() {
        value.into_bound_py_any(py)
    } else if let Some(value) = value.as_u64() {
        value.into_bound_py_any(py)
    } else if let Some(value) = value.as_f64() {
        value.into_bound_py_any(py)
    } else {
        Err(conversion_error("invalid JSON number"))
    }
}

fn json_object_to_python<'py>(
    py: Python<'py>,
    values: &Map<String, JsonValue>,
) -> PyResult<Bound<'py, PyAny>> {
    let dict = PyDict::new(py);
    for (key, value) in values {
        dict.set_item(key, json_to_python(py, value)?)?;
    }
    Ok(dict.into_any())
}

pub(crate) fn runtime_error(error: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(error.to_string())
}

fn closed_error() -> PyErr {
    PyRuntimeError::new_err("database is closed")
}

fn conversion_error(message: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(format!("database value conversion failed: {message}"))
}

/// The embedded devondb Python module.
#[pymodule(name = "devondb")]
fn devondb_module(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyDatabase>()?;
    module.add_class::<PyQueryResult>()?;
    Ok(())
}

#[cfg(test)]
#[allow(
    unsafe_code,
    reason = "the allocation-counting test allocator requires unsafe GlobalAlloc impls"
)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    thread_local! {
        /// Allocations made by THIS thread. The global counter above sees
        /// the libtest harness threads too (their reporting allocates while
        /// a test body runs), which made the structural assert flaky; the
        /// per-thread count is what the conversion test reads. Const-
        /// initialized so touching it from inside the allocator never
        /// allocates.
        static THREAD_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Counts every allocation so the conversion test can assert structure
    /// (no per-cell `String`) instead of timing.
    struct CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            let _ = THREAD_ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    /// Serializes the unit tests so a concurrent test's allocations cannot
    /// land inside another test's counting window.
    static SERIALIZER: Mutex<()> = Mutex::new(());

    #[test]
    fn poisoned_engine_lock_refuses_the_handle() {
        let _serial = SERIALIZER.lock().unwrap_or_else(PoisonError::into_inner);
        let handle = PyDatabase {
            database: Mutex::new(None),
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _guard = handle.database.lock();
            panic!("simulated panic while holding the engine lock");
        }));
        assert!(outcome.is_err(), "the simulated panic must unwind");
        assert!(handle.database.is_poisoned());

        let message = match handle.lock_engine() {
            Ok(_) => panic!("a poisoned engine lock must refuse the handle"),
            Err(message) => message,
        };
        assert_eq!(
            message,
            "engine poisoned by an earlier panic; reopen the database"
        );
    }

    #[test]
    fn closed_handle_fails_check_open_before_parsing() {
        let _serial = SERIALIZER.lock().unwrap_or_else(PoisonError::into_inner);
        let handle = PyDatabase {
            database: Mutex::new(None),
        };
        let message = match handle.check_open() {
            Ok(()) => panic!("a closed handle must fail check_open"),
            Err(message) => message,
        };
        assert_eq!(message, "database is closed");
    }

    #[test]
    fn non_finite_and_negative_zero_floats_round_trip_typed() {
        let _serial = SERIALIZER.lock().unwrap_or_else(PoisonError::into_inner);
        let converted = |input: f64| match natural_value(&Value::Float64(input)).unwrap() {
            NaturalValue::Float64(output) => output,
            _ => panic!("Float64 must convert to NaturalValue::Float64"),
        };
        assert!(converted(f64::NAN).is_nan());
        assert_eq!(converted(f64::INFINITY), f64::INFINITY);
        assert_eq!(converted(f64::NEG_INFINITY), f64::NEG_INFINITY);
        let negative_zero = converted(-0.0);
        assert!(negative_zero == 0.0 && negative_zero.is_sign_negative());
    }

    #[test]
    fn scalar_cells_convert_without_allocating_a_string() {
        let _serial = SERIALIZER.lock().unwrap_or_else(PoisonError::into_inner);
        let cells = [
            Value::Null,
            Value::Bool(true),
            Value::Int64(-7),
            Value::Float64(f64::NAN),
            Value::Float64(f64::INFINITY),
            Value::Float64(f64::NEG_INFINITY),
            Value::Float64(-0.0),
            Value::Timestamp(1_723_161_600_123_456),
        ];
        let before = THREAD_ALLOCATIONS.with(std::cell::Cell::get);
        for cell in &cells {
            let converted = natural_value(cell).unwrap();
            std::hint::black_box(converted);
        }
        let allocated = THREAD_ALLOCATIONS.with(std::cell::Cell::get) - before;
        assert_eq!(
            allocated, 0,
            "scalar cell conversion allocated {allocated} times"
        );
    }
}
