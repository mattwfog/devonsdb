//! `FREE_PAGES` storage-layer tests (`docs/FREE_PAGES.md`): retirement at
//! catalog publication, pin-gated reuse-first allocation, degraded mode on
//! a torn extension, the consumption durability kill-pair, and tail recycling.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use devondb_storage::catalog::{Catalog, TableStorage};
use devondb_storage::free_pages::{EXTENSION_OFFSET, LedgerEntry, LedgerPage, SuperblockExtension};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::FREE_PAGES_FLAG;
use devondb_types::logical_type::LogicalType;
use devondb_types::schema::{Column, NodeTableSchema};
use devondb_types::value::Value;
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"free-pages-core!";

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

fn group_with_rows(first_id: i64, rows: usize) -> NodeGroup {
    let mut group = NodeGroup::new(vec![LogicalType::Int64, LogicalType::String]).unwrap();
    for offset in 0..rows {
        group
            .push_row(vec![
                Value::Int64(first_id + offset as i64),
                Value::String(format!("body-{first_id}-{offset}-{}", "x".repeat(64))),
            ])
            .unwrap();
    }
    group
}

/// One churn publication: write a fresh group, point the catalog at it
/// alone, save at `publish_lsn`. The previous generation's group and
/// catalog pages all retire.
fn publish_generation(pager: &Pager, catalog: &mut Catalog, publish_lsn: u64, first_id: i64) {
    let group_id = group_with_rows(first_id, 8).write(pager).unwrap();
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

fn create_with_table(path: &Path) -> (Pager, Catalog) {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema()).unwrap();
    (pager, catalog)
}

/// Overwrites one raw page in the database file, preserving its length.
/// This is the damage-crafting primitive for CRC-valid ledger fixtures.
fn write_raw_page(path: &Path, page_id: u64, page: &[u8]) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let offset = page_id * u64::from(PAGE_SIZE);
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(page).unwrap();
    file.sync_all().unwrap();
}

/// Builds a CRC-valid ledger page with one entry.
fn ledger_page_with_entry(entry: LedgerEntry, next_page: u64) -> Vec<u8> {
    LedgerPage {
        next_page,
        consumed_count: 0,
        entries: vec![entry],
    }
    .encode(PAGE_SIZE as usize)
    .unwrap()
}

/// Rebuilds a ledger page after changing one entry's page id, keeping the
/// CRC valid — the damage class the allocator must degrade on, not fail.
fn rewrite_entry_page_id(path: &Path, ledger_page_id: u64, entry_index: usize, new_page_id: u64) {
    let pager = Pager::open(path).unwrap();
    let mut page = pager
        .free_pages_ledger_pages()
        .unwrap()
        .into_iter()
        .find_map(|(page_id, page)| (page_id == ledger_page_id).then_some(page))
        .unwrap();
    drop(pager);
    page.entries[entry_index].page_id = new_page_id;
    write_raw_page(
        path,
        ledger_page_id,
        &page.encode(PAGE_SIZE as usize).unwrap(),
    );
}

#[test]
fn publication_retires_the_superseded_generation_and_claims_bit_5() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("retire.devondb");
    let (pager, mut catalog) = create_with_table(&path);

    publish_generation(&pager, &mut catalog, 1, 0);
    assert_eq!(
        pager.superblock().feature_flags & FREE_PAGES_FLAG,
        0,
        "the first publication supersedes nothing and must not claim the bit"
    );

    let first_groups = catalog.table_storage("Doc").unwrap().groups.clone();
    publish_generation(&pager, &mut catalog, 2, 100);

    assert_ne!(
        pager.superblock().feature_flags & FREE_PAGES_FLAG,
        0,
        "the first ledger-writing publication claims bit 5"
    );
    let ledger = pager.free_pages_ledger_pages().unwrap();
    let retired: Vec<u64> = ledger
        .iter()
        .flat_map(|(_, page)| page.entries.iter().map(|entry| entry.page_id))
        .collect();
    for group in &first_groups {
        assert!(
            retired.contains(group),
            "superseded group directory {group} must be in the ledger"
        );
    }
    assert!(
        ledger
            .iter()
            .flat_map(|(_, page)| &page.entries)
            .all(|entry| entry.retired_lsn == 2),
        "every entry carries the retiring publication's LSN"
    );
    let extension = pager.free_pages_extension().unwrap();
    assert_eq!(extension.retired_total, retired.len() as u64);
}

