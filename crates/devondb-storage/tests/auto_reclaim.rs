//! Continuous reclamation tests: snapshot-watermark safety and bounded
//! physical truncation of an already-free file suffix.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, mpsc},
    thread,
};

use devondb_storage::{
    catalog::Catalog,
    node_table::{DeleteCompaction, NodeTable},
    overlay::{NodeDmlEffects, PkKey},
    pager::Pager,
    superblock::FREE_PAGES_FLAG,
};
use devondb_types::{
    logical_type::LogicalType,
    schema::{Column, NodeTableSchema},
    value::Value,
};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"auto-reclaim-db!";
const TABLE: &str = "Document";
const CHURN_PAYLOAD_BYTES: usize = 800;
const CHURN_SEED: u64 = 0x2790_0903;

fn schema() -> NodeTableSchema {
    NodeTableSchema::new(
        TABLE.to_owned(),
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

fn row(id: i64, body: impl Into<String>) -> Vec<Value> {
    vec![Value::Int64(id), Value::String(body.into())]
}

fn seed_table(pager: &Pager) -> (Catalog, Vec<Vec<Value>>) {
    let table_schema = schema();
    let mut catalog = Catalog::default();
    catalog.add_node_table(table_schema.clone()).unwrap();
    catalog.save(pager, 1).unwrap();

    let expected = (0..256)
        .map(|id| row(id, format!("seed-{id:03}-{}", "x".repeat(96))))
        .collect::<Vec<_>>();
    let mut table = NodeTable::new(table_schema);
    for value in &expected {
        table.recover_row(value.clone()).unwrap();
    }
    table.checkpoint(pager, &mut catalog).unwrap();
    catalog.save(pager, 2).unwrap();
    (catalog, expected)
}

fn checkpoint_update(pager: &Pager, catalog: &mut Catalog, lsn: u64, id: i64, body: &str) {
    let replacement = row(id, body);
    let mut effects = NodeDmlEffects::default();
    effects
        .updates
        .insert(PkKey::Int64(id), replacement.as_slice());
    NodeTable::new(schema())
        .checkpoint_with_dml(pager, catalog, effects, DeleteCompaction::Permitted)
        .unwrap();
    catalog.save(pager, lsn).unwrap();
}

#[test]
fn pinned_catalog_generation_blocks_reuse_while_checkpoints_write() {
    let directory = tempdir().unwrap();
    let pager = Arc::new(
        Pager::create(
            directory.path().join("snapshot-watermark.devondb"),
            PAGE_SIZE,
            DB_ID,
        )
        .unwrap(),
    );
    let (mut current, expected) = seed_table(&pager);
    pager.raise_min_pin(2);

    let pinned = current.clone();
    let reader_pager = Arc::clone(&pager);
    let (scan_request, requests) = mpsc::channel();
    let (scanned, acknowledgements) = mpsc::channel();
    let reader = thread::spawn(move || {
        while requests.recv().is_ok() {
            let rows = NodeTable::new(schema())
                .scan(&reader_pager, &pinned)
                .unwrap();
            scanned.send(rows).unwrap();
        }
    });

    scan_request.send(()).unwrap();
    assert_eq!(acknowledgements.recv().unwrap(), expected);
    checkpoint_update(&pager, &mut current, 3, 7, "writer-generation-three");
    scan_request.send(()).unwrap();
    assert_eq!(acknowledgements.recv().unwrap(), expected);
    checkpoint_update(&pager, &mut current, 4, 9, "writer-generation-four");
    scan_request.send(()).unwrap();
    assert_eq!(acknowledgements.recv().unwrap(), expected);

    drop(scan_request);
    reader.join().unwrap();
    assert_eq!(pager.min_pin(), 2, "the held reader pins generation two");

    pager.raise_min_pin(4);
    let reused_before = pager.free_pages_session_reused();
    checkpoint_update(&pager, &mut current, 5, 11, "reader-released");
    assert!(
        pager.free_pages_session_reused() > reused_before,
        "a later checkpoint should reuse pages only after the reader watermark advances"
    );
}

fn publish_retirements(pager: &Pager, page_ids: Vec<u64>, lsn: u64) {
    pager.retire_pages(page_ids, lsn).unwrap();
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn = lsn;
    if pager.has_free_pages_ledger() {
        superblock.feature_flags |= FREE_PAGES_FLAG;
    }
    pager.commit_superblock(superblock).unwrap();
}

fn marker(page_id: u64) -> Vec<u8> {
    vec![(page_id % 251) as u8; PAGE_SIZE as usize]
}

fn assert_markers(pager: &Pager, expected: &BTreeMap<u64, Vec<u8>>) {
    for (page_id, bytes) in expected {
        assert_eq!(
            pager.read_page(*page_id).unwrap().as_slice(),
            bytes.as_slice(),
            "page {page_id}"
        );
    }
}

#[test]
fn delete_heavy_free_tail_is_incrementally_truncated_with_exact_live_bytes() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("tail-truncation.devondb");
    let pager = Pager::create(&path, PAGE_SIZE, DB_ID).unwrap();

    // Insert-heavy phase: allocate a 200-page physical body and pin the
    // byte-for-byte contents that the delete phase must not disturb.
    assert_eq!(pager.allocate_run(200).unwrap(), 2);
    let mut live = BTreeMap::new();
    for page_id in 2..=201 {
        let bytes = marker(page_id);
        pager.write_page(page_id, &bytes).unwrap();
        if (22..100).contains(&page_id) {
            live.insert(page_id, bytes);
        }
    }

    // Build a low-page ledger home, then make pages 100..=202 unreachable,
    // mirroring a detach-delete checkpoint that compacts most stored rows.
    publish_retirements(&pager, (2..=20).collect(), 1);
    pager.raise_min_pin(2);
    publish_retirements(&pager, vec![21], 2);
    pager.raise_min_pin(3);
    assert_eq!(pager.allocate_run(18).unwrap(), 3);
    for page_id in 3..=20 {
        let bytes = marker(page_id);
        pager.write_page(page_id, &bytes).unwrap();
        live.insert(page_id, bytes);
    }
    publish_retirements(&pager, (100..=201).collect(), 3);
    let delete_peak = std::fs::metadata(&path).unwrap().len();
    assert_markers(&pager, &live);

    // The strict comparison supplies the recovery-slot delay: generation-3
    // retirements become eligible only after the watermark reaches 4.
    pager.raise_min_pin(4);
    publish_retirements(&pager, Vec::new(), 4);
    let compacted = std::fs::metadata(&path).unwrap().len();
    assert!(
        compacted < delete_peak,
        "{delete_peak} did not shrink below {compacted}"
    );
    assert_eq!(pager.free_pages_session_truncated(), 64);
    assert_markers(&pager, &live);
    drop(pager);

    let reopened = Pager::open(&path).unwrap();
    assert_markers(&reopened, &live);
    assert!(!reopened.free_pages_degraded());
    assert!(!reopened.free_pages_ledger_pages().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Regression: single-row churn on a multi-group table must not grow the file.
//
// In a 20,000-row table (10 node groups, 800-byte incompressible payloads), one
// upsert of an existing row + checkpoint per cycle, grew 19.4 MB -> 85.0 MB
// over 150 cycles with live bytes constant — every cycle rewrote one ~403-page
// group and the ledger-order run scan, limited to skipping one ledger page of
// entries, missed the retired generation's run and appended. The seed below
// reproduces the CLI's ledger shape (partial tail-group rewrites leave
// variable-size retired runs ahead of the churn batches) in-process; the
// controls proved in-process and cross-process are byte-identical.
// ---------------------------------------------------------------------------

fn churn_schema() -> NodeTableSchema {
    NodeTableSchema::new(
        "Churn".to_owned(),
        vec![
            Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            },
            Column {
                name: "version".to_owned(),
                ty: LogicalType::Int64,
                primary_key: false,
            },
            Column {
                name: "payload".to_owned(),
                ty: LogicalType::String,
                primary_key: false,
            },
        ],
    )
    .unwrap()
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn incompressible_payload(seed: u64, id: u64, lane: u64) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut state = seed ^ id.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ lane;
    let mut bytes = Vec::with_capacity(CHURN_PAYLOAD_BYTES);
    for _ in 0..CHURN_PAYLOAD_BYTES {
        state = splitmix64(state);
        bytes.push(HEX[(state >> 60) as usize]);
    }
    String::from_utf8(bytes).unwrap()
}

fn churn_row(id: i64, version: i64, seed: u64, lane: u64) -> Vec<Value> {
    vec![
        Value::Int64(id),
        Value::Int64(version),
        Value::String(incompressible_payload(seed, id as u64, lane)),
    ]
}

/// Seeds `rows` rows in `batch`-sized checkpoints — the CLI's autocheckpoint
/// shape, whose partial tail-group rewrites are the ledger debris that sat
/// ahead of every churn batch in the measured incident.
fn seed_multi_group_table(path: &Path, rows: usize, batch: usize) {
    let pager = Pager::create(path, PAGE_SIZE, DB_ID).unwrap();
    let schema = churn_schema();
    let mut catalog = Catalog::default();
    catalog.add_node_table(schema.clone()).unwrap();
    catalog.save(&pager, 1).unwrap();
    let mut next = 1;
    while next <= rows {
        let end = (next + batch - 1).min(rows);
        let mut table = NodeTable::new(schema.clone());
        for id in next..=end {
            table
                .recover_row(churn_row(id as i64, 0, CHURN_SEED, 0x43))
                .unwrap();
        }
        let generation = pager.superblock().checkpoint_lsn;
        pager.raise_min_pin(generation);
        table.checkpoint(&pager, &mut catalog).unwrap();
        catalog.save(&pager, generation + 1).unwrap();
        next = end + 1;
    }
}

/// One CLI-shaped cycle: fresh open, one upsert of an existing row, checkpoint.
fn reopen_upsert_checkpoint(path: &Path, cycle: usize, rows: usize) {
    let pager = Pager::open(path).unwrap();
    let mut catalog = Catalog::load(&pager).unwrap();
    let generation = pager.superblock().checkpoint_lsn;
    pager.raise_min_pin(generation);
    let id = cycle % rows + 1;
    let replacement = churn_row(id as i64, cycle as i64 + 1, CHURN_SEED ^ cycle as u64, 0x55);
    let mut effects = NodeDmlEffects::default();
    effects
        .updates
        .insert(PkKey::Int64(id as i64), replacement.as_slice());
    NodeTable::new(churn_schema())
        .checkpoint_with_dml(&pager, &mut catalog, effects, DeleteCompaction::Permitted)
        .unwrap();
    catalog.save(&pager, generation + 1).unwrap();
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// Free pages = unconsumed ledger entries (the `.stats` definition).
fn free_pages(path: &Path) -> u64 {
    let pager = Pager::open(path).unwrap();
    pager
        .free_pages_ledger_pages()
        .unwrap()
        .iter()
        .map(|(_, ledger)| ledger.entries.len() as u64 - u64::from(ledger.consumed_count))
        .sum()
}

#[test]
fn multi_group_single_row_churn_stays_bounded() {
    const ROWS: usize = 20_000;
    const SEED_BATCH: usize = 1_000;
    const CYCLES: usize = 60;
    // One rewritten group: 2,048 rows × 800-byte payloads ≈ 403 pages.
    const GROUP_BYTES: u64 = 403 * PAGE_SIZE as u64;
    const SLACK_BYTES: u64 = 1024 * 1024;

    let directory = tempdir().unwrap();
    let path = directory.path().join("churn.devondb");
    seed_multi_group_table(&path, ROWS, SEED_BATCH);
    let seeded = file_bytes(&path);

    let mut at_half = 0;
    for cycle in 0..CYCLES {
        reopen_upsert_checkpoint(&path, cycle, 150);
        if (cycle + 1) % 10 == 0 {
            eprintln!(
                "CURVE cycle={} bytes={} free={}",
                cycle + 1,
                file_bytes(&path),
                free_pages(&path)
            );
        }
        if cycle + 1 == CYCLES / 2 {
            at_half = file_bytes(&path);
        }
    }
    let final_bytes = file_bytes(&path);
    let total_pages = final_bytes / u64::from(PAGE_SIZE);
    let free = free_pages(&path);

    // The pin horizon holds at most two retired generations plus the deferred
    // republish lag; eight group runs is a generous absolute ceiling that the
    // unfixed scan blew through by cycle 30 (measured 42 MB vs a 19 MB seed).
    assert!(
        final_bytes <= seeded + 8 * GROUP_BYTES + SLACK_BYTES,
        "single-row churn grew the file: seeded {seeded} B -> {final_bytes} B after {CYCLES} cycles"
    );
    assert_eq!(
        final_bytes,
        at_half,
        "the file must plateau: {at_half} B at cycle {} vs {final_bytes} B at cycle {CYCLES}",
        CYCLES / 2
    );
    assert!(
        free * 2 < total_pages,
        "free pages must stay a minority at steady state: {free} of {total_pages}"
    );

    // Every row reads back with its latest version and payload.
    let pager = Pager::open(&path).unwrap();
    let catalog = Catalog::load(&pager).unwrap();
    let rows = NodeTable::new(churn_schema())
        .scan(&pager, &catalog)
        .unwrap();
    assert_eq!(rows.len(), ROWS);
    let mut latest: BTreeMap<i64, (i64, String)> = BTreeMap::new();
    for cycle in 0..CYCLES {
        let id = (cycle % 150 + 1) as i64;
        latest.insert(
            id,
            (
                cycle as i64 + 1,
                incompressible_payload(CHURN_SEED ^ cycle as u64, id as u64, 0x55),
            ),
        );
    }
    for row in &rows {
        let Value::Int64(id) = row[0] else {
            panic!("id")
        };
        let (version, payload) = latest
            .get(&id)
            .cloned()
            .unwrap_or_else(|| (0, incompressible_payload(CHURN_SEED, id as u64, 0x43)));
        assert_eq!(row[1], Value::Int64(version), "row {id} version");
        assert_eq!(row[2], Value::String(payload), "row {id} payload");
    }
}
