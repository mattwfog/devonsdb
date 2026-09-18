//! Adversarial storage tests against the binding specifications.
//!
//! Every test names the law it checks and cites its specification section.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use crc32c::{crc32c, crc32c_append};
use devondb_storage::budget::MemoryBudget;
use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::csr_group::CsrGroup;
use devondb_storage::free_pages::{
    EXTENSION_END, EXTENSION_OFFSET, LEDGER_ENTRIES_OFFSET, LedgerEntry, LedgerPage,
    SuperblockExtension,
};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::overlay::{CommitDelta, CommitLink, OverlayEdge, PublishedState};
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{
    COLUMN_ENCODINGS_FLAG, FREE_PAGES_FLAG, SUPPORTED_FORMAT_VERSION, Superblock, ZONE_MAPS_FLAG,
    choose_authoritative,
};
use devondb_storage::txn_log::{WalPayload, group_transactions};
use devondb_storage::wal::{WalReader, WalWriter, replay};
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::value::Value;
use proptest::collection;
use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const PAGE_BYTES: usize = 4096;
const DB_ID: [u8; 16] = *b"adversarial-db!!";
const PROPTEST_SEED: u64 = 0x2144_5653_5441_5200;
const SUPERBLOCK_HEADER_LEN: usize = 64;
const SUPERBLOCK_CRC_OFFSET: usize = 60;
const WAL_HEADER_LEN: usize = 16;
const DIRECTORY_HEADER_LEN: usize = 16;
const DIRECTORY_ENTRY_LEN: usize = 16;
const SECTION_HEADER_LEN: usize = 8;
const ZONE_MAP_RECORD_LEN: usize = 24;
const LEDGER_CRC_OFFSET: usize = 24;
const EXTENSION_CRC_OFFSET: usize = 88;

fn pager_at(name: &str) -> (TempDir, Pager) {
    let directory = tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    (directory, pager)
}

fn raw_page(path: &Path, page_id: u64) -> Vec<u8> {
    let mut file = OpenOptions::new().read(true).open(path).unwrap();
    file.seek(SeekFrom::Start(page_id * u64::from(PAGE_SIZE)))
        .unwrap();
    let mut page = vec![0_u8; PAGE_BYTES];
    file.read_exact(&mut page).unwrap();
    page
}

fn write_raw_page(path: &Path, page_id: u64, page: &[u8]) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(page_id * u64::from(PAGE_SIZE)))
        .unwrap();
    file.write_all(page).unwrap();
    file.sync_all().unwrap();
}

fn flip_byte(path: &Path, page_id: u64, offset: usize, mask: u8) {
    let mut page = raw_page(path, page_id);
    page[offset] ^= mask;
    write_raw_page(path, page_id, &page);
}

fn flip_wal_byte(path: &Path, byte_offset: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(byte_offset)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(byte_offset)).unwrap();
    file.write_all(&[byte[0] ^ 0x20]).unwrap();
}

fn schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Doc".to_owned(),
        vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "body".to_owned(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    )
    .unwrap()
}

fn node_group(rows: usize) -> NodeGroup {
    let mut group = NodeGroup::new(vec![LogicalType::Int64, LogicalType::String]).unwrap();
    for index in 0..rows {
        group
            .push_row(vec![
                Value::Int64(index as i64),
                Value::String(format!("body-{index}-{}", "x".repeat(48))),
            ])
            .unwrap();
    }
    group
}

fn catalog_with_table() -> Catalog {
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema()).unwrap();
    catalog
}

fn publish_generation(pager: &Pager, catalog: &mut Catalog, publish_lsn: u64, first_id: i64) {
    let mut group = NodeGroup::new(vec![LogicalType::Int64, LogicalType::String]).unwrap();
    for offset in 0..8_i64 {
        group
            .push_row(vec![
                Value::Int64(first_id + offset),
                Value::String(format!("body-{first_id}-{offset}-{}", "x".repeat(48))),
            ])
            .unwrap();
    }
    let group_id = group.write(pager).unwrap();
    catalog
        .set_table_storage(
            "Doc",
            TableStorage {
                groups: vec![group_id],
            },
        )
        .unwrap();
    catalog.save(pager, publish_lsn).unwrap();
}

fn ledger_chain(pager: &Pager) -> Vec<(u64, LedgerPage)> {
    pager.free_pages_ledger_pages().unwrap()
}

fn ledger_entry_count(chain: &[(u64, LedgerPage)]) -> usize {
    chain.iter().map(|(_, page)| page.entries.len()).sum()
}

fn consume_ledger(pager: &Pager) -> Vec<u64> {
    let count = ledger_entry_count(&ledger_chain(pager));
    (0..count).map(|_| pager.allocate_page().unwrap()).collect()
}

fn encode_superblock(superblock: &Superblock) -> Vec<u8> {
    let page_len = PAGE_SIZE.max(65_536) as usize;
    let mut page = vec![0xff_u8; page_len];
    superblock.encode(&mut page).unwrap();
    page
}

fn sample_superblock(checkpoint_lsn: u64, flags: u64) -> Superblock {
    Superblock {
        format_version: SUPPORTED_FORMAT_VERSION,
        min_reader_version: SUPPORTED_FORMAT_VERSION,
        feature_flags: flags,
        page_size: PAGE_SIZE,
        db_id: DB_ID,
        checkpoint_lsn,
        catalog_root: 7,
    }
}

fn wal_bytes(lsn: u64, payload: &[u8], corrupt_crc: bool) -> Vec<u8> {
    let checksum = crc32c_append(crc32c(&lsn.to_le_bytes()), payload);
    let mut bytes = Vec::with_capacity(WAL_HEADER_LEN + payload.len());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(if corrupt_crc { checksum ^ 1 } else { checksum }).to_le_bytes());
    bytes.extend_from_slice(&lsn.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn arbitrary_wal_contents() -> BoxedStrategy<Vec<u8>> {
    let high_entropy = collection::vec(any::<u8>(), 0..=16_384);
    let giant_header = collection::vec(any::<u8>(), 0..=2_048).prop_map(|body| {
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[9_u8; 12]);
        bytes.extend(body);
        bytes
    });
    let valid_then_corrupt = collection::vec(collection::vec(any::<u8>(), 0..=512), 0..=3)
        .prop_map(|payloads| {
            let mut bytes = Vec::new();
            for (index, payload) in payloads.iter().enumerate() {
                bytes.extend(wal_bytes(u64::try_from(index).unwrap(), payload, false));
            }
            if !bytes.is_empty() {
                let last = bytes.len() - 1;
                bytes[last] ^= 0x80;
            }
            bytes
        });
    prop_oneof![3 => high_entropy, 1 => giant_header, 2 => valid_then_corrupt].boxed()
}

