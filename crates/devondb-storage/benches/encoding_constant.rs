//! Compares constant and plain decoding for the same 2048-row column under
//! the benchmark protocol in `docs/SCALE.md` §8.3.
//!
//! Both sides go through the same public typed read
//! (`NodeGroup::read_column_typed`): same pager, same page count for the
//! directory, same CRC law — the delta is the values-section decode. The
//! constant group carries a 2-page directory + 1 payload page where the
//! plain group carries 2049 payload bytes × 8, so the comparison includes
//! the page-read savings.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-encoding!!";
const ROWS: i64 = 2048;

struct Fixture {
    _directory: tempfile::TempDir,
    pager: Pager,
    directory_page: u64,
}

/// Builds a 2048-row single-column Int64 group (constant value 7, no
/// NULLs), written plain or forced to constant.
fn fixture(name: &str, forced: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::Int64]).unwrap();
    for _ in 0..ROWS {
        group.push_row(vec![Value::Int64(7)]).unwrap();
    }
    let directory_page = if forced {
        group.write_forcing_encodings(&pager, &[(0, 1)]).unwrap()
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

fn decode_constant_vs_plain(c: &mut Criterion) {
    let plain = fixture("plain.devondb", false);
    let constant = fixture("constant.devondb", true);
    let mut group = c.benchmark_group("encoding_constant_2048_rows");
    group.bench_function("decode_plain", |bencher| {
        bencher.iter(|| decode_column_once(&plain));
    });
    group.bench_function("decode_constant", |bencher| {
        bencher.iter(|| decode_column_once(&constant));
    });
    group.finish();
}

criterion_group!(benches, decode_constant_vs_plain);
criterion_main!(benches);
