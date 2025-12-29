use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use josh_core::{cache, cache_sled, cache_stack, filter};
use rs_tracing::trace_state_change;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

struct RepoFixture {
    repo_gitdir: PathBuf,
    commit: git2::Oid,
    cache: std::sync::Arc<cache_stack::CacheStack>,
    paths_1000: Vec<PathBuf>,
    paths_5000: Vec<PathBuf>,
}

fn fixture() -> &'static RepoFixture {
    static FIXTURE: OnceLock<RepoFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // Leak the tempdir so it lives for the entire bench process.
        let tmp = Box::leak(Box::new(tmp));

        let workdir = tmp.path().join("repo");
        std::fs::create_dir_all(&workdir).expect("create workdir");
        let repo = git2::Repository::init(&workdir).expect("init repo");

        // Create a single commit containing a lot of files. This is intentionally a simple shape:
        // many leaf files spread across directories.
        let mut paths_5000 = Vec::with_capacity(5_000);
        for i in 0..5_000usize {
            let dir = (i % 256) as u32;
            let p = PathBuf::from(format!("dir{dir:03}/file_{i:06}.txt"));
            let abspath = workdir.join(&p);
            if let Some(parent) = abspath.parent() {
                std::fs::create_dir_all(parent).expect("mkdirs");
            }
            std::fs::write(&abspath, format!("content {i}\n")).expect("write file");
            paths_5000.push(p);
        }
        let paths_1000 = paths_5000[..1_000].to_vec();

        // Commit everything.
        let mut index = repo.index().expect("index");
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree_id = index.write_tree().expect("write tree");
        index.write().unwrap();

        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::now("bench", "bench@example.com").unwrap();
        let commit = repo
            .commit(Some("HEAD"), &sig, &sig, "bench commit", &tree, &[])
            .unwrap();

        let repo_gitdir = repo.path().to_path_buf();

        // Josh transaction requires sled DB init.
        cache_sled::sled_load(&repo_gitdir).expect("sled_load");
        let cache = std::sync::Arc::new(
            cache_stack::CacheStack::new().with_backend(cache_sled::SledCacheBackend::default()),
        );

        RepoFixture {
            repo_gitdir,
            commit,
            cache,
            paths_1000,
            paths_5000,
        }
    })
}

fn open_tx(
    repo_gitdir: &Path,
    cache: std::sync::Arc<cache_stack::CacheStack>,
) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache)
        .open(None)
        .expect("open tx")
}

fn make_exclude_spec(paths: &[PathBuf]) -> String {
    // Keep it URL-compatible: no whitespace/newlines.
    let mut s = String::from(":exclude[");
    for (i, p) in paths.iter().enumerate() {
        if i != 0 {
            s.push(',');
        }
        s.push_str("::");
        s.push_str(&p.to_string_lossy());
    }
    s.push(']');
    s
}

fn wide_sizes() -> Vec<usize> {
    let mut sizes = vec![1_000usize, 5_000usize];
    if std::env::var_os("JOSH_BENCH_BIG").is_some() {
        sizes.push(50_000usize);
    }
    sizes
}

fn mk_paths(n: usize, seed: u32) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let dir = (i % 256) as u32;
            PathBuf::from(format!("dir{dir:03}/file_{seed:08}_{i:06}.txt"))
        })
        .collect()
}

fn bench_parse_exclude(c: &mut Criterion) {
    let mut group = c.benchmark_group("wide_filters/parse");
    let sizes = wide_sizes();
    group.sample_size(10);

    for (idx, n) in sizes.iter().copied().enumerate() {
        let spec = make_exclude_spec(&mk_paths(n, idx as u32));
        group.bench_with_input(BenchmarkId::new("parse_exclude", n), &spec, |b, spec| {
            b.iter(|| std::hint::black_box(filter::parse(spec).expect("parse")));
        });
    }

    group.finish();
}

fn bench_optimize_exclude_and_pin(c: &mut Criterion) {
    let mut group = c.benchmark_group("wide_filters/optimize");
    let sizes = wide_sizes();
    group.sample_size(10);

    // Use distinct path sets per iteration to avoid OPTIMIZED-cache artifacts.
    let mut seed: u32 = 0;

    for &n in sizes.iter() {
        group.bench_with_input(BenchmarkId::new("optimize_exclude", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    seed = seed.wrapping_add(1);
                    filter::bench_exclude_files(&mk_paths(n, seed))
                },
                |filt| std::hint::black_box(filter::optimize(filt)),
                BatchSize::LargeInput,
            )
        });

        group.bench_with_input(BenchmarkId::new("optimize_pin", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    seed = seed.wrapping_add(1);
                    filter::bench_pin_files(&mk_paths(n, seed))
                },
                |filt| std::hint::black_box(filter::optimize(filt)),
                BatchSize::LargeInput,
            )
        });
    }

    group.finish();
}

#[cfg(feature = "pathset_builders")]
fn bench_build_pin_exclude(c: &mut Criterion) {
    let mut group = c.benchmark_group("wide_filters/build");
    let sizes = wide_sizes();
    group.sample_size(10);

    let mut seed: u32 = 150;
    for &n in sizes.iter() {
        group.bench_with_input(BenchmarkId::new("build_exclude_paths", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    seed = seed.wrapping_add(1);
                    mk_paths(n, seed)
                },
                |paths| std::hint::black_box(filter::exclude_paths(paths)),
                BatchSize::LargeInput,
            )
        });

        group.bench_with_input(BenchmarkId::new("build_pin_paths", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    seed = seed.wrapping_add(1);
                    mk_paths(n, seed)
                },
                |paths| std::hint::black_box(filter::pin_paths(paths)),
                BatchSize::LargeInput,
            )
        });
    }

    group.finish();
}

