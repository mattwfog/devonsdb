use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use devondb_exec::simd::{cosine_distance, l2_squared};

const DIMENSIONS: [usize; 2] = [64, 768];
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const LEFT_SEED: u64 = 0xd3_70_db_20_00;
const RIGHT_SEED: u64 = 0x12_34_56_78_9a_bc_de_f0;

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        let fraction = (self.0 >> 40) as f32 / (1_u32 << 24) as f32;
        2.0 * fraction - 1.0
    }

    fn vector(&mut self, dimension: usize) -> Vec<f32> {
        (0..dimension).map(|_| self.next_f32()).collect()
    }
}

fn benchmark_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("distance");
    for dimension in DIMENSIONS {
        let left = Lcg::new(LEFT_SEED).vector(dimension);
        let right = Lcg::new(RIGHT_SEED).vector(dimension);
        group.throughput(Throughput::Elements(dimension as u64));
        group.bench_function(BenchmarkId::new("l2_dispatched", dimension), |b| {
            b.iter(|| {
                black_box(l2_squared(
                    black_box(left.as_slice()),
                    black_box(right.as_slice()),
                ))
            });
        });
        group.bench_function(BenchmarkId::new("cosine_dispatched", dimension), |b| {
            b.iter(|| {
                black_box(cosine_distance(
                    black_box(left.as_slice()),
                    black_box(right.as_slice()),
                ))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, benchmark_distance);
criterion_main!(benches);
