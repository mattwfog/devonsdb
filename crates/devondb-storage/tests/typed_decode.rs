//! Typed decode tests (`docs/SCALE.md` §6.5/§6.6):
//! `NodeGroup::read_column_typed` decodes payload bytes straight into typed
//! [`Column`] storage and must equal the boxed `read_column` value-for-value
//! for every type, including NULL patterns at bitmap bit boundaries; the
//! payload corruption matrix runs through the typed path with the boxed
//! path's checks in force; and the §6.6 budget charge of a typed column is
//! exactly `len × width` + bitmap words.

use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use devondb_storage::{
    node_group::{NODE_GROUP_CAPACITY, NodeGroup},
    pager::Pager,
    superblock::ZONE_MAPS_FLAG,
};
use devondb_types::{
    Decimal128, GeoPoint,
    column::Column,
    logical_type::{LogicalType, VectorEncoding},
    value::Value,
};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"typed-decode-db!";
const PAGE_SIZE_USIZE: usize = PAGE_SIZE as usize;
const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;

/// NULL slots at every bitmap bit boundary the spec pins: the first and last
/// bits of the first byte, the first bit of the second byte, the last and
/// first bits of the first and second u64 words, and the final row.
const NULL_BOUNDARY_ROWS: [usize; 6] = [0, 7, 8, 63, 64, 2047];

/// Publishes `ZONE_MAPS` the way a real checkpoint's catalog save does (the
/// fixtures here write groups straight through the pager; see
/// `vector_props.rs` for the precedent).
fn publish_zone_maps(pager: &Pager) {
    let mut superblock = pager.superblock();
    superblock.feature_flags |= ZONE_MAPS_FLAG;
    // The pager enforces checkpoint-LSN monotonicity on every commit.
    superblock.checkpoint_lsn += 1;
    pager.commit_superblock(superblock).unwrap();
}

/// CRC-32C (Castagnoli, reflected polynomial) computed locally so doctored
/// payloads can be re-checksummed without a new dev-dependency.
/// `crc32c_helper_matches_the_writer` pins it against `NodeGroup::write`.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    directory_page: u64,
}

/// Writes one full-capacity group of `ty` with NULLs at NULL_BOUNDARY_ROWS
/// and `value(row)` elsewhere.
fn write_full_group(ty: &LogicalType, value: impl Fn(usize) -> Value) -> Fixture {
    write_group(ty, NODE_GROUP_CAPACITY, |row| {
        if NULL_BOUNDARY_ROWS.contains(&row) {
            Value::Null
        } else {
            value(row)
        }
    })
}

