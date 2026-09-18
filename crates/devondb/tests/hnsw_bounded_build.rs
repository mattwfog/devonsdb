//! End-to-end gates for page-backed, budget-bounded HNSW construction.

use std::{collections::BTreeSet, path::Path};

use crc32c::crc32c;
use devondb::{Database, Options, Plan, QueryResult, Statement};
use devondb_plan::{
    expr::Metric,
    ops::{KnnMode, Operator, PLAN_VERSION},
};
use devondb_storage::{catalog::Catalog, pager::Pager};
use devondb_types::{logical_type::LogicalType, schema::Column, value::Value};
use tempfile::TempDir;

const PAGE_SIZE: u32 = 4096;
const LOW_MEMORY_LIMIT: usize = 1024 * 1024;
const SEED_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const TABLE: &str = "Corpus";
const VECTOR_COLUMN: &str = "embedding";
const INDEX: &str = "corpus_embedding_l2";

const BOUNDED_ROWS: usize = 160;
const BOUNDED_DIMENSION: usize = 6554;
const BOUNDED_VECTOR_BYTES: usize = BOUNDED_ROWS * BOUNDED_DIMENSION * size_of::<f32>();
const _: () = assert!(BOUNDED_VECTOR_BYTES > LOW_MEMORY_LIMIT * 3);
const BOUNDED_DB_ID: [u8; 16] = *b"hnsw-bound-build";
const GOLDEN_DB_ID: [u8; 16] = *b"hnsw-page-gold!!";

#[test]
fn persisted_vector_corpus_larger_than_budget_builds_and_answers_knn() {
    let directory = TempDir::new().expect("create bounded-build directory");
    let path = directory.path().join("bounded.devondb");
    seed_persisted_corpus(&path, BOUNDED_DB_ID, BOUNDED_ROWS, BOUNDED_DIMENSION);

    let mut database = open_with_limit(&path, LOW_MEMORY_LIMIT);
    // Before the page-backed accessor this fails here: ConstructionRows scans
    // and clones the complete 4 MiB vector corpus under a 1 MiB budget.
    create_index(&mut database);
    assert!(database.memory_budget().charged() <= LOW_MEMORY_LIMIT);
    drop(database);

    let mut verifier = open_with_limit(&path, SEED_MEMORY_LIMIT);
    for row in [0, BOUNDED_ROWS / 2, BOUNDED_ROWS - 1] {
        let query = constant_vector(row, BOUNDED_DIMENSION);
        let approximate = verifier
            .run(&knn_plan(query.clone(), KnnMode::Approximate))
            .expect("run approximate KNN after bounded build");
        let exact = verifier
            .run(&knn_plan(query, KnnMode::Exact))
            .expect("run exact KNN oracle");
        assert_eq!(approximate, exact);
        assert_eq!(
            result_id(&approximate),
            i64::try_from(row).expect("row id fits")
        );
    }
}

#[test]
fn small_fixture_keeps_pre_page_accessor_index_pages_byte_identical() {
    let directory = TempDir::new().expect("create byte-golden directory");
    let path = directory.path().join("golden.devondb");
    seed_persisted_corpus(&path, GOLDEN_DB_ID, 24, 4);
    let mut database = open_with_limit(&path, SEED_MEMORY_LIMIT);
    create_index(&mut database);
    drop(database);

    let actual = index_page_digests(&path);
    // Captured from the pre-page-accessor implementation for this fixed
    // db_id, table snapshot, parameters, seed derivation, and insertion order.
    let expected: &[(u64, u32)] = &[
        (7, 823_766_738),
        (8, 484_350_325),
        (9, 3_520_254_302),
        (10, 1_394_111_233),
        (11, 3_214_457_791),
    ];
    assert_eq!(actual, expected, "pre-accessor HNSW page golden changed");
}

