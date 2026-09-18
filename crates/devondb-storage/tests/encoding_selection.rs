//! Adaptive encoding selection at group write (`docs/SCALE.md` §8.4).
//! These tests pin the *choices*
//! the default writer makes for constructed column shapes, the
//! plain-when-nothing-wins law, the estimate-fallback chain, determinism,
//! and feature-bit-13 derivation from real files written through the
//! default path. Byte layouts per encoding are pinned by the per-encoding
//! goldens, not here.

use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::COLUMN_ENCODINGS_FLAG;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::value::Value;
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"encoding-select!";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const ROWS: usize = 2048;

/// Builds a group from per-column generators and writes it through the
/// default (adaptive) writer path.
fn write_adaptive(
    directory: &TempDir,
    name: &str,
    types: &[LogicalType],
    columns: &[Vec<Value>],
) -> (Pager, u64) {
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(types.to_vec()).unwrap();
    for row in 0..columns[0].len() {
        group
            .push_row(columns.iter().map(|column| column[row].clone()).collect())
            .unwrap();
    }
    let directory_page = group.write(&pager).unwrap();
    (pager, directory_page)
}

/// Publishes one single-group table through a real catalog save, which
/// derives superblock feature bit 13 from the written directories
/// (`docs/SCALE.md` §8.1).
fn publish(pager: &Pager, table: &str, types: &[LogicalType], group: u64) {
    let mut catalog = Catalog::default();
    // The schema floor requires exactly one Int64/String primary key; the
    // group under test carries only its own columns, so the published
    // schema prepends a synthetic Int64 primary key. Catalog save derives
    // feature bit 13 by sniffing directory pages, not via the schema.
    let mut columns = vec![Column {
        name: "pk".to_owned(),
        ty: LogicalType::Int64,
        primary_key: true,
    }];
    columns.extend(types.iter().enumerate().map(|(index, ty)| Column {
        name: format!("c{index}"),
        ty: *ty,
        primary_key: false,
    }));
    catalog
        .add_node_table(NodeTableSchema::new(table.to_owned(), columns).unwrap())
        .unwrap();
    catalog
        .set_table_storage(
            table,
            TableStorage {
                groups: vec![group],
            },
        )
        .unwrap();
    catalog.save(pager, 1).unwrap();
}

/// The directory flags word of a written group.
fn directory_flags(pager: &Pager, directory_page: u64) -> u32 {
    let page = pager.read_page(directory_page).unwrap();
    u32::from_le_bytes(page[12..16].try_into().unwrap())
}

/// The encoding ids the directory's `COLUMN_ENCODINGS` section declares,
/// in catalog column order. Panics when the section is absent.
fn selected_encoding_ids(pager: &Pager, directory_page: u64, column_count: usize) -> Vec<u8> {
    let page = pager.read_page(directory_page).unwrap();
    assert_eq!(
        directory_flags(pager, directory_page) & 0b10,
        0b10,
        "the group must declare a COLUMN_ENCODINGS section"
    );
    let zone_map_end = HEADER_LEN + column_count * ENTRY_LEN;
    let start = zone_map_end + SECTION_HEADER_LEN + column_count * ZONE_MAP_RECORD_LEN;
    let section_len = u32::from_le_bytes(page[start..start + 4].try_into().unwrap()) as usize;
    assert_eq!(section_len, column_count * 4);
    page[start + SECTION_HEADER_LEN..start + SECTION_HEADER_LEN + section_len]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|record| record[0])
        .collect()
}

/// Asserts the single-column group selected `expected_id` and reads back
/// identical values through the published, governed path.
fn assert_choice(
    directory: &TempDir,
    name: &str,
    ty: LogicalType,
    column: Vec<Value>,
    expected_id: u8,
) {
    let (pager, directory_page) =
        write_adaptive(directory, name, &[ty], std::slice::from_ref(&column));
    assert_eq!(
        selected_encoding_ids(&pager, directory_page, 1),
        vec![expected_id],
        "{name}: selected encoding"
    );
    publish(&pager, "T", &[ty], directory_page);
    let decoded = NodeGroup::read(&pager, directory_page, &[ty]).unwrap();
    for (row, value) in column.iter().enumerate() {
        assert_eq!(decoded.value(row, 0), Some(value), "{name}: row {row}");
    }
}

#[test]
fn a_constant_column_selects_constant() {
    let directory = tempdir().unwrap();
    assert_choice(
        &directory,
        "constant.devondb",
        LogicalType::Int64,
        vec![Value::Int64(7); 512],
        1,
    );
}

