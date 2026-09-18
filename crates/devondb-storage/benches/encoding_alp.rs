//! Compares ALP and plain decoding for the same 2048-row `Float64` column
//! under the benchmark protocol in `docs/SCALE.md` §8.3.
//!
//! "Representative" data: dollars-and-cents prices, `v_i = ((i × 7919)
//! mod 100_000) / 100` — two-decimal values in [0, 1000), the shape ALP
//! exists for. They encode at e=2 f=0 with bit_width 17 and no
//! exceptions, so the alp payload is ~4.4 KB against plain's 16 KB; both
//! sides go through the same public typed read
//! (`NodeGroup::read_column_typed`): same pager, same directory page,
//! same CRC law — the delta is the values-section decode plus the
//! page-read savings.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-alp!!!!!!!";
const ROWS: usize = 2048;

struct Fixture {
    _directory: tempfile::TempDir,
    pager: Pager,
    directory_page: u64,
}

/// The representative column: two-decimal prices, no NULLs.
fn rows() -> Vec<Value> {
    (0..ROWS)
        .map(|i| Value::Float64(((i * 7919) % 100_000) as f64 / 100.0))
        .collect()
}

/// Builds the 2048-row single-column Float64 group, written plain or
/// forced to alp.
fn fixture(name: &str, forced: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Float64]).unwrap();
    for row in rows() {
        group.push_row(vec![row]).unwrap();
    }
    let directory_page = if forced {
        group.write_forcing_encodings(&pager, &[(0, 6)]).unwrap()
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
        &[LogicalType::Float64],
        0,
    )
    .unwrap();
    assert_eq!(row_count, ROWS);
    criterion::black_box(column);
}

fn decode_alp_vs_plain(c: &mut Criterion) {
    let plain = fixture("plain.devondb", false);
    let alp = fixture("alp.devondb", true);
    let mut group = c.benchmark_group("encoding_alp_2048_rows");
    group.bench_function("decode_plain", |bencher| {
        bencher.iter(|| decode_column_once(&plain));
    });
    group.bench_function("decode_alp", |bencher| {
        bencher.iter(|| decode_column_once(&alp));
    });
    group.finish();
}

criterion_group!(benches, decode_alp_vs_plain);
criterion_main!(benches);
