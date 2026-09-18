//! A writer reopen sweeps stale spill state but must
//! never delete a live follower's spill directory — liveness is proven by
//! the follower's held `.lock`, never by names or pids, which recycle.
//!
//! The test uses two handles in one process because the operating system
//! enforces the lock identically across processes.

use std::fs;
use std::path::{Path, PathBuf};

use devondb::{Database, Statement};
use devondb_types::{logical_type::LogicalType, schema::Column};
use tempfile::tempdir;

const PAGE_SIZE: u32 = 4096;

fn spill_base(path: &Path) -> PathBuf {
    let mut base = path.as_os_str().to_owned();
    base.push(".tmp");
    PathBuf::from(base)
}

fn handle_dirs(base: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(base)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn writer_reopen_sweeps_stale_spill_state_but_never_a_live_follower() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("hygiene.devondb");
    let mut writer = Database::create(&path, PAGE_SIZE).unwrap();
    writer
        .execute(&Statement::CreateNodeTable {
            name: "Person".to_owned(),
            columns: vec![Column {
                name: "id".to_owned(),
                ty: LogicalType::Int64,
                primary_key: true,
            }],
        })
        .unwrap();
    drop(writer);
    Database::activate_multiprocess(&path).unwrap();

    let follower = Database::open_read_only(&path).unwrap();
    let base = spill_base(&fs::canonicalize(&path).unwrap());
    let after_follower = handle_dirs(&base);
    assert_eq!(
        after_follower.len(),
        1,
        "expected exactly the follower's spill dir, found {after_follower:?}"
    );
    let follower_dir = after_follower[0].clone();

    // A crashed process's leftovers: an unlocked handle directory with a
    // run inside, plus a legacy flat run file.
    let stale_dir = base.join("h-999-00000000deadbeef-0");
    fs::create_dir(&stale_dir).unwrap();
    fs::write(stale_dir.join(".lock"), b"").unwrap();
    fs::write(stale_dir.join("stale.run"), b"leftover").unwrap();
    let flat_file = base.join("sort-1-2-3.run");
    fs::write(&flat_file, b"legacy").unwrap();

    let writer = Database::open(&path).unwrap();

    assert!(
        follower_dir.exists(),
        "the writer reopen deleted the live follower's spill directory"
    );
    assert!(!stale_dir.exists(), "the stale handle directory survived");
    assert!(!flat_file.exists(), "the legacy flat run file survived");
    let after_reopen = handle_dirs(&base);
    assert_eq!(
        after_reopen.len(),
        2,
        "expected the follower's and the new writer's dirs, found {after_reopen:?}"
    );

    // Dropping each handle removes its own directory.
    drop(follower);
    assert!(!follower_dir.exists());
    drop(writer);
    assert_eq!(handle_dirs(&base).len(), 0);
}
