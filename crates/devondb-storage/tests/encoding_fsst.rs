//! FSST encoding (id 5) tests: frozen byte golden, round trips over
//! deterministic value sets including all-NULL, single-row, and 2048-row
//! patterns, the corruption matrix with exact messages, a seeded
//! never-panics fuzz, inadmissible-type refusal, and plain-equivalence of
//! an fsst-read group.

use std::path::{Path, PathBuf};

use crc32c::crc32c;
use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::{DevonError, value::Value};
use proptest::collection;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"encoding-fsst!!!";
const GOLDEN_DB_ID: [u8; 16] = *b"enc-fsst-golden!";
const ANCHOR_FILE: &str = "encoding-fsst.devondb";
const HEADER_LEN: usize = 16;
const ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const PROPTEST_SEED: u64 = 0x4653_5354_5f46_5353;

/// The golden directory's `COLUMN_ENCODINGS` section: id plain, fsst,
/// fsst — parameters zero.
const GOLDEN_SECTION: [u8; 12] = [0, 0, 0, 0, 5, 0, 0, 0, 5, 0, 0, 0];
/// The frozen values-section payloads of the committed golden's FSST columns,
/// validity byte first:
/// - `name` (NULL at row 2 → validity `0b1011`): 4 symbols "alice-an",
///   "derson", "bob-brow", "n"; offsets 0,2,4,4,6; heap decodes
///   "alice-anderson", "bob-brown", "", "alice-anderson".
/// - `tag` (NULL at row 3 → validity `0b0111`): 2 symbols "frozen-t",
///   "ag"; offsets 0,2,4,6,6; heap decodes "frozen-tag" × 3, "".
const GOLDEN_NAME_PAYLOAD: [u8; 55] = [
    11, 4, 8, 97, 108, 105, 99, 101, 45, 97, 110, 6, 100, 101, 114, 115, 111, 110, 8, 98, 111, 98,
    45, 98, 114, 111, 119, 1, 110, 0, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0, 4, 0, 0, 0, 6, 0, 0, 0, 0,
    1, 2, 3, 0, 1,
];
const GOLDEN_TAG_PAYLOAD: [u8; 40] = [
    7, 2, 8, 102, 114, 111, 122, 101, 110, 45, 116, 2, 97, 103, 0, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0,
    6, 0, 0, 0, 6, 0, 0, 0, 0, 1, 0, 1, 0, 1,
];

fn golden_rows() -> Vec<Vec<Value>> {
    vec![
        vec![
            Value::Int64(1),
            Value::String("alice-anderson".to_owned()),
            Value::String("frozen-tag".to_owned()),
        ],
        vec![
            Value::Int64(2),
            Value::String("bob-brown".to_owned()),
            Value::String("frozen-tag".to_owned()),
        ],
        vec![
            Value::Int64(3),
            Value::Null,
            Value::String("frozen-tag".to_owned()),
        ],
        vec![
            Value::Int64(4),
            Value::String("alice-anderson".to_owned()),
            Value::Null,
        ],
    ]
}

fn golden_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Fsst".to_owned(),
        vec![
            column("id", LogicalType::Int64, true),
            column("name", LogicalType::String, false),
            column("tag", LogicalType::String, false),
        ],
    )
    .unwrap()
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn golden_types() -> Vec<LogicalType> {
    golden_schema()
        .columns()
        .iter()
        .map(|column| column.ty)
        .collect()
}

/// Mints the golden at `path` through the forcing hook, the
/// golden-anchor pattern of `crates/devondb/tests/golden.rs`:
/// deterministic db_id via `Pager::create`, pager-level group write,
/// catalog save (which derives feature bit 13).
fn mint_encoding_fsst_anchor_at(path: &Path) -> u64 {
    let pager = Pager::create(path, PAGE_SIZE, GOLDEN_DB_ID).unwrap();
    let schema = golden_schema();
    let types = golden_types();
    let mut group = NodeGroup::new(types).unwrap();
    for row in golden_rows() {
        group.push_row(row).unwrap();
    }
    let group_id = group
        .write_forcing_encodings(&pager, &[(1, 5), (2, 5)])
        .unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema).unwrap();
    catalog
        .set_table_storage(
            "Fsst",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();
    drop(pager);
    group_id
}