#[test]
fn reuse_is_pin_gated_zeroing_and_accounted() {
    // No reuse occurs at or above the pin horizon,
    // reused pages come back zeroed, and the accounting identity holds.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("reuse.devondb");
    let (pager, mut catalog) = create_with_table(&path);

    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let retired_at_two: Vec<u64> = pager
        .free_pages_ledger_pages()
        .unwrap()
        .iter()
        .flat_map(|(_, page)| page.entries.iter().map(|entry| entry.page_id))
        .collect();
    assert!(!retired_at_two.is_empty());

    // min_pin = 2 (the current generation): entries retired AT 2 are not
    // eligible — the strict comparison IS the one-generation delay.
    pager.raise_min_pin(2);
    let appended = pager.allocate_page().unwrap();
    assert!(
        !retired_at_two.contains(&appended),
        "an entry retired at the horizon must not be reused"
    );

    // A later durable publication raises the horizon past 2: now eligible.
    pager.raise_min_pin(3);
    let reused = pager.allocate_page().unwrap();
    assert!(
        retired_at_two.contains(&reused),
        "page {reused} should come from the retired set {retired_at_two:?}"
    );
    assert_eq!(
        pager.read_page(reused).unwrap(),
        vec![0_u8; PAGE_SIZE as usize],
        "reused pages are handed out zeroed"
    );
    assert_eq!(pager.free_pages_session_reused(), 1);

    // `retired_total − (durably live − in-session pops)` equals cumulative
    // reuse. Nothing was flushed yet, so the durable walk
    // still shows every entry live and the whole difference is this
    // session's counter.
    let extension = pager.free_pages_extension().unwrap();
    let durable_live: u64 = pager
        .free_pages_ledger_pages()
        .unwrap()
        .iter()
        .map(|(_, page)| page.entries.len() as u64 - u64::from(page.consumed_count))
        .sum();
    assert_eq!(durable_live, extension.retired_total);
    assert_eq!(
        extension.retired_total - (durable_live - pager.free_pages_session_reused()),
        pager.free_pages_session_reused(),
        "cumulative-retired minus live equals cumulative reuse"
    );

    // The pop becomes durable at the next publication: the flushed walk
    // then shows exactly one consumed entry.
    publish_generation(&pager, &mut catalog, 3, 200);
    let flushed_live: u64 = pager
        .free_pages_ledger_pages()
        .unwrap()
        .iter()
        .map(|(_, page)| page.entries.len() as u64 - u64::from(page.consumed_count))
        .sum();
    let extension = pager.free_pages_extension().unwrap();
    assert_eq!(
        extension.retired_total - flushed_live,
        pager.free_pages_session_reused(),
        "after the flip the durable ledger carries the consumption"
    );
}

#[test]
fn fragmented_prefix_preserves_skips_and_reuses_a_later_run() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("fragmented-prefix.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();
    for _ in 2..=14 {
        let page_id = pager.allocate_page().unwrap();
        pager
            .write_page(page_id, &vec![0x5a; PAGE_SIZE as usize])
            .unwrap();
    }
    let retired = vec![2, 3, 4, 5, 6, 8, 10, 11, 12, 13, 14];
    pager.retire_pages(retired, 1).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 1;
    superblock.feature_flags |= FREE_PAGES_FLAG;
    pager.commit_superblock(superblock).unwrap();
    pager.raise_min_pin(2);

    assert_eq!(pager.allocate_page().unwrap(), 2);
    assert_eq!(
        pager.allocate_run(5).unwrap(),
        10,
        "the residual 3..=6 prefix must not hide the later 10..=14 run"
    );
    assert_eq!(
        pager.allocate_run(4).unwrap(),
        3,
        "skipped eligible pages remain reusable in the same publication"
    );
    assert_eq!(pager.free_pages_session_reused(), 10);
    for page_id in [2, 3, 4, 5, 6, 10, 11, 12, 13, 14] {
        assert_eq!(
            pager.read_page(page_id).unwrap(),
            vec![0; PAGE_SIZE as usize]
        );
    }
    assert_eq!(
        pager.read_page(8).unwrap(),
        vec![0x5a; PAGE_SIZE as usize],
        "a skipped page is not handed out or overwritten"
    );

    pager.retire_pages(Vec::new(), 2).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = 2;
    pager.commit_superblock(superblock).unwrap();
    drop(pager);

    // The flip republishes the unused skip without growing the
    // file — the skipped page 8 becomes the publication's own ledger page
    // (the head had no eligible entry left to pop) and the fully consumed
    // first ledger page, 15, is the entry it carries.
    let reopened = Pager::open(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        16 * u64::from(PAGE_SIZE),
        "republishing a skip never appends"
    );
    let ledger = reopened.free_pages_ledger_pages().unwrap();
    assert_eq!(
        ledger
            .iter()
            .map(|(page_id, _)| *page_id)
            .collect::<Vec<_>>(),
        vec![8],
        "the skipped page is the live ledger page"
    );
    assert_eq!(
        ledger[0]
            .1
            .entries
            .iter()
            .map(|entry| entry.page_id)
            .collect::<Vec<_>>(),
        vec![15],
        "the retired first ledger page is the republished entry"
    );
    reopened.raise_min_pin(u64::MAX);
    assert_eq!(
        reopened.allocate_page().unwrap(),
        15,
        "the republished entry is handed out after the flip"
    );
    assert_eq!(reopened.read_page(15).unwrap(), vec![0; PAGE_SIZE as usize]);
}

