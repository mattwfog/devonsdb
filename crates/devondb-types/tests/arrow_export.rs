//! Struct-walk test for the canonical Arrow C Data Interface builder
//! (`devondb_types::arrow`), moved here from `devondb-c/tests/arrow_smoke.rs`
//! when the builder got its single home. The expected format strings and
//! buffer bytes below are the constants captured from the pre-move exporter:
//! the inputs mirror that test's seeded query result exactly (two rows,
//! eleven columns covering every mapped type), so this walk is the
//! byte-identity check for the move.

#![allow(
    unsafe_code,
    reason = "walking exported Arrow C Data Interface structs requires raw-pointer reads"
)]

use std::ffi::{CStr, c_char, c_void};

use devondb_types::arrow::{ArrowArray, ArrowSchema, export};
use devondb_types::value::Value;
use devondb_types::{Decimal128, GeoPoint};

const TIMESTAMP_MICROS: i64 = 1_723_161_600_123_456;

/// The same inputs the pre-move smoke test seeded through a database:
/// `(1, true, -42, 1.5, "devon", [1.0, 2.0, 3.0], geo(45.5, -122.625),
/// timestamp("2024-08-09T00:00:00.123456Z"), bytes("00ff1a"),
/// decimal("12345678.90"), json("{\"k\":1}"), null)` and an all-null row.
fn fixture() -> (Vec<String>, Vec<Vec<Value>>) {
    let columns = [
        "flag",
        "num",
        "score",
        "name",
        "embedding",
        "place",
        "happened",
        "payload",
        "amount",
        "document",
        "nothing",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect();
    let first = vec![
        Value::Bool(true),
        Value::Int64(-42),
        Value::Float64(1.5),
        Value::String("devon".to_owned()),
        Value::Vector(vec![1.0, 2.0, 3.0]),
        Value::GeoPoint(GeoPoint::from_canonical(45.5, -122.625).unwrap()),
        Value::Timestamp(TIMESTAMP_MICROS),
        Value::Bytes(vec![0x00, 0xff, 0x1a]),
        Value::Decimal(Decimal128::new(1_234_567_890, 2).unwrap()),
        Value::Json("{\"k\":1}".to_owned()),
        Value::Null,
    ];
    let second = vec![Value::Null; 11];
    (columns, vec![first, second])
}

unsafe fn text(pointer: *const c_char) -> String {
    assert!(!pointer.is_null());
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .unwrap()
        .to_owned()
}

unsafe fn schema_child(schema: *mut ArrowSchema, index: usize) -> *mut ArrowSchema {
    unsafe {
        assert!((index as i64) < (*schema).n_children);
        *(*schema).children.add(index)
    }
}

unsafe fn array_child(array: *mut ArrowArray, index: usize) -> *mut ArrowArray {
    unsafe {
        assert!((index as i64) < (*array).n_children);
        *(*array).children.add(index)
    }
}

unsafe fn buffer(array: *mut ArrowArray, index: usize) -> *const c_void {
    unsafe {
        assert!((index as i64) < (*array).n_buffers);
        *(*array).buffers.add(index)
    }
}

unsafe fn i64s(array: *mut ArrowArray, index: usize) -> &'static [i64] {
    unsafe {
        let pointer = buffer(array, index).cast::<i64>();
        std::slice::from_raw_parts(pointer, (*array).length as usize)
    }
}

unsafe fn f64s(array: *mut ArrowArray, index: usize) -> &'static [f64] {
    unsafe {
        let pointer = buffer(array, index).cast::<f64>();
        std::slice::from_raw_parts(pointer, (*array).length as usize)
    }
}

unsafe fn offsets(array: *mut ArrowArray) -> &'static [i32] {
    unsafe {
        let pointer = buffer(array, 1).cast::<i32>();
        std::slice::from_raw_parts(pointer, (*array).length as usize + 1)
    }
}

unsafe fn data_bytes(array: *mut ArrowArray, length: usize) -> &'static [u8] {
    unsafe { std::slice::from_raw_parts(buffer(array, 2).cast::<u8>(), length) }
}

/// The Arrow metadata layout: int32 pair count, then int32-length-prefixed
/// keys and values (native endianness).
unsafe fn metadata_entries(schema: *mut ArrowSchema) -> Vec<(String, String)> {
    unsafe {
        let pointer = (*schema).metadata;
        assert!(!pointer.is_null());
        let read_i32 = |offset: usize| {
            i32::from_ne_bytes(
                std::slice::from_raw_parts(pointer.cast::<u8>().add(offset), 4)
                    .try_into()
                    .unwrap(),
            )
        };
        let read_text = |offset: usize, length: usize| {
            String::from_utf8(
                std::slice::from_raw_parts(pointer.cast::<u8>().add(offset), length).to_vec(),
            )
            .unwrap()
        };
        let count = read_i32(0) as usize;
        let mut entries = Vec::with_capacity(count);
        let mut cursor = 4;
        for _ in 0..count {
            let key_length = read_i32(cursor) as usize;
            let key = read_text(cursor + 4, key_length);
            cursor += 4 + key_length;
            let value_length = read_i32(cursor) as usize;
            let value = read_text(cursor + 4, value_length);
            cursor += 4 + value_length;
            entries.push((key, value));
        }
        entries
    }
}

