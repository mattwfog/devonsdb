//! Compares write throughput with §8.4 adaptive selection and the
//! always-plain path.
//!
//! The fixture mirrors the §5.6 scan baseline's shape at node-group
//! granularity: 2048-row groups of `(id Int64, score Float64, name
//! String)` with `score = id / 10` and `name` drawn from the two-value
//! shuffled pattern (`benches/scan.rs` in the `devondb` crate holds the
//! query-level original, which lives outside this crate's dependency
//! direction). Both sides write the same group to a fresh pager per
//! iteration; the only delta is `NodeGroup::write` (selection) vs
//! `write_forcing_encodings(&[])` (always plain). Per §5.6's shape the
//! selections are: id → bitpack_for, score → alp, name → dictionary.

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::node_group::NodeGroup;
use devondb_storage::pager::Pager;
use devondb_types::logical_type::LogicalType;
use devondb_types::value::Value;

const PAGE_SIZE: u32 = 4096;
const DB_ID: [u8; 16] = *b"bench-select-246";
const ROWS: usize = 2048;
const SELECTIVE_ROW_COUNT: usize = 1_000;
const SHUFFLE_MULTIPLIER: usize = 7_919;
const FIXTURE_ROWS: usize = 100_000;

/// One 2048-row group of the §5.6 fixture shape.
fn fixture_group() -> NodeGroup {
    let types = vec![
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::String,
    ];
    let mut group = NodeGroup::new(types).unwrap();
    for row in 0..ROWS {
        let shuffled = (row * SHUFFLE_MULTIPLIER) % FIXTURE_ROWS;
        let name = if shuffled < SELECTIVE_ROW_COUNT {
            "selected"
        } else {
            "other"
        };
        group
            .push_row(vec![
                Value::Int64(row as i64),
                Value::Float64(row as f64 / 10.0),
                Value::String(name.to_owned()),
            ])
            .unwrap();
    }
    group
}

/// Writes the group to a fresh pager, with selection (`adaptive`) or
/// forced plain.
fn write_once(directory: &tempfile::TempDir, group: &NodeGroup, adaptive: bool) {
    let pager = Pager::create(directory.path().join("bench.devondb"), PAGE_SIZE, DB_ID).unwrap();
    let directory_page = if adaptive {
        group.write(&pager).unwrap()
    } else {
        group.write_forcing_encodings(&pager, &[]).unwrap()
    };
    criterion::black_box(directory_page);
}

fn selection_vs_plain(c: &mut Criterion) {
    let group = fixture_group();
    let mut bench = c.benchmark_group("encoding_selection_write_2048_rows");
    bench.bench_function("with_selection", |bencher| {
        bencher.iter_batched(
            || tempfile::tempdir().unwrap(),
            |directory| write_once(&directory, &group, true),
            criterion::BatchSize::SmallInput,
        );
    });
    bench.bench_function("always_plain", |bencher| {
        bencher.iter_batched(
            || tempfile::tempdir().unwrap(),
            |directory| write_once(&directory, &group, false),
            criterion::BatchSize::SmallInput,
        );
    });
    bench.finish();
}

criterion_group!(benches, selection_vs_plain);
criterion_main!(benches);