fn write_group(ty: &LogicalType, row_count: usize, value: impl Fn(usize) -> Value) -> Fixture {
    let directory = tempdir().unwrap();
    let path = directory.path().join("typed.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![*ty]).unwrap();
    for row in 0..row_count {
        group.push_row(vec![value(row)]).unwrap();
    }
    let directory_page = group.write_forcing_encodings(&pager, &[]).unwrap();
    publish_zone_maps(&pager);
    drop(pager);
    Fixture {
        _directory: directory,
        path,
        directory_page,
    }
}

fn read_both(fixture: &Fixture, ty: &LogicalType) -> (Vec<Value>, Column, usize) {
    let pager = Pager::open(&fixture.path).unwrap();
    let (boxed, rows) =
        NodeGroup::read_column(&pager, fixture.directory_page, std::slice::from_ref(ty), 0)
            .expect("boxed read");
    let (typed, typed_rows) =
        NodeGroup::read_column_typed(&pager, fixture.directory_page, std::slice::from_ref(ty), 0)
            .expect("typed read");
    assert_eq!(rows, typed_rows, "row counts disagree");
    (boxed, typed, rows)
}

/// Typed decode equals boxed decode value-for-value. NULL slots hold zero
/// in the typed vector, and the validity bitmap reports
/// exactly the NULL rows.
fn assert_typed_equals_boxed(ty: &LogicalType, value: impl Fn(usize) -> Value) {
    let fixture = write_full_group(ty, &value);
    let (boxed, typed, rows) = read_both(&fixture, ty);
    assert_eq!(rows, NODE_GROUP_CAPACITY);
    assert_eq!(typed.len(), boxed.len());
    for (row, expected) in boxed.iter().enumerate() {
        assert_eq!(typed.value_at(row), *expected, "row {row} diverged");
    }
    for &null_row in &NULL_BOUNDARY_ROWS {
        assert_eq!(typed.value_at(null_row), Value::Null, "row {null_row}");
    }
    match (&typed, ty) {
        (Column::Int64 { values, validity }, LogicalType::Int64)
        | (Column::Timestamp { values, validity }, LogicalType::Timestamp) => {
            for &null_row in &NULL_BOUNDARY_ROWS {
                assert_eq!(values[null_row], 0, "null slot {null_row} must stay zero");
            }
            assert_validity(validity, rows);
        }
        (Column::Float64 { values, validity }, LogicalType::Float64) => {
            for &null_row in &NULL_BOUNDARY_ROWS {
                assert_eq!(
                    values[null_row].to_bits(),
                    0,
                    "null slot {null_row} must stay zero"
                );
            }
            assert_validity(validity, rows);
        }
        (Column::Bool { values, validity }, LogicalType::Bool) => {
            for &null_row in &NULL_BOUNDARY_ROWS {
                assert!(!values[null_row], "null slot {null_row} must stay false");
            }
            assert_validity(validity, rows);
        }
        (
            Column::Decimal {
                values,
                scale,
                validity,
            },
            LogicalType::Decimal {
                scale: declared_scale,
                ..
            },
        ) => {
            assert_eq!(scale, declared_scale);
            for &null_row in &NULL_BOUNDARY_ROWS {
                assert_eq!(values[null_row], 0, "null slot {null_row} must stay zero");
            }
            assert_validity(validity, rows);
        }
        (Column::Boxed(_), _) => {}
        (typed, ty) => panic!("column for {ty} decoded to an unexpected variant: {typed:?}"),
    }
}

fn assert_validity(validity: &Option<devondb_types::column::Bitmap>, rows: usize) {
    let bitmap = validity.as_ref().expect("NULLs present, bitmap must exist");
    assert_eq!(bitmap.len(), rows);
    for row in 0..rows {
        assert_eq!(
            bitmap.is_valid(row),
            !NULL_BOUNDARY_ROWS.contains(&row),
            "validity row {row}"
        );
    }
}

#[test]
fn int64_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Int64, |row| {
        Value::Int64(if row == 1 { i64::MIN } else { row as i64 * -3 })
    });
}

#[test]
fn float64_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Float64, |row| {
        Value::Float64(if row == 1 { -0.0 } else { row as f64 / 7.0 })
    });
}

#[test]
fn bool_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Bool, |row| Value::Bool(row % 2 == 0));
}

#[test]
fn timestamp_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Timestamp, |row| {
        Value::Timestamp(if row == 1 {
            i64::MIN
        } else {
            row as i64 * 1_000_000 - 42
        })
    });
}

#[test]
fn decimal_typed_equals_boxed() {
    let ty = LogicalType::Decimal {
        precision: 10,
        scale: 2,
    };
    assert_typed_equals_boxed(&ty, |row| {
        Value::Decimal(Decimal128::new(row as i128 * 111 - 5_000, 2).unwrap())
    });
}

#[test]
fn string_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::String, |row| {
        Value::String(if row == 1 {
            String::new()
        } else {
            format!("row-{row}")
        })
    });
}

#[test]
fn bytes_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Bytes, |row| {
        Value::Bytes(vec![(row % 251) as u8; row % 17])
    });
}

#[test]
fn json_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Json, |row| {
        Value::Json(format!(r#"{{"row":{row},"even":{}}}"#, row % 2 == 0))
    });
}

#[test]
fn vector_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::Vector { dim: 3 }, |row| {
        Value::Vector(vec![row as f32, -(row as f32), 0.5])
    });
}