fn seed_persisted_corpus(path: &Path, db_id: [u8; 16], rows: usize, dimension: usize) {
    drop(Pager::create(path, PAGE_SIZE, db_id).expect("create deterministic database"));
    let mut database = open_with_limit(path, SEED_MEMORY_LIMIT);
    database
        .execute(&Statement::CreateNodeTable {
            name: TABLE.to_owned(),
            columns: vec![
                Column {
                    name: "id".to_owned(),
                    ty: LogicalType::Int64,
                    primary_key: true,
                },
                Column {
                    name: VECTOR_COLUMN.to_owned(),
                    ty: LogicalType::Vector {
                        dim: u32::try_from(dimension).expect("test dimension fits u32"),
                    },
                    primary_key: false,
                },
            ],
        })
        .expect("create corpus table");
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows: (0..rows)
                .map(|row| {
                    vec![
                        Value::Int64(i64::try_from(row).expect("test row id fits i64")),
                        Value::Vector(constant_vector(row, dimension)),
                    ]
                })
                .collect(),
        })
        .expect("insert vector corpus");
    database.checkpoint().expect("persist vector corpus");
}

fn create_index(database: &mut Database) {
    database
        .execute(&Statement::CreateHnswIndex {
            name: INDEX.to_owned(),
            table: TABLE.to_owned(),
            column: VECTOR_COLUMN.to_owned(),
            metric: Metric::L2,
        })
        .expect("build HNSW index");
}

fn open_with_limit(path: &Path, memory_limit: usize) -> Database {
    Database::open_with(
        path,
        Options {
            page_size: PAGE_SIZE,
            memory_limit,
        },
    )
    .expect("open database")
}

fn constant_vector(row: usize, dimension: usize) -> Vec<f32> {
    vec![row as f32; dimension]
}

fn knn_plan(query: Vec<f32>, mode: KnnMode) -> Plan {
    Plan {
        v: PLAN_VERSION,
        plan: Operator::KnnScan {
            table: TABLE.to_owned(),
            column: VECTOR_COLUMN.to_owned(),
            query: query.into(),
            k: 1,
            metric: Metric::L2,
            mode,
        },
    }
}

fn result_id(result: &QueryResult) -> i64 {
    match result.rows.first().and_then(|row| row.first()) {
        Some(Value::Int64(id)) => *id,
        other => panic!("unexpected KNN id value: {other:?}"),
    }
}

fn index_page_digests(path: &Path) -> Vec<(u64, u32)> {
    let pager = Pager::open(path).expect("open pager for page golden");
    let catalog = Catalog::load(&pager).expect("load indexed catalog");
    let root_id = catalog
        .indexes()
        .iter()
        .find(|entry| entry.name == INDEX)
        .expect("find golden index")
        .root;
    let root = pager.read_page(root_id).expect("read HNSW root");
    assert_eq!(&root[..4], b"HNSW");
    let layer_count = usize::from(root[41]);
    let group_count = read_u32(&root, 44) as usize;
    let directory_first = read_u64(&root, 48);
    let directory_len = read_u32(&root, 56) as usize;
    assert_eq!(directory_len, layer_count * group_count * 8);

    let page_size = PAGE_SIZE as usize;
    let mut pages = BTreeSet::from([root_id]);
    let mut directory = Vec::with_capacity(directory_len);
    for page_index in 0..directory_len.div_ceil(page_size) {
        let page_id = directory_first + u64::try_from(page_index).expect("page index fits u64");
        pages.insert(page_id);
        let page = pager.read_page(page_id).expect("read layer directory page");
        let take = (directory_len - directory.len()).min(page_size);
        directory.extend_from_slice(&page[..take]);
    }
    for cell in directory.as_chunks::<8>().0 {
        let csr_id = u64::from_le_bytes(*cell);
        if csr_id == 0 || !pages.insert(csr_id) {
            continue;
        }
        let csr = pager.read_page(csr_id).expect("read HNSW CSR directory");
        assert_eq!(&csr[..4], b"RCSR");
        collect_payload_pages(&mut pages, &csr, 16, page_size);
        collect_payload_pages(&mut pages, &csr, 32, page_size);
    }
    pages
        .into_iter()
        .map(|page_id| {
            let page = pager.read_page(page_id).expect("read golden index page");
            (page_id, crc32c(&page))
        })
        .collect()
}

fn collect_payload_pages(
    pages: &mut BTreeSet<u64>,
    directory: &[u8],
    entry_offset: usize,
    page_size: usize,
) {
    let first = read_u64(directory, entry_offset);
    let byte_len = read_u32(directory, entry_offset + 8) as usize;
    for page_index in 0..byte_len.div_ceil(page_size) {
        pages.insert(first + u64::try_from(page_index).expect("page index fits u64"));
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("u32 bytes"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("u64 bytes"))
}