/// Releases every node of the tree in post-order, asserting each node's
/// `release` is NULL immediately after its own release ran — at that point
/// the node's struct is still alive (a node's struct allocation is freed by
/// its parent's release, per the spec's producer contract), so these reads
/// never touch freed memory while still proving every release ran.
unsafe fn release_schema_tree(schema: *mut ArrowSchema) {
    unsafe {
        let children: Vec<*mut ArrowSchema> = (0..(*schema).n_children)
            .map(|index| *(*schema).children.add(index as usize))
            .collect();
        for child in children {
            release_schema_tree(child);
        }
        let release = (*schema).release.expect("release callback must be set");
        release(schema);
        assert!((*schema).release.is_none());
        assert!((*schema).private_data.is_null());
    }
}

unsafe fn release_array_tree(array: *mut ArrowArray) {
    unsafe {
        let children: Vec<*mut ArrowArray> = (0..(*array).n_children)
            .map(|index| *(*array).children.add(index as usize))
            .collect();
        for child in children {
            release_array_tree(child);
        }
        let release = (*array).release.expect("release callback must be set");
        release(array);
        assert!((*array).release.is_none());
        assert!((*array).private_data.is_null());
    }
}

#[test]
fn export_walks_every_type_and_releases_cleanly() {
    let (columns, rows) = fixture();
    let (schema, array) = export(&columns, &rows).unwrap();
    let schema = Box::into_raw(schema);
    let array = Box::into_raw(array);
    unsafe {
        walk_schema(schema);
        walk_arrays(schema, array);
        release_schema_tree(schema);
        release_array_tree(array);
        drop(Box::from_raw(schema));
        drop(Box::from_raw(array));
    }

    // The consumer pattern: release only the top-level structs; each release
    // releases its children recursively and frees every allocation.
    let (schema, array) = export(&columns, &rows).unwrap();
    let schema = Box::into_raw(schema);
    let array = Box::into_raw(array);
    unsafe {
        (*schema).release.unwrap()(schema);
        (*array).release.unwrap()(array);
        assert!((*schema).release.is_none());
        assert!((*array).release.is_none());
        drop(Box::from_raw(schema));
        drop(Box::from_raw(array));
    }
}

unsafe fn walk_schema(schema: *mut ArrowSchema) {
    unsafe {
        assert_eq!(text((*schema).format), "+s");
        assert!((*schema).name.is_null());
        assert_eq!((*schema).flags, 0);
        assert_eq!((*schema).n_children, 11);
        assert!((*schema).dictionary.is_null());
        let expected = [
            ("flag", "b"),
            ("num", "l"),
            ("score", "g"),
            ("name", "u"),
            ("embedding", "+w:3"),
            ("place", "+s"),
            ("happened", "tsu:UTC"),
            ("payload", "z"),
            ("amount", "d:10,2"),
            ("document", "u"),
            ("nothing", "n"),
        ];
        for (index, (name, format)) in expected.iter().enumerate() {
            let child = schema_child(schema, index);
            assert_eq!(text((*child).name), *name, "column {index} name");
            assert_eq!(text((*child).format), *format, "column {name} format");
            assert_eq!((*child).flags, 2, "column {name} must be nullable");
        }
        let document = schema_child(schema, 9);
        assert_eq!(
            metadata_entries(document),
            vec![("ARROW:extension:name".to_owned(), "arrow.json".to_owned())]
        );
        for other in [0, 1, 2, 3, 4, 5, 6, 7, 8, 10] {
            assert!(
                (*schema_child(schema, other)).metadata.is_null(),
                "only the Json column carries extension metadata"
            );
        }
        // Fixed-size list and GeoPoint nesting.
        let embedding = schema_child(schema, 4);
        assert_eq!((*embedding).n_children, 1);
        let item = schema_child(embedding, 0);
        assert_eq!(text((*item).format), "f");
        assert_eq!(text((*item).name), "item");
        assert_eq!((*item).flags, 0);
        let place = schema_child(schema, 5);
        assert_eq!((*place).n_children, 2);
        let lat = schema_child(place, 0);
        let lng = schema_child(place, 1);
        assert_eq!(
            (text((*lat).name), text((*lat).format)),
            ("lat_deg".into(), "g".into())
        );
        assert_eq!(
            (text((*lng).name), text((*lng).format)),
            ("lng_deg".into(), "g".into())
        );
    }
}

