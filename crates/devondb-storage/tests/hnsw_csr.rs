use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use crc32c::crc32c;
use devondb_storage::budget::MemoryBudget;
use devondb_storage::csr_group::CsrGroup;
use devondb_storage::hnsw::csr::{CsrSlotReadOptions, CsrSlotReader, VerifiedCsrGroups};
use devondb_storage::pager::Pager;
use devondb_types::{DevonError, DevonResult};
use tempfile::{TempDir, tempdir};

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"hnsw-csr-test!!!";
const DIRECTORY_HEADER_LEN: u64 = 16;
const DIRECTORY_ENTRY_LEN: u64 = 16;

#[derive(Debug, Clone, Copy)]
struct Entry {
    first_page: u64,
    byte_len: u32,
}

#[test]
fn slot_reads_match_full_reader_across_multi_page_runs() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("multi-page.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let group = uniform_group(1_400, 3, 1_400);
    let directory_page = group.write(&pager).unwrap();
    let full = CsrGroup::read(&pager, directory_page, &[]).unwrap();
    let reader = CsrSlotReader::open(&pager, directory_page).unwrap();
    let budget = MemoryBudget::unlimited();
    let mut verified = VerifiedCsrGroups::new(&budget, 1).unwrap();
    let options = options(10_000, 20_000, 3);

    assert!(reader.edge_count() * 8 > PAGE_SIZE as usize);
    assert!((reader.row_count() + 1) * 4 > PAGE_SIZE as usize);
    for slot in 0..reader.row_count() {
        let expected: Vec<_> = full
            .edge_range(slot)
            .unwrap()
            .map(|edge| full.neighbor(edge).unwrap())
            .collect();
        let actual = reader.read_slot(slot, options, &mut verified).unwrap();
        assert_eq!(actual.neighbors(), expected);
    }
    assert_eq!(verified.len(), 1);
}

#[test]
fn poisoned_unvisited_group_is_local_until_first_access() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("locality.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let first_page = valid_group().write(&pager).unwrap();
    let poisoned_page = valid_group().write(&pager).unwrap();
    drop(pager);
    flip_payload_byte(&path, poisoned_page, 1, 0);

    let pager = Pager::open(&path).unwrap();
    let first = CsrSlotReader::open(&pager, first_page).unwrap();
    let poisoned = CsrSlotReader::open(&pager, poisoned_page).unwrap();
    let budget = MemoryBudget::unlimited();
    let mut verified = VerifiedCsrGroups::new(&budget, 2).unwrap();

    assert_eq!(
        first
            .read_slot(0, options(0, 4, 2), &mut verified)
            .unwrap()
            .neighbors(),
        &[1, 2]
    );
    assert_eq!(verified.len(), 1);
    assert_corrupt(poisoned.read_slot(0, options(0, 4, 2), &mut verified));
    assert_eq!(verified.len(), 1);
}

#[test]
fn verified_group_repeat_reads_skip_full_payload_validation() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("verified-repeat.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = valid_group().write(&pager).unwrap();
    let reader = CsrSlotReader::open(&pager, directory_page).unwrap();
    let budget = MemoryBudget::unlimited();
    let mut verified = VerifiedCsrGroups::new(&budget, 1).unwrap();

    let first = reader
        .read_slot(0, options(0, 4, 2), &mut verified)
        .unwrap();
    assert_eq!(first.neighbors(), &[1, 2]);
    drop(first);
    flip_payload_byte(&path, directory_page, 1, 3 * size_of::<u64>());

    let repeated = reader
        .read_slot(0, options(0, 4, 2), &mut verified)
        .unwrap();
    assert_eq!(repeated.neighbors(), &[1, 2]);
    assert_eq!(verified.len(), 1);
}

#[test]
fn offsets_must_start_at_zero() {
    let fixture = corruption_fixture("offset-start");
    replace_u32(&fixture.1, fixture.2, 0, 0, 1);
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 2));
}

#[test]
fn offset_endpoints_must_be_monotonic() {
    let fixture = corruption_fixture("offset-order");
    replace_u32(&fixture.1, fixture.2, 0, 1, 3);
    replace_u32(&fixture.1, fixture.2, 0, 2, 2);
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 3));
}

#[test]
fn final_offset_must_equal_edge_count() {
    let fixture = corruption_fixture("offset-final");
    replace_u32(&fixture.1, fixture.2, 0, 4, 4);
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 2));
}

#[test]
fn neighbors_must_be_below_covered_rows() {
    let fixture = corruption_fixture("neighbor-bound");
    replace_u64(&fixture.1, fixture.2, 1, 0, 4);
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 2));
}

#[test]
fn duplicate_neighbors_are_corruption() {
    let fixture = corruption_fixture("neighbor-duplicate");
    replace_u64(&fixture.1, fixture.2, 1, 1, 1);
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 2));
}

#[test]
fn self_neighbors_are_corruption() {
    let fixture = corruption_fixture("neighbor-self");
    replace_u64(&fixture.1, fixture.2, 1, 0, 0);
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 2));
}

#[test]
fn layer_degree_cap_is_enforced() {
    let fixture = corruption_fixture("degree-cap");
    assert_first_slot_corrupt(&fixture.1, fixture.2, options(0, 4, 1));
}

#[test]
fn layer_eligibility_is_enforced() {
    let fixture = corruption_fixture("eligibility");
    let pager = Pager::open(&fixture.1).unwrap();
    let reader = CsrSlotReader::open(&pager, fixture.2).unwrap();
    let budget = MemoryBudget::unlimited();
    let mut verified = VerifiedCsrGroups::new(&budget, 1).unwrap();
    let result =
        reader.read_slot_with_eligibility(0, options(0, 4, 2), &mut verified, |neighbor| {
            Ok(neighbor != 1)
        });
    assert_corrupt(result);
}