#[test]
fn a_run_heavy_column_selects_rle() {
    let mut column = Vec::new();
    for run in 0..32_i64 {
        column.extend(std::iter::repeat_n(Value::Int64(run * 1_000_000), 64));
    }
    assert_eq!(column.len(), ROWS);
    let directory = tempdir().unwrap();
    assert_choice(&directory, "rle.devondb", LogicalType::Int64, column, 2);
}

#[test]
fn a_narrow_range_column_selects_bitpack_for() {
    let column: Vec<Value> = (0..ROWS)
        .map(|row| Value::Int64(1_000_000 + row as i64 % 61))
        .collect();
    let directory = tempdir().unwrap();
    assert_choice(&directory, "bitpack.devondb", LogicalType::Int64, column, 3);
}

#[test]
fn a_low_cardinality_string_column_selects_dictionary() {
    let names = ["alpha", "beta", "gamma", "delta"];
    let column: Vec<Value> = (0..ROWS)
        .map(|row| Value::String(names[row % names.len()].to_owned()))
        .collect();
    let directory = tempdir().unwrap();
    assert_choice(
        &directory,
        "dictionary.devondb",
        LogicalType::String,
        column,
        4,
    );
}

#[test]
fn a_repetitive_high_cardinality_string_column_selects_fsst() {
    let column: Vec<Value> = (0..ROWS)
        .map(|row| {
            Value::String(format!(
                "https://edge.example.com/sensors/reading/{row:06}/celsius"
            ))
        })
        .collect();
    let directory = tempdir().unwrap();
    assert_choice(&directory, "fsst.devondb", LogicalType::String, column, 5);
}

#[test]
fn a_decimal_like_float_column_selects_alp() {
    let column: Vec<Value> = (0..ROWS)
        .map(|row| Value::Float64(row as f64 / 100.0))
        .collect();
    let directory = tempdir().unwrap();
    assert_choice(&directory, "alp.devondb", LogicalType::Float64, column, 6);
}

/// The plain-when-nothing-wins law (§8.4): all-distinct high-entropy
/// strings qualify no candidate, so the group carries no
/// `COLUMN_ENCODINGS` section at all and the publication derives no
/// feature bit 13.
#[test]
fn all_distinct_random_strings_stay_plain() {
    let directory = tempdir().unwrap();
    let mut state = 0x243F_6A88_85A3_08D3_u64;
    let column: Vec<Value> = (0..ROWS)
        .map(|_| {
            let mut text = String::with_capacity(32);
            for _ in 0..32 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push(char::from((state >> 33) as u8 % 94 + 33));
            }
            Value::String(text)
        })
        .collect();
    let (pager, directory_page) = write_adaptive(
        &directory,
        "plain.devondb",
        &[LogicalType::String],
        &[column],
    );
    assert_eq!(
        directory_flags(&pager, directory_page),
        0b1,
        "an all-plain group writes zone maps only — no encodings section"
    );
    publish(&pager, "T", &[LogicalType::String], directory_page);
    assert_eq!(
        pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        0,
        "an all-plain publication must not set feature bit 13"
    );
}

/// Feature bit 13 is derived from real files written through the default
/// path: the publication that first reaches a non-plain group sets it.
#[test]
fn feature_bit_13_is_derived_from_a_default_path_write() {
    let directory = tempdir().unwrap();
    let column = vec![Value::Int64(42); 256];
    let (pager, directory_page) = write_adaptive(
        &directory,
        "derived.devondb",
        &[LogicalType::Int64],
        &[column],
    );
    publish(&pager, "T", &[LogicalType::Int64], directory_page);
    assert_eq!(
        pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        COLUMN_ENCODINGS_FLAG,
        "the publication reaching a selected non-plain group must set bit 13"
    );
}

/// A sampled choice the unsampled rows disprove must fall through the
/// ranking, never produce invalid bytes: 2048 sevens with one deviating
/// unsampled row (257 is past the 256-row head and off the 8-stride)
/// samples as constant, fails the full constant encode, and lands on the
/// next ranked candidate.
#[test]
fn a_disproved_sampled_constant_falls_back_down_the_ranking() {
    let directory = tempdir().unwrap();
    let mut column = vec![Value::Int64(7); ROWS];
    column[257] = Value::Int64(9);
    let (pager, directory_page) = write_adaptive(
        &directory,
        "fallback.devondb",
        &[LogicalType::Int64],
        &[column.clone()],
    );
    let ids = selected_encoding_ids(&pager, directory_page, 1);
    assert_ne!(
        ids,
        vec![1],
        "constant must refuse the deviating row and fall back"
    );
    publish(&pager, "T", &[LogicalType::Int64], directory_page);
    let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::Int64]).unwrap();
    for (row, value) in column.iter().enumerate() {
        assert_eq!(decoded.value(row, 0), Some(value), "row {row}");
    }
}