fn assert_corrupt<T>(result: Result<T, devondb_types::DevonError>, expected: &str) {
    match result {
        Err(devondb_types::DevonError::Corrupt { context }) => assert!(
            context.contains(expected),
            "wrong corruption context\nactual:   {context}\nexpected: {expected}"
        ),
        Err(other) => panic!("expected Corrupt({expected}), got {other}"),
        Ok(_) => panic!("expected Corrupt({expected}), decoded successfully"),
    }
}

fn delta(rows: &[i64], edges: &[(u64, u64, i64)]) -> CommitDelta {
    let mut delta = CommitDelta::default();
    delta.nodes.insert(
        "Person".to_owned(),
        rows.iter()
            .map(|id| vec![Value::Int64(*id), Value::String(format!("person-{id}"))])
            .collect(),
    );
    delta.edges.insert(
        "Knows".to_owned(),
        edges
            .iter()
            .map(|(from, to, value)| OverlayEdge {
                from: *from,
                to: *to,
                values: vec![Value::Int64(*value)],
            })
            .collect(),
    );
    delta
}

fn published_state(chain: Option<Arc<CommitLink>>) -> PublishedState {
    let last_commit_lsn = chain.as_ref().map_or(0, |link| link.commit_lsn);
    let mut catalog = Catalog::default();
    let person_schema = NodeTableSchema::new(
        "Person".to_owned(),
        vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "name".to_owned(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    )
    .unwrap();
    catalog.add_node_table(person_schema).unwrap();
    PublishedState {
        catalog: Arc::new(catalog),
        chain,
        last_commit_lsn,
        catalog_generation: 0,
        recent_summaries: Vec::new(),
    }
}

fn link(
    prev: Option<Arc<CommitLink>>,
    lsn: u64,
    delta_value: CommitDelta,
    budget: &Arc<MemoryBudget>,
) -> Arc<CommitLink> {
    CommitLink::new_arc(prev, lsn, delta_value, Arc::clone(budget)).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(PROPTEST_SEED),
        ..ProptestConfig::default()
    })]

    /// FORMAT § WAL sidecar: arbitrary sidecar bytes never panic any public decode path.
    #[test]
    fn fuzzed_wal_never_panics_public_decode_paths(bytes in arbitrary_wal_contents()) {
        let directory = tempdir().unwrap();
        let path = directory.path().join("fuzz.devondb-wal");
        fs::write(&path, bytes).unwrap();
        let _ = replay(&path);
        if let Ok(reader) = WalReader::open(&path) {
            let _: Vec<_> = reader.collect();
        }
        let _ = WalWriter::open(&path, 0);
    }

    /// FREE_PAGES § Ledger pages: fuzzed ledger pages return errors or decode; never panic.
    #[test]
    fn fuzzed_ledger_pages_never_panic(bytes in collection::vec(any::<u8>(), 0..=PAGE_BYTES)) {
        let mut page = vec![0_u8; PAGE_BYTES];
        let copy_len = bytes.len().min(PAGE_BYTES);
        page[..copy_len].copy_from_slice(&bytes[..copy_len]);
        let _ = LedgerPage::decode(&page);
    }

    /// FORMAT § Superblock: fuzzed slot prefixes are rejected without panicking.
    #[test]
    fn fuzzed_superblock_prefixes_never_panic(prefix in collection::vec(any::<u8>(), 0..SUPERBLOCK_HEADER_LEN)) {
        let mut page = encode_superblock(&sample_superblock(9, 0));
        page[..prefix.len()].copy_from_slice(&prefix);
        let _ = Superblock::decode(&page);
    }
}

/// FORMAT § Superblock: header CRC covers exactly bytes 0..60.
#[test]
fn superblock_crc_frames_exactly_the_first_sixty_bytes() {
    let mut page = encode_superblock(&sample_superblock(42, 0));
    for offset in 0..SUPERBLOCK_CRC_OFFSET {
        let original = page[offset];
        page[offset] ^= 0x80;
        assert!(
            Superblock::decode(&page).is_err(),
            "byte {offset} is outside CRC framing"
        );
        page[offset] = original;
    }
    let checksum = u32::from_le_bytes(
        page[SUPERBLOCK_CRC_OFFSET..SUPERBLOCK_HEADER_LEN]
            .try_into()
            .unwrap(),
    );
    assert_eq!(checksum, crc32c(&page[..SUPERBLOCK_CRC_OFFSET]));
}

/// FORMAT § Superblock extension / FREE_PAGES § On-disk layout: extension damage does not affect header arbitration.
#[test]
fn superblock_extension_crc_is_independent_of_slot_crc() {
    let extension = SuperblockExtension {
        retire_ledger_head: 11,
        retire_ledger_tail: 11,
        retired_total: 3,
    };
    let mut page = encode_superblock(&sample_superblock(80, FREE_PAGES_FLAG));
    extension.encode_into(&mut page).unwrap();
    assert_eq!(SuperblockExtension::decode_from(&page), Some(extension));

    let original = page[70];
    page[70] ^= 0xff;
    assert_eq!(SuperblockExtension::decode_from(&page), None);
    assert!(Superblock::decode(&page).is_ok());
    page[70] = original;

    let header_checksum = u32::from_le_bytes(
        page[SUPERBLOCK_CRC_OFFSET..SUPERBLOCK_HEADER_LEN]
            .try_into()
            .unwrap(),
    );
    assert_eq!(header_checksum, crc32c(&page[..SUPERBLOCK_CRC_OFFSET]));
}

/// FORMAT § Superblock extension / FREE_PAGES § On-disk layout: bytes 92+ stay writer-zeroed.
#[test]
fn free_pages_extension_claims_only_bytes_64_to_91() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("extension-fence.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);

    assert_ne!(pager.superblock().feature_flags & FREE_PAGES_FLAG, 0);
    let authoritative_page = pager.superblock().checkpoint_lsn % 2;
    let page = raw_page(&path, authoritative_page);
    assert_eq!(
        SuperblockExtension::decode_from(&page).map(|extension| extension.has_ledger()),
        Some(true)
    );
    assert!(
        page[EXTENSION_END..].iter().all(|byte| *byte == 0),
        "writer wrote nonzero byte after claimed extension"
    );
    assert!(
        page[EXTENSION_OFFSET..EXTENSION_END]
            .iter()
            .any(|byte| *byte != 0)
    );
}