#[test]
fn consumption_kill_pair_reoffers_before_the_flip_never_after() {
    // Before the flip, reuse whose publication never completes is re-offered on
    // reopen — safe precisely because no durable catalog names the reused
    // page. After: a completed publication makes the consumption durable
    // and the entry is never re-offered.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("kill-pair.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);

    pager.raise_min_pin(3);
    let reused = pager.allocate_page().unwrap();
    assert_eq!(pager.free_pages_session_reused(), 1);
    // No retire_pages, no commit_superblock: the pop stays in-session
    // only. Dropping here IS the crash before the flip.
    drop(pager);

    let reopened = Pager::open(&path).unwrap();
    reopened.raise_min_pin(3);
    assert_eq!(
        reopened.allocate_page().unwrap(),
        reused,
        "the un-flipped consumption must be re-offered on reopen"
    );

    // Now complete a publication (flip): consumption becomes durable.
    let mut catalog = Catalog::load(&reopened).unwrap();
    publish_generation(&reopened, &mut catalog, 3, 200);
    drop(reopened);

    let survivor = Pager::open(&path).unwrap();
    survivor.raise_min_pin(u64::MAX);
    let mut handed_out = Vec::new();
    // Drain every eligible entry: the durably consumed page never reappears.
    loop {
        let extension = survivor.free_pages_extension().unwrap();
        let live: u64 = survivor
            .free_pages_ledger_pages()
            .unwrap()
            .iter()
            .map(|(_, page)| page.entries.len() as u64 - u64::from(page.consumed_count))
            .sum();
        if live == survivor.free_pages_session_reused() {
            break;
        }
        assert!(extension.has_ledger());
        handed_out.push(survivor.allocate_page().unwrap());
    }
    assert!(
        !handed_out.contains(&reused),
        "a durably consumed entry must never be re-offered"
    );
}

#[test]
fn torn_extension_is_degraded_mode_not_corruption() {
    // A torn extension leaves arbitration unchanged and reads untouched;
    // the ledger is treated as absent, and the next publication rewrites a valid
    // extension on a fresh chain.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("torn.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let authoritative_root = pager.superblock().catalog_root;
    let retired_before = pager.free_pages_extension().unwrap().retired_total;
    drop(pager);

    // Tear the extension of BOTH slots (bit flips past the header).
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    for slot in 0..2_u64 {
        let offset = slot * u64::from(PAGE_SIZE) + EXTENSION_OFFSET as u64 + 3;
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[byte[0] ^ 0xff]).unwrap();
    }
    file.sync_all().unwrap();
    drop(file);

    let reopened = Pager::open(&path).unwrap();
    assert!(reopened.free_pages_degraded(), "torn extension → degraded");
    assert!(reopened.free_pages_extension().is_none());
    assert_eq!(
        reopened.superblock().catalog_root,
        authoritative_root,
        "slot arbitration must not consult the extension"
    );
    let recovered = Catalog::load(&reopened).unwrap();
    assert_eq!(recovered.table_storage("Doc").unwrap().groups.len(), 1);

    // Degraded allocation appends (the pre-bit behavior).
    reopened.raise_min_pin(u64::MAX);
    let file_pages = fs::metadata(&path).unwrap().len() / u64::from(PAGE_SIZE);
    assert_eq!(reopened.allocate_page().unwrap(), file_pages);

    // The next publication starts a fresh, valid chain.
    let mut catalog = recovered;
    publish_generation(&reopened, &mut catalog, 3, 200);
    let extension = reopened.free_pages_extension().unwrap();
    assert!(
        extension.has_ledger(),
        "post-degrade publication rebuilds the chain"
    );
    assert!(
        extension.retired_total >= retired_before,
        "retired_total stays monotone across the degrade ({} >= {retired_before})",
        extension.retired_total
    );
    drop(reopened);
    let survivor = Pager::open(&path).unwrap();
    assert!(!survivor.free_pages_degraded());
    assert!(survivor.free_pages_extension().unwrap().has_ledger());
}