#[test]
fn vector_encoded_typed_equals_boxed() {
    let ty = LogicalType::VectorEncoded {
        dim: 3,
        encoding: VectorEncoding::I8,
    };
    // Quantized decode is lossy against the input, but typed and boxed
    // decode the same payload bytes and must agree with each other.
    assert_typed_equals_boxed(&ty, |row| {
        Value::Vector(vec![row as f32 % 4.0 - 2.0, 0.25, -0.5])
    });
}

#[test]
fn geo_point_typed_equals_boxed() {
    assert_typed_equals_boxed(&LogicalType::GeoPoint, |row| {
        Value::GeoPoint(
            GeoPoint::from_canonical(51.5 + (row % 100) as f64 / 1_000.0, -((row % 180) as f64))
                .unwrap(),
        )
    });
}

#[test]
fn all_valid_rows_carry_no_validity_bitmap() {
    let fixture = write_group(&LogicalType::Int64, 5, |row| Value::Int64(row as i64));
    let (_, typed, rows) = read_both(&fixture, &LogicalType::Int64);
    assert_eq!(rows, 5);
    let Column::Int64 { values, validity } = &typed else {
        panic!("expected typed Int64 column: {typed:?}");
    };
    assert!(
        validity.is_none(),
        "all-valid column must not allocate a bitmap"
    );
    assert_eq!(values, &[0, 1, 2, 3, 4]);
}

#[test]
fn tail_group_repacks_partial_bitmap_words() {
    // 65 rows with NULLs at 63 and 64: the FORMAT bytes straddle a u64 word
    // boundary in the §6.3 repack.
    let fixture = write_group(&LogicalType::Int64, 65, |row| {
        if row == 63 || row == 64 {
            Value::Null
        } else {
            Value::Int64(row as i64)
        }
    });
    let (boxed, typed, rows) = read_both(&fixture, &LogicalType::Int64);
    assert_eq!(rows, 65);
    for (row, expected) in boxed.iter().enumerate() {
        assert_eq!(typed.value_at(row), *expected, "row {row} diverged");
    }
}

#[test]
fn exact_budget_charge_of_all_int64_2048_row_group() {
    // docs/SCALE.md §6.6: fixed-width typed storage charges len × width +
    // bitmap words (8 bytes per u64 word); `Column::approx_bytes` IS that
    // charge rule, applied wherever the scan path's working-set sites
    // charge columns/rows. (The decoded group itself is not charged at the
    // scan site — the boxed `NodeGroup` never was either; see the `GroupScan`
    // budget note in crates/devondb/src/database/view.rs.)
    let fixture = write_group(&LogicalType::Int64, NODE_GROUP_CAPACITY, |row| {
        Value::Int64(row as i64)
    });
    let pager = Pager::open(&fixture.path).unwrap();
    let (typed, rows) =
        NodeGroup::read_column_typed(&pager, fixture.directory_page, &[LogicalType::Int64], 0)
            .unwrap();
    assert_eq!(rows, NODE_GROUP_CAPACITY);
    assert_eq!(
        typed.approx_bytes(),
        NODE_GROUP_CAPACITY * size_of::<i64>(),
        "all-valid Int64 group charges len × width, no bitmap"
    );

    // One NULL: the validity bitmap adds ceil(2048 / 64) words × 8 bytes.
    let fixture = write_group(&LogicalType::Int64, NODE_GROUP_CAPACITY, |row| {
        if row == 2047 {
            Value::Null
        } else {
            Value::Int64(row as i64)
        }
    });
    let pager = Pager::open(&fixture.path).unwrap();
    let (typed, _) =
        NodeGroup::read_column_typed(&pager, fixture.directory_page, &[LogicalType::Int64], 0)
            .unwrap();
    assert_eq!(
        typed.approx_bytes(),
        NODE_GROUP_CAPACITY * size_of::<i64>() + (NODE_GROUP_CAPACITY / 64) * size_of::<u64>(),
        "a NULL adds exactly the bitmap words"
    );
}