/// FORMAT § Superblock: arbitration chooses highest valid checkpoint LSN and ignores torn extensions.
#[test]
fn slot_arbitration_uses_header_lsn_and_ignores_extension_damage() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("arbitration.devondb");

    {
        let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
        let mut newer = pager.superblock();
        newer.checkpoint_lsn = 90;
        newer.catalog_root = 9;
        pager.commit_superblock(newer).unwrap();
    }

    let old_slot = 1; // generation 90 lands in slot 0, so the stale copy is slot 1
    let mut old_page = raw_page(&path, old_slot);
    let extension = SuperblockExtension {
        retire_ledger_head: 77,
        retire_ledger_tail: 77,
        retired_total: 1,
    };
    extension.encode_into(&mut old_page).unwrap();
    old_page[EXTENSION_CRC_OFFSET] ^= 0x55;
    write_raw_page(&path, old_slot, &old_page);

    let pager = Pager::open(&path).unwrap();
    assert_eq!(pager.superblock().checkpoint_lsn, 90);
    assert_eq!(pager.superblock().catalog_root, 9);
}

/// FORMAT § Superblock: equal-LSN arbitration deterministically prefers slot zero.
#[test]
fn equal_lsn_arbitration_prefers_slot_zero() {
    let left = Ok(sample_superblock(50, 0));
    let right = Ok(sample_superblock(50, FREE_PAGES_FLAG));
    let left_flags = sample_superblock(50, 0).feature_flags;
    assert_eq!(
        choose_authoritative(left, right).unwrap().feature_flags,
        left_flags
    );
}

/// FORMAT § Superblock: a higher-LSN slot with an unsupported min_reader_version is fatal, not ignored.
#[test]
fn version_mismatch_is_fatal_even_when_the_other_slot_is_valid() {
    let mut future = sample_superblock(100, 0);
    future.min_reader_version = SUPPORTED_FORMAT_VERSION + 1;
    let result = choose_authoritative(
        Ok(sample_superblock(10, 0)),
        Err(devondb_types::DevonError::VersionMismatch {
            file_version: SUPPORTED_FORMAT_VERSION,
            min_reader_version: SUPPORTED_FORMAT_VERSION + 1,
            supported: SUPPORTED_FORMAT_VERSION,
        }),
    );
    assert!(matches!(
        result,
        Err(devondb_types::DevonError::VersionMismatch { .. })
    ));
    let _ = future;
}

/// FORMAT § WAL sidecar: CRC covers LSN plus payload; any body byte breaks the record.
#[test]
fn wal_crc_frames_lsn_and_entire_payload() {
    let payload = b"canonical compact JSON";
    for byte_index in 0..payload.len() {
        let mut damaged = payload.to_vec();
        damaged[byte_index] ^= 0x40;
        let mut bytes = wal_bytes(7, &damaged, false);
        let checksum = crc32c_append(crc32c(&7_u64.to_le_bytes()), payload);
        bytes[WAL_HEADER_LEN + 4..WAL_HEADER_LEN + 8].copy_from_slice(&checksum.to_le_bytes());
        let path = Path::new("/tmp/devondb-adversarial-wal-frame");
        fs::write(path, &bytes).unwrap();
        assert!(
            replay(path).unwrap().is_empty(),
            "payload byte {byte_index} escaped CRC framing"
        );
    }
    let _ = fs::remove_file(Path::new("/tmp/devondb-adversarial-wal-frame"));
}

/// FORMAT § WAL sidecar: a torn tail after the last intact record is silently discarded.
#[test]
fn torn_wal_tail_is_silently_discarded() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("torn.devondb-wal");
    let mut writer = WalWriter::open(&path, 3).unwrap();
    writer.append(b"first").unwrap();
    writer.append(b"second").unwrap();
    writer.sync().unwrap();
    drop(writer);

    let mut bytes = fs::read(&path).unwrap();
    bytes.truncate(bytes.len() - 2);
    fs::write(&path, &bytes).unwrap();
    assert_eq!(replay(&path).unwrap().len(), 1);
}

/// FORMAT § WAL sidecar: mid-file corruption truncates replay to the known-good prefix and repairs on reopen.
#[test]
fn mid_file_wal_corruption_truncates_to_known_good_prefix() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("midfile.devondb-wal");
    let mut writer = WalWriter::open(&path, 11).unwrap();
    writer.append(b"good-one").unwrap();
    writer.sync().unwrap();
    let known_good_len = fs::metadata(&path).unwrap().len();
    writer.append(b"victim").unwrap();
    writer.append(b"after").unwrap();
    writer.sync().unwrap();
    drop(writer);

    flip_wal_byte(&path, known_good_len + WAL_HEADER_LEN as u64);
    assert_eq!(replay(&path).unwrap().len(), 1);
    let mut repaired = WalWriter::open(&path, 11).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), known_good_len);
    let lsn = repaired.append(b"replacement").unwrap();
    assert_eq!(lsn, 12);
}

/// FORMAT § WAL sidecar: commit record count mismatch is corruption naming the commit LSN.
#[test]
fn wal_commit_count_mismatch_is_corruption_at_commit_lsn() {
    let records = [
        WalPayload::NodeInsert {
            table: "Alpha".to_owned(),
            row: vec![Value::Int64(1)],
        },
        WalPayload::Commit { records: 2 },
    ];
    let encoded = devondb_storage::txn_log::encode_payload(&records[1]).unwrap();
    let error = group_transactions(
        [
            (
                0_u64,
                devondb_storage::txn_log::encode_payload(&records[0]).unwrap(),
            ),
            (1_u64, encoded),
        ],
        0,
    )
    .unwrap_err();
    assert!(
        matches!(&error, devondb_types::DevonError::Corrupt { context }
            if context.contains("LSN 1") && context.contains("declares 2") && context.contains("contains 1")),
        "{error}"
    );
}

/// FORMAT § WAL sidecar: trailing in-flight groups without commits are discarded.
#[test]
fn trailing_uncommitted_wal_group_is_discarded() {
    let payloads = [
        WalPayload::NodeInsert {
            table: "Alpha".to_owned(),
            row: vec![Value::Int64(1)],
        },
        WalPayload::Commit { records: 1 },
        WalPayload::NodeInsert {
            table: "Beta".to_owned(),
            row: vec![Value::Int64(2)],
        },
    ];
    let encoded: Vec<Vec<u8>> = payloads
        .iter()
        .map(devondb_storage::txn_log::encode_payload)
        .collect::<Result<_, _>>()
        .unwrap();
    let groups = group_transactions(
        encoded
            .iter()
            .enumerate()
            .map(|(index, bytes)| (u64::try_from(index).unwrap(), bytes.clone())),
        0,
    )
    .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].commit_lsn, 1);
}