#[test]
fn retained_hop_memory_is_one_bounded_adjacency_not_the_neighbor_run() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("bounded-hop.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let group = uniform_group(256, 16, 256);
    let directory_page = group.write(&pager).unwrap();
    let reader = CsrSlotReader::open(&pager, directory_page).unwrap();
    let neighbor_run_bytes = reader.edge_count() * 8;
    let budget = MemoryBudget::new(512);
    let mut verified = VerifiedCsrGroups::new(&budget, 1).unwrap();

    let adjacency = reader
        .read_slot(117, options(10_000, 11_000, 16), &mut verified)
        .unwrap();
    assert_eq!(adjacency.len(), 16);
    assert!(adjacency.allocated_bytes() <= 16 * 8);
    assert!(adjacency.allocated_bytes() + (PAGE_SIZE as usize) < neighbor_run_bytes);
    assert!(budget.charged() <= budget.limit());
    assert_eq!(
        budget.charged(),
        verified.charged_bytes() + adjacency.charged_bytes()
    );
}

fn valid_group() -> CsrGroup {
    let mut group = CsrGroup::new(4, Vec::new()).unwrap();
    for neighbor in [1, 2] {
        group.push_edge(0, neighbor, Vec::new()).unwrap();
    }
    group.push_edge(1, 0, Vec::new()).unwrap();
    group.push_edge(2, 3, Vec::new()).unwrap();
    group.push_edge(3, 0, Vec::new()).unwrap();
    group
}

fn uniform_group(row_count: usize, degree: usize, neighbor_modulus: usize) -> CsrGroup {
    let mut group = CsrGroup::new(row_count, Vec::new()).unwrap();
    for slot in 0..row_count {
        for distance in 1..=degree {
            let neighbor = ((slot + distance) % neighbor_modulus) as u64;
            group.push_edge(slot, neighbor, Vec::new()).unwrap();
        }
    }
    group
}

fn options(group_start: u64, covered_rows: u64, degree_cap: usize) -> CsrSlotReadOptions {
    CsrSlotReadOptions {
        group_start,
        covered_rows,
        degree_cap,
    }
}

fn corruption_fixture(name: &str) -> (TempDir, PathBuf, u64) {
    let directory = tempdir().unwrap();
    let path = directory.path().join(format!("{name}.devondb"));
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    let directory_page = valid_group().write(&pager).unwrap();
    drop(pager);
    (directory, path, directory_page)
}

fn assert_first_slot_corrupt(path: &Path, directory_page: u64, options: CsrSlotReadOptions) {
    let pager = Pager::open(path).unwrap();
    let reader = CsrSlotReader::open(&pager, directory_page).unwrap();
    let budget = MemoryBudget::unlimited();
    let mut verified = VerifiedCsrGroups::new(&budget, 1).unwrap();
    assert_corrupt(reader.read_slot(0, options, &mut verified));
}

fn assert_corrupt<T>(result: DevonResult<T>) {
    assert!(matches!(result, Err(DevonError::Corrupt { .. })));
}

fn replace_u32(path: &Path, directory_page: u64, entry_index: usize, index: usize, value: u32) {
    replace_payload_bytes(
        path,
        directory_page,
        entry_index,
        index * size_of::<u32>(),
        &value.to_le_bytes(),
        true,
    );
}

fn replace_u64(path: &Path, directory_page: u64, entry_index: usize, index: usize, value: u64) {
    replace_payload_bytes(
        path,
        directory_page,
        entry_index,
        index * size_of::<u64>(),
        &value.to_le_bytes(),
        true,
    );
}

fn flip_payload_byte(path: &Path, directory_page: u64, entry_index: usize, byte_index: usize) {
    let entry = read_entry(path, directory_page, entry_index);
    let offset = entry.first_page * u64::from(PAGE_SIZE) + byte_index as u64;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}

fn replace_payload_bytes(
    path: &Path,
    directory_page: u64,
    entry_index: usize,
    byte_index: usize,
    bytes: &[u8],
    refresh_crc: bool,
) {
    let entry = read_entry(path, directory_page, entry_index);
    let offset = entry.first_page * u64::from(PAGE_SIZE) + byte_index as u64;
    let mut file = OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    drop(file);
    if refresh_crc {
        refresh_checksum(path, directory_page, entry_index);
    }
}

fn read_entry(path: &Path, directory_page: u64, index: usize) -> Entry {
    let offset = directory_page * u64::from(PAGE_SIZE)
        + DIRECTORY_HEADER_LEN
        + index as u64 * DIRECTORY_ENTRY_LEN;
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut bytes = [0_u8; 16];
    file.read_exact(&mut bytes).unwrap();
    Entry {
        first_page: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        byte_len: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
    }
}

fn refresh_checksum(path: &Path, directory_page: u64, entry_index: usize) {
    let entry = read_entry(path, directory_page, entry_index);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.seek(SeekFrom::Start(entry.first_page * u64::from(PAGE_SIZE)))
        .unwrap();
    let mut payload = vec![0_u8; entry.byte_len as usize];
    file.read_exact(&mut payload).unwrap();
    let checksum_offset = directory_page * u64::from(PAGE_SIZE)
        + DIRECTORY_HEADER_LEN
        + entry_index as u64 * DIRECTORY_ENTRY_LEN
        + 12;
    file.seek(SeekFrom::Start(checksum_offset)).unwrap();
    file.write_all(&crc32c(&payload).to_le_bytes()).unwrap();
    file.sync_all().unwrap();
}
