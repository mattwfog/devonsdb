//! End-to-end encoded-vector wiring through the typed embedded API.

use std::{
    env, fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use devondb::{Database, Plan, Statement};
use devondb_plan::{
    expr::Metric,
    ops::{Operator, PLAN_VERSION},
};
use devondb_storage::vector_encoding::{decode_f16, decode_i8, encode_f16, encode_i8};
use devondb_types::{
    logical_type::{B1Rescore, LogicalType, VectorEncoding},
    schema::Column,
    value::Value,
};

const PAGE_SIZE: u32 = 4096;
const TABLE: &str = "Corpus";
const DIM: usize = 64;
const N: usize = 512;
const K: u64 = 10;
const CORPUS_SEED: u64 = 0xd3_70_db_20_00;
const QUERY_SEED: u64 = 0x12_34_56_78_9a_bc_de_f0;
const B1_SEED: u64 = 0x4841_5348_2026_0803;
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const AUTOCHECKPOINT_ENV: &str = "DEVONDB_AUTOCHECKPOINT";
const RECOVERY_CHILD_ENV: &str = "DEVONDB_QUANTIZED_RECOVERY_CHILD";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "devondb-quantized-{label}-{timestamp}-{sequence}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn db_path(&self) -> PathBuf {
        self.path.join("db.devondb")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct Lcg(u64);

impl Lcg {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        let fraction = (self.0 >> 40) as f32 / (1_u32 << 24) as f32;
        (2.0 * fraction - 1.0) * 0.125
    }

    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.next_f32()).collect()
    }
}

#[test]
fn scans_and_knn_use_dequantized_values_after_checkpoint_reopen() {
    let directory = TestDirectory::new("checkpoint");
    let path = directory.db_path();
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    create_table(&mut database);

    let vectors = corpus(N);
    insert_vectors(&mut database, &vectors);
    database.checkpoint().expect("checkpoint encoded vectors");
    drop(database);

    let mut reopened = Database::open(&path).expect("reopen checkpointed database");
    assert_eq!(scan(&mut reopened), persisted_rows(&vectors));

    let query = Lcg::new(QUERY_SEED).vector();
    let mut plain_ids = knn_ids(&mut reopened, "plain", &query);
    let mut f16_ids = knn_ids(&mut reopened, "f16", &query);
    plain_ids.sort_unstable();
    f16_ids.sort_unstable();
    assert_eq!(f16_ids, plain_ids, "f16 changed the top-10 id set");
}

#[test]
fn committed_uncheckpointed_f32_rows_recover_from_wal() {
    if run_autocheckpoint_off_child(
        "committed_uncheckpointed_f32_rows_recover_from_wal",
        RECOVERY_CHILD_ENV,
    ) {
        return;
    }
    let directory = TestDirectory::new("wal-recovery");
    let path = directory.db_path();
    let vectors = corpus(17);
    let mut database = Database::create(&path, PAGE_SIZE).expect("create database");
    create_table(&mut database);
    insert_vectors(&mut database, &vectors);
    drop(database);

    let mut reopened = Database::open(&path).expect("recover committed WAL rows");
    assert_eq!(scan(&mut reopened), overlay_rows(&vectors));
}

fn create_table(database: &mut Database) {
    database
        .execute(&Statement::CreateNodeTable {
            name: TABLE.to_owned(),
            columns: vec![
                column("id", LogicalType::Int64, true),
                column("plain", LogicalType::Vector { dim: DIM as u32 }, false),
                column("f16", encoded(VectorEncoding::F16), false),
                column("i8", encoded(VectorEncoding::I8), false),
                column(
                    "b1",
                    encoded(VectorEncoding::B1 {
                        rotation_seed: B1_SEED,
                        rescore: B1Rescore::F32,
                    }),
                    false,
                ),
            ],
        })
        .expect("create typed encoded-vector table");
}

fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
    Column {
        name: name.to_owned(),
        ty,
        primary_key,
    }
}

fn encoded(encoding: VectorEncoding) -> LogicalType {
    LogicalType::VectorEncoded {
        dim: DIM as u32,
        encoding,
    }
}

fn corpus(count: usize) -> Vec<Vec<f32>> {
    let mut generator = Lcg::new(CORPUS_SEED);
    (0..count).map(|_| generator.vector()).collect()
}

fn insert_vectors(database: &mut Database, vectors: &[Vec<f32>]) {
    let rows = vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| {
            vec![
                Value::Int64(index as i64),
                Value::Vector(vector.clone()),
                Value::Vector(vector.clone()),
                Value::Vector(vector.clone()),
                Value::Vector(vector.clone()),
            ]
        })
        .collect();
    database
        .execute(&Statement::InsertNode {
            table: TABLE.to_owned(),
            rows,
        })
        .expect("insert deterministic vectors");
}

fn run_autocheckpoint_off_child(test_name: &str, child_env: &str) -> bool {
    if env::var_os(child_env).is_some() {
        return false;
    }
    let output = Command::new(env::current_exe().expect("current test executable"))
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(child_env, "1")
        .env(AUTOCHECKPOINT_ENV, "off")
        .output()
        .expect("spawn auto-checkpoint-off child");
    assert!(
        output.status.success(),
        "auto-checkpoint-off child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn scan(database: &mut Database) -> Vec<Vec<Value>> {
    database
        .run(&Plan {
            v: PLAN_VERSION,
            plan: Operator::ScanNodes {
                table: TABLE.to_owned(),
                binding: "c".to_owned(),
            },
        })
        .expect("scan encoded-vector table")
        .rows
}

fn persisted_rows(vectors: &[Vec<f32>]) -> Vec<Vec<Value>> {
    vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| {
            vec![
                Value::Int64(index as i64),
                Value::Vector(vector.clone()),
                Value::Vector(decode_f16(&encode_f16(vector)).unwrap()),
                Value::Vector(decode_i8(&encode_i8(vector)).unwrap()),
                Value::Vector(vector.clone()),
            ]
        })
        .collect()
}

fn overlay_rows(vectors: &[Vec<f32>]) -> Vec<Vec<Value>> {
    vectors
        .iter()
        .enumerate()
        .map(|(index, vector)| {
            vec![
                Value::Int64(index as i64),
                Value::Vector(vector.clone()),
                Value::Vector(vector.clone()),
                Value::Vector(vector.clone()),
                Value::Vector(vector.clone()),
            ]
        })
        .collect()
}

fn knn_ids(database: &mut Database, column: &str, query: &[f32]) -> Vec<i64> {
    database
        .run(&Plan {
            v: PLAN_VERSION,
            plan: Operator::KnnScan {
                table: TABLE.to_owned(),
                column: column.to_owned(),
                query: query.to_vec().into(),
                k: K,
                metric: Metric::L2,
                mode: devondb_plan::ops::KnnMode::Exact,
            },
        })
        .expect("run KNN over vector column")
        .rows
        .iter()
        .map(|row| match row.first() {
            Some(Value::Int64(id)) => *id,
            other => panic!("unexpected KNN id value: {other:?}"),
        })
        .collect()
}
