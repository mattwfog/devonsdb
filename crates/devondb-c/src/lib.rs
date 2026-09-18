//! C FFI for devondb: `include/devondb.h` is the contract; every exported
//! symbol is `devondb_`-prefixed and panic-free at the boundary.

#![allow(
    unsafe_code,
    reason = "C FFI exports require unsafe extern fns and raw-pointer marshalling"
)]

use std::any::Any;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Mutex;

use devondb::text::parser::{Parsed, parse};
use devondb::{Database, Options as DevonOptions, QueryResult, Value};
use serde_json::{Map, Value as JsonValue, json};

pub mod arrow;

pub use arrow::{ArrowArray, ArrowSchema};

type FfiResult<T> = Result<T, String>;

/// Options for `devondb_create_with` and `devondb_open_with`.
///
/// `size_bytes` is the struct size in bytes and lets the library grow the
/// struct without breaking older callers. Zero-valued fields select the
/// library defaults.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DevondbOptions {
    /// Size of this struct in bytes; must be at least
    /// `size_of::<DevondbOptions>()`.
    pub size_bytes: usize,
    /// Page size in bytes; used on create only. Zero selects 4096.
    pub page_size: u32,
    /// Memory budget in bytes; minimum 1 MiB. Zero selects 64 MiB.
    pub memory_limit: usize,
}

impl Default for DevondbOptions {
    fn default() -> Self {
        Self {
            size_bytes: size_of::<Self>(),
            page_size: 0,
            memory_limit: 0,
        }
    }
}

impl DevondbOptions {
    fn engine_options(&self) -> FfiResult<DevonOptions> {
        if self.size_bytes < size_of::<Self>() {
            return Err(format!(
                "devondb_options.size_bytes is {} but the library requires at least {}",
                self.size_bytes,
                size_of::<Self>()
            ));
        }
        Ok(DevonOptions {
            page_size: if self.page_size == 0 {
                4096
            } else {
                self.page_size
            },
            memory_limit: if self.memory_limit == 0 {
                DevonOptions::default().memory_limit
            } else {
                self.memory_limit
            },
        })
    }
}

/// Status returned by fallible devondb C API calls.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevondbStatus {
    /// The operation completed successfully.
    Ok = 0,
    /// The operation failed; consult `devondb_last_error` when a handle exists.
    Err = 1,
}

/// Opaque database handle owned by C callers.
///
/// The mutex serializes all handle state so that concurrent misuse of one
/// handle degrades into serialized calls instead of a data race on
/// `last_error` or the engine; the supported contract remains one handle
/// per thread (handles are `Send`, not `Sync`-safe for lock-free use).
#[repr(C)]
pub struct DevondbDatabase {
    state: Mutex<DevondbHandleState>,
}

struct DevondbHandleState {
    database: Database,
    last_error: CString,
}

fn empty_error() -> *const c_char {
    c"".as_ptr()
}

fn sanitized_c_string(message: String) -> CString {
    match CString::new(message) {
        Ok(message) => message,
        Err(error) => {
            let bytes = error
                .into_vec()
                .into_iter()
                .filter(|byte| *byte != 0)
                .collect::<Vec<_>>();
            CString::new(bytes).unwrap_or_default()
        }
    }
}