/// A mixed-shape table written through the default path reads back
/// identical, with each column carrying the expected selection.
#[test]
fn a_mixed_shape_group_round_trips_through_the_default_path() {
    let directory = tempdir().unwrap();
    let types = vec![
        LogicalType::Int64,     // constant
        LogicalType::Timestamp, // narrow range -> bitpack_for
        LogicalType::Float64,   // two-decimal values -> alp
        LogicalType::String,    // low cardinality -> dictionary
        LogicalType::Bool,      // single non-null bool value -> constant
    ];
    let names = ["north", "south", "east", "west"];
    let columns: Vec<Vec<Value>> = vec![
        vec![Value::Int64(-5); ROWS],
        (0..ROWS)
            .map(|row| Value::Timestamp(1_700_000_000_000_000 + row as i64))
            .collect(),
        (0..ROWS)
            .map(|row| Value::Float64(row as f64 / 20.0))
            .collect(),
        (0..ROWS)
            .map(|row| Value::String(names[row % names.len()].to_owned()))
            .collect(),
        vec![Value::Bool(true); ROWS],
    ];
    let (pager, directory_page) = write_adaptive(&directory, "mixed.devondb", &types, &columns);
    assert_eq!(
        selected_encoding_ids(&pager, directory_page, types.len()),
        vec![1, 3, 6, 4, 1],
        "per-column selections for the mixed shapes"
    );
    publish(&pager, "Mixed", &types, directory_page);
    let decoded = NodeGroup::read(&pager, directory_page, &types).unwrap();
    for row in 0..ROWS {
        for (column_index, column) in columns.iter().enumerate() {
            assert_eq!(
                decoded.value(row, column_index),
                Some(&column[row]),
                "row {row} column {column_index}"
            );
        }
    }
    // The typed scan path agrees column by column.
    let directory = NodeGroup::read_directory(&pager, directory_page, &types).unwrap();
    let typed = NodeGroup::read_columns_typed_from_directory(&pager, directory, &types).unwrap();
    assert_eq!(typed.len(), types.len());
    for (column_index, column) in columns.iter().enumerate() {
        for (row, value) in column.iter().enumerate() {
            assert_eq!(
                typed[column_index].value_at(row),
                *value,
                "typed row {row} column {column_index}"
            );
        }
    }
}

/// Selection is deterministic across pagers: two fresh writes of the same
/// group produce byte-identical directory pages.
#[test]
fn selection_is_deterministic_across_writes() {
    let directory = tempdir().unwrap();
    let column: Vec<Value> = (0..ROWS)
        .map(|row| Value::Int64(5_000 + row as i64 % 97))
        .collect();
    let (first_pager, first_page) = write_adaptive(
        &directory,
        "first.devondb",
        &[LogicalType::Int64],
        std::slice::from_ref(&column),
    );
    let (second_pager, second_page) = write_adaptive(
        &directory,
        "second.devondb",
        &[LogicalType::Int64],
        &[column],
    );
    assert_eq!(
        first_pager.read_page(first_page).unwrap(),
        second_pager.read_page(second_page).unwrap(),
        "identical input must produce identical directory bytes"
    );
}

/// NULLs flow through selection: an all-NULL column selects constant (the
/// zero spelling), and a mostly-NULL run column still round-trips.
#[test]
fn null_patterns_select_and_round_trip() {
    let directory = tempdir().unwrap();
    let types = vec![LogicalType::Int64, LogicalType::Int64];
    let mut sparse = vec![Value::Null; ROWS];
    for (row, slot) in sparse.iter_mut().enumerate() {
        if row % 64 < 8 {
            *slot = Value::Int64(1);
        }
    }
    let columns = vec![vec![Value::Null; ROWS], sparse];
    let (pager, directory_page) = write_adaptive(&directory, "nulls.devondb", &types, &columns);
    assert_eq!(
        selected_encoding_ids(&pager, directory_page, types.len())[0],
        1,
        "an all-NULL column selects constant (zero spelling)"
    );
    publish(&pager, "Nulls", &types, directory_page);
    let decoded = NodeGroup::read(&pager, directory_page, &types).unwrap();
    for row in 0..ROWS {
        for (column_index, column) in columns.iter().enumerate() {
            assert_eq!(
                decoded.value(row, column_index),
                Some(&column[row]),
                "row {row} column {column_index}"
            );
        }
    }
}
