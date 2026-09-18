//! Physical storage accounting and stable text output.
//!
//! # Stable text format
//!
//! The first three lines are `storage stats`, `kind: …`, and `file bytes:
//! …`. Main files then print page size; the fixed page rows `total`, `live`,
//! `free`, `metadata`, `unaccounted`; ledger state; node tables; relationship
//! tables; WAL bytes; and feature bits. Tables sort by bytewise display name
//! and feature bits sort by bit number. DEVONPACK prints physical and logical
//! sizes, then says `not applicable` for page categories, tables, WAL, and
//! embedded feature bits. This is a text contract; there is no JSON mode.

use std::fs;
use std::path::{Path, PathBuf};

use devondb_storage::catalog::{Catalog, RelStorage, TableStorage};
use devondb_storage::csr_group::{CsrGroup, csr_page_inventory};
use devondb_storage::node_group::{NodeGroup, group_page_inventory};
use devondb_storage::pager::Pager;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema, RelTableSchema};
use devondb_types::value::Value;
use tempfile::TempDir;

#[path = "../src/stats.rs"]
mod stats;

use stats::{FreeLedgerStatus, StatsError, StorageKind, collect, write_text};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"storage-stats-db";

fn column(name: &str, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty: LogicalType::Int64,
        primary_key,
    }
}

fn node_schema(name: &str) -> NodeTableSchema {
    NodeTableSchema::new(
        name.to_owned(),
        vec![column("id", true), column("score", false)],
    )
    .unwrap()
}

fn rel_schema() -> RelTableSchema {
    RelTableSchema::new(
        "Knows".to_owned(),
        "Person".to_owned(),
        "Person".to_owned(),
        vec![column("since", false)],
    )
    .unwrap()
}

fn node_group(pager: &Pager, rows: &[(i64, i64)]) -> u64 {
    let mut group = NodeGroup::new(vec![LogicalType::Int64, LogicalType::Int64]).unwrap();
    for (id, score) in rows {
        group
            .push_row(vec![Value::Int64(*id), Value::Int64(*score)])
            .unwrap();
    }
    group.write_forcing_encodings(pager, &[]).unwrap()
}

fn relationship_groups(pager: &Pager, row_count: usize, edges: &[(usize, u64, i64)]) -> (u64, u64) {
    let mut forward = CsrGroup::new(row_count, vec![LogicalType::Int64]).unwrap();
    let mut backward = CsrGroup::new(row_count, vec![LogicalType::Int64]).unwrap();
    for (from, to, since) in edges {
        forward
            .push_edge(*from, *to, vec![Value::Int64(*since)])
            .unwrap();
        backward
            .push_edge(*to as usize, *from as u64, vec![Value::Int64(*since)])
            .unwrap();
    }
    (
        forward.write(pager).unwrap(),
        backward.write(pager).unwrap(),
    )
}

struct Fixture {
    _directory: TempDir,
    path: PathBuf,
    person_root: u64,
    company_root: u64,
    rel_roots: (u64, u64),
}