/// FORMAT § WAL sidecar: complete groups at or below checkpoint_lsn are skipped entirely.
#[test]
fn materialized_wal_groups_are_skipped_by_checkpoint_lsn() {
    let first = WalPayload::NodeInsert {
        table: "Alpha".to_owned(),
        row: vec![Value::Int64(1)],
    };
    let second = WalPayload::NodeInsert {
        table: "Beta".to_owned(),
        row: vec![Value::Int64(2)],
    };
    let records = [
        first.clone(),
        WalPayload::Commit { records: 1 },
        second.clone(),
        WalPayload::Commit { records: 1 },
    ];
    let encoded: Vec<Vec<u8>> = records
        .iter()
        .map(devondb_storage::txn_log::encode_payload)
        .collect::<Result<_, _>>()
        .unwrap();
    let groups = group_transactions(
        encoded
            .iter()
            .enumerate()
            .map(|(index, bytes)| (u64::try_from(index).unwrap(), bytes.clone())),
        1,
    )
    .unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].commit_lsn, 3);
    assert_eq!(groups[0].records.len(), 1);
    assert_eq!(groups[0].records[0].begin_lsn, 3);
    assert_eq!(groups[0].records[0].payload, second);
}

/// FORMAT § WAL sidecar: canonical order rejects out-of-phase DML kinds at grouping time.
#[test]
fn wal_group_rejects_out_of_phase_record_kinds() {
    let records = [
        WalPayload::RelInsert {
            rel: "Knows".to_owned(),
            from: 0,
            to: 1,
            values: vec![],
        },
        WalPayload::NodeInsert {
            table: "Alpha".to_owned(),
            row: vec![Value::Int64(1)],
        },
        WalPayload::Commit { records: 2 },
    ];
    let encoded: Vec<Vec<u8>> = records
        .iter()
        .map(devondb_storage::txn_log::encode_payload)
        .collect::<Result<_, _>>()
        .unwrap();
    let error = group_transactions(
        encoded
            .iter()
            .enumerate()
            .map(|(index, bytes)| (u64::try_from(index).unwrap(), bytes.clone())),
        0,
    )
    .unwrap_err();
    assert!(
        matches!(error, devondb_types::DevonError::Corrupt { .. }),
        "{error}"
    );
}

/// FORMAT § Node group pages: directory magic, row_count, column_count, and unknown flags are fenced.
#[test]
fn node_group_directory_header_fields_are_fenced() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("node-header.devondb");
    let types = vec![LogicalType::Int64, LogicalType::String];
    let expected = node_group(3);
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = expected.write(&pager).unwrap();
    pager.sync().unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    // Writes with the COLUMN_ENCODINGS feature (FORMAT.md § Feature flag
    // registry, bit 13) may select non-plain column encodings,
    // which sets directory flags bit 1; the reader fences that bit unless
    // the superblock admits the feature, so the fixture must claim it.
    superblock.feature_flags |= ZONE_MAPS_FLAG | COLUMN_ENCODINGS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    drop(pager);

    let cases = [
        (0_u64, 4_usize, &b"XGRP"[..], "magic"),
        (4, 4, &0_u32.to_le_bytes()[..], "row_count is zero"),
        (8, 4, &0_u32.to_le_bytes()[..], "column_count is zero"),
        (
            12,
            4,
            &(1_u32 | (1 << 31)).to_le_bytes()[..],
            "unknown bits",
        ),
    ];
    for (page_offset, width, replacement, expected_context) in cases {
        let offset = page_offset as usize;
        let original = raw_page(&path, directory_page)[offset..offset + width].to_vec();
        let mut page = raw_page(&path, directory_page);
        page[offset..offset + width].copy_from_slice(replacement);
        write_raw_page(&path, directory_page, &page);
        let pager = Pager::open(&path).unwrap();
        assert_corrupt(
            NodeGroup::read(&pager, directory_page, &types),
            expected_context,
        );
        drop(pager);
        let mut restored = raw_page(&path, directory_page);
        restored[offset..offset + width].copy_from_slice(&original);
        write_raw_page(&path, directory_page, &restored);
    }
    let pager = Pager::open(&path).unwrap();
    assert_eq!(
        NodeGroup::read(&pager, directory_page, &types).unwrap(),
        expected
    );
}

/// FORMAT § Node group pages: every directory padding byte stays writer-zero and reader-fenced.
#[test]
fn node_group_directory_padding_is_zero_fenced() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("node-padding.devondb");
    let types = vec![LogicalType::Int64, LogicalType::String];
    let expected = node_group(2);
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = expected.write(&pager).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    // The adaptive writer (`docs/SCALE.md` §8.4) may select
    // non-plain column encodings, setting directory flags bit 1; the reader
    // fences that bit unless the superblock admits COLUMN_ENCODINGS
    // (FORMAT.md § Feature flag registry, bit 13), so the fixture claims it.
    superblock.feature_flags |= ZONE_MAPS_FLAG | COLUMN_ENCODINGS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    drop(pager);

    let entries_end = DIRECTORY_HEADER_LEN + DIRECTORY_ENTRY_LEN * types.len();
    let mut sections_end = entries_end + SECTION_HEADER_LEN + ZONE_MAP_RECORD_LEN * types.len();
    // A directory carrying COLUMN_ENCODINGS appends a second framed section
    // (FORMAT.md § Extension sections): 4 bytes per column after the header.
    let directory_flags =
        u32::from_le_bytes(raw_page(&path, directory_page)[12..16].try_into().unwrap());
    if directory_flags & (1 << 1) != 0 {
        sections_end += SECTION_HEADER_LEN + 4 * types.len();
    }
    for page_offset in [
        sections_end + 1,
        (sections_end + 1 + PAGE_BYTES - 1) / 2,
        PAGE_BYTES - 1,
    ] {
        let original = raw_page(&path, directory_page)[page_offset];
        assert_eq!(original, 0);
        flip_byte(&path, directory_page, page_offset, 0x5a);
        let pager = Pager::open(&path).unwrap();
        assert_corrupt(
            NodeGroup::read(&pager, directory_page, &types),
            "padding is not zero",
        );
        drop(pager);
        flip_byte(&path, directory_page, page_offset, 0x5a);
    }
}

