use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use josh_core::filter;

fn make_file_filters(n: usize, seed: u32) -> Vec<filter::Filter> {
    (0..n)
        .map(|i| {
            // Create many distinct, mostly non-overlapping leaf paths.
            // `seed` lets the caller force a different filter set without parsing strings.
            let dir = (i % 1024) as u32;
            let path = format!("dir{dir:04}/file_{seed:08}_{i:08}.txt");
            filter::file(path)
        })
        .collect()
}

fn bench_prefix_sort(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter::prefix_sort");
    group.sample_size(10);

    // Keep the default sizes small enough to run quickly on CI/dev machines.
    // Opt-in to the very large case via env var to avoid accidental multi-minute benches.
    let mut sizes = vec![100usize, 1_000, 5_000];
    if std::env::var_os("JOSH_BENCH_BIG").is_some() {
        sizes.push(10_000);
        sizes.push(40_000);
    }

    for &n in &sizes {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || make_file_filters(n, 0),
                |filters| std::hint::black_box(filter::prefix_sort(&filters)),
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_prefix_sort);
criterion_main!(benches);
