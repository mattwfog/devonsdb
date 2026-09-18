//! The public database facade, immutable snapshots, and serialized commits.
//!
//! Readers pin an `Arc<PublishedState>` and never take the commit lock.
//! Writers buffer key-based changes privately and publish one immutable link
//! only after their complete WAL transaction has been fsynced.
//!
//! Automatic threshold and clean-close checkpoints are enabled by default.
//! Set `DEVONDB_AUTOCHECKPOINT=off` to disable both, for example when staging
//! an uncheckpointed WAL recovery fixture. Other values retain the default.
//! Manual checkpoints and crash recovery are unaffected.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use devondb_exec::{
    chunk::{CHUNK_CAPACITY, Chunk, ChunkBuilder},
    eval::{classof_column_key, scoreof_column_key},
    expand::Expand,
    knn::{DISTANCE_COLUMN_NAME, KnnScan as ExactKnnScan},
    operators::{
        Aggregate, Filter, InterfaceScanInput, Limit, Project, ScanInterface as ExecScanInterface,
        Sort, SpillConfig,
    },
    source::ChunkSource,
};
use devondb_plan::{
    expr::{BinaryOp, Expr, Metric},
    ops::{
        AggregateFunction, AggregateItem, Direction, KnnMode, Operator, PLAN_VERSION, Plan,
        ProjectionItem, SortKey,
    },
    statement::Statement,
};
use devondb_storage::{
    budget::MemoryBudget,
    catalog::Catalog,
    node_group::{NODE_GROUP_CAPACITY, NodeGroup},
    node_table::NodeTable,
    overlay::{
        COMMIT_LINK_OVERHEAD_BYTES, CommitDelta, CommitLink, CommitSummary, DdlOp, OverlayEdge,
        PublishedState, clear_recent_summaries, node_offset, prune_recent_summaries,
        shed_pk_caches,
    },
    pager::{Pager, generate_db_id},
    rel_table::RelTable,
    txn_log::{
        CommittedGroup, DdlPayload, WalPayload, decode_payload, encode_transaction,
        group_transactions,
    },
    wal::{WalWriter, replay},
};
use devondb_types::{
    DevonError, DevonResult,
    logical_type::LogicalType,
    schema::{NodeTableSchema, RelTableSchema},
    value::Value,
};

use crate::{
    graph::{GraphSnapshot, resolve_node_offset},
    txn::{PendingEdge, Snapshot, Transaction, WriteSet},
};

mod checkpoint;
mod commit;
mod copy;
mod dump;
mod hnsw;
mod multiprocess;
mod options;
mod projection;
mod recovery;
mod typing;
mod view;

pub(crate) use commit::{commit_transaction, execute_transaction, run_transaction};
pub use dump::{VisibleNode, VisibleNodeCursor, VisibleRelationship, VisibleRelationshipCursor};
pub(crate) use options::{CommitPipe, Shared};
pub use options::{Database, Options, QueryResult};
pub(crate) use view::run_snapshot;

fn truncate_wal(path: &Path) -> DevonResult<()> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

fn wal_path(path: &Path) -> PathBuf {
    let mut path_with_suffix = OsString::from(path.as_os_str());
    path_with_suffix.push("-wal");
    PathBuf::from(path_with_suffix)
}

fn spill_tmp_path(path: &Path) -> PathBuf {
    let mut path_with_suffix = OsString::from(path.as_os_str());
    path_with_suffix.push(".tmp");
    PathBuf::from(path_with_suffix)
}

const SPILL_LOCK_FILE: &str = ".lock";

/// A handle-owned spill subdirectory under `<db path>.tmp/`
/// (`docs/MVCC.md` §8.2).
///
/// The `.lock` file inside carries an exclusive OS file lock for as long as
/// this guard lives; [`sweep_stale_spill_dirs`] uses that lock — never names
/// or pids, both of which recycle — to tell a live handle's directory from a
/// crashed process's leftovers. Every handle spills only inside its own
/// directory, so a writer reopen can never delete a live follower's runs.
pub(crate) struct SpillDirGuard {
    dir: PathBuf,
    _lock: File,
}