fn publish_features(pager: &Pager) {
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn += 1;
    superblock.feature_flags |= COLUMN_ENCODINGS_FLAG | ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
}

/// Writes a forced-fsst String group and publishes the feature bits.
fn write_fsst_group(directory: &TempDir, name: &str, rows: Vec<Value>) -> (Pager, u64) {
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    for row in rows {
        group.push_row(vec![row]).unwrap();
    }
    let directory_page = group.write_forcing_encodings(&pager, &[(0, 5)]).unwrap();
    publish_features(&pager);
    (pager, directory_page)
}

fn corrupt_context(error: &DevonError) -> &str {
    match error {
        DevonError::Corrupt { context } => context,
        other => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
#[ignore = "mints the committed encoding-fsst golden database"]
fn mint_encoding_fsst_anchor() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(ANCHOR_FILE);
    assert!(
        !path.exists(),
        "golden anchors are permanent; {ANCHOR_FILE} already exists"
    );
    mint_encoding_fsst_anchor_at(&path);
}

#[test]
fn encoding_fsst_anchor_mint_is_byte_deterministic() {
    let directory = tempdir().unwrap();
    let first = directory.path().join("fsst-first.devondb");
    let second = directory.path().join("fsst-second.devondb");

    mint_encoding_fsst_anchor_at(&first);
    mint_encoding_fsst_anchor_at(&second);

    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "minting identical encoding-fsst anchors produced different bytes"
    );
}

/// The frozen byte golden: the committed file's section and payload bytes
/// must equal the literals below, and the rows must decode exactly. The
/// literals pin the byte layout.
#[test]
fn committed_anchor_matches_the_frozen_bytes() {
    let golden = golden_path();
    let pager = Pager::open(&golden).unwrap();
    assert_eq!(
        pager.superblock().feature_flags & COLUMN_ENCODINGS_FLAG,
        COLUMN_ENCODINGS_FLAG,
        "the golden must carry feature bit 13"
    );
    let catalog = Catalog::load(&pager).unwrap();
    let types = golden_types();
    let group_id = catalog.table_storage("Fsst").unwrap().groups[0];
    let directory = pager.read_page(group_id).unwrap();

    assert_eq!(
        u32::from_le_bytes(directory[12..16].try_into().unwrap()),
        0b11,
        "zone maps + encodings directory flags"
    );
    let section_start = HEADER_LEN + 3 * ENTRY_LEN + SECTION_HEADER_LEN + 3 * ZONE_MAP_RECORD_LEN;
    assert_eq!(
        u32::from_le_bytes(
            directory[section_start..section_start + 4]
                .try_into()
                .unwrap()
        ),
        12
    );
    assert_eq!(
        &directory[section_start + SECTION_HEADER_LEN..section_start + SECTION_HEADER_LEN + 12],
        &GOLDEN_SECTION,
        "the directory section drifted from the frozen bytes"
    );

    assert_eq!(
        column_payload(&pager, &directory, 1),
        GOLDEN_NAME_PAYLOAD,
        "the name fsst payload drifted from the frozen bytes"
    );
    assert_eq!(
        column_payload(&pager, &directory, 2),
        GOLDEN_TAG_PAYLOAD,
        "the tag fsst payload drifted from the frozen bytes"
    );

    let decoded = NodeGroup::read(&pager, group_id, &types).unwrap();
    for (row, expected) in golden_rows().iter().enumerate() {
        for (column, value) in expected.iter().enumerate() {
            assert_eq!(
                decoded.value(row, column),
                Some(value),
                "row {row} column {column}"
            );
        }
    }
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/encoding-fsst.devondb")
}

/// Reads one column's encoded payload bytes through its directory entry.
fn column_payload(pager: &Pager, directory: &[u8], column: usize) -> Vec<u8> {
    let entry = HEADER_LEN + column * ENTRY_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    let byte_len = u32::from_le_bytes(directory[entry + 8..entry + 12].try_into().unwrap());
    let page = pager.read_page(first_page).unwrap();
    page[..byte_len as usize].to_vec()
}