#[test]
fn out_of_bounds_column_is_refused() {
    let fixture = write_group(&LogicalType::Int64, 1, |_| Value::Int64(1));
    let pager = Pager::open(&fixture.path).unwrap();
    let error =
        NodeGroup::read_column_typed(&pager, fixture.directory_page, &[LogicalType::Int64], 1)
            .unwrap_err();
    assert!(error.to_string().contains("out of bounds"), "{error}");
}

#[test]
fn rescore_sidecar_column_is_refused() {
    let ty = LogicalType::VectorEncoded {
        dim: 3,
        encoding: VectorEncoding::B1 {
            rotation_seed: 7,
            rescore: devondb_types::logical_type::B1Rescore::F16,
        },
    };
    let fixture = write_group(&ty, 2, |_| Value::Vector(vec![1.0, 2.0, 3.0]));
    let pager = Pager::open(&fixture.path).unwrap();
    let error = NodeGroup::read_column_typed(&pager, fixture.directory_page, &[ty], 0).unwrap_err();
    assert!(error.to_string().contains("rescore sidecar"), "{error}");
}

// ---------------------------------------------------------------------------
// Corruption matrix through the typed path. Every check the boxed path runs
// (docs/FORMAT.md § Node group pages: "Any mismatch — bad magic, wrong
// byte_len, CRC failure, non-monotonic offsets, invalid UTF-8 — is
// corruption") must fire identically on the typed path.
// ---------------------------------------------------------------------------

struct RawEntry {
    first_page: u64,
    byte_len: u32,
}

fn read_directory_entry(path: &Path, directory_page: u64, column: usize) -> RawEntry {
    let mut file = OpenOptions::new().read(true).open(path).unwrap();
    let offset = directory_page * PAGE_SIZE as u64
        + (DIRECTORY_HEADER_LEN + column * DIRECTORY_ENTRY_LEN) as u64;
    let mut entry = [0_u8; DIRECTORY_ENTRY_LEN];
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.read_exact(&mut entry).unwrap();
    RawEntry {
        first_page: u64::from_le_bytes(entry[..8].try_into().unwrap()),
        byte_len: u32::from_le_bytes(entry[8..12].try_into().unwrap()),
    }
}

/// Rewrites the checksum stored in the directory entry for `column` to match
/// the (possibly doctored) payload bytes, so checks deeper than the CRC run.
fn refresh_entry_checksum(path: &Path, directory_page: u64, column: usize, payload: &[u8]) {
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    let offset = directory_page * PAGE_SIZE as u64
        + (DIRECTORY_HEADER_LEN + column * DIRECTORY_ENTRY_LEN + 12) as u64;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&crc32c(payload).to_le_bytes()).unwrap();
}

/// Reads the payload bytes of `column` out of the file, mutates them, writes
/// them back, and refreshes the directory CRC so the requested corruption
/// reaches the decoder.
fn doctor_payload(fixture: &Fixture, column: usize, mutate: impl FnOnce(&mut [u8])) {
    let entry = read_directory_entry(&fixture.path, fixture.directory_page, column);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.path)
        .unwrap();
    let start = entry.first_page * PAGE_SIZE as u64;
    let mut payload = vec![0_u8; entry.byte_len as usize];
    file.seek(SeekFrom::Start(start)).unwrap();
    file.read_exact(&mut payload).unwrap();
    mutate(&mut payload);
    file.seek(SeekFrom::Start(start)).unwrap();
    file.write_all(&payload).unwrap();
    drop(file);
    refresh_entry_checksum(&fixture.path, fixture.directory_page, column, &payload);
}

fn assert_typed_read_corrupt(fixture: &Fixture, ty: &LogicalType, expected: &str) {
    let pager = Pager::open(&fixture.path).unwrap();
    let result =
        NodeGroup::read_column_typed(&pager, fixture.directory_page, std::slice::from_ref(ty), 0);
    match result {
        Err(devondb_types::DevonError::Corrupt { context }) => {
            assert!(
                context.contains(expected),
                "expected `{expected}` in `{context}`"
            );
        }
        other => panic!("expected Corrupt ({expected}), got {other:?}"),
    }
}