/// FORMAT § Rel table adjacency: CSR directory padding is zero-fenced too.
#[test]
fn csr_group_directory_padding_is_zero_fenced() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("csr-padding.devondb");
    let types = vec![LogicalType::String, LogicalType::Int64];
    let mut expected = CsrGroup::new(3, types.clone()).unwrap();
    expected
        .push_edge(
            0,
            4,
            vec![Value::String("outgoing".to_owned()), Value::Int64(11)],
        )
        .unwrap();
    expected
        .push_edge(2, 1, vec![Value::Null, Value::Null])
        .unwrap();
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = expected.write(&pager).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    superblock.feature_flags |= ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    drop(pager);

    let entries_end = DIRECTORY_HEADER_LEN + DIRECTORY_ENTRY_LEN * (2 + types.len());
    for page_offset in [entries_end, PAGE_BYTES - 1] {
        assert_eq!(raw_page(&path, directory_page)[page_offset], 0);
        flip_byte(&path, directory_page, page_offset, 0xc3);
        let pager = Pager::open(&path).unwrap();
        assert_corrupt(
            CsrGroup::read(&pager, directory_page, &types),
            "padding is not zero",
        );
        drop(pager);
        flip_byte(&path, directory_page, page_offset, 0xc3);
    }
    let pager = Pager::open(&path).unwrap();
    assert_eq!(
        CsrGroup::read(&pager, directory_page, &types).unwrap(),
        expected
    );
}

/// FORMAT § Node group pages: payload CRC frames the declared run exactly.
#[test]
fn node_group_payload_crc_frames_declared_run() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("node-crc.devondb");
    let types = vec![LogicalType::Int64, LogicalType::String];
    let expected = node_group(2);
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = expected.write(&pager).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    // Claim COLUMN_ENCODINGS (bit 13) as in the other node-group
    // fixtures so the adaptive writer's directory flags pass the fence.
    superblock.feature_flags |= ZONE_MAPS_FLAG | COLUMN_ENCODINGS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    drop(pager);

    let directory_bytes = raw_page(&path, directory_page);
    for column in 0..types.len() {
        let entry_offset = DIRECTORY_HEADER_LEN + column * DIRECTORY_ENTRY_LEN;
        let payload_page = u64::from_le_bytes(
            directory_bytes[entry_offset..entry_offset + 8]
                .try_into()
                .unwrap(),
        );
        let byte_len = u32::from_le_bytes(
            directory_bytes[entry_offset + 8..entry_offset + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        let checksum = u32::from_le_bytes(
            directory_bytes[entry_offset + 12..entry_offset + 16]
                .try_into()
                .unwrap(),
        );
        assert_ne!(checksum, 0);
        // Flip bytes inside the declared run (never the zero-fenced page
        // padding beyond byte_len): first, middle, and last byte of the
        // first page's share of the run must each trip the CRC fence.
        let run_in_first_page = byte_len.min(PAGE_BYTES);
        for offset in [0, run_in_first_page / 2, run_in_first_page - 1] {
            flip_byte(&path, payload_page, offset, 0x01);
            let pager = Pager::open(&path).unwrap();
            assert_corrupt(
                NodeGroup::read(&pager, directory_page, &types),
                "CRC-32C does not match",
            );
            drop(pager);
            flip_byte(&path, payload_page, offset, 0x01);
        }
    }
    let pager = Pager::open(&path).unwrap();
    assert_eq!(
        NodeGroup::read(&pager, directory_page, &types).unwrap(),
        expected
    );
}

/// FORMAT § Rel table adjacency: CSR neighbor bounds are checked when the endpoint count is supplied.
#[test]
fn csr_checked_read_rejects_neighbor_outside_endpoint_row_count() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("csr-bounds.devondb");
    let types = vec![LogicalType::Int64];
    let mut group = CsrGroup::new(2, types.clone()).unwrap();
    group.push_edge(0, 9, vec![Value::Int64(1)]).unwrap();
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = group.write(&pager).unwrap();
    drop(pager);

    let unchecked = Pager::open(&path).unwrap();
    assert!(CsrGroup::read(&unchecked, directory_page, &types).is_ok());
    assert_corrupt(
        CsrGroup::read_checked(&unchecked, directory_page, &types, 4),
        "outside endpoint row count 4",
    );
    assert!(CsrGroup::read_checked(&unchecked, directory_page, &types, 10).is_ok());
}

/// FREE_PAGES § On-disk layout: ledger CRC frames header fields and the exact entry array.
#[test]
fn ledger_crc_frames_header_and_exact_entries() {
    let ledger = LedgerPage {
        next_page: 44,
        consumed_count: 1,
        entries: vec![
            LedgerEntry {
                page_id: 9,
                retired_lsn: 3,
            },
            LedgerEntry {
                page_id: 10,
                retired_lsn: 3,
            },
        ],
    };
    let page = ledger.encode(PAGE_BYTES).unwrap();
    assert_eq!(LedgerPage::decode(&page).unwrap(), ledger);

    for offset in [8_usize, 16, 20] {
        let mut damaged = page.clone();
        damaged[offset] ^= 0x08;
        assert!(
            LedgerPage::decode(&damaged).is_err(),
            "header byte {offset} escaped CRC framing"
        );
    }
    for index in 0..ledger.entries.len() {
        let entry_offset = LEDGER_ENTRIES_OFFSET + index * 16;
        let mut damaged = page.clone();
        damaged[entry_offset] ^= 0x04;
        assert!(
            LedgerPage::decode(&damaged).is_err(),
            "entry {index} escaped CRC framing"
        );
    }
    let mut reserved = page.clone();
    reserved[28] = 0x11;
    assert!(
        LedgerPage::decode(&reserved).is_ok(),
        "reserved word must stay outside CRC framing"
    );
    let _ = LEDGER_CRC_OFFSET;
}

/// FREE_PAGES § Ledger pages: consumed_count can never exceed entry_count.
#[test]
fn ledger_consumed_count_never_exceeds_entry_count() {
    let ledger = LedgerPage {
        next_page: 0,
        consumed_count: 2,
        entries: vec![LedgerEntry {
            page_id: 9,
            retired_lsn: 3,
        }],
    };
    assert!(ledger.encode(PAGE_BYTES).is_err());
    let mut page = vec![0_u8; PAGE_BYTES];
    page[..8].copy_from_slice(b"DEVONFPL");
    page[8..16].copy_from_slice(&0_u64.to_le_bytes());
    page[16..20].copy_from_slice(&1_u32.to_le_bytes());
    page[20..24].copy_from_slice(&2_u32.to_le_bytes());
    page[LEDGER_ENTRIES_OFFSET..LEDGER_ENTRIES_OFFSET + 8].copy_from_slice(&9_u64.to_le_bytes());
    page[LEDGER_ENTRIES_OFFSET + 8..LEDGER_ENTRIES_OFFSET + 16]
        .copy_from_slice(&3_u64.to_le_bytes());
    let checksum = crc32c_append(
        crc32c(&page[..LEDGER_CRC_OFFSET]),
        &page[LEDGER_ENTRIES_OFFSET..LEDGER_ENTRIES_OFFSET + 16],
    );
    page[LEDGER_CRC_OFFSET..LEDGER_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
    assert_corrupt(
        LedgerPage::decode(&page),
        "ledger consumed_count exceeds entry_count",
    );
}

/// FREE_PAGES § On-disk layout: chain walks stop at retire_ledger_tail even when next_page continues.
#[test]
fn ledger_chain_walk_stops_at_extension_tail() {
    let (_, pager) = pager_at("tail-stop.devondb");
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);

    let chain = ledger_chain(&pager);
    assert_eq!(chain.len(), 1);
    let (head, head_page) = &chain[0];
    let extension = pager.free_pages_extension().unwrap();
    assert_eq!(extension.retire_ledger_head, *head);
    assert_eq!(extension.retire_ledger_tail, *head);
    assert_eq!(head_page.next_page, 0);
}