fn build_fixture() -> Fixture {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("accounting.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let person = node_schema("Person");
    let company = node_schema("Acme");
    let knows = rel_schema();
    let mut catalog = Catalog::default();
    // Deliberately not alphabetical: output ordering must not depend on DDL.
    catalog.add_node_table(person.clone()).unwrap();
    catalog.add_node_table(company.clone()).unwrap();
    catalog.add_rel_table(knows.clone()).unwrap();

    let old_person = node_group(&pager, &[(1, 10), (2, 20), (3, 30)]);
    let company_root = node_group(&pager, &[(7, 70)]);
    let old_rel = relationship_groups(&pager, 3, &[(0, 1, 2001), (2, 1, 2002)]);
    catalog
        .set_table_storage(
            person.name(),
            TableStorage {
                groups: vec![old_person],
            },
        )
        .unwrap();
    catalog
        .set_table_storage(
            company.name(),
            TableStorage {
                groups: vec![company_root],
            },
        )
        .unwrap();
    catalog
        .set_rel_storage(
            knows.name(),
            RelStorage {
                fwd: vec![old_rel.0],
                bwd: vec![old_rel.1],
            },
        )
        .unwrap();
    catalog.save(&pager, 1).unwrap();

    // Materialize a detach of Person(3): its node row and incident edge
    // disappear together, and the checkpoint retires every superseded run.
    let person_root = node_group(&pager, &[(1, 10), (2, 20)]);
    let rel_roots = relationship_groups(&pager, 2, &[(0, 1, 2001)]);
    catalog
        .set_table_storage(
            person.name(),
            TableStorage {
                groups: vec![person_root],
            },
        )
        .unwrap();
    catalog
        .set_rel_storage(
            knows.name(),
            RelStorage {
                fwd: vec![rel_roots.0],
                bwd: vec![rel_roots.1],
            },
        )
        .unwrap();
    catalog.save(&pager, 2).unwrap();

    // Failed prospective writes are intentionally not catalog-reachable and
    // not ledger-retired: these are the honest residual the incident needed.
    for marker in [0x51_u8, 0x52] {
        let page_id = pager.allocate_page().unwrap();
        let mut page = vec![0_u8; PAGE_SIZE as usize];
        page[0] = marker;
        pager.write_page(page_id, &page).unwrap();
    }
    pager.sync().unwrap();
    drop(pager);
    fs::write(wal_path(&path), b"pending-wal-bytes").unwrap();

    Fixture {
        _directory: directory,
        path,
        person_root,
        company_root,
        rel_roots,
    }
}

fn wal_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push("-wal");
    value.into()
}

#[test]
fn every_page_category_is_nonzero_and_sums_exactly_to_file_bytes() {
    let fixture = build_fixture();
    let before_main = fs::read(&fixture.path).unwrap();
    let before_wal = fs::read(wal_path(&fixture.path)).unwrap();

    let observed = collect(&fixture.path, 1024 * 1024).unwrap();

    assert_eq!(observed.kind, StorageKind::Database);
    assert_eq!(observed.file_bytes, before_main.len() as u64);
    assert_eq!(observed.wal_bytes, Some(before_wal.len() as u64));
    assert_eq!(observed.free_ledger, FreeLedgerStatus::Active);
    let pages = observed.pages.unwrap();
    assert!(pages.live > 0);
    assert!(pages.free > 0);
    assert!(pages.metadata > 0);
    assert!(pages.unaccounted >= 2);
    assert_eq!(
        pages.live + pages.free + pages.metadata + pages.unaccounted,
        pages.total
    );
    assert_eq!(
        pages.total * u64::from(PAGE_SIZE),
        observed.file_bytes,
        "the four physical categories are the entire file"
    );
    assert_eq!(fs::read(&fixture.path).unwrap(), before_main);
    assert_eq!(fs::read(wal_path(&fixture.path)).unwrap(), before_wal);
}

#[test]
fn per_table_bytes_are_exact_and_text_order_is_stable() {
    let fixture = build_fixture();
    let pager = Pager::open(&fixture.path).unwrap();
    let node_types = [LogicalType::Int64, LogicalType::Int64];
    let rel_types = [LogicalType::Int64];
    let person_pages = group_page_inventory(&pager, fixture.person_root, &node_types)
        .unwrap()
        .len() as u64;
    let company_pages = group_page_inventory(&pager, fixture.company_root, &node_types)
        .unwrap()
        .len() as u64;
    let rel_pages = csr_page_inventory(&pager, fixture.rel_roots.0, &rel_types)
        .unwrap()
        .len() as u64
        + csr_page_inventory(&pager, fixture.rel_roots.1, &rel_types)
            .unwrap()
            .len() as u64;
    drop(pager);

    let observed = collect(&fixture.path, 1024 * 1024).unwrap();
    let nodes = observed.node_tables.as_ref().unwrap();
    let relationships = observed.rel_tables.as_ref().unwrap();
    assert_eq!(
        nodes
            .iter()
            .map(|table| table.name.as_str())
            .collect::<Vec<_>>(),
        ["Acme", "Person"]
    );
    assert_eq!(nodes[0].pages, company_pages);
    assert_eq!(nodes[1].pages, person_pages);
    assert_eq!(relationships[0].name, "Knows");
    assert_eq!(relationships[0].pages, rel_pages);
    assert!(
        observed
            .feature_bits
            .as_ref()
            .unwrap()
            .iter()
            .any(|name| name == "FREE_PAGES (bit 5)")
    );

    let mut text = Vec::new();
    write_text(&observed, &mut text).unwrap();
    let text = String::from_utf8(text).unwrap();
    let expected_prefix = format!(
        "storage stats\nkind: database\nfile bytes: {}\npage size: 4096\npages:\n",
        observed.file_bytes
    );
    assert!(text.starts_with(&expected_prefix));
    assert!(text.find("  Acme:").unwrap() < text.find("  Person:").unwrap());
    assert!(text.contains("  unaccounted:"));
    assert!(text.contains("wal bytes: 17\n"));
    assert!(text.ends_with("  FREE_PAGES (bit 5)\n"));
}