#[cfg(not(feature = "pathset_builders"))]
fn bench_build_pin_exclude(_c: &mut Criterion) {}

fn bench_spec_exclude(c: &mut Criterion) {
    let mut group = c.benchmark_group("wide_filters/spec");
    let sizes = wide_sizes();
    group.sample_size(10);

    let mut seed: u32 = 100;
    for &n in sizes.iter() {
        seed = seed.wrapping_add(1);
        let filter = filter::optimize(filter::bench_exclude_files(&mk_paths(n, seed)));
        group.bench_function(BenchmarkId::new("spec_exclude", n), move |b| {
            b.iter(|| std::hint::black_box(filter::spec(filter)))
        });
    }

    let mut seed: u32 = 200;
    for &n in sizes.iter() {
        let spec = make_exclude_spec(&mk_paths(n, seed));
        seed = seed.wrapping_add(1);
        group.bench_with_input(BenchmarkId::new("spec_roundtrip", n), &spec, |b, spec| {
            b.iter(|| {
                let parsed = filter::parse(spec).expect("parse");
                let optimized = filter::optimize(parsed);
                std::hint::black_box(filter::spec(optimized))
            })
        });
    }

    group.finish();
}

fn bench_apply_exclude(c: &mut Criterion) {
    let f = fixture();
    let mut group = c.benchmark_group("wide_filters/apply");
    let fallback = std::env::var_os("JOSH_DISABLE_EXCLUDE_FASTPATH").is_some();
    let use_mempack = std::env::var_os("JOSH_ENABLE_MEMPACK").is_some();

    // With fallback semantics, apply can be extremely slow. Keep the bench runnable by default by
    // taking only a few one-shot samples.
    if fallback {
        group.sample_size(3);
    } else {
        group.sample_size(10);
    }

    let mode = if fallback { "fallback" } else { "fastpath" };

    for (name, paths) in [("1000", &f.paths_1000), ("5000", &f.paths_5000)] {
        let exclude = filter::optimize(filter::bench_exclude_files(paths));

        group.bench_function(
            BenchmarkId::new(format!("apply_exclude_cold_tx_{mode}"), name),
            |b| {
                if fallback {
                    b.iter_custom(|_iters| {
                        let tx = open_tx(&f.repo_gitdir, f.cache.clone());
                        let repo = tx.repo();
                        let odb = repo.odb().expect("odb");
                        let mempack = if use_mempack {
                            odb.add_new_mempack_backend(1000).ok()
                        } else {
                            None
                        };

                        let commit = repo.find_commit(f.commit).expect("commit");
                        let tree = commit.tree().expect("tree");
                        let start = std::time::Instant::now();
                        let out = filter::apply(&tx, exclude, filter::Apply::from_tree(tree))
                            .expect("apply");
                        std::hint::black_box(out.tree().id());

                        if let Some(mempack) = mempack {
                            let mut buf = git2::Buf::new();
                            mempack.dump(repo, &mut buf).unwrap();
                            if buf.len() > 32 {
                                let mut w = odb.packwriter().unwrap();
                                w.write(&buf).unwrap();
                                w.commit().unwrap();
                            }
                        }
                        start.elapsed()
                    });
                } else {
                    b.iter_batched(
                        || open_tx(&f.repo_gitdir, f.cache.clone()),
                        |tx| {
                            let repo = tx.repo();
                            let odb = repo.odb().expect("odb");
                            let mempack = if use_mempack {
                                odb.add_new_mempack_backend(1000).ok()
                            } else {
                                None
                            };

                            let commit = repo.find_commit(f.commit).expect("commit");
                            let tree = commit.tree().expect("tree");
                            let out = filter::apply(&tx, exclude, filter::Apply::from_tree(tree))
                                .expect("apply");
                            std::hint::black_box(out.tree().id());

                            if let Some(mempack) = mempack {
                                let mut buf = git2::Buf::new();
                                mempack.dump(repo, &mut buf).unwrap();
                                if buf.len() > 32 {
                                    let mut w = odb.packwriter().unwrap();
                                    w.write(&buf).unwrap();
                                    w.commit().unwrap();
                                }
                            }
                        },
                        BatchSize::LargeInput,
                    )
                }
            },
        );
    }

    group.finish();
}

fn bench_trace_active_optimize_exclude(c: &mut Criterion) {
    if std::env::var_os("JOSH_BENCH_TRACE_ACTIVE").is_none() {
        return;
    }
    // Activate rs_tracing without opening a file so we isolate the cost of trace payload creation
    // (not I/O).
    rs_tracing::trace_activate!();

    let mut group = c.benchmark_group("wide_filters/trace_active");
    let sizes = wide_sizes();
    group.sample_size(10);

    let mut seed: u32 = 250;
    for &n in sizes.iter() {
        group.bench_with_input(
            BenchmarkId::new("optimize_exclude_trace_active", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        seed = seed.wrapping_add(1);
                        filter::bench_exclude_files(&mk_paths(n, seed))
                    },
                    |filt| std::hint::black_box(filter::optimize(filt)),
                    BatchSize::LargeInput,
                )
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_parse_exclude,
    bench_optimize_exclude_and_pin,
    bench_build_pin_exclude,
    bench_spec_exclude,
    bench_trace_active_optimize_exclude,
    bench_apply_exclude
);
criterion_main!(benches);
