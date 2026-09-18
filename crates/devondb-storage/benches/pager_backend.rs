//! Criterion benchmark for the pager-backend dispatch shape described in
//! `docs/OBJECT_STORAGE.md`: local `read_page_ref` cache-hit and forced-miss
//! distributions. The cache-hit median should remain within 2% of its
//! baseline, allowing for benchmark noise.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use devondb_storage::budget::MemoryBudget;
use devondb_storage::pager::Pager;

const PAGE_SIZE: u32 = 4096;
const FRAME_OVERHEAD: usize = 64;
const PAGE_COUNT: u64 = 64;

fn pager_backend(c: &mut Criterion) {
    let directory = tempfile::tempdir().expect("create pager benchmark directory");
    let path = directory.path().join("bench.devondb");
    let pager =
        Pager::create(&path, PAGE_SIZE, [0x42; 16]).expect("create pager benchmark database");
    for index in 0..PAGE_COUNT {
        let page_id = pager.allocate_page().expect("allocate benchmark page");
        pager
            .write_page(page_id, &vec![index as u8; PAGE_SIZE as usize])
            .expect("write benchmark page");
    }
    pager.sync().expect("sync benchmark database");
    drop(pager);

    let budget = Arc::new(MemoryBudget::new(
        (PAGE_SIZE as usize + FRAME_OVERHEAD) * (PAGE_COUNT as usize + 8),
    ));
    let cached = Pager::open(&path)
        .expect("open cached pager")
        .with_budget(budget);
    // Warm the frame: every timed iteration below is a pure cache hit.
    drop(cached.read_page_ref(2).expect("warm cached frame"));
    c.bench_function("read_page_ref_cache_hit", |bencher| {
        bencher.iter(|| black_box(cached.read_page_ref(black_box(2)).expect("cached read")));
    });
    drop(cached);

    // No budget attached: every `read_page_ref` is a forced backend miss.
    let uncached = Pager::open(&path).expect("open uncached pager");
    c.bench_function("read_page_ref_forced_miss", |bencher| {
        bencher.iter(|| black_box(uncached.read_page_ref(black_box(2)).expect("uncached read")));
    });
}

criterion_group!(benches, pager_backend);
criterion_main!(benches);
