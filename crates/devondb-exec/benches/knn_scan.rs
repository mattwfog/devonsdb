use std::{collections::VecDeque, hint::black_box};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use devondb_exec::{
    chunk::{Chunk, ChunkBuilder},
    knn::KnnScan,
    source::ChunkSource,
};
use devondb_plan::expr::Metric;
use devondb_types::{DevonResult, logical_type::LogicalType, value::Value};

const N: usize = 2_000;
const DIM: usize = 64;
const K_VALUES: [u64; 3] = [1, 10, 100];
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const CORPUS_SEED: u64 = 0xd3_70_db_20_00;
const L2_QUERY_SEED: u64 = 0x12_34_56_78_9a_bc_de_f0;
const COSINE_QUERY_SEED: u64 = 0x0c_05_1e_20_26;
const ROW_ID_BASE: i64 = 10_000;
const VECTOR_COLUMN: usize = 1;

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

    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.next_f32()).collect()
    }
}

struct Corpus {
    chunk: Chunk,
    l2_query: Vec<f32>,
    cosine_query: Vec<f32>,
}

impl Corpus {
    fn generate() -> Self {
        let mut generator = Lcg::new(CORPUS_SEED);
        let mut builder = ChunkBuilder::new(vec![
            LogicalType::Int64,
            LogicalType::Vector { dim: DIM as u32 },
        ]);
        for row_order in 0..N {
            let row_order = i64::try_from(row_order)
                .unwrap_or_else(|error| panic!("corpus row index must fit i64: {error}"));
            if let Err(error) = builder.push_row(vec![
                Value::Int64(ROW_ID_BASE + row_order),
                Value::Vector(generator.vector()),
            ]) {
                panic!("fixed benchmark corpus must build: {error}");
            }
        }
        Self {
            chunk: builder.finish(),
            l2_query: Lcg::new(L2_QUERY_SEED).vector(),
            cosine_query: Lcg::new(COSINE_QUERY_SEED).vector(),
        }
    }

    fn query(&self, metric: Metric) -> &[f32] {
        match metric {
            Metric::L2 => &self.l2_query,
            Metric::Cosine => &self.cosine_query,
        }
    }
}

struct InMemorySource {
    chunks: VecDeque<Chunk>,
}

impl InMemorySource {
    fn new(chunk: Chunk) -> Self {
        Self {
            chunks: VecDeque::from([chunk]),
        }
    }
}

impl ChunkSource for InMemorySource {
    fn next_chunk(&mut self) -> DevonResult<Option<Chunk>> {
        Ok(self.chunks.pop_front())
    }
}

fn run_to_completion(mut scan: KnnScan) -> usize {
    let mut rows = 0;
    loop {
        match scan.next_chunk() {
            Ok(Some(chunk)) => rows += chunk.row_count(),
            Ok(None) => return rows,
            Err(error) => panic!("fixed benchmark scan must succeed: {error}"),
        }
    }
}

fn benchmark_knn_scan(c: &mut Criterion) {
    let corpus = Corpus::generate();
    let mut group = c.benchmark_group("knn_scan");
    group.sample_size(10);
    group.throughput(Throughput::Elements(N as u64));

    for metric in [Metric::L2, Metric::Cosine] {
        let metric_name = match metric {
            Metric::L2 => "l2",
            Metric::Cosine => "cosine",
        };
        let query = corpus.query(metric).to_vec();
        for k in K_VALUES {
            group.bench_with_input(BenchmarkId::new(metric_name, k), &k, |b, &k| {
                b.iter_batched(
                    || {
                        KnnScan::new(
                            Box::new(InMemorySource::new(corpus.chunk.clone())),
                            VECTOR_COLUMN,
                            query.clone(),
                            k,
                            metric,
                        )
                    },
                    |scan| black_box(run_to_completion(scan)),
                    BatchSize::PerIteration,
                );
            });
        }
    }
    group.finish();
}

criterion_group!(benches, benchmark_knn_scan);
criterion_main!(benches);