#[test]
fn fully_consumed_head_recycles_without_self_reference() {
    // A fully consumed head page is unlinked by the next
    // publication, its retirement entry lands in a DIFFERENT page, the
    // chain stays walkable, and the old page is later reused.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("tail-recycle.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);

    let first_chain: Vec<u64> = pager
        .free_pages_ledger_pages()
        .unwrap()
        .iter()
        .map(|(page_id, _)| *page_id)
        .collect();
    assert_eq!(first_chain.len(), 1, "one generation fits one ledger page");
    let old_head = first_chain[0];

    // Consume every entry, then publish again: the drained head recycles.
    pager.raise_min_pin(3);
    let entry_count = pager.free_pages_ledger_pages().unwrap()[0].1.entries.len();
    for _ in 0..entry_count {
        pager.allocate_page().unwrap();
    }
    publish_generation(&pager, &mut catalog, 3, 200);

    let chain = pager.free_pages_ledger_pages().unwrap();
    for (page_id, page) in &chain {
        assert_ne!(
            *page_id, old_head,
            "the drained head must be unlinked from the chain"
        );
        assert!(
            page.entries.iter().any(|entry| entry.page_id == old_head),
            "the drained head's retirement entry lands in a different page"
        );
        assert!(
            page.entries.iter().all(|entry| entry.page_id != *page_id),
            "a ledger page never carries its own retirement entry"
        );
    }

    // The recycled head is later reused like any retired page.
    pager.raise_min_pin(4);
    let mut reused = Vec::new();
    for _ in 0..chain
        .iter()
        .map(|(_, page)| page.entries.len())
        .sum::<usize>()
    {
        reused.push(pager.allocate_page().unwrap());
    }
    assert!(
        reused.contains(&old_head),
        "the recycled ledger page {old_head} is reused ({reused:?})"
    );
}

#[test]
fn extension_survives_reopen_via_either_slot() {
    // The extension rides the dual-slot protocol: alternate publications
    // land it in alternate slots and reopen reads the authoritative one.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("slots.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    for generation in 1..=4 {
        publish_generation(&pager, &mut catalog, generation, generation as i64 * 100);
    }
    let expected = pager.free_pages_extension().unwrap();
    assert!(expected.has_ledger());
    drop(pager);

    let reopened = Pager::open(&path).unwrap();
    assert_eq!(reopened.free_pages_extension(), Some(expected));
    assert!(!reopened.free_pages_degraded());
}

/// Regression pin for the honest-fixture rule: the extension decoder must
/// not accept a zeroed region (pre-bit pages), which is why open gates the
/// read on the feature bit instead of sniffing bytes.
#[test]
fn zeroed_extension_never_decodes() {
    let page = vec![0_u8; PAGE_SIZE as usize];
    assert_eq!(SuperblockExtension::decode_from(&page), None);
    let _ = PathBuf::new();
}

#[test]
fn empty_publication_syncs_dirty_consumption_before_the_flip() {
    // Reuse a page, publish with an empty superseded set, and crash between
    // the consumption pwrite and the
    // flip. Reopen must not hand the reused page out twice.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("empty-flip.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);

    pager.raise_min_pin(3);
    let reused = pager.allocate_page().unwrap();
    pager.retire_pages(Vec::new(), 3).unwrap();
    drop(pager);

    // The empty publication flushed and synced the consumption rewrite, so
    // the reused page is durably consumed: reopen must not hand it out again
    // and appends instead. This pins the post-flush semantics; distinguishing
    // pwrite from pwrite+sync requires a page-cache-dropping kill harness.
    let reopened = Pager::open(&path).unwrap();
    reopened.raise_min_pin(3);
    assert_ne!(
        reopened.allocate_page().unwrap(),
        reused,
        "a durably consumed page must never be handed out twice"
    );

    let mut catalog = Catalog::load(&reopened).unwrap();
    publish_generation(&reopened, &mut catalog, 3, 200);
    drop(reopened);
    let survivor = Pager::open(&path).unwrap();
    survivor.raise_min_pin(u64::MAX);
    let mut handed_out = Vec::new();
    loop {
        let live: u64 = survivor
            .free_pages_ledger_pages()
            .unwrap()
            .iter()
            .map(|(_, page)| page.entries.len() as u64 - u64::from(page.consumed_count))
            .sum();
        if live == survivor.free_pages_session_reused() {
            break;
        }
        handed_out.push(survivor.allocate_page().unwrap());
    }
    assert!(
        !handed_out.contains(&reused),
        "the empty publication's flip made consumption durable"
    );
}