#[test]
fn round_trip_over_deterministic_sets() {
    let repetitive = "devondb-fsst-round-trip".to_owned();
    let cases: Vec<Vec<Value>> = vec![
        vec![Value::String("single".to_owned())],
        vec![Value::Null],
        vec![Value::String(String::new())],
        vec![Value::Null; 3],
        vec![
            Value::String(repetitive.clone()),
            Value::Null,
            Value::String(String::new()),
            Value::String(repetitive.clone()),
            Value::Null,
            Value::String(repetitive),
        ],
        vec![Value::String("édith 🦀 − ünïcode".to_owned()); 3],
        // Symbol-length boundaries: 1, 8, 9 bytes, and a long string.
        vec![
            Value::String("a".to_owned()),
            Value::String("12345678".to_owned()),
            Value::String("123456789".to_owned()),
            Value::String("z".repeat(2048)),
        ],
        // Every ASCII byte, exercising escapes for rare bytes.
        vec![Value::String(
            (0_u8..=127).map(char::from).collect::<String>(),
        )],
    ];
    for (index, rows) in cases.into_iter().enumerate() {
        let directory = tempdir().unwrap();
        let (pager, directory_page) =
            write_fsst_group(&directory, &format!("case-{index}.devondb"), rows.clone());
        let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::String]).unwrap();
        for (row, value) in rows.iter().enumerate() {
            assert_eq!(
                decoded.value(row, 0),
                Some(value),
                "case {index} row {row} diverged"
            );
        }
        // The typed scan path (§6.5) decodes the same column identically.
        let (typed, row_count) =
            NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::String], 0)
                .unwrap();
        assert_eq!(row_count, rows.len());
        assert_eq!(typed, rows, "case {index} typed decode diverged");
    }
}

#[test]
fn round_trip_2048_rows() {
    let rows: Vec<Value> = (0..2048)
        .map(|index| {
            if index % 7 == 3 {
                Value::Null
            } else {
                Value::String(format!(
                    "https://www.example.devondb/users/{}/profile",
                    index % 97
                ))
            }
        })
        .collect();
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "full.devondb", rows.clone());
    let decoded = NodeGroup::read(&pager, directory_page, &[LogicalType::String]).unwrap();
    for (row, value) in rows.iter().enumerate() {
        assert_eq!(decoded.value(row, 0), Some(value), "row {row} diverged");
    }
    let (typed, row_count) =
        NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::String], 0).unwrap();
    assert_eq!(row_count, 2048);
    assert_eq!(typed, rows);
}

#[test]
fn fsst_group_reads_back_identical_to_plain() {
    let directory = tempdir().unwrap();
    let rows = vec![
        Value::String("same-same-same".to_owned()),
        Value::Null,
        Value::String("same-same-same".to_owned()),
        Value::String("other".to_owned()),
    ];
    let (fsst_pager, fsst_page) = write_fsst_group(&directory, "fsst.devondb", rows.clone());

    let plain_pager =
        Pager::create(directory.path().join("plain.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut plain = NodeGroup::new(vec![LogicalType::String]).unwrap();
    for row in &rows {
        plain.push_row(vec![row.clone()]).unwrap();
    }
    let plain_page = plain.write(&plain_pager).unwrap();
    publish_features(&plain_pager);

    let boxed_fsst = NodeGroup::read(&fsst_pager, fsst_page, &[LogicalType::String]).unwrap();
    let boxed_plain = NodeGroup::read(&plain_pager, plain_page, &[LogicalType::String]).unwrap();
    for row in 0..rows.len() {
        assert_eq!(boxed_fsst.value(row, 0), boxed_plain.value(row, 0));
    }

    let (typed_fsst, _) =
        NodeGroup::read_column_typed(&fsst_pager, fsst_page, &[LogicalType::String], 0).unwrap();
    let (typed_plain, _) =
        NodeGroup::read_column_typed(&plain_pager, plain_page, &[LogicalType::String], 0).unwrap();
    assert_eq!(typed_fsst, typed_plain);

    // The whole-group typed scan path agrees too.
    let directory_fsst =
        NodeGroup::read_directory(&fsst_pager, fsst_page, &[LogicalType::String]).unwrap();
    let columns = NodeGroup::read_columns_typed_from_directory(
        &fsst_pager,
        directory_fsst,
        &[LogicalType::String],
    )
    .unwrap();
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0], typed_plain);
}