impl SpillDirGuard {
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for SpillDirGuard {
    fn drop(&mut self) {
        // Best-effort hygiene: a failure here leaves a stale directory that
        // the next writable open's sweep removes.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Creates and locks this handle's own spill subdirectory.
fn acquire_spill_handle_dir(base: &Path) -> DevonResult<SpillDirGuard> {
    const MAX_ATTEMPTS: u32 = 64;
    fs::create_dir_all(base)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let token = (nanos as u64) ^ ((nanos >> 64) as u64);
    for attempt in 0..MAX_ATTEMPTS {
        let dir = base.join(format!("h-{}-{:016x}-{attempt}", std::process::id(), token));
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
        let lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(SPILL_LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => return Ok(SpillDirGuard { dir, _lock: lock }),
            // A lock held on a directory this process just created can only
            // mean the name was recycled out from under us — try the next.
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    Err(std::io::Error::other(format!(
        "could not claim a spill directory under {} after {MAX_ATTEMPTS} attempts",
        base.display()
    ))
    .into())
}

/// Removes provably stale spill state under `<db path>.tmp/` on writable
/// opens: subdirectories whose `.lock` is not held (the owning handle is
/// gone) and legacy flat files. Directories whose lock is held belong to a
/// live handle — a concurrent read-only follower mid-merge — and survive.
fn sweep_stale_spill_dirs(base: &Path) -> DevonResult<()> {
    let entries = match fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            remove_ignoring_missing(fs::remove_file(&path))?;
            continue;
        }
        match File::open(path.join(SPILL_LOCK_FILE)) {
            Ok(lock) => match lock.try_lock() {
                Ok(()) => {
                    // Lock acquired: the owning handle is gone. A fresh
                    // handle never reuses an existing directory name, so
                    // removal cannot race a new owner.
                    drop(lock);
                    remove_ignoring_missing(fs::remove_dir_all(&path))?;
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(error.into()),
            },
            // No lock file: never a live handle's directory.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                remove_ignoring_missing(fs::remove_dir_all(&path))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Two writable opens racing their sweeps (the writer lease is taken after
/// the sweep) may both target one stale entry; the loser's `NotFound` is
/// the desired outcome, not an error.
fn remove_ignoring_missing(result: std::io::Result<()>) -> DevonResult<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn next_lsn(pager: &Pager) -> DevonResult<u64> {
    pager
        .superblock()
        .checkpoint_lsn
        .checked_add(1)
        .ok_or_else(|| invalid_argument("WAL LSN space is exhausted"))
}

fn invalid_argument(context: impl Into<String>) -> DevonError {
    DevonError::InvalidArgument {
        context: context.into(),
    }
}

fn corrupt(context: impl Into<String>) -> DevonError {
    DevonError::Corrupt {
        context: context.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use devondb_plan::{
        expr::{BinaryOp, Expr, Metric},
        ops::{Direction, Operator, Plan, ProjectionItem},
        statement::{RelRow, Statement},
    };
    use devondb_storage::{
        overlay::{COMMIT_LINK_OVERHEAD_BYTES, CommitDelta, CommitSummary},
        txn_log::{WalPayload, encode_payload},
        wal::{WalWriter, replay},
    };
    use devondb_types::{DevonError, logical_type::LogicalType, schema::Column, value::Value};

    use super::{Database, Options, QueryResult, WriteSet};

    const PAGE_SIZE: u32 = 4096;
    const AUTOCHECKPOINT_ENV: &str = "DEVONDB_AUTOCHECKPOINT";
    const RECOVERY_CHILD_ENV: &str = "DEVONDB_TXN_RECOVERY_SUMMARY_CHILD";
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "devondb-database-test-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn database(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn run_autocheckpoint_off_child(test_name: &str, child_env: &str) -> bool {
        if std::env::var_os(child_env).is_some() {
            return false;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(child_env, "1")
            .env(AUTOCHECKPOINT_ENV, "off")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "auto-checkpoint-off child failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    fn column(name: &str, ty: LogicalType, primary_key: bool) -> Column {
        Column {
            name: name.to_owned(),
            ty,
            primary_key,
        }
    }

    fn create_person(database: &mut Database) {
        database
            .execute(&Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("name", LogicalType::String, false),
                    column("age", LogicalType::Int64, false),
                ],
            })
            .unwrap();
    }

    fn insert_people(database: &mut Database, rows: Vec<Vec<Value>>) {
        database
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows,
            })
            .unwrap();
    }

    fn create_knows(database: &mut Database) {
        database
            .execute(&Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: vec![column("since", LogicalType::Int64, false)],
            })
            .unwrap();
    }

    fn insert_knows(database: &mut Database, rows: &[(i64, i64, i64)]) {
        database
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: rows
                    .iter()
                    .map(|(from, to, since)| RelRow {
                        from_key: Value::Int64(*from),
                        to_key: Value::Int64(*to),
                        values: vec![Value::Int64(*since)],
                    })
                    .collect(),
            })
            .unwrap();
    }

    fn scan(table: &str, binding: &str) -> Operator {
        Operator::ScanNodes {
            table: table.to_owned(),
            binding: binding.to_owned(),
        }
    }

    fn plan(operator: Operator) -> Plan {
        Plan {
            v: 0,
            plan: operator,
        }
    }

    fn expand(
        rel: &str,
        direction: Direction,
        from_binding: &str,
        binding: &str,
        input: Operator,
    ) -> Operator {
        Operator::Expand {
            rel: rel.to_owned(),
            direction,
            from_binding: from_binding.to_owned(),
            binding: binding.to_owned(),
            input: Box::new(input),
        }
    }

    fn scan_people(database: &mut Database) -> QueryResult {
        database.run(&plan(scan("Person", "p"))).unwrap()
    }

    fn person(id: i64, name: &str, age: i64) -> Vec<Value> {
        vec![
            Value::Int64(id),
            Value::String(name.to_owned()),
            Value::Int64(age),
        ]
    }

    fn node_path(nodes: &[Vec<Value>]) -> Vec<Value> {
        nodes.iter().flatten().cloned().collect()
    }

    fn binary(operator: BinaryOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op: operator,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    #[test]
    fn public_api_runs_scan_filter_project_and_limit() {
        let directory = TestDirectory::new();
        let path = directory.database("pipeline.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(
            &mut database,
            vec![
                person(1, "Ada", 36),
                person(2, "Grace", 50),
                person(3, "Linus", 30),
                person(4, "Barbara", 45),
            ],
        );
        let filtered = Operator::Filter {
            predicate: binary(
                BinaryOp::Gt,
                Expr::Col("p.age".to_owned()),
                Expr::Lit(Value::Int64(30)),
            ),
            input: Box::new(scan("Person", "p")),
        };
        let limited = Operator::Limit {
            count: 2,
            offset: Some(1),
            input: Box::new(filtered),
        };
        let query = plan(Operator::Project {
            exprs: vec![
                ProjectionItem {
                    expr: Expr::Col("p.name".to_owned()),
                    alias: "name".to_owned(),
                },
                ProjectionItem {
                    expr: binary(
                        BinaryOp::Add,
                        Expr::Col("p.age".to_owned()),
                        Expr::Lit(Value::Int64(1)),
                    ),
                    alias: "next_age".to_owned(),
                },
            ],
            input: Box::new(limited),
        });

        assert_eq!(
            database.run(&query).unwrap(),
            QueryResult {
                columns: vec!["name".to_owned(), "next_age".to_owned()],
                rows: vec![
                    vec![Value::String("Grace".to_owned()), Value::Int64(51)],
                    vec![Value::String("Barbara".to_owned()), Value::Int64(46)],
                ],
            }
        );
    }

    #[test]
    fn checkpoint_empties_wal_and_rows_survive_reopen() {
        let directory = TestDirectory::new();
        let path = directory.database("checkpoint.devondb");
        let expected = vec![person(1, "Ada", 36), person(2, "Grace", 50)];
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, expected.clone());

        database.checkpoint().unwrap();

        assert_eq!(fs::metadata(wal_path(&path)).unwrap().len(), 0);
        assert_eq!(scan_people(&mut database).rows, expected);
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(scan_people(&mut reopened).rows, expected);
    }

    #[test]
    fn crash_between_catalog_publish_and_wal_reset_does_not_duplicate_data() {
        let directory = TestDirectory::new();
        let path = directory.database("checkpoint-crash-window.devondb");
        let people = vec![
            person(1, "Ada", 36),
            person(2, "Grace", 50),
            person(3, "Linus", 30),
        ];
        let edges = [(1, 2, 1843), (2, 3, 1952)];
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        create_knows(&mut database);
        insert_people(&mut database, people);
        insert_knows(&mut database, &edges);
        let wal_path = wal_path(&path);
        let pre_truncation_wal = fs::read(&wal_path).unwrap();

        database.checkpoint().unwrap();
        fs::write(&wal_path, pre_truncation_wal).unwrap();
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        reopened.checkpoint().unwrap();

        assert_eq!(scan_people(&mut reopened).rows.len(), 3);
        let relationships = reopened
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(relationships.rows.len(), edges.len());
    }

    #[test]
    fn schema_only_ddl_stays_in_wal_and_next_crash_recovers_insert() {
        let directory = TestDirectory::new();
        let path = directory.database("schema-only-crash-recovery.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        let previous_lsn = database.shared.pager.superblock().checkpoint_lsn;
        let previous_root = database.shared.pager.superblock().catalog_root;
        let previous_wal_len = fs::metadata(wal_path(&path)).unwrap().len();

        database
            .execute(&Statement::CreateNodeTable {
                name: "City".to_owned(),
                columns: vec![column("id", LogicalType::Int64, true)],
            })
            .unwrap();

        assert_eq!(
            database.shared.pager.superblock().checkpoint_lsn,
            previous_lsn
        );
        assert_eq!(
            database.shared.pager.superblock().catalog_root,
            previous_root
        );
        assert!(fs::metadata(wal_path(&path)).unwrap().len() > previous_wal_len);
        database
            .execute(&Statement::InsertNode {
                table: "City".to_owned(),
                rows: vec![vec![Value::Int64(7)]],
            })
            .unwrap();
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(
            reopened.run(&plan(scan("City", "c"))).unwrap().rows,
            vec![vec![Value::Int64(7)]]
        );
    }

    #[test]
    fn lsn_continuity_survives_checkpoint_and_crash_recovery() {
        let directory = TestDirectory::new();
        let path = directory.database("checkpoint-lsn-continuity.devondb");
        let first_rows = vec![person(1, "Ada", 36), person(2, "Grace", 50)];
        let post_checkpoint = person(3, "Linus", 30);
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, first_rows.clone());
        let wal_path = wal_path(&path);
        let last_applied_lsn = replay(&wal_path).unwrap().last().unwrap().0;

        database.checkpoint().unwrap();

        assert_eq!(
            database.shared.pager.superblock().checkpoint_lsn,
            last_applied_lsn
        );
        insert_people(&mut database, vec![post_checkpoint.clone()]);
        let post_checkpoint_lsn = replay(&wal_path).unwrap().last().unwrap().0;
        assert_eq!(post_checkpoint_lsn, last_applied_lsn + 2);
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        let mut expected = first_rows;
        expected.push(post_checkpoint);
        assert_eq!(scan_people(&mut reopened).rows, expected);
    }

    #[test]
    fn drop_without_checkpoint_recovers_rows_from_wal() {
        let directory = TestDirectory::new();
        let path = directory.database("crash.devondb");
        let expected = vec![person(1, "Ada", 36), person(2, "Grace", 50)];
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, expected.clone());
        drop(database);

        let mut reopened = Database::open(&path).unwrap();

        assert_eq!(scan_people(&mut reopened).rows, expected);
    }