/// FREE_PAGES § Retirement and reclamation sequence: retirement appends before publication and claims bit 5 only then.
#[test]
fn retirement_claims_bit_5_on_first_superseding_publication() {
    let (_, pager) = pager_at("retire-bit.devondb");
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    assert_eq!(pager.superblock().feature_flags & FREE_PAGES_FLAG, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    assert_ne!(pager.superblock().feature_flags & FREE_PAGES_FLAG, 0);
    let chain = ledger_chain(&pager);
    assert!(!chain.is_empty());
    assert!(
        chain
            .iter()
            .flat_map(|(_, page)| page.entries.iter())
            .all(|entry| entry.retired_lsn == 2 && entry.page_id >= 2)
    );
}

/// FREE_PAGES § The pin horizon: entries are reusable only after min_pin passes their retired_lsn.
#[test]
fn pin_horizon_gates_ledger_reuse() {
    let (_, pager) = pager_at("pin-horizon.devondb");
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let before = ledger_chain(&pager);
    assert!(!before.is_empty());

    let allocated_before_min_pin = pager.allocate_page().unwrap();
    assert!(
        ledger_chain(&pager)
            .iter()
            .flat_map(|(_, page)| page.entries.iter())
            .all(|entry| entry.page_id != allocated_before_min_pin)
    );

    pager.raise_min_pin(3);
    let reused = consume_ledger(&pager);
    assert!(!reused.is_empty());
    assert!(reused.iter().all(|page_id| {
        before
            .iter()
            .flat_map(|(_, page)| page.entries.iter())
            .any(|entry| entry.page_id == *page_id)
    }));
}

/// FREE_PAGES § Ledger pages: a fully consumed head recycles into a different ledger page.
#[test]
fn drained_ledger_head_recycles_without_owning_its_entry() {
    let (_, pager) = pager_at("drain-head.devondb");
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let old_head = ledger_chain(&pager)[0].0;

    pager.raise_min_pin(3);
    consume_ledger(&pager);
    publish_generation(&pager, &mut catalog, 3, 200);

    let chain = ledger_chain(&pager);
    assert!(!chain.is_empty());
    assert!(chain.iter().all(|(page_id, _)| *page_id != old_head));
    assert!(
        chain
            .iter()
            .any(|(_, page)| page.entries.iter().any(|entry| entry.page_id == old_head))
    );
    assert!(
        chain
            .iter()
            .all(|(page_id, page)| page.entries.iter().all(|entry| entry.page_id != *page_id))
    );
}

/// FREE_PAGES § On-disk layout: a torn authoritative extension is degraded mode, never Corrupt or slot flip.
#[test]
fn torn_free_pages_extension_degrades_without_slot_flip_or_corrupt() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("degraded.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let authoritative_page = pager.superblock().checkpoint_lsn % 2;
    drop(pager);

    flip_byte(&path, authoritative_page, 70, 0xff);
    let reopened = Pager::open(&path).unwrap();
    assert!(reopened.free_pages_degraded());
    assert_eq!(reopened.free_pages_extension(), None);
    assert_eq!(reopened.superblock().checkpoint_lsn, 2);
    assert!(reopened.allocate_page().is_ok());
}

/// FREE_PAGES § Crash windows: fresh ledger pages stay unreachable until the superblock flip names them.
#[test]
fn staged_retirement_is_invisible_until_superblock_flip() {
    let (_, pager) = pager_at("staged-retire.devondb");
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    let old_catalog_root = pager.superblock().catalog_root;
    let old_group = catalog.table_storage("Doc").unwrap().groups[0];

    let new_group = node_group(8).write(&pager).unwrap();
    catalog
        .set_table_storage(
            "Doc",
            TableStorage {
                groups: vec![new_group],
            },
        )
        .unwrap();
    catalog.save(&pager, 2).unwrap();

    let extension = pager.free_pages_extension().unwrap();
    assert!(extension.has_ledger());
    let chain = ledger_chain(&pager);
    assert!(
        chain
            .iter()
            .any(|(_, page)| page.entries.iter().any(|entry| entry.page_id == old_group))
    );
    assert!(chain.iter().any(|(_, page)| {
        page.entries
            .iter()
            .any(|entry| entry.page_id == old_catalog_root)
    }));
    assert_ne!(pager.superblock().catalog_root, old_catalog_root);
}

/// MVCC § MVCC and page immutability: checkpointed data pages listed by a catalog version are immutable.
#[test]
fn published_group_pages_are_immutable_across_republish() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("immutable-group.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    let first_group = catalog.table_storage("Doc").unwrap().groups[0];
    let first_snapshot = NodeGroup::read(
        &pager,
        first_group,
        &[LogicalType::Int64, LogicalType::String],
    )
    .unwrap();
    publish_generation(&pager, &mut catalog, 2, 100);
    let reopened = Pager::open(&path).unwrap();
    assert_eq!(
        NodeGroup::read(
            &reopened,
            first_group,
            &[LogicalType::Int64, LogicalType::String]
        )
        .unwrap(),
        first_snapshot
    );
}

/// MVCC §3 rule 2: overlay read order walks commit links oldest-first across shared prefixes.
#[test]
fn overlay_commit_links_iterate_oldest_first() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let first = link(None, 10, delta(&[1, 2], &[(0, 1, 100)]), &budget);
    let second = link(
        Some(Arc::clone(&first)),
        20,
        delta(&[3], &[(1, 2, 200)]),
        &budget,
    );
    let third = link(
        Some(Arc::clone(&second)),
        30,
        delta(&[], &[(2, 3, 300)]),
        &budget,
    );
    let state = published_state(Some(third));

    let lsns: Vec<_> = state
        .commit_links_oldest_first()
        .map(|link| link.commit_lsn)
        .collect();
    assert_eq!(lsns, [10, 20, 30]);
    let rows: Vec<i64> = state
        .node_rows("Person")
        .map(|row| match row[0] {
            Value::Int64(value) => value,
            _ => panic!("non-int row"),
        })
        .collect();
    assert_eq!(rows, [1, 2, 3]);
    let edges: Vec<(u64, u64)> = state
        .rel_edges("Knows")
        .map(|edge| (edge.from, edge.to))
        .collect();
    assert_eq!(edges, [(0, 1), (1, 2), (2, 3)]);
}