fn panic_message(function: &str, payload: &(dyn Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload");
    format!("panic in {function}: {detail}")
}

/// Panic boundary for handle-less calls (`devondb_open`/`devondb_create`
/// and their `_with` forms): errors are reported only through the status
/// code because a failed open/create yields no handle to store them on.
fn status_boundary(operation: impl FnOnce() -> FfiResult<()>) -> DevondbStatus {
    let guarded = catch_unwind(AssertUnwindSafe(|| {
        match catch_unwind(AssertUnwindSafe(operation)) {
            Ok(Ok(())) => DevondbStatus::Ok,
            Ok(Err(_)) | Err(_) => DevondbStatus::Err,
        }
    }));
    match guarded {
        Ok(status) => status,
        Err(_) => DevondbStatus::Err,
    }
}

/// Panic boundary for calls on a live handle. The handle mutex is held for
/// the whole call so clear-error → operation → store-error is atomic with
/// respect to other threads misusing the same handle.
fn handle_boundary(
    handle: *mut DevondbDatabase,
    function: &str,
    operation: impl FnOnce(&mut DevondbHandleState) -> FfiResult<()>,
) -> DevondbStatus {
    let guarded = catch_unwind(AssertUnwindSafe(|| {
        let Some(handle) = (unsafe { handle.as_ref() }) else {
            return DevondbStatus::Err;
        };
        let Ok(mut state) = handle.state.lock() else {
            // A poisoned handle means an earlier call panicked while holding
            // the lock; refuse further use rather than resume torn state.
            return DevondbStatus::Err;
        };
        state.last_error = CString::default();
        match catch_unwind(AssertUnwindSafe(|| operation(&mut state))) {
            Ok(Ok(())) => DevondbStatus::Ok,
            Ok(Err(error)) => {
                state.last_error = sanitized_c_string(error);
                DevondbStatus::Err
            }
            Err(payload) => {
                state.last_error = sanitized_c_string(panic_message(function, payload.as_ref()));
                DevondbStatus::Err
            }
        }
    }));
    match guarded {
        Ok(status) => status,
        Err(_) => DevondbStatus::Err,
    }
}

unsafe fn input_string(pointer: *const c_char, argument: &str) -> FfiResult<String> {
    if pointer.is_null() {
        return Err(format!("{argument} must not be NULL"));
    }
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| format!("{argument} must be valid UTF-8"))
}

unsafe fn initialize_handle_out(
    out: *mut *mut DevondbDatabase,
) -> FfiResult<*mut *mut DevondbDatabase> {
    if out.is_null() {
        return Err("out must not be NULL".to_owned());
    }
    unsafe { *out = ptr::null_mut() };
    Ok(out)
}

unsafe fn initialize_string_out(out: *mut *mut c_char) -> FfiResult<*mut *mut c_char> {
    if out.is_null() {
        return Err("out_json must not be NULL".to_owned());
    }
    unsafe { *out = ptr::null_mut() };
    Ok(out)
}

unsafe fn initialize_arrow_schema_out(out: *mut ArrowSchema) -> FfiResult<*mut ArrowSchema> {
    if out.is_null() {
        return Err("out_schema must not be NULL".to_owned());
    }
    // Leave the out-parameter in the released state so a consumer may call
    // `release` unconditionally, even after a failed export.
    unsafe { ptr::write(out, ArrowSchema::released()) };
    Ok(out)
}

unsafe fn initialize_arrow_array_out(out: *mut ArrowArray) -> FfiResult<*mut ArrowArray> {
    if out.is_null() {
        return Err("out_array must not be NULL".to_owned());
    }
    // Leave the out-parameter in the released state so a consumer may call
    // `release` unconditionally, even after a failed export.
    unsafe { ptr::write(out, ArrowArray::released()) };
    Ok(out)
}

unsafe fn return_string(value: String, out: *mut *mut c_char) -> FfiResult<()> {
    let value = CString::new(value)
        .map_err(|_| "serialized JSON unexpectedly contained a NUL byte".to_owned())?;
    unsafe { *out = value.into_raw() };
    Ok(())
}

fn natural_value(tagged: JsonValue) -> FfiResult<JsonValue> {
    match tagged {
        JsonValue::String(tag) if tag == "Null" => Ok(JsonValue::Null),
        JsonValue::Object(object) => natural_tagged_value(object),
        other => Err(format!("unexpected serialized database value: {other}")),
    }
}