    #[test]
    fn interleaved_two_table_wal_records_replay_to_their_tables() {
        let directory = TestDirectory::new();
        let path = directory.database("routing.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        for name in ["Person", "Company"] {
            database
                .execute(&Statement::CreateNodeTable {
                    name: name.to_owned(),
                    columns: vec![
                        column("id", LogicalType::Int64, true),
                        column("label", LogicalType::String, false),
                    ],
                })
                .unwrap();
        }
        for (table, id, label) in [
            ("Person", 1, "Ada"),
            ("Company", 10, "Analytical Engines"),
            ("Person", 2, "Grace"),
            ("Company", 11, "Compilers Inc"),
        ] {
            database
                .execute(&Statement::InsertNode {
                    table: table.to_owned(),
                    rows: vec![vec![Value::Int64(id), Value::String(label.to_owned())]],
                })
                .unwrap();
        }
        drop(database);

        let mut reopened = Database::open(&path).unwrap();

        assert_eq!(
            reopened.run(&plan(scan("Person", "p"))).unwrap().rows,
            vec![
                vec![Value::Int64(1), Value::String("Ada".to_owned())],
                vec![Value::Int64(2), Value::String("Grace".to_owned())],
            ]
        );
        assert_eq!(
            reopened.run(&plan(scan("Company", "c"))).unwrap().rows,
            vec![
                vec![
                    Value::Int64(10),
                    Value::String("Analytical Engines".to_owned()),
                ],
                vec![Value::Int64(11), Value::String("Compilers Inc".to_owned()),],
            ]
        );
    }

    #[test]
    fn rel_create_validates_endpoints_and_duplicate_names() {
        let directory = TestDirectory::new();
        let path = directory.database("rel-ddl.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);

        let missing = database
            .execute(&Statement::CreateRelTable {
                name: "MissingRel".to_owned(),
                from: "Person".to_owned(),
                to: "Missing".to_owned(),
                columns: Vec::new(),
            })
            .unwrap_err();
        assert!(matches!(missing, DevonError::NotFound { what } if what.contains("Missing")));

        create_knows(&mut database);
        let duplicate = database
            .execute(&Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: Vec::new(),
            })
            .unwrap_err();
        assert_invalid_mentions(duplicate, &["Knows", "already"]);
    }

    #[test]
    fn rel_insert_resolves_checkpointed_and_buffered_keys_and_types() {
        let directory = TestDirectory::new();
        let path = directory.database("rel-keys.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        create_knows(&mut database);
        insert_people(&mut database, vec![person(1, "Ada", 36)]);
        database.checkpoint().unwrap();
        insert_people(&mut database, vec![person(2, "Grace", 50)]);
        insert_knows(&mut database, &[(1, 2, 1843)]);

        let result = database
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(
            result.rows,
            vec![node_path(&[person(1, "Ada", 36), person(2, "Grace", 50)])]
        );

        let unknown = database
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: vec![RelRow {
                    from_key: Value::Int64(999),
                    to_key: Value::Int64(2),
                    values: vec![Value::Int64(2026)],
                }],
            })
            .unwrap_err();
        assert!(
            matches!(unknown, DevonError::NotFound { what } if what.contains("Person") && what.contains("999"))
        );

