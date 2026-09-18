//! Compares RLE and plain decoding for the same 2048-row column under the
//! benchmark protocol in `docs/SCALE.md` §8.3.
//!
//! "Representative" data: a run-heavy Int64 status-code column — 2048 rows
//! as 32 repetitions of the 64-row unit `32×7, 16×3, 8×42, 8×(-1)` (128
//! runs total, mean run length 16, four distinct values). No NULLs. The
//! rle values section is 4 + 128 × 12 = 1,540 bytes against plain's
//! 16,384 — the comparison includes the page-read savings. Both sides go
//! through the same public typed read
//! (`NodeGroup::read_column_typed`) with the same pager, directory, and
//! CRC law; the delta is the values-section decode.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-rle-encode";
const ROWS: i64 = 2048;

struct Fixture {
    _directory: tempfile::TempDir,
    pager: Pager,
    directory_page: u64,
}

/// One 64-row unit of the representative pattern: 32×7, 16×3, 8×42, 8×-1.
fn unit_rows() -> [i64; 64] {
    let mut unit = [7_i64; 64];
    unit[32..48].fill(3);
    unit[48..56].fill(42);
    unit[56..64].fill(-1);
    unit
}

/// Builds the 2048-row single-column Int64 group, written plain or forced
/// to rle.
fn fixture(name: &str, forced: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    let unit = unit_rows();
    for row in 0..ROWS {
        group
            .push_row(vec![Value::Int64(unit[(row % 64) as usize])])
            .unwrap();
    }
    let directory_page = if forced {
        group.write_forcing_encodings(&pager, &[(0, 2)]).unwrap()
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
        &[LogicalType::Int64],
        0,
    )
    .unwrap();
    assert_eq!(row_count, ROWS as usize);
    criterion::black_box(column);
}

fn decode_rle_vs_plain(c: &mut Criterion) {
    let plain = fixture("plain.devondb", false);
    let rle = fixture("rle.devondb", true);
    let mut group = c.benchmark_group("encoding_rle_2048_rows");
    group.bench_function("decode_plain", |bencher| {
        bencher.iter(|| decode_column_once(&plain));
    });
    group.bench_function("decode_rle", |bencher| {
        bencher.iter(|| decode_column_once(&rle));
    });
    group.finish();
}

criterion_group!(benches, decode_rle_vs_plain);
criterion_main!(benches);