fn natural_tagged_value(object: Map<String, JsonValue>) -> FfiResult<JsonValue> {
    let mut entries = object.into_iter();
    let Some((variant, value)) = entries.next() else {
        return Err("serialized database value has no variant".to_owned());
    };
    if entries.next().is_some() {
        return Err("serialized database value has multiple variants".to_owned());
    }
    match variant.as_str() {
        // Non-finite floats never reach these arms: `natural_json` tags
        // them from the typed value before serde can collapse them to null.
        "Bool" | "Int64" | "String" | "Vector" | "Float64" => Ok(value),
        // Keep the C API byte-compatible with the UI's natural GeoPoint form.
        "GeoPoint" => Ok(JsonValue::Object(Map::from_iter([(
            "geo".to_owned(),
            value,
        )]))),
        // Scalar-v2 values use the tagged natural spellings from DevonPlan.
        // Timestamp remains an i64 JSON token and Decimal remains a string,
        // so neither exact type crosses a floating-point boundary.
        "Timestamp" => Ok(natural_object("ts", value)),
        "Bytes" => natural_bytes(value),
        "Decimal" => Ok(natural_object("decimal", value)),
        "Json" => Ok(natural_object("json", value)),
        _ => Err(format!(
            "unknown serialized database value variant `{variant}`"
        )),
    }
}

/// Serializes a non-finite float so it cannot collide with SQL NULL:
/// `{"f64":"NaN"}` / `{"f64":"inf"}` / `{"f64":"-inf"}` (documented in
/// `include/devondb.h`).
fn nonfinite_float(number: f64) -> JsonValue {
    let text = if number.is_nan() {
        "NaN"
    } else if number == f64::INFINITY {
        "inf"
    } else {
        "-inf"
    };
    natural_object("f64", JsonValue::String(text.to_owned()))
}

/// Serializes a Vector, tagging any non-finite element with the same
/// `{"f64":...}` spelling so it cannot collapse into JSON null.
fn natural_vector(elements: &[f32]) -> JsonValue {
    elements
        .iter()
        .map(|element| {
            if element.is_finite() {
                JsonValue::from(*element)
            } else {
                nonfinite_float(f64::from(*element))
            }
        })
        .collect()
}

fn natural_object(tag: &str, value: JsonValue) -> JsonValue {
    JsonValue::Object(Map::from_iter([(tag.to_owned(), value)]))
}

fn natural_bytes(value: JsonValue) -> FfiResult<JsonValue> {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

    let bytes = serde_json::from_value::<Vec<u8>>(value)
        .map_err(|error| format!("invalid serialized Bytes value: {error}"))?;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    Ok(natural_object("bytes", JsonValue::String(hex)))
}

fn query_result_json(result: QueryResult) -> FfiResult<String> {
    let rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(natural_json).collect::<FfiResult<Vec<_>>>())
        .collect::<FfiResult<Vec<_>>>()?;
    serde_json::to_string(&json!({
        "columns": result.columns,
        "rows": rows,
    }))
    .map_err(|error| error.to_string())
}

/// Converts one typed database value to its natural JSON form. Non-finite
/// floats are tagged from the typed value, because `serde_json::to_value`
/// would silently collapse them into the same `null` as SQL NULL.
fn natural_json(value: &Value) -> FfiResult<JsonValue> {
    match value {
        Value::Float64(number) if !number.is_finite() => Ok(nonfinite_float(*number)),
        Value::Vector(elements) => Ok(natural_vector(elements)),
        _ => serde_json::to_value(value)
            .map_err(|error| error.to_string())
            .and_then(natural_value),
    }
}

/// Opens an existing database.
///
/// # Safety
///
/// `path` must point to a readable NUL-terminated byte string and `out` must
/// point to writable storage for one database-handle pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_open(
    path: *const c_char,
    out: *mut *mut DevondbDatabase,
) -> DevondbStatus {
    status_boundary(|| {
        let out = unsafe { initialize_handle_out(out) }?;
        let path = unsafe { input_string(path, "path") }?;
        let database = Database::open(path).map_err(|error| error.to_string())?;
        unsafe { *out = new_handle(database) };
        Ok(())
    })
}