#[test]
fn damaged_head_entry_degrades_to_append_allocation() {
    // A CRC-valid head entry naming a superblock page is damage, never a
    // permanent allocation failure; allocation falls through to append.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("head-page-zero.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let ledger_page = pager.free_pages_ledger_pages().unwrap()[0].0;
    drop(pager);

    rewrite_entry_page_id(&path, ledger_page, 0, 0);
    let length_before = fs::metadata(&path).unwrap().len();
    let reopened = Pager::open(&path).unwrap();
    reopened.raise_min_pin(u64::MAX);
    let file_pages = length_before / u64::from(PAGE_SIZE);
    assert_eq!(reopened.allocate_page().unwrap(), file_pages);
    assert!(reopened.free_pages_degraded());
    // Append allocation grows the file by exactly the one page handed out
    // — never by a pwrite at the bogus id.
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        length_before + u64::from(PAGE_SIZE)
    );
}

#[test]
fn past_eof_head_entry_degrades_without_extending_the_file() {
    // A bogus id must not pwrite past EOF and move the allocation cursor.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("head-past-eof.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    let ledger_page = pager.free_pages_ledger_pages().unwrap()[0].0;
    drop(pager);

    rewrite_entry_page_id(&path, ledger_page, 0, u64::MAX / u64::from(PAGE_SIZE));
    let length_before = fs::metadata(&path).unwrap().len();
    let reopened = Pager::open(&path).unwrap();
    reopened.raise_min_pin(u64::MAX);
    let file_pages = length_before / u64::from(PAGE_SIZE);
    assert_eq!(reopened.allocate_page().unwrap(), file_pages);
    assert!(reopened.free_pages_degraded());
    // Append allocation grows the file by exactly the one page handed out
    // — never by a pwrite at the bogus id.
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        length_before + u64::from(PAGE_SIZE)
    );
}

#[test]
fn ledger_cycle_degrades_allocation_and_errors_the_doctor_walk() {
    // A CRC-valid A↔B cycle must never hang a writer under the mutex:
    // allocation degrades and appends; the doctor surface names the cycle.
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("ledger-cycle.devondb");
    let (pager, mut catalog) = create_with_table(&path);
    publish_generation(&pager, &mut catalog, 1, 0);
    publish_generation(&pager, &mut catalog, 2, 100);
    // Add a second ledger page by retiring another generation, then point
    // the first page back at itself to create the cycle.
    publish_generation(&pager, &mut catalog, 3, 200);
    let chain: Vec<u64> = pager
        .free_pages_ledger_pages()
        .unwrap()
        .iter()
        .map(|(page_id, _)| *page_id)
        .collect();
    drop(pager);

    let entry = LedgerEntry {
        page_id: chain[0],
        retired_lsn: 1,
    };
    let cyclic_page = ledger_page_with_entry(entry, chain[0]);
    write_raw_page(&path, chain[0], &cyclic_page);

    let reopened = Pager::open(&path).unwrap();
    // The doctor surface names the cycle while the chain is still linked;
    // once allocation degrades, the chain is abandoned (leaked, sweepable)
    // and the walk honestly reports no ledger.
    assert!(reopened.free_pages_ledger_pages().is_err());
    reopened.raise_min_pin(u64::MAX);
    let length_before = fs::metadata(&path).unwrap().len();
    let file_pages = length_before / u64::from(PAGE_SIZE);
    assert_eq!(reopened.allocate_page().unwrap(), file_pages);
    assert!(reopened.free_pages_degraded());
    // Append allocation grows the file by exactly the one page handed out
    // — never by a pwrite at the bogus id.
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        length_before + u64::from(PAGE_SIZE)
    );
    assert!(reopened.free_pages_ledger_pages().unwrap().is_empty());
}

/// Unit pin for the failed-flip floor law: a slot pwrite that may have
/// landed before a sync failure must raise the session floor to the staged
/// total. The storage harness has no sync fault hook, so this pins the
/// state transition directly.
#[test]
fn failed_flip_floor_uses_the_staged_total() {
    let mut state = devondb_storage::pager::FreeStateForTest {
        retired_total_floor: 10,
        pending_extension: Some(SuperblockExtension {
            retire_ledger_head: 2,
            retire_ledger_tail: 2,
            retired_total: 25,
        }),
    };
    devondb_storage::pager::apply_failed_flip_floor(&mut state);
    assert_eq!(state.retired_total_floor, 25);
}