#[test]
fn empty_database_has_only_two_metadata_pages() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("empty.devondb");
    drop(Pager::create(&path, PAGE_SIZE, DB_ID).unwrap());

    let observed = collect(&path, 1024 * 1024).unwrap();
    let pages = observed.pages.unwrap();
    assert_eq!(pages.total, 2);
    assert_eq!(pages.live, 0);
    assert_eq!(pages.free, 0);
    assert_eq!(pages.metadata, 2);
    assert_eq!(pages.unaccounted, 0);
    assert_eq!(observed.free_ledger, FreeLedgerStatus::NotPresent);
}

#[test]
fn inspection_working_set_is_budgeted() {
    let fixture = build_fixture();
    let error = collect(&fixture.path, PAGE_SIZE as usize).unwrap_err();
    assert!(matches!(error, StatsError::BudgetExceeded { .. }));
}

#[test]
fn pack_reports_container_fields_and_explicitly_marks_nonapplicable_fields() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("fixture.pack");
    write_pack_fixture(&path);

    let observed = collect(&path, 1024 * 1024).unwrap();
    assert_eq!(observed.kind, StorageKind::Pack);
    assert_eq!(observed.file_bytes, 63);
    assert_eq!(observed.pack_logical_pages, Some(2));
    assert_eq!(observed.pack_logical_bytes, Some(8192));
    assert_eq!(observed.pages, None);
    assert_eq!(observed.wal_bytes, None);

    let mut text = Vec::new();
    write_text(&observed, &mut text).unwrap();
    let text = String::from_utf8(text).unwrap();
    assert_eq!(
        text,
        "storage stats\n\
kind: pack\n\
file bytes: 63\n\
logical main bytes: 8192\n\
logical main pages: 2\n\
page size: not applicable (DEVONPACK container)\n\
pages: not applicable (compressed container)\n\
free ledger: not applicable (compressed container)\n\
node tables: not applicable (compressed container)\n\
relationship tables: not applicable (compressed container)\n\
wal bytes: not applicable (checkpoint image)\n\
feature bits: not applicable (stored inside compressed pages)\n"
    );
}

fn write_pack_fixture(path: &Path) {
    let mut header = [0_u8; 40];
    header[..9].copy_from_slice(b"DEVONPACK");
    header[9] = 1;
    header[10..12].copy_from_slice(&1_u16.to_le_bytes());
    header[12..16].copy_from_slice(&PAGE_SIZE.to_le_bytes());
    header[16..24].copy_from_slice(&2_u64.to_le_bytes());
    header[24..28].copy_from_slice(&2_u32.to_le_bytes());
    header[28..36].copy_from_slice(&1_u64.to_le_bytes());
    let header_crc = crc32c(&header[..36]);
    header[36..40].copy_from_slice(&header_crc.to_le_bytes());

    let mut directory = [0_u8; 16];
    directory[..8].copy_from_slice(&60_u64.to_le_bytes());
    directory[8..12].copy_from_slice(&3_u32.to_le_bytes());
    directory[12..16].copy_from_slice(&0x1234_u32.to_le_bytes());
    let directory_crc = crc32c(&directory);
    let mut bytes = Vec::from(header);
    bytes.extend_from_slice(&directory);
    bytes.extend_from_slice(&directory_crc.to_le_bytes());
    bytes.extend_from_slice(&[1, 2, 3]);
    fs::write(path, bytes).unwrap();
}

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0x82f6_3b78 & mask);
        }
    }
    !crc
}