/// Opens an existing database with explicit options (NULL selects defaults).
///
/// # Safety
///
/// `path` must point to a readable NUL-terminated byte string, `options`
/// must point to a readable `DevondbOptions` (or be NULL for defaults), and
/// `out` must point to writable storage for one database-handle pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_open_with(
    path: *const c_char,
    options: *const DevondbOptions,
    out: *mut *mut DevondbDatabase,
) -> DevondbStatus {
    status_boundary(|| {
        let out = unsafe { initialize_handle_out(out) }?;
        let path = unsafe { input_string(path, "path") }?;
        let database = Database::open_with(path, unsafe { input_options(options) }?)
            .map_err(|error| error.to_string())?;
        unsafe { *out = new_handle(database) };
        Ok(())
    })
}

/// Creates a database with the requested page size.
///
/// # Safety
///
/// `path` must point to a readable NUL-terminated byte string and `out` must
/// point to writable storage for one database-handle pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_create(
    path: *const c_char,
    page_size: u32,
    out: *mut *mut DevondbDatabase,
) -> DevondbStatus {
    status_boundary(|| {
        let out = unsafe { initialize_handle_out(out) }?;
        let path = unsafe { input_string(path, "path") }?;
        let database = Database::create(path, page_size).map_err(|error| error.to_string())?;
        unsafe { *out = new_handle(database) };
        Ok(())
    })
}

/// Creates a new database with explicit options (NULL selects defaults).
///
/// # Safety
///
/// `path` must point to a readable NUL-terminated byte string, `options`
/// must point to a readable `DevondbOptions` (or be NULL for defaults), and
/// `out` must point to writable storage for one database-handle pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_create_with(
    path: *const c_char,
    options: *const DevondbOptions,
    out: *mut *mut DevondbDatabase,
) -> DevondbStatus {
    status_boundary(|| {
        let out = unsafe { initialize_handle_out(out) }?;
        let path = unsafe { input_string(path, "path") }?;
        let database = Database::create_with(path, unsafe { input_options(options) }?)
            .map_err(|error| error.to_string())?;
        unsafe { *out = new_handle(database) };
        Ok(())
    })
}

fn new_handle(database: Database) -> *mut DevondbDatabase {
    Box::into_raw(Box::new(DevondbDatabase {
        state: Mutex::new(DevondbHandleState {
            database,
            last_error: CString::default(),
        }),
    }))
}

unsafe fn input_options(options: *const DevondbOptions) -> FfiResult<DevonOptions> {
    let options = if options.is_null() {
        DevondbOptions::default()
    } else {
        unsafe { *options }
    };
    options.engine_options()
}

/// Parses and executes one DevonPlan text statement.
///
/// # Safety
///
/// `db` must be a live handle returned by this library, and `text` must point
/// to a readable NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_execute(
    db: *mut DevondbDatabase,
    text: *const c_char,
) -> DevondbStatus {
    handle_boundary(db, "devondb_execute", |state| {
        let text = unsafe { input_string(text, "text") }?;
        let statement = match parse(&text).map_err(|error| error.to_string())? {
            Parsed::Statement(statement) => statement,
            Parsed::Query(_) => return Err("expected a statement, found a query".to_owned()),
        };
        state
            .database
            .execute(&statement.stmt)
            .map_err(|error| error.to_string())
    })
}

/// Runs one DevonPlan text query and returns compact natural-value JSON.
///
/// # Safety
///
/// `db` must be a live handle returned by this library, `text` must point to
/// a readable NUL-terminated byte string, and `out_json` must point to
/// writable storage for one C-string pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_query_json(
    db: *mut DevondbDatabase,
    text: *const c_char,
    out_json: *mut *mut c_char,
) -> DevondbStatus {
    handle_boundary(db, "devondb_query_json", |state| {
        let out_json = unsafe { initialize_string_out(out_json) }?;
        let text = unsafe { input_string(text, "text") }?;
        let plan = match parse(&text).map_err(|error| error.to_string())? {
            Parsed::Query(plan) => plan,
            Parsed::Statement(_) => return Err("expected a query, found a statement".to_owned()),
        };
        let result = state
            .database
            .run(&plan)
            .map_err(|error| error.to_string())?;
        let json = query_result_json(result)?;
        unsafe { return_string(json, out_json) }
    })
}