/// MVCC §3 rule 1: published links are immutable; older snapshots retain their own visibility.
#[test]
fn overlay_visibility_is_immutable_per_published_state() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let first = link(None, 10, delta(&[1], &[]), &budget);
    let snapshot = published_state(Some(Arc::clone(&first)));
    let second = link(Some(first), 20, delta(&[2], &[]), &budget);
    let current = published_state(Some(second));

    fn ids(state: &PublishedState) -> Vec<i64> {
        state
            .node_rows("Person")
            .map(|row| match row[0] {
                Value::Int64(value) => value,
                _ => panic!("non-int row"),
            })
            .collect()
    }
    assert_eq!(ids(&snapshot), [1]);
    assert_eq!(ids(&current), [1, 2]);
    assert_eq!(snapshot.last_commit_lsn, 10);
    assert_eq!(current.last_commit_lsn, 20);
}

/// MVCC §3 rule 3: overlay offsets remain checkpointed position plus oldest-first overlay position.
#[test]
fn overlay_positions_are_stable_append_order() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let first = link(None, 10, delta(&[7, 8], &[]), &budget);
    let second = link(Some(first), 20, delta(&[9], &[]), &budget);
    let state = published_state(Some(second));
    let rows: Vec<i64> = state
        .node_rows("Person")
        .map(|row| match row[0] {
            Value::Int64(value) => value,
            _ => panic!("non-int row"),
        })
        .collect();
    assert_eq!(rows, [7, 8, 9]);
}

/// MVCC §3 rule 4: dropping a uniquely owned long chain releases charges iteratively.
#[test]
fn uniquely_owned_long_commit_chain_drops_iteratively() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let mut head = None;
    for lsn in 1..=6_000_u64 {
        head = Some(link(head, lsn, delta(&[(lsn % 97) as i64], &[]), &budget));
    }
    drop(head);
    assert_eq!(budget.charged(), 0);
}

/// MVCC §7.2: overlay charge equals retained link charges plus retained summary estimates.
#[test]
fn overlay_charged_bytes_counts_links_not_budget_total() {
    let budget = Arc::new(MemoryBudget::unlimited());
    let first = link(None, 10, delta(&[1], &[]), &budget);
    let second = link(Some(Arc::clone(&first)), 20, delta(&[2], &[]), &budget);
    let state = published_state(Some(second));
    let expected = first.charged_bytes + state.chain.as_ref().unwrap().charged_bytes;
    assert_eq!(state.overlay_charged_bytes(), expected);
    assert!(state.overlay_charged_bytes() > 0);
}

/// MVCC §2.1/FORMAT § WAL sidecar: recovered deltas preserve canonical transaction order and stamp one commit LSN.
#[test]
fn recovered_transaction_delta_preserves_canonical_order_and_lsn() {
    let records = [
        WalPayload::NodeInsert {
            table: "Alpha".to_owned(),
            row: vec![Value::Int64(1)],
        },
        WalPayload::NodeInsert {
            table: "Beta".to_owned(),
            row: vec![Value::Int64(2)],
        },
        WalPayload::RelInsert {
            rel: "Knows".to_owned(),
            from: 0,
            to: 1,
            values: vec![Value::Int64(9)],
        },
        WalPayload::Commit { records: 3 },
    ];
    let encoded: Vec<Vec<u8>> = records
        .iter()
        .map(devondb_storage::txn_log::encode_payload)
        .collect::<Result<_, _>>()
        .unwrap();
    let mut groups = group_transactions(
        encoded
            .iter()
            .enumerate()
            .map(|(index, bytes)| (u64::try_from(index).unwrap(), bytes.clone())),
        0,
    )
    .unwrap();
    assert_eq!(groups.len(), 1);
    let delta = CommitDelta::try_from(groups.remove(0)).unwrap();
    let alpha = delta.nodes.get("Alpha").unwrap();
    let beta = delta.nodes.get("Beta").unwrap();
    assert_eq!(alpha.len(), 1);
    assert_eq!(beta.len(), 1);
    assert_eq!(delta.edges.get("Knows").map(Vec::len), Some(1));
}

/// FORMAT § WAL sidecar: WAL-assigned LSNs strictly increase and survive reader/writer round trips.
#[test]
fn wal_lsns_strictly_increase_across_round_trip() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("lsns.devondb-wal");
    let mut writer = WalWriter::open(&path, 41).unwrap();
    let assigned: Vec<_> = (0..5).map(|_| writer.append(b"record").unwrap()).collect();
    writer.sync().unwrap();
    drop(writer);
    assert_eq!(assigned, [41, 42, 43, 44, 45]);
    assert_eq!(
        replay(&path).unwrap(),
        assigned
            .iter()
            .map(|lsn| (*lsn, b"record".to_vec()))
            .collect::<Vec<_>>()
    );
}

/// FORMAT § WAL sidecar: reopening assigns LSNs above every intact prior record even with a torn tail.
#[test]
fn wal_repair_assigns_above_last_intact_record() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("repair-lsn.devondb-wal");
    let mut writer = WalWriter::open(&path, 7).unwrap();
    writer.append(b"intact").unwrap();
    writer.sync().unwrap();
    let intact_len = fs::metadata(&path).unwrap().len();
    writer.append(b"torn").unwrap();
    drop(writer);
    let mut bytes = fs::read(&path).unwrap();
    bytes.truncate(intact_len as usize + 3);
    fs::write(&path, &bytes).unwrap();

    let mut repaired = WalWriter::open(&path, 0).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), intact_len);
    assert_eq!(repaired.append(b"next").unwrap(), 8);
}

/// FORMAT § Compatibility rules: unknown unsupported feature bits refuse the file rather than decode.
#[test]
fn unknown_feature_bit_refuses_decode_as_version_mismatch() {
    let flagged = sample_superblock(42, 1 << 11);
    assert!(matches!(
        Superblock::decode(&encode_superblock(&flagged)),
        Err(devondb_types::DevonError::VersionMismatch { .. })
    ));
}