        let wrong_type = database
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: vec![RelRow {
                    from_key: Value::String("1".to_owned()),
                    to_key: Value::Int64(2),
                    values: vec![Value::Int64(2026)],
                }],
            })
            .unwrap_err();
        assert_invalid_mentions(wrong_type, &["Person", "Int64", "1"]);
    }

    #[test]
    fn rel_checkpoint_with_only_edges_survives_reopen() {
        let directory = TestDirectory::new();
        let path = directory.database("rel-checkpoint.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        create_knows(&mut database);
        insert_people(
            &mut database,
            vec![person(1, "Ada", 36), person(2, "Grace", 50)],
        );
        database.checkpoint().unwrap();
        insert_knows(&mut database, &[(1, 2, 1843)]);

        database.checkpoint().unwrap();

        assert_eq!(fs::metadata(wal_path(&path)).unwrap().len(), 0);
        drop(database);
        let mut reopened = Database::open(&path).unwrap();
        let result = reopened
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(
            result.rows,
            vec![node_path(&[person(1, "Ada", 36), person(2, "Grace", 50)])]
        );
    }

    #[test]
    fn rel_wal_replay_recovers_edges_without_checkpoint() {
        let directory = TestDirectory::new();
        let path = directory.database("rel-recovery.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        create_knows(&mut database);
        insert_people(
            &mut database,
            vec![person(1, "Ada", 36), person(2, "Grace", 50)],
        );
        database.checkpoint().unwrap();
        insert_knows(&mut database, &[(1, 2, 1843)]);
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        let result = reopened
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(
            result.rows,
            vec![node_path(&[person(1, "Ada", 36), person(2, "Grace", 50)])]
        );
    }

    #[test]
    fn expand_out_in_both_and_two_hops_return_exact_rows() {
        let directory = TestDirectory::new();
        let path = directory.database("expand-directions.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        create_knows(&mut database);
        let people = vec![
            person(1, "Ada", 36),
            person(2, "Grace", 50),
            person(3, "Linus", 30),
            person(4, "Barbara", 45),
        ];
        insert_people(&mut database, people.clone());
        insert_knows(&mut database, &[(1, 2, 1), (2, 3, 2), (3, 4, 3)]);

        let out = database
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(
            out.rows,
            vec![
                node_path(&[people[0].clone(), people[1].clone()]),
                node_path(&[people[1].clone(), people[2].clone()]),
                node_path(&[people[2].clone(), people[3].clone()]),
            ]
        );

        let incoming = database
            .run(&plan(expand(
                "Knows",
                Direction::In,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(
            incoming.rows,
            vec![
                node_path(&[people[1].clone(), people[0].clone()]),
                node_path(&[people[2].clone(), people[1].clone()]),
                node_path(&[people[3].clone(), people[2].clone()]),
            ]
        );

        let both = database
            .run(&plan(expand(
                "Knows",
                Direction::Both,
                "p",
                "friend",
                scan("Person", "p"),
            )))
            .unwrap();
        assert_eq!(
            both.rows,
            vec![
                node_path(&[people[0].clone(), people[1].clone()]),
                node_path(&[people[1].clone(), people[2].clone()]),
                node_path(&[people[1].clone(), people[0].clone()]),
                node_path(&[people[2].clone(), people[3].clone()]),
                node_path(&[people[2].clone(), people[1].clone()]),
                node_path(&[people[3].clone(), people[2].clone()]),
            ]
        );

        let first_hop = expand("Knows", Direction::Out, "p", "friend", scan("Person", "p"));
        let two_hops = database
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "friend",
                "friend2",
                first_hop,
            )))
            .unwrap();
        assert_eq!(
            two_hops.rows,
            vec![
                node_path(&[people[0].clone(), people[1].clone(), people[2].clone()]),
                node_path(&[people[1].clone(), people[2].clone(), people[3].clone()]),
            ]
        );
    }

    #[test]
    fn expand_filters_and_internal_offsets_stay_out_of_results() {
        let directory = TestDirectory::new();
        let path = directory.database("expand-filter.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        create_knows(&mut database);
        let people = vec![
            person(1, "Ada", 36),
            person(2, "Grace", 50),
            person(3, "Linus", 30),
            person(4, "Barbara", 45),
        ];
        insert_people(&mut database, people.clone());
        insert_knows(&mut database, &[(1, 2, 1), (2, 3, 2), (3, 4, 3)]);

        let bare = database.run(&plan(scan("Person", "p"))).unwrap();
        assert_eq!(
            bare.columns,
            vec!["p.id".to_owned(), "p.name".to_owned(), "p.age".to_owned()]
        );
        assert!(bare.rows.iter().all(|row| row.len() == 3));

        let filtered_input = Operator::Filter {
            predicate: binary(
                BinaryOp::Gt,
                Expr::Col("p.age".to_owned()),
                Expr::Lit(Value::Int64(35)),
            ),
            input: Box::new(scan("Person", "p")),
        };
        let before = database
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                filtered_input,
            )))
            .unwrap();
        assert_eq!(
            before.rows,
            vec![
                node_path(&[people[0].clone(), people[1].clone()]),
                node_path(&[people[1].clone(), people[2].clone()]),
            ]
        );

        let expanded = expand("Knows", Direction::Out, "p", "friend", scan("Person", "p"));
        let after = database
            .run(&plan(Operator::Filter {
                predicate: binary(
                    BinaryOp::Gt,
                    Expr::Col("friend.age".to_owned()),
                    Expr::Lit(Value::Int64(40)),
                ),
                input: Box::new(expanded),
            }))
            .unwrap();
        assert_eq!(
            after.columns,
            vec![
                "p.id".to_owned(),
                "p.name".to_owned(),
                "p.age".to_owned(),
                "friend.id".to_owned(),
                "friend.name".to_owned(),
                "friend.age".to_owned(),
            ]
        );
        assert_eq!(
            after.rows,
            vec![
                node_path(&[people[0].clone(), people[1].clone()]),
                node_path(&[people[2].clone(), people[3].clone()]),
            ]
        );
        assert!(
            after
                .rows
                .iter()
                .all(|row| row.len() == after.columns.len())
        );

        let limited = database
            .run(&plan(Operator::Limit {
                count: 1,
                offset: None,
                input: Box::new(expand(
                    "Knows",
                    Direction::Out,
                    "p",
                    "friend",
                    scan("Person", "p"),
                )),
            }))
            .unwrap();
        assert_eq!(
            limited.rows,
            vec![node_path(&[people[0].clone(), people[1].clone()])]
        );
        assert_eq!(limited.rows[0].len(), limited.columns.len());

        let projected_input = Operator::Project {
            exprs: vec![ProjectionItem {
                expr: Expr::Col("p.name".to_owned()),
                alias: "source_name".to_owned(),
            }],
            input: Box::new(scan("Person", "p")),
        };
        let after_project = database
            .run(&plan(expand(
                "Knows",
                Direction::Out,
                "p",
                "friend",
                projected_input,
            )))
            .unwrap();
        assert_eq!(
            after_project.columns,
            vec![
                "source_name".to_owned(),
                "friend.id".to_owned(),
                "friend.name".to_owned(),
                "friend.age".to_owned(),
            ]
        );
        assert_eq!(
            after_project.rows,
            vec![
                vec![
                    Value::String("Ada".to_owned()),
                    Value::Int64(2),
                    Value::String("Grace".to_owned()),
                    Value::Int64(50),
                ],
                vec![
                    Value::String("Grace".to_owned()),
                    Value::Int64(3),
                    Value::String("Linus".to_owned()),
                    Value::Int64(30),
                ],
                vec![
                    Value::String("Linus".to_owned()),
                    Value::Int64(4),
                    Value::String("Barbara".to_owned()),
                    Value::Int64(45),
                ],
            ]
        );
    }

    #[test]
    fn expand_both_accepts_relationships_with_different_endpoints() {
        let directory = TestDirectory::new();
        let path = directory.database("expand-both-endpoints.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        database
            .execute(&Statement::CreateNodeTable {
                name: "City".to_owned(),
                columns: vec![column("id", LogicalType::Int64, true)],
            })
            .unwrap();
        database
            .execute(&Statement::CreateRelTable {
                name: "LivesIn".to_owned(),
                from: "Person".to_owned(),
                to: "City".to_owned(),
                columns: Vec::new(),
            })
            .unwrap();

        let result = database
            .run(&plan(expand(
                "LivesIn",
                Direction::Both,
                "p",
                "city",
                scan("Person", "p"),
            )))
            .unwrap();

        assert_eq!(result.columns, ["p.id", "p.name", "p.age", "city.id"]);
        assert!(result.rows.is_empty());
    }

    #[test]
    fn validation_errors_for_unknown_table_and_column_surface_unchanged() {
        let directory = TestDirectory::new();
        let path = directory.database("validation.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);

        let missing_table = database.run(&plan(scan("Missing", "m"))).unwrap_err();
        let DevonError::NotFound { what } = missing_table else {
            panic!("expected NotFound, got {missing_table}");
        };
        assert!(what.contains("Missing"));
        assert!(what.contains("ScanNodes"));

        let missing_column = database
            .run(&plan(Operator::Filter {
                predicate: Expr::Col("p.missing".to_owned()),
                input: Box::new(scan("Person", "p")),
            }))
            .unwrap_err();
        let DevonError::InvalidArgument { context } = missing_column else {
            panic!("expected InvalidArgument, got {missing_column}");
        };
        assert!(context.contains("Filter"));
        assert!(context.contains("missing"));
    }

    #[test]
    fn knn_scan_over_an_empty_table_returns_no_rows_with_full_columns() {
        let directory = TestDirectory::new();
        let path = directory.database("knn-empty.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Document".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("embedding", LogicalType::Vector { dim: 2 }, false),
                ],
            })
            .unwrap();
        let result = database
            .run(&plan(Operator::KnnScan {
                table: "Document".to_owned(),
                column: "embedding".to_owned(),
                query: vec![0.0, 1.0].into(),
                k: 1,
                metric: Metric::L2,
                mode: devondb_plan::ops::KnnMode::Exact,
            }))
            .unwrap();
        assert_eq!(
            result.columns,
            vec!["Document.id", "Document.embedding", "distance"]
        );
        assert!(result.rows.is_empty());
    }

    #[test]
    fn create_node_table_is_durable_without_checkpoint() {
        let directory = TestDirectory::new();
        let path = directory.database("ddl.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        let result = scan_people(&mut reopened);

        assert_eq!(
            result.columns,
            vec!["p.id".to_owned(), "p.name".to_owned(), "p.age".to_owned()]
        );
        assert!(result.rows.is_empty());
    }

    #[test]
    fn bare_null_projection_is_a_value() {
        let directory = TestDirectory::new();
        let path = directory.database("null.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, vec![person(1, "Ada", 36)]);
        let result = database
            .run(&plan(Operator::Project {
                exprs: vec![ProjectionItem {
                    expr: Expr::Lit(Value::Null),
                    alias: "nothing".to_owned(),
                }],
                input: Box::new(scan("Person", "p")),
            }))
            .unwrap();

        assert_eq!(result.columns, vec!["nothing"]);
        assert_eq!(result.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn options_defaults_match_the_design_defaults() {
        let options = Options::default();

        assert_eq!(options.page_size, 4096);
        assert_eq!(options.memory_limit, 64 * 1024 * 1024);
    }

    #[test]
    fn sub_minimum_memory_limit_is_rejected_on_create_and_open() {
        let directory = TestDirectory::new();
        let path = directory.database("memory-limit.devondb");
        let too_small = Options {
            memory_limit: 1024 * 1024 - 1,
            ..Options::default()
        };

        let Err(create_error) = Database::create_with(&path, too_small) else {
            panic!("expected create_with to reject a sub-1MiB memory_limit");
        };
        assert_invalid_mentions(create_error, &["memory_limit", "minimum", "1 MiB"]);

        drop(Database::create(&path, PAGE_SIZE).unwrap());
        let Err(open_error) = Database::open_with(&path, too_small) else {
            panic!("expected open_with to reject a sub-1MiB memory_limit");
        };
        assert_invalid_mentions(open_error, &["memory_limit", "minimum", "1 MiB"]);
    }

    #[test]
    fn explicit_options_thread_the_memory_limit_and_round_trip() {
        let directory = TestDirectory::new();
        let path = directory.database("options.devondb");
        let options = Options {
            page_size: PAGE_SIZE,
            memory_limit: 1024 * 1024,
        };
        let mut database = Database::create_with(&path, options).unwrap();
        assert_eq!(database.shared.budget.limit(), options.memory_limit);
        assert_eq!(database.shared.budget.charged(), 0);
        create_person(&mut database);
        insert_people(&mut database, vec![person(1, "Ada", 36)]);
        drop(database);

        let mut reopened = Database::open_with(&path, options).unwrap();
        assert_eq!(reopened.shared.budget.limit(), options.memory_limit);
        assert_eq!(scan_people(&mut reopened).rows, vec![person(1, "Ada", 36)]);
    }

    #[test]
    fn txn_database_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Database>();
    }

    #[test]
    fn txn_snapshot_is_unchanged_by_a_concurrent_commit() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-snapshot.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, vec![person(1, "Ada", 36)]);
        let snapshot = database.snapshot();
        let before = snapshot.run(&plan(scan("Person", "p"))).unwrap();

        let mut writer = database.begin().unwrap();
        writer
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(2, "Grace", 50)],
            })
            .unwrap();
        writer.commit().unwrap();

        assert_eq!(snapshot.run(&plan(scan("Person", "p"))).unwrap(), before);
        assert_eq!(
            database
                .snapshot()
                .run(&plan(scan("Person", "p")))
                .unwrap()
                .rows
                .len(),
            2
        );
    }

    #[test]
    fn txn_duplicate_pk_conflict_has_exact_display_in_both_commit_orders() {
        for winner_is_first in [true, false] {
            let directory = TestDirectory::new();
            let path = directory.database("txn-pk-conflict.devondb");
            let mut database = Database::create(&path, PAGE_SIZE).unwrap();
            create_person(&mut database);
            let mut first = database.begin().unwrap();
            let mut second = database.begin().unwrap();
            for transaction in [&mut first, &mut second] {
                transaction
                    .execute(&Statement::InsertNode {
                        table: "Person".to_owned(),
                        rows: vec![person(7, "Winner", 42)],
                    })
                    .unwrap();
            }

            let error = if winner_is_first {
                first.commit().unwrap();
                second.commit().unwrap_err()
            } else {
                second.commit().unwrap();
                first.commit().unwrap_err()
            };

            assert_eq!(
                error.to_string(),
                "transaction conflict: node table `Person` primary key 7 was inserted by a concurrent transaction (committed at LSN 4)"
            );
            assert_eq!(scan_people(&mut database).rows.len(), 1);
        }
    }

    #[test]
    fn txn_ddl_name_collision_has_exact_display() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-ddl-conflict.devondb");
        let database = Database::create(&path, PAGE_SIZE).unwrap();
        let statement = Statement::CreateNodeTable {
            name: "T".to_owned(),
            columns: vec![column("id", LogicalType::Int64, true)],
        };
        let mut first = database.begin().unwrap();
        let mut second = database.begin().unwrap();
        first.execute(&statement).unwrap();
        second.execute(&statement).unwrap();

        first.commit().unwrap();
        let error = second.commit().unwrap_err();

        assert_eq!(
            error.to_string(),
            "transaction conflict: table `T` was created by a concurrent transaction (committed at LSN 2)"
        );
        assert!(
            database
                .snapshot()
                .run(&plan(scan("T", "t")))
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn txn_commit_is_atomic_across_drop_and_reopen() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-atomic-reopen.devondb");
        let database = Database::create(&path, PAGE_SIZE).unwrap();
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("name", LogicalType::String, false),
                    column("age", LogicalType::Int64, false),
                ],
            })
            .unwrap();
        transaction
            .execute(&Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: vec![column("since", LogicalType::Int64, false)],
            })
            .unwrap();
        transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, "Ada", 36), person(2, "Grace", 50)],
            })
            .unwrap();
        transaction
            .execute(&Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: vec![RelRow {
                    from_key: Value::Int64(1),
                    to_key: Value::Int64(2),
                    values: vec![Value::Int64(1843)],
                }],
            })
            .unwrap();
        assert_eq!(
            transaction
                .run(&plan(expand(
                    "Knows",
                    Direction::Out,
                    "p",
                    "friend",
                    scan("Person", "p"),
                )))
                .unwrap()
                .rows
                .len(),
            1
        );
        transaction.commit().unwrap();
        drop(database);

        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(scan_people(&mut reopened).rows.len(), 2);
        assert_eq!(
            reopened
                .run(&plan(expand(
                    "Knows",
                    Direction::Out,
                    "p",
                    "friend",
                    scan("Person", "p"),
                )))
                .unwrap()
                .rows
                .len(),
            1
        );
    }

    #[test]
    fn txn_recovery_groups_multiple_transactions_and_discards_trailing_group() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-recovery-groups.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, vec![person(1, "Ada", 36)]);
        insert_people(&mut database, vec![person(2, "Grace", 50)]);
        drop(database);

        let wal_path = wal_path(&path);
        let mut wal = WalWriter::open(&wal_path, 1).unwrap();
        wal.append(
            &encode_payload(&WalPayload::NodeInsert {
                table: "Person".to_owned(),
                row: person(999, "Uncommitted", 1),
            })
            .unwrap(),
        )
        .unwrap();
        wal.sync().unwrap();
        drop(wal);

        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(scan_people(&mut reopened).rows.len(), 2);
        insert_people(&mut reopened, vec![person(3, "Linus", 30)]);
        drop(reopened);

        let mut reopened_again = Database::open(&path).unwrap();
        assert_eq!(
            scan_people(&mut reopened_again)
                .rows
                .into_iter()
                .map(|row| row[0].clone())
                .collect::<Vec<_>>(),
            [1, 2, 3].map(Value::Int64)
        );
    }

    #[test]
    fn knn_scan_runs_through_the_facade_with_output_only_distance() {
        use devondb_plan::text::parser::{Parsed, parse};

        let directory = TestDirectory::new();
        let path = directory.database("knn-smoke.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Doc".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("embedding", LogicalType::Vector { dim: 2 }, false),
                ],
            })
            .unwrap();
        database
            .execute(&Statement::InsertNode {
                table: "Doc".to_owned(),
                rows: vec![
                    vec![Value::Int64(1), Value::Vector(vec![0.0, 1.0])],
                    vec![Value::Int64(2), Value::Vector(vec![3.0, 4.0])],
                    vec![Value::Int64(3), Value::Vector(vec![0.0, 2.0])],
                ],
            })
            .unwrap();

        let Parsed::Query(bare) = parse("knn(Doc.embedding, [0.0, 0.0], 2, l2)").unwrap() else {
            panic!("expected a query");
        };
        let result = database.run(&bare).unwrap();
        assert_eq!(result.columns, vec!["Doc.id", "Doc.embedding", "distance"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[0][2], Value::Float64(1.0));
        assert_eq!(result.rows[1][0], Value::Int64(3));
        assert_eq!(result.rows[1][2], Value::Float64(2.0));

        let Parsed::Query(composed) =
            parse("knn(Doc.embedding, [0.0, 0.0], 2, l2) | filter Doc.id > 1 | project Doc.id")
                .unwrap()
        else {
            panic!("expected a query");
        };
        let result = database.run(&composed).unwrap();
        assert_eq!(result.columns, vec!["Doc.id"]);
        assert_eq!(result.rows, vec![vec![Value::Int64(3)]]);
    }

    #[test]
    fn txn_recovery_recharges_commit_summaries() {
        if run_autocheckpoint_off_child(
            "database::tests::txn_recovery_recharges_commit_summaries",
            RECOVERY_CHILD_ENV,
        ) {
            return;
        }
        let directory = TestDirectory::new();
        let path = directory.database("txn-recovery-summary-charge.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        insert_people(&mut database, vec![person(1, &"x".repeat(4096), 36)]);
        drop(database);

        let reopened = Database::open(&path).unwrap();
        let state = reopened.shared.current_state();
        let summary_bytes = state
            .recent_summaries
            .iter()
            .map(|(_, summary)| {
                let estimated = summary.estimated_bytes().unwrap();
                assert_eq!(summary.charged_bytes(), estimated);
                estimated
            })
            .sum::<usize>();

        assert_eq!(state.recent_summaries.len(), 2);
        assert!(summary_bytes > 0);
        assert!(reopened.shared.budget.charged() >= summary_bytes);
    }

    #[test]
    fn txn_checkpoint_retains_conflict_summary_for_old_writer() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-summary-retention.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        let mut old = database.begin().unwrap();
        let mut winner = database.begin().unwrap();
        winner
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(7, "Winner", 42)],
            })
            .unwrap();
        winner.commit().unwrap();
        database.checkpoint().unwrap();

        old.execute(&Statement::InsertNode {
            table: "Person".to_owned(),
            rows: vec![person(7, "Loser", 43)],
        })
        .unwrap();
        let error = old.commit().unwrap_err();

        assert_eq!(
            error.to_string(),
            "transaction conflict: node table `Person` primary key 7 was inserted by a concurrent transaction (committed at LSN 4)"
        );
    }

    #[test]
    fn txn_pinned_writer_charges_summaries_until_checkpoint_prunes_them() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-summary-charge-retention.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Keyed".to_owned(),
                columns: vec![column("id", LogicalType::String, true)],
            })
            .unwrap();
        database.checkpoint().unwrap();
        // Retirement reads of the old catalog and ledger head cache
        // frames like any other read; measure summary charges net of the
        // page cache's legitimate occupancy.
        let net_charged = |database: &Database| {
            database.shared.budget.charged() - database.shared.pager.cache_charged_bytes()
        };
        let baseline = net_charged(&database);
        let pinned = database.begin().unwrap();
        let commit_count = 6;
        for commit in 0..commit_count {
            let rows = (0..16)
                .map(|row| vec![Value::String(format!("{commit}-{row}-{}", "x".repeat(512)))])
                .collect();
            database
                .execute(&Statement::InsertNode {
                    table: "Keyed".to_owned(),
                    rows,
                })
                .unwrap();
        }

        database.checkpoint().unwrap();

        let state = database.shared.current_state();
        assert!(state.chain.is_none());
        assert_eq!(state.recent_summaries.len(), commit_count);
        let summary_bytes = state
            .recent_summaries
            .iter()
            .map(|(_, summary)| {
                assert_eq!(summary.charged_bytes(), summary.estimated_bytes().unwrap());
                summary.charged_bytes()
            })
            .sum::<usize>();
        assert!(summary_bytes > commit_count * 8 * 1024);
        assert_eq!(net_charged(&database), baseline + summary_bytes);
        drop(state);

        pinned.abort();
        database.checkpoint().unwrap();

        assert!(database.shared.current_state().recent_summaries.is_empty());
        assert_eq!(net_charged(&database), baseline);
    }

    #[test]
    fn txn_primary_key_validation_rejects_null_and_existing_duplicates() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-pk-validation.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        let mut transaction = database.begin().unwrap();
        let null_error = transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![vec![
                    Value::Null,
                    Value::String("Nobody".to_owned()),
                    Value::Int64(0),
                ]],
            })
            .unwrap_err();
        assert_invalid_mentions(null_error, &["primary key", "id", "null"]);
        transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, "Ada", 36)],
            })
            .unwrap();
        transaction.commit().unwrap();

        let duplicate = database
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, "Other", 37)],
            })
            .unwrap_err();
        assert_eq!(
            duplicate.to_string(),
            "invalid argument: duplicate primary key `1` in node table `Person`"
        );
    }

    #[test]
    fn txn_autocommit_and_explicit_transaction_return_identical_results() {
        let directory = TestDirectory::new();
        let auto_path = directory.database("txn-autocommit.devondb");
        let explicit_path = directory.database("txn-explicit.devondb");
        let mut autocommit = Database::create(&auto_path, PAGE_SIZE).unwrap();
        let mut explicit = Database::create(&explicit_path, PAGE_SIZE).unwrap();
        let statements = vec![
            Statement::CreateNodeTable {
                name: "Person".to_owned(),
                columns: vec![
                    column("id", LogicalType::Int64, true),
                    column("name", LogicalType::String, false),
                    column("age", LogicalType::Int64, false),
                ],
            },
            Statement::CreateRelTable {
                name: "Knows".to_owned(),
                from: "Person".to_owned(),
                to: "Person".to_owned(),
                columns: vec![column("since", LogicalType::Int64, false)],
            },
            Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, "Ada", 36), person(2, "Grace", 50)],
            },
            Statement::InsertRel {
                table: "Knows".to_owned(),
                rows: vec![RelRow {
                    from_key: Value::Int64(1),
                    to_key: Value::Int64(2),
                    values: vec![Value::Int64(1843)],
                }],
            },
        ];
        for statement in &statements {
            autocommit.execute(statement).unwrap();
        }
        let mut transaction = explicit.begin().unwrap();
        for statement in &statements {
            transaction.execute(statement).unwrap();
        }
        transaction.commit().unwrap();

        let query = plan(expand(
            "Knows",
            Direction::Out,
            "p",
            "friend",
            scan("Person", "p"),
        ));
        assert_eq!(
            autocommit.run(&query).unwrap(),
            explicit.run(&query).unwrap()
        );
    }

    #[test]
    fn txn_streaming_scan_crosses_node_group_boundary() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-streaming-scan.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        let rows = (0..=2048)
            .map(|id| person(id, "row", id))
            .collect::<Vec<_>>();
        insert_people(&mut database, rows.clone());
        database.checkpoint().unwrap();

        assert_eq!(
            database
                .snapshot()
                .run(&plan(scan("Person", "p")))
                .unwrap()
                .rows,
            rows
        );
    }

    #[test]
    fn txn_budget_failure_surfaces_before_anything_is_durable() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-budget-preflight.devondb");
        let options = Options {
            page_size: PAGE_SIZE,
            memory_limit: 1024 * 1024,
        };
        let mut database = Database::create_with(&path, options).unwrap();
        create_person(&mut database);
        database.checkpoint().unwrap();
        let baseline = database.shared.budget.charged();
        let wal_len_before = fs::metadata(wal_path(&path)).unwrap().len();

        let oversized = "x".repeat(2 * 1024 * 1024);
        let mut transaction = database.begin().unwrap();
        let error = transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![vec![
                    Value::Int64(1),
                    Value::String(oversized),
                    Value::Int64(1),
                ]],
            })
            .unwrap_err();

        assert!(matches!(error, DevonError::BudgetExceeded { .. }));
        assert_eq!(database.shared.budget.charged(), baseline);
        assert_eq!(fs::metadata(wal_path(&path)).unwrap().len(), wal_len_before);
        assert!(matches!(
            transaction
                .execute(&Statement::InsertNode {
                    table: "Person".to_owned(),
                    rows: vec![person(2, "poisoned", 2)],
                })
                .unwrap_err(),
            DevonError::BudgetExceeded { .. }
        ));
        drop(transaction);
        assert_eq!(database.shared.budget.charged(), baseline);
        insert_people(&mut database, vec![person(2, "working", 2)]);
        assert_eq!(scan_people(&mut database).rows.len(), 1);
        drop(database);
        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(scan_people(&mut reopened).rows.len(), 1);
    }

    #[test]
    fn txn_write_set_charge_is_visible_and_commit_transfers_ownership() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-write-charge-transfer.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        database.checkpoint().unwrap();
        let baseline = database.shared.budget.charged();
        let mut transaction = database.begin().unwrap();

        transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, &"x".repeat(4096), 36)],
            })
            .unwrap();

        let write_charge = transaction.writes.charged;
        assert!(write_charge > 4096);
        assert_eq!(database.shared.budget.charged(), baseline + write_charge);
        transaction.commit().unwrap();

        let state = database.shared.current_state();
        let link_charge = state
            .commit_links_oldest_first()
            .map(|link| link.charged_bytes)
            .sum::<usize>();
        let summary_charge = state
            .recent_summaries
            .iter()
            .map(|(_, summary)| summary.charged_bytes())
            .sum::<usize>();
        assert_eq!(
            database.shared.budget.charged(),
            baseline + link_charge + summary_charge
        );
    }

    #[test]
    fn txn_abort_releases_the_recorded_write_set_charge() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-write-charge-abort.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        database.checkpoint().unwrap();
        let baseline = database.shared.budget.charged();
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, &"x".repeat(4096), 36)],
            })
            .unwrap();
        assert!(database.shared.budget.charged() > baseline);

        transaction.abort();

        assert_eq!(database.shared.budget.charged(), baseline);
    }

    #[test]
    fn txn_drop_releases_the_recorded_write_set_charge() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-write-charge-drop.devondb");
        let mut database = Database::create(&path, PAGE_SIZE).unwrap();
        create_person(&mut database);
        database.checkpoint().unwrap();
        let baseline = database.shared.budget.charged();
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&Statement::InsertNode {
                table: "Person".to_owned(),
                rows: vec![person(1, &"x".repeat(4096), 36)],
            })
            .unwrap();
        assert!(database.shared.budget.charged() > baseline);

        drop(transaction);

        assert_eq!(database.shared.budget.charged(), baseline);
    }

    #[test]
    fn txn_summary_charge_failure_precedes_durability_and_database_recovers() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-summary-budget-preflight.devondb");
        let options = Options {
            page_size: PAGE_SIZE,
            memory_limit: 1024 * 1024,
        };
        let mut database = Database::create_with(&path, options).unwrap();
        database
            .execute(&Statement::CreateNodeTable {
                name: "Keyed".to_owned(),
                columns: vec![column("id", LogicalType::String, true)],
            })
            .unwrap();
        database.checkpoint().unwrap();
        let baseline = database.shared.budget.charged();
        let available = options.memory_limit - baseline;
        let empty_row = vec![Value::String(String::new())];
        let empty_key = Value::String(String::new());
        let fixed = WriteSet::default()
            .node_insert_bytes(
                "Keyed",
                std::slice::from_ref(&empty_row),
                std::slice::from_ref(&empty_key),
            )
            .unwrap();
        let key = "k".repeat((available - fixed) / 2);
        let row = vec![Value::String(key.clone())];
        let raw_key = Value::String(key);
        let write_bytes = WriteSet::default()
            .node_insert_bytes(
                "Keyed",
                std::slice::from_ref(&row),
                std::slice::from_ref(&raw_key),
            )
            .unwrap();
        let delta = CommitDelta {
            nodes: BTreeMap::from([("Keyed".to_owned(), vec![row.clone()])]),
            ..CommitDelta::default()
        };
        let summary = CommitSummary::new(
            BTreeMap::from([("Keyed".to_owned(), vec![raw_key.clone()])]),
            Vec::new(),
        );
        let link_bytes = COMMIT_LINK_OVERHEAD_BYTES + delta.estimated_bytes().unwrap();
        let publication_bytes = link_bytes + summary.estimated_bytes().unwrap();
        assert!(write_bytes <= available);
        assert!(link_bytes <= available);
        assert!(publication_bytes > available);
        let wal_len_before = fs::metadata(wal_path(&path)).unwrap().len();
        let mut transaction = database.begin().unwrap();
        transaction
            .execute(&Statement::InsertNode {
                table: "Keyed".to_owned(),
                rows: vec![row],
            })
            .unwrap();

        let error = transaction.commit().unwrap_err();

        assert!(matches!(error, DevonError::BudgetExceeded { .. }));
        assert_eq!(database.shared.budget.charged(), baseline);
        assert_eq!(fs::metadata(wal_path(&path)).unwrap().len(), wal_len_before);
        database
            .execute(&Statement::InsertNode {
                table: "Keyed".to_owned(),
                rows: vec![vec![Value::String("small".to_owned())]],
            })
            .unwrap();
        assert_eq!(
            database.run(&plan(scan("Keyed", "k"))).unwrap().rows.len(),
            1
        );
    }

    #[test]
    fn txn_overlay_crossing_quarter_budget_triggers_checkpoint() {
        let directory = TestDirectory::new();
        let path = directory.database("txn-auto-checkpoint.devondb");
        let options = Options {
            page_size: PAGE_SIZE,
            memory_limit: 1024 * 1024,
        };
        let mut database = Database::create_with(&path, options).unwrap();
        create_person(&mut database);
        let rows = (0..700)
            .map(|id| person(id, &"x".repeat(400), id))
            .collect::<Vec<_>>();

        insert_people(&mut database, rows);

        assert!(database.shared.current_state().chain.is_none());
        assert_eq!(fs::metadata(wal_path(&path)).unwrap().len(), 0);
        assert_eq!(scan_people(&mut database).rows.len(), 700);
    }

    fn wal_path(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}-wal", path.display()))
    }

    fn assert_invalid_mentions(error: DevonError, expected: &[&str]) {
        let DevonError::InvalidArgument { context } = error else {
            panic!("expected InvalidArgument, got {error}");
        };
        for text in expected {
            assert!(
                context.contains(text),
                "expected `{context}` to contain `{text}`"
            );
        }
    }
}