/// Runs one DevonPlan text query and exports the result as an Arrow C Data
/// Interface struct array (`+s`) whose children are the result columns.
///
/// On success the caller owns `out_schema`/`out_array` and must call their
/// release callbacks when done (top-level only; each release releases its
/// children recursively). On failure both are left in the released state
/// (`release == NULL`), so calling `release` unconditionally is safe. The
/// devondb-to-Arrow type mapping is documented in `include/devondb.h` and
/// `crates/devondb-c/src/arrow.rs`.
///
/// # Safety
///
/// `db` must be a live handle returned by this library, `text` must point to
/// a readable NUL-terminated byte string, and `out_schema`/`out_array` must
/// point to writable storage for one `ArrowSchema`/`ArrowArray` each.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_query_arrow(
    db: *mut DevondbDatabase,
    text: *const c_char,
    out_schema: *mut ArrowSchema,
    out_array: *mut ArrowArray,
) -> DevondbStatus {
    handle_boundary(db, "devondb_query_arrow", |state| {
        let out_schema = unsafe { initialize_arrow_schema_out(out_schema) }?;
        let out_array = unsafe { initialize_arrow_array_out(out_array) }?;
        let text = unsafe { input_string(text, "text") }?;
        let plan = match parse(&text).map_err(|error| error.to_string())? {
            Parsed::Query(plan) => plan,
            Parsed::Statement(_) => return Err("expected a query, found a statement".to_owned()),
        };
        let result = state
            .database
            .run(&plan)
            .map_err(|error| error.to_string())?;
        let (schema, array) = arrow::export(&result)?;
        // SAFETY: both out pointers were null-checked above and point to
        // writable storage for one struct each.
        unsafe {
            ptr::write(out_schema, *schema);
            ptr::write(out_array, *array);
        }
        Ok(())
    })
}

/// Returns the database schema as compact JSON.
///
/// # Safety
///
/// `db` must be a live handle returned by this library, and `out_json` must
/// point to writable storage for one C-string pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_schema_json(
    db: *mut DevondbDatabase,
    out_json: *mut *mut c_char,
) -> DevondbStatus {
    handle_boundary(db, "devondb_schema_json", |state| {
        let out_json = unsafe { initialize_string_out(out_json) }?;
        let json = serde_json::to_string(&state.database.schema_summary())
            .map_err(|error| error.to_string())?;
        unsafe { return_string(json, out_json) }
    })
}

/// Returns the most recent error stored on a database handle.
///
/// A null handle returns an empty string rather than a null pointer.
///
/// # Safety
///
/// A non-null `db` must be a live handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_last_error(db: *const DevondbDatabase) -> *const c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        if db.is_null() {
            empty_error()
        } else {
            match unsafe { &*db }.state.lock() {
                Ok(state) => state.last_error.as_ptr(),
                Err(_) => empty_error(),
            }
        }
    })) {
        Ok(error) => error,
        Err(_) => empty_error(),
    }
}

/// Frees a string returned by a devondb JSON function.
///
/// # Safety
///
/// A non-null `string` must have been returned by `devondb_query_json` or
/// `devondb_schema_json`, and it must not have been freed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_string_free(string: *mut c_char) {
    let _guarded = catch_unwind(AssertUnwindSafe(|| {
        if !string.is_null() {
            drop(unsafe { CString::from_raw(string) });
        }
    }));
}

/// Closes and frees a database handle; a null pointer is a no-op.
///
/// # Safety
///
/// A non-null `db` must be a live handle returned by this library, and it
/// must not have been closed already.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn devondb_close(db: *mut DevondbDatabase) {
    let _guarded = catch_unwind(AssertUnwindSafe(|| {
        if !db.is_null() {
            drop(unsafe { Box::from_raw(db) });
        }
    }));
}