/// FORMAT § Feature flag registry: read-safe bits open read-only while unsupported writers refuse.
#[test]
fn read_safe_bits_are_admitted_but_writer_gated() {
    let read_safe = sample_superblock(42, FREE_PAGES_FLAG | (1 << 12));
    let decoded = Superblock::decode(&encode_superblock(&read_safe)).unwrap();
    assert!(decoded.requires_read_only());
    let plain = sample_superblock(42, 0);
    assert!(
        !Superblock::decode(&encode_superblock(&plain))
            .unwrap()
            .requires_read_only()
    );
}

/// FORMAT § Superblock: page-size laws reject non-powers-of-two and out-of-range sizes.
#[test]
fn superblock_page_size_bounds_are_enforced() {
    let mut page = encode_superblock(&sample_superblock(1, 0));
    let valid_size_bytes = page[24..28].to_vec();
    for page_size in [131_072_u32, 2048, 5000] {
        page[24..28].copy_from_slice(&page_size.to_le_bytes());
        let checksum = crc32c(&page[..SUPERBLOCK_CRC_OFFSET]);
        page[SUPERBLOCK_CRC_OFFSET..SUPERBLOCK_HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        assert!(Superblock::decode(&page).is_err(), "{page_size}");
    }
    page[24..28].copy_from_slice(&valid_size_bytes);
    let checksum = crc32c(&page[..SUPERBLOCK_CRC_OFFSET]);
    page[SUPERBLOCK_CRC_OFFSET..SUPERBLOCK_HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
    assert!(Superblock::decode(&page).is_ok());
    for page_size in [4096_u32, 8192, 65_536] {
        let mut valid = sample_superblock(1, 0);
        valid.page_size = page_size;
        assert!(
            Superblock::decode(&encode_superblock(&valid)).is_ok(),
            "{page_size}"
        );
    }
}

/// FORMAT § Superblock: db_id is copied verbatim and participates in the frozen layout.
#[test]
fn superblock_db_id_round_trips_verbatim() {
    let mut superblock = sample_superblock(19, 0);
    superblock.db_id = *b"verbatim-db-id!!";
    let decoded = Superblock::decode(&encode_superblock(&superblock)).unwrap();
    assert_eq!(decoded.db_id, superblock.db_id);
}

/// FORMAT § WAL sidecar: declared-giant lengths cannot drive allocation or panic the envelope reader.
#[test]
fn wal_declared_giant_length_is_bounded() {
    let mut bytes = vec![0xa5_u8; 1024 * 1024];
    bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    let checksum = crc32c(&bytes[8..]);
    bytes[4..8].copy_from_slice(&checksum.to_le_bytes());
    let directory = tempdir().unwrap();
    let path = directory.path().join("giant.devondb-wal");
    fs::write(&path, &bytes).unwrap();
    assert!(replay(&path).unwrap().is_empty());
    if let Ok(reader) = WalReader::open(&path) {
        assert!(reader.count() == 0);
    }
}

/// FORMAT § Node group pages: fuzzed directories return errors or decode; never panic.
#[test]
fn fuzzed_node_group_directories_never_panic_fixed_seed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("node-fuzz-source.devondb");
    let types = vec![LogicalType::Int64, LogicalType::String];
    let source = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = node_group(3).write(&source).unwrap();
    let original = source.read_page(directory_page).unwrap();
    drop(source);

    let mut state = PROPTEST_SEED;
    for case in 0..256_u32 {
        let mut page = original.clone();
        for _ in 0..(case % 5) + 1 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let offset = (state as usize) % PAGE_BYTES;
            page[offset] = (state >> 33) as u8;
        }
        let fuzz_dir = tempdir().unwrap();
        let fuzz_path = fuzz_dir.path().join("node-fuzz.devondb");
        let pager = Pager::create(&fuzz_path, PAGE_SIZE, DB_ID).unwrap();
        let target = pager.allocate_page().unwrap();
        pager.write_page(target, &page).unwrap();
        let mut superblock = pager.superblock();
        superblock.checkpoint_lsn = 1;
        superblock.feature_flags |= ZONE_MAPS_FLAG;
        pager.commit_superblock(superblock).unwrap();
        let _ = NodeGroup::read(&pager, target, &types);
    }
}

/// FORMAT § Rel table adjacency: fuzzed CSR directories return errors or decode; never panic.
#[test]
fn fuzzed_csr_directories_never_panic_fixed_seed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("csr-fuzz-source.devondb");
    let types = vec![LogicalType::String, LogicalType::Int64];
    let mut group = CsrGroup::new(3, types.clone()).unwrap();
    group
        .push_edge(0, 2, vec![Value::String("a".to_owned()), Value::Int64(1)])
        .unwrap();
    group
        .push_edge(2, 1, vec![Value::Null, Value::Null])
        .unwrap();
    let source = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = group.write(&source).unwrap();
    let original = source.read_page(directory_page).unwrap();
    drop(source);

    let mut state = PROPTEST_SEED ^ 0x4353_525f_4655_5a5a;
    for case in 0..192_u32 {
        let mut page = original.clone();
        for _ in 0..(case % 4) + 1 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let offset = (state as usize) % PAGE_BYTES;
            page[offset] = (state >> 33) as u8;
        }
        let fuzz_dir = tempdir().unwrap();
        let fuzz_path = fuzz_dir.path().join("csr-fuzz.devondb");
        let pager = Pager::create(&fuzz_path, PAGE_SIZE, DB_ID).unwrap();
        let target = pager.allocate_page().unwrap();
        pager.write_page(target, &page).unwrap();
        let _ = CsrGroup::read(&pager, target, &types);
    }
}

/// FREE_PAGES § Ledger pages: fuzzed extensions and ledgers through public readers never panic.
#[test]
fn fuzzed_free_pages_public_paths_never_panic_fixed_seed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("free-fuzz.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = catalog_with_table();
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    drop(pager);

    let mut state = PROPTEST_SEED ^ 0x4652_4545_5f50_4147;
    for case in 0..160_u32 {
        let scratch = tempdir().unwrap();
        let scratch_path = scratch.path().join("case.devondb");
        fs::copy(&path, &scratch_path).unwrap();
        for _ in 0..(case % 4) + 1 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let page_id = (state >> 20) % 8;
            let offset = ((state >> 8) as usize) % PAGE_BYTES;
            if page_id < 2 && offset >= SUPERBLOCK_HEADER_LEN {
                continue;
            }
            flip_byte(&scratch_path, page_id, offset, 0x5a);
        }
        if let Ok(reopened) = Pager::open(&scratch_path) {
            let _ = reopened.free_pages_ledger_pages();
            let _ = reopened.allocate_page();
        }
    }
}
