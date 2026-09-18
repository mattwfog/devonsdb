use std::fs::File;
use std::path::Path;

use devondb_storage::lock::{LockPaths, PublicationGate, WriterLease};
use devondb_types::{DevonError, DevonResult};
use tempfile::{TempDir, tempdir};

fn make_lock_paths() -> (TempDir, LockPaths) {
    let directory = tempdir().unwrap();
    let main = directory.path().join("database.devon");
    File::create(&main).unwrap();
    let paths = LockPaths::for_main(&main).unwrap();
    (directory, paths)
}

fn assert_lock_busy<T>(result: DevonResult<T>, role: &str, path: &Path) {
    match result {
        Err(DevonError::Busy { context }) => {
            assert!(context.contains(role), "missing role in: {context}");
            assert!(
                context.contains(&path.display().to_string()),
                "missing path in: {context}"
            );
        }
        Err(other) => panic!("expected Busy, got {other}"),
        Ok(_) => panic!("expected contended lock"),
    }
}

#[test]
fn lock_writer_contention_reports_context_and_drop_releases() {
    let (_directory, paths) = make_lock_paths();
    let first = WriterLease::try_acquire(&paths).unwrap();

    assert_lock_busy(
        WriterLease::try_acquire(&paths),
        "writer lease",
        &paths.writer,
    );

    drop(first);
    let _successor = WriterLease::try_acquire(&paths).unwrap();
}

#[test]
fn lock_publication_modes_contend_and_guard_drop_releases() {
    let (_directory, paths) = make_lock_paths();
    let first_gate = PublicationGate::open(&paths).unwrap();
    let second_gate = PublicationGate::open(&paths).unwrap();
    let third_gate = PublicationGate::open(&paths).unwrap();
    let exclusive = first_gate.try_exclusive().unwrap();

    assert_lock_busy(second_gate.try_shared(), "publication gate", &paths.publish);
    assert_lock_busy(
        second_gate.try_exclusive(),
        "publication gate",
        &paths.publish,
    );

    drop(exclusive);
    let first_shared = first_gate.try_shared().unwrap();
    assert_lock_busy(
        second_gate.try_exclusive(),
        "publication gate",
        &paths.publish,
    );
    let second_shared = second_gate.try_shared().unwrap();

    drop(first_shared);
    drop(second_shared);
    let _exclusive_after_drop = third_gate.try_exclusive().unwrap();
}

#[cfg(unix)]
#[test]
fn lock_paths_canonicalize_symlink_identity() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().unwrap();
    let real = directory.path().join("real.devon");
    let link = directory.path().join("link.devon");
    File::create(&real).unwrap();
    symlink(&real, &link).unwrap();

    assert_eq!(
        LockPaths::for_main(&real).unwrap(),
        LockPaths::for_main(&link).unwrap()
    );
}

#[test]
fn lock_paths_reject_missing_main_without_creating_sidecars() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing.devon");
    let writer = directory.path().join("missing.devon.lock-writer");
    let publish = directory.path().join("missing.devon.lock-publish");

    assert!(matches!(
        LockPaths::for_main(&missing),
        Err(DevonError::Io(_))
    ));
    assert!(!writer.exists());
    assert!(!publish.exists());
}

#[test]
fn lock_sidecars_persist_and_are_reused() {
    let (_directory, paths) = make_lock_paths();
    let lease = WriterLease::try_acquire(&paths).unwrap();
    let gate = PublicationGate::open(&paths).unwrap();
    let guard = gate.try_exclusive().unwrap();

    drop(guard);
    drop(gate);
    drop(lease);
    assert!(paths.writer.is_file());
    assert!(paths.publish.is_file());

    let _reused_lease = WriterLease::try_acquire(&paths).unwrap();
    let reused_gate = PublicationGate::open(&paths).unwrap();
    let _reused_guard = reused_gate.try_shared().unwrap();
}

#[test]
fn lock_writer_lease_and_publication_gate_are_independent() {
    let (_directory, paths) = make_lock_paths();
    let lease = WriterLease::try_acquire(&paths).unwrap();
    let gate = PublicationGate::open(&paths).unwrap();
    let guard = gate.try_exclusive().unwrap();

    drop(lease);
    let _new_lease_while_gate_is_held = WriterLease::try_acquire(&paths).unwrap();
    drop(guard);
}