#[test]
fn forcing_fsst_on_a_non_string_column_is_refused() {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join("int.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    group.push_row(vec![Value::Int64(1)]).unwrap();
    let error = group
        .write_forcing_encodings(&pager, &[(0, 5)])
        .expect_err("fsst on Int64 must be refused");
    let DevonError::InvalidArgument { context } = error else {
        panic!("expected InvalidArgument, got {error}");
    };
    assert_eq!(context, "encoding fsst is not admissible for Int64");
}

/// Doctors one column's payload bytes in place, fixing the directory
/// entry's byte_len and CRC so the corruption under test is the section
/// content, not the frame.
fn doctor_payload(pager: &Pager, directory_page: u64, payload: &[u8]) -> DevonError {
    let mut directory = pager.read_page(directory_page).unwrap();
    let entry = HEADER_LEN;
    let first_page = u64::from_le_bytes(directory[entry..entry + 8].try_into().unwrap());
    directory[entry + 8..entry + 12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    directory[entry + 12..entry + 16].copy_from_slice(&crc32c(payload).to_le_bytes());
    pager.write_page(first_page, &page_with(payload)).unwrap();
    pager.write_page(directory_page, &directory).unwrap();
    NodeGroup::read(pager, directory_page, &[LogicalType::String])
        .expect_err("doctored payload must not decode")
}

fn page_with(payload: &[u8]) -> Vec<u8> {
    let mut page = vec![0_u8; PAGE_SIZE as usize];
    page[..payload.len()].copy_from_slice(payload);
    page
}

/// The doctored payloads below target a two-row group (one NULL) whose
/// honest payload is produced by `write_fsst_group`; each case rebuilds a
/// fresh fixture because doctoring mutates the pager.
#[test]
fn fsst_corruption_matrix_reports_exact_messages() {
    let rows = || {
        vec![
            Value::String("matrix-matrix".to_owned()),
            Value::Null,
            Value::String("matrix-matrix".to_owned()),
        ]
    };
    // Validity for 3 rows with NULL at row 1.
    const VALIDITY: u8 = 0b101;

    // Empty values section (validity only).
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "empty.devondb", rows());
    let error = doctor_payload(&pager, directory_page, &[VALIDITY]);
    assert_eq!(
        corrupt_context(&error),
        "fsst values section is empty, expected at least the symbol count"
    );

    // Symbol length zero.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "len0.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[VALIDITY, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst symbol 0 length is 0, expected 1..=8"
    );

    // Symbol length above 8.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "len9.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[VALIDITY, 1, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst symbol 0 length is 9, expected 1..=8"
    );

    // Symbol table runs past the section.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "trunc.devondb", rows());
    let error = doctor_payload(&pager, directory_page, &[VALIDITY, 1, 4]);
    assert_eq!(
        corrupt_context(&error),
        "fsst symbol 0 is truncated: 4 bytes declared, 0 remain"
    );

    // Offsets array shorter than (row_count + 1) × 4.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "offs.devondb", rows());
    let error = doctor_payload(&pager, directory_page, &[VALIDITY, 0, 0, 0, 0, 0]);
    assert_eq!(
        corrupt_context(&error),
        "fsst offsets section is 4 bytes, expected 16 for 3 rows"
    );

    // First offset nonzero: symbol_count 0, offsets [1, 1, 1, 1], heap
    // empty — but the start-at-0 law trips first.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "start.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[VALIDITY, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst offsets start at 1, expected 0"
    );

    // Decreasing offsets.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "dec.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[VALIDITY, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    assert_eq!(corrupt_context(&error), "fsst offsets decrease at row 1");

    // Last offset disagrees with the heap length.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "end.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[VALIDITY, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst offsets end at 5 but the heap is 0 bytes"
    );

    // A NULL row (row 1) with a nonempty code stream: offsets
    // [0, 0, 2, 2] over a 2-byte heap holding an escaped 'a'.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "null.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[
            VALIDITY, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 255, b'a',
        ],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst NULL row 1 has a nonempty code stream"
    );

    // A code past the symbol table: symbol_count 0, row 0's stream holds
    // code 1 (255 is the escape; 1 ≥ 0 is corruption).
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "code.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[
            VALIDITY, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1,
        ],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst heap byte at offset 0 is code 1, but the symbol table has 0 symbols"
    );

    // An escape as the row's last code byte.
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "esc.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[
            VALIDITY, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 255,
        ],
    );
    assert_eq!(
        corrupt_context(&error),
        "fsst escape at heap offset 0 has no literal byte"
    );

    // Invalid UTF-8 after decoding: row 0's stream is [escape, 0xff].
    let directory = tempdir().unwrap();
    let (pager, directory_page) = write_fsst_group(&directory, "utf8.devondb", rows());
    let error = doctor_payload(
        &pager,
        directory_page,
        &[
            VALIDITY, 0, 0, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 255, 0xff,
        ],
    );
    let context = corrupt_context(&error);
    assert!(
        context.starts_with("fsst decoded row 0 is not valid UTF-8"),
        "{context}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    /// The fuzz_decode idiom: arbitrary payload bytes under a fixed,
    /// frame-valid directory must never panic the fsst decoder.
    #[test]
    fn arbitrary_fsst_payloads_never_panic(
        byte_len in 0_usize..=96,
        noise in collection::vec(any::<u8>(), 0..=96),
    ) {
        let directory = tempdir().unwrap();
        let pager = Pager::create(
            directory.path().join("fuzz.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap();
        let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
        group
            .push_row(vec![Value::String("fuzz-fuzz".to_owned())])
            .unwrap();
        group.push_row(vec![Value::Null]).unwrap();
        group
            .push_row(vec![Value::String("fuzz-fuzz".to_owned())])
            .unwrap();
        let directory_page = group.write_forcing_encodings(&pager, &[(0, 5)]).unwrap();
        publish_features(&pager);

        let mut directory_page_bytes = pager.read_page(directory_page).unwrap();
        let first_page =
            u64::from_le_bytes(directory_page_bytes[HEADER_LEN..HEADER_LEN + 8].try_into().unwrap());
        let byte_len = byte_len.min(noise.len());
        let payload = &noise[..byte_len];
        directory_page_bytes[HEADER_LEN + 8..HEADER_LEN + 12]
            .copy_from_slice(&(byte_len as u32).to_le_bytes());
        directory_page_bytes[HEADER_LEN + 12..HEADER_LEN + 16]
            .copy_from_slice(&crc32c(payload).to_le_bytes());
        pager.write_page(first_page, &page_with(payload)).unwrap();
        pager.write_page(directory_page, &directory_page_bytes).unwrap();

        let outcome = std::panic::catch_unwind(|| {
            let _ = NodeGroup::read(&pager, directory_page, &[LogicalType::String]);
            let _ = NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::String], 0);
        });
        prop_assert!(outcome.is_ok(), "fsst decoder panicked");
    }

    /// Round-trip property: arbitrary deterministic string sets with
    /// arbitrary NULL patterns survive encode → decode byte-identically.
    #[test]
    fn arbitrary_string_columns_round_trip(
        rows in collection::vec(
            prop::option::of(
                prop_oneof![
                    Just(String::new()),
                    Just("frozen-fsst-symbol".to_owned()),
                    "[ -~]{0,24}",
                    "[\\p{L}🦀−]{0,12}",
                ]
            ),
            1..=64,
        ),
    ) {
        let values: Vec<Value> = rows
            .iter()
            .map(|row| match row {
                Some(text) => Value::String(text.clone()),
                None => Value::Null,
            })
            .collect();
        let directory = tempdir().unwrap();
        let (pager, directory_page) = write_fsst_group(&directory, "prop.devondb", values.clone());
        let (typed, row_count) =
            NodeGroup::read_column_typed(&pager, directory_page, &[LogicalType::String], 0)
                .unwrap();
        prop_assert_eq!(row_count, values.len());
        prop_assert_eq!(typed, values);
    }
}
