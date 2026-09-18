//! Compares FSST and plain decoding for the same 2048-row `String` column
//! under the benchmark protocol in `docs/SCALE.md` §8.3.
//!
//! Both sides go through the same public typed read
//! (`NodeGroup::read_column_typed`): same pager, same directory law, same
//! CRC law — the delta is the values-section decode plus the page-read
//! savings.
//!
//! "Representative" data: URL-shaped strings with a long shared prefix
//! and a short varying suffix —
//! `https://www.example.devondb/users/{id}/profile?tab=settings` with
//! `{id}` cycling over 97 values — the profile/telemetry shape FSST is
//! for. The fsst section compresses this column to roughly a sixth of
//! the plain heap.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-enc-fsst!!";
const ROWS: usize = 2048;

struct Fixture {
    _directory: tempfile::TempDir,
    pager: Pager,
    directory_page: u64,
}

/// Builds a 2048-row single-column String group (no NULLs), written
/// plain or forced to fsst.
fn fixture(name: &str, forced: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    for row in 0..ROWS {
        group
            .push_row(vec![Value::String(format!(
                "https://www.example.devondb/users/{}/profile?tab=settings",
                row % 97
            ))])
            .unwrap();
    }
    let directory_page = if forced {
        group.write_forcing_encodings(&pager, &[(0, 5)]).unwrap()
    } else {
        group.write(&pager).unwrap()
    };
    let mut superblock = pager.superblock();
    superblock.checkpoint_lsn += 1;
    superblock.feature_flags |= COLUMN_ENCODINGS_FLAG | ZONE_MAPS_FLAG;
    pager.commit_superblock(superblock).unwrap();
    Fixture {
        _directory: directory,
        pager,
        directory_page,
    }
}

fn decode_column_once(fixture: &Fixture) {
    let (column, row_count) = NodeGroup::read_column_typed(
        &fixture.pager,
        fixture.directory_page,
        &[LogicalType::String],
        0,
    )
    .unwrap();
    assert_eq!(row_count, ROWS);
    criterion::black_box(column);
}

fn decode_fsst_vs_plain(c: &mut Criterion) {
    let plain = fixture("plain.devondb", false);
    let fsst = fixture("fsst.devondb", true);
    let mut group = c.benchmark_group("encoding_fsst_2048_rows");
    group.bench_function("decode_plain", |bencher| {
        bencher.iter(|| decode_column_once(&plain));
    });
    group.bench_function("decode_fsst", |bencher| {
        bencher.iter(|| decode_column_once(&fsst));
    });
    group.finish();
}

criterion_group!(benches, decode_fsst_vs_plain);
criterion_main!(benches);