unsafe fn walk_arrays(schema: *mut ArrowSchema, array: *mut ArrowArray) {
    unsafe {
        let _ = schema;
        assert_eq!((*array).length, 2);
        assert_eq!((*array).null_count, 0);
        assert_eq!((*array).n_buffers, 1);
        assert!(buffer(array, 0).is_null(), "no top-level validity");
        assert_eq!((*array).n_children, 11);

        // flag: Bool — row 0 valid+true, row 1 null.
        let flag = array_child(array, 0);
        assert_eq!((*flag).null_count, 1);
        assert_eq!((*flag).n_buffers, 2);
        assert_eq!(*buffer(flag, 0).cast::<u64>(), 0b01);
        assert_eq!(*buffer(flag, 1).cast::<u64>(), 0b01);

        // num: Int64 — typed buffers hold zero at null slots (SCALE.md §6.3).
        let num = array_child(array, 1);
        assert_eq!(*buffer(num, 0).cast::<u64>(), 0b01);
        assert_eq!(i64s(num, 1), &[-42, 0]);

        // score: Float64.
        let score = array_child(array, 2);
        assert_eq!(f64s(score, 1), &[1.5, 0.0]);

        // name: utf8 with int32 offsets; a null row is an empty span.
        let name = array_child(array, 3);
        assert_eq!((*name).n_buffers, 3);
        assert_eq!(offsets(name), &[0, 5, 5]);
        assert_eq!(data_bytes(name, 5), b"devon");

        // embedding: fixed-size list; child length is 3 × rows, null rows zeroed.
        let embedding = array_child(array, 4);
        assert_eq!((*embedding).n_buffers, 1);
        assert_eq!(*buffer(embedding, 0).cast::<u64>(), 0b01);
        let item = array_child(embedding, 0);
        assert_eq!((*item).length, 6);
        assert_eq!((*item).null_count, 0);
        let elements = std::slice::from_raw_parts(buffer(item, 1).cast::<f32>(), 6);
        assert_eq!(elements, &[1.0, 2.0, 3.0, 0.0, 0.0, 0.0]);

        // place: GeoPoint struct; components zeroed at the null row.
        let place = array_child(array, 5);
        assert_eq!(*buffer(place, 0).cast::<u64>(), 0b01);
        assert_eq!(f64s(array_child(place, 0), 1), &[45.5, 0.0]);
        assert_eq!(f64s(array_child(place, 1), 1), &[-122.625, 0.0]);

        // happened: timestamp(µs, UTC).
        let happened = array_child(array, 6);
        assert_eq!(i64s(happened, 1), &[TIMESTAMP_MICROS, 0]);

        // payload: binary.
        let payload = array_child(array, 7);
        assert_eq!(offsets(payload), &[0, 3, 3]);
        assert_eq!(data_bytes(payload, 3), &[0x00, 0xff, 0x1a]);

        // amount: decimal128 — 16-byte little-endian two's-complement digits.
        let amount = array_child(array, 8);
        assert_eq!((*amount).n_buffers, 2);
        let digits = std::slice::from_raw_parts(buffer(amount, 1).cast::<u64>(), 4);
        assert_eq!(digits[0], 1_234_567_890u64);
        assert_eq!(digits[1], 0);
        assert_eq!((digits[2], digits[3]), (0, 0), "null decimal is zeroed");

        // document: json as utf8.
        let document = array_child(array, 9);
        assert_eq!(offsets(document), &[0, 7, 7]);
        assert_eq!(data_bytes(document, 7), br#"{"k":1}"#);

        // nothing: every value NULL and the type is unknown — Arrow null.
        let nothing = array_child(array, 10);
        assert_eq!((*nothing).n_buffers, 0);
        assert!((*nothing).buffers.is_null());
        assert_eq!((*nothing).length, 2);
        assert_eq!((*nothing).null_count, 2);
        assert_eq!((*nothing).n_children, 0);
    }
}

#[test]
fn mixed_types_in_one_column_are_an_error_naming_the_column() {
    let columns = vec!["whoops".to_owned()];
    let rows = vec![vec![Value::Int64(1)], vec![Value::String("two".to_owned())]];
    let error = export(&columns, &rows).unwrap_err();
    assert_eq!(
        error,
        "mixed types in column `whoops`: `Int64` and `String`"
    );
}

#[test]
fn a_row_missing_a_column_is_an_error() {
    let columns = vec!["a".to_owned(), "b".to_owned()];
    let rows = vec![vec![Value::Int64(1)]];
    let error = export(&columns, &rows).unwrap_err();
    assert_eq!(error, "result row is missing column `b`");
}

#[test]
fn decimal_columns_merge_precision_across_rows() {
    let columns = vec!["amount".to_owned()];
    let rows = vec![
        vec![Value::Decimal(Decimal128::new(19_99, 2).unwrap())],
        vec![Value::Decimal(Decimal128::new(12_345_678_901, 2).unwrap())],
    ];
    let (schema, array) = export(&columns, &rows).unwrap();
    let schema = Box::into_raw(schema);
    let array = Box::into_raw(array);
    unsafe {
        let child = schema_child(schema, 0);
        assert_eq!(text((*child).format), "d:11,2");
        (*schema).release.unwrap()(schema);
        (*array).release.unwrap()(array);
        drop(Box::from_raw(schema));
        drop(Box::from_raw(array));
    }
}