#[test]
fn crc32c_helper_matches_the_writer() {
    let fixture = write_full_group(&LogicalType::Int64, |row| Value::Int64(row as i64));
    let entry = read_directory_entry(&fixture.path, fixture.directory_page, 0);
    let mut file = OpenOptions::new().read(true).open(&fixture.path).unwrap();
    let mut payload = vec![0_u8; entry.byte_len as usize];
    file.seek(SeekFrom::Start(entry.first_page * PAGE_SIZE as u64))
        .unwrap();
    file.read_exact(&mut payload).unwrap();
    let directory = fs::read(&fixture.path).unwrap();
    let checksum_offset =
        fixture.directory_page as usize * PAGE_SIZE_USIZE + DIRECTORY_HEADER_LEN + 12;
    let stored = u32::from_le_bytes(
        directory[checksum_offset..checksum_offset + 4]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        crc32c(&payload),
        stored,
        "local CRC-32C diverged from the writer"
    );
}

#[test]
fn flipped_payload_byte_fails_crc() {
    let fixture = write_full_group(&LogicalType::Int64, |row| Value::Int64(row as i64));
    let entry = read_directory_entry(&fixture.path, fixture.directory_page, 0);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fixture.path)
        .unwrap();
    file.seek(SeekFrom::Start(entry.first_page * PAGE_SIZE as u64 + 1))
        .unwrap();
    file.write_all(&[0xff]).unwrap();
    drop(file);
    assert_typed_read_corrupt(&fixture, &LogicalType::Int64, "CRC-32C does not match");
}

#[test]
fn doctored_byte_len_is_rejected() {
    let fixture = write_full_group(&LogicalType::Int64, |row| Value::Int64(row as i64));
    let entry = read_directory_entry(&fixture.path, fixture.directory_page, 0);
    let mut file = OpenOptions::new().write(true).open(&fixture.path).unwrap();
    let offset = fixture.directory_page * PAGE_SIZE as u64 + (DIRECTORY_HEADER_LEN + 8) as u64;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&(entry.byte_len + 1).to_le_bytes()).unwrap();
    drop(file);
    assert_typed_read_corrupt(&fixture, &LogicalType::Int64, "byte_len is");
}

#[test]
fn wrong_schema_type_is_rejected() {
    let fixture = write_full_group(&LogicalType::Int64, |row| Value::Int64(row as i64));
    // The directory's Int64 stats record violates Bool's never-stats rule
    // before the payload length is even consulted — corruption either way.
    assert_typed_read_corrupt(&fixture, &LogicalType::Bool, "presence violates Bool rules");
}

#[test]
fn null_int64_slot_not_zero_is_rejected() {
    // Row 0 is NULL; its 8-byte slot must stay zero (FORMAT law). The
    // validity bitmap occupies the first ceil(2048 / 8) = 256 payload bytes.
    let fixture = write_full_group(&LogicalType::Int64, |row| Value::Int64(row as i64));
    doctor_payload(&fixture, 0, |payload| payload[256] = 1);
    assert_typed_read_corrupt(
        &fixture,
        &LogicalType::Int64,
        "row 0 null value slot is not zero",
    );
}

#[test]
fn null_bool_slot_set_is_rejected() {
    // Bool values are a second bitmap after the 256 validity bytes; row 0's
    // bit is bit 0 of byte 0 of that second bitmap.
    let fixture = write_full_group(&LogicalType::Bool, |row| Value::Bool(row % 2 == 1));
    doctor_payload(&fixture, 0, |payload| payload[256] |= 1);
    assert_typed_read_corrupt(
        &fixture,
        &LogicalType::Bool,
        "row 0 null Bool slot is not zero",
    );
}

#[test]
fn validity_trailing_bits_set_is_rejected() {
    // Three rows: the validity byte's upper five bits must stay zero.
    let fixture = write_group(&LogicalType::Int64, 3, |row| Value::Int64(row as i64));
    doctor_payload(&fixture, 0, |payload| payload[0] |= 0b1111_1000);
    assert_typed_read_corrupt(
        &fixture,
        &LogicalType::Int64,
        "validity bitmap trailing bits are not zero",
    );
}

