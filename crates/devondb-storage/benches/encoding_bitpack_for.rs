//! Compares bitpack-for and plain decoding for the same 2048-row column
//! under the benchmark protocol in `docs/SCALE.md` §8.3.
//!
//! Both sides go through the same public typed read
//! (`NodeGroup::read_column_typed`): same pager, same directory page,
//! same CRC law — the delta is the values-section decode. "Representative
//! data" is the shape the encoding exists for: epoch-microsecond event
//! timestamps in a narrow frame — `1_758_000_000_000_000 + i × 1_000 +
//! (i mod 7) × 13` (~1 kHz events with small jitter) — so deltas fit 22
//! bits and the encoded section is 5.9 KB against plain's 16 KB, letting
//! the comparison includes the page-read savings.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-bitpack!!!";
const ROWS: i64 = 2048;

struct Fixture {
    _directory: tempfile::TempDir,
    pager: Pager,
    directory_page: u64,
}

/// Builds the 2048-row single-column Timestamp group (no NULLs), written
/// plain or forced to bitpack_for.
fn fixture(name: &str, forced: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Timestamp]).unwrap();
    for row in 0..ROWS {
        let micros = 1_758_000_000_000_000 + row * 1_000 + (row % 7) * 13;
        group.push_row(vec![Value::Timestamp(micros)]).unwrap();
    }
    let directory_page = if forced {
        group.write_forcing_encodings(&pager, &[(0, 3)]).unwrap()
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
        &[LogicalType::Timestamp],
        0,
    )
    .unwrap();
    assert_eq!(row_count, ROWS as usize);
    criterion::black_box(column);
}

fn decode_bitpack_for_vs_plain(c: &mut Criterion) {
    let plain = fixture("plain.devondb", false);
    let bitpack_for = fixture("bitpack_for.devondb", true);
    let mut group = c.benchmark_group("encoding_bitpack_for_2048_rows");
    group.bench_function("decode_plain", |bencher| {
        bencher.iter(|| decode_column_once(&plain));
    });
    group.bench_function("decode_bitpack_for", |bencher| {
        bencher.iter(|| decode_column_once(&bitpack_for));
    });
    group.finish();
}

criterion_group!(benches, decode_bitpack_for_vs_plain);
criterion_main!(benches);
