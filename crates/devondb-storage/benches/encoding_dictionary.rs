//! Compares dictionary and plain decoding for the same 2048-row `String`
//! column under the benchmark protocol in `docs/SCALE.md` §8.3.
//!
//! Both sides go through the same public typed read
//! (`NodeGroup::read_column_typed`): same pager, same page count for the
//! directory, same CRC law — the delta is the values-section decode.
//!
//! "Representative" data: a low-cardinality String column, the workload
//! dictionary encoding exists for — 2048 rows cycling over 32 distinct
//! 8-byte tags (`tag-0000`…`tag-0031`), no NULLs. The dictionary section is
//! 4 + 33×4 offsets + 256 heap + 2048×4 codes ≈ 8.6 KiB where the plain
//! section is 2049×4 offsets + 16 KiB heap ≈ 24 KiB, so the comparison
//! includes the page-read savings.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_storage::superblock::{COLUMN_ENCODINGS_FLAG, ZONE_MAPS_FLAG};
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-encoding!!";
const ROWS: usize = 2048;
const DISTINCT: usize = 32;

struct Fixture {
    _directory: tempfile::TempDir,
    pager: Pager,
    directory_page: u64,
}

/// Builds the 2048-row single-column String group, written plain or forced
/// to dictionary.
fn fixture(name: &str, forced: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let pager = Pager::create(directory.path().join(name), PAGE_SIZE, DB_ID).unwrap();
    let mut group = NodeGroup::new(vec![LogicalType::String]).unwrap();
    for row in 0..ROWS {
        group
            .push_row(vec![Value::String(format!("tag-{:04}", row % DISTINCT))])
            .unwrap();
    }
    let directory_page = if forced {
        group.write_forcing_encodings(&pager, &[(0, 4)]).unwrap()
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

fn decode_dictionary_vs_plain(c: &mut Criterion) {
    let plain = fixture("plain.devondb", false);
    let dictionary = fixture("dictionary.devondb", true);
    let mut group = c.benchmark_group("encoding_dictionary_2048_rows");
    group.bench_function("decode_plain", |bencher| {
        bencher.iter(|| decode_column_once(&plain));
    });
    group.bench_function("decode_dictionary", |bencher| {
        bencher.iter(|| decode_column_once(&dictionary));
    });
    group.finish();
}

criterion_group!(benches, decode_dictionary_vs_plain);
criterion_main!(benches);