#[test]
fn decimal_digits_exceeding_precision_are_rejected() {
    let ty = LogicalType::Decimal {
        precision: 3,
        scale: 0,
    };
    let fixture = write_group(&ty, 2, |row| {
        Value::Decimal(Decimal128::new(row as i128, 0).unwrap())
    });
    // Row 1's 16-byte slot starts after the 1-byte validity bitmap and row
    // 0's slot: digits 1000 do not fit Decimal(3, 0).
    doctor_payload(&fixture, 0, |payload| {
        payload[1 + 16..1 + 32].copy_from_slice(&1_000_i128.to_le_bytes());
    });
    assert_typed_read_corrupt(&fixture, &ty, "Decimal digits exceed declared precision 3");
}

#[test]
fn string_heap_not_utf8_is_rejected() {
    let fixture = write_group(&LogicalType::String, 2, |row| {
        Value::String(format!("row-{row}"))
    });
    // Validity 1 byte, then (2 + 1) × 4 offset bytes; the heap follows.
    // "row-1"'s first byte sits 5 bytes into the heap.
    doctor_payload(&fixture, 0, |payload| {
        let heap_start = 1 + 3 * 4;
        payload[heap_start + 5] = 0xff;
    });
    assert_typed_read_corrupt(&fixture, &LogicalType::String, "is not valid UTF-8");
}

#[test]
fn string_offsets_not_monotonic_are_rejected() {
    let fixture = write_group(&LogicalType::String, 2, |row| {
        Value::String(format!("row-{row}"))
    });
    doctor_payload(&fixture, 0, |payload| {
        // offsets[1] = 5 ("row-0" end) > offsets[2] := 1.
        payload[1 + 8..1 + 12].copy_from_slice(&1_u32.to_le_bytes());
    });
    assert_typed_read_corrupt(&fixture, &LogicalType::String, "offsets are not monotonic");
}

#[test]
fn read_columns_typed_from_directory_matches_full_read() {
    // The scan path's multi-column decode: every column equals the boxed
    // full read value-for-value, across types and NULL rows.
    let types = vec![
        LogicalType::Int64,
        LogicalType::String,
        LogicalType::Bool,
        LogicalType::Timestamp,
        LogicalType::Decimal {
            precision: 12,
            scale: 4,
        },
        LogicalType::Vector { dim: 2 },
    ];
    let directory = tempdir().unwrap();
    let path = directory.path().join("multi.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(types.clone()).unwrap();
    for row in 0..NODE_GROUP_CAPACITY {
        let null = NULL_BOUNDARY_ROWS.contains(&row);
        group
            .push_row(vec![
                if null {
                    Value::Null
                } else {
                    Value::Int64(row as i64)
                },
                if null {
                    Value::Null
                } else {
                    Value::String(format!("n-{row}"))
                },
                if null {
                    Value::Null
                } else {
                    Value::Bool(row % 3 == 0)
                },
                if null {
                    Value::Null
                } else {
                    Value::Timestamp(row as i64 * 10)
                },
                if null {
                    Value::Null
                } else {
                    Value::Decimal(Decimal128::new(row as i128 * 7, 4).unwrap())
                },
                if null {
                    Value::Null
                } else {
                    Value::Vector(vec![row as f32, 1.0])
                },
            ])
            .unwrap();
    }
    let directory_page = group.write_forcing_encodings(&pager, &[]).unwrap();
    publish_zone_maps(&pager);
    drop(pager);

    let pager = Pager::open(path).unwrap();
    let full = NodeGroup::read(&pager, directory_page, &types).unwrap();
    let directory = NodeGroup::read_directory(&pager, directory_page, &types).unwrap();
    let columns = NodeGroup::read_columns_typed_from_directory(&pager, directory, &types).unwrap();
    assert_eq!(columns.len(), types.len());
    for (column_index, typed) in columns.iter().enumerate() {
        assert_eq!(typed.len(), full.row_count());
        for row in 0..full.row_count() {
            assert_eq!(
                typed.value_at(row),
                *full.value(row, column_index).unwrap(),
                "column {column_index} row {row} diverged"
            );
        }
    }
}
