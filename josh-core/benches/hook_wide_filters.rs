use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use josh_core::{cache, cache_sled, cache_stack, filter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

struct RepoFixture {
    repo_gitdir: PathBuf,
    cache: Arc<cache_stack::CacheStack>,
}

fn fixture() -> &'static RepoFixture {
    static FIXTURE: OnceLock<RepoFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // Leak the tempdir so it lives for the entire bench process.
        let tmp = Box::leak(Box::new(tmp));

        let workdir = tmp.path().join("repo");
        std::fs::create_dir_all(&workdir).expect("mkdirs");
        let repo = git2::Repository::init(&workdir).expect("init repo");

        // Create a linear history where each commit updates a single file. This keeps the tree
        // small so the bench primarily reflects hook/filter parsing and optimization overhead.
        let sig = git2::Signature::now("bench", "bench@example.com").unwrap();

        let mut parent: Option<git2::Commit> = None;
        let commit_count = std::env::var("JOSH_HOOK_BENCH_COMMITS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(25);

        for i in 0..commit_count {
            std::fs::write(workdir.join("included.txt"), format!("v{i}\n")).expect("write file");
            let mut index = repo.index().expect("index");
            index
                .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
                .unwrap();
            let tree_id = index.write_tree().expect("write tree");
            index.write().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();

            let oid = match parent.as_ref() {
                None => repo
                    .commit(Some("HEAD"), &sig, &sig, &format!("c{i}"), &tree, &[])
                    .unwrap(),
                Some(p) => repo
                    .commit(Some("HEAD"), &sig, &sig, &format!("c{i}"), &tree, &[p])
                    .unwrap(),
            };
            parent = Some(repo.find_commit(oid).unwrap());
        }

        let _tip = parent.expect("tip commit").id();
        let repo_gitdir = repo.path().to_path_buf();

        // Josh transaction requires sled DB init.
        cache_sled::sled_load(&repo_gitdir).expect("sled_load");
        let cache = Arc::new(
            cache_stack::CacheStack::new().with_backend(cache_sled::SledCacheBackend::default()),
        );

        RepoFixture { repo_gitdir, cache }
    })
}

fn open_tx(repo_gitdir: &Path, cache: Arc<cache_stack::CacheStack>) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache)
        .open(None)
        .expect("open tx")
}

fn wide_sizes() -> Vec<usize> {
    let mut sizes = vec![5_000usize];
    if std::env::var_os("JOSH_BENCH_BIG").is_some() {
        sizes.push(50_000usize);
    }
    sizes
}

fn make_many_exclude_spec(n: usize) -> String {
    let mut spec = String::from(":exclude[");
    for i in 0..n {
        let dir = (i % 1024) as u32;
        let path = format!("dir{dir:04}/file_{i:08}.txt");
        if i != 0 {
            spec.push(',');
        }
        spec.push_str("::");
        // Keep the spec generator simple; Josh itself will invoke its quoting logic when
        // serializing filter specs (a major hot path for wide excludes).
        spec.push_str(&path);
    }
    spec.push(']');
    spec
}

struct ParsingHook {
    spec: Arc<String>,
}

impl cache::FilterHook for ParsingHook {
    fn filter_for_commit(
        &self,
        _commit_oid: git2::Oid,
        _arg: &str,
    ) -> josh_core::JoshResult<filter::Filter> {
        // Emulate Copyberry2-style behavior: build a filter per commit from a (very large) textual spec
        // and run the optimizer before returning.
        let parsed = filter::parse(&self.spec)?;
        Ok(filter::optimize(parsed))
    }
}

struct CachedHook {
    filter: filter::Filter,
}

impl cache::FilterHook for CachedHook {
    fn filter_for_commit(
        &self,
        _commit_oid: git2::Oid,
        _arg: &str,
    ) -> josh_core::JoshResult<filter::Filter> {
        Ok(self.filter)
    }
}

fn calls_per_iter() -> usize {
    std::env::var("JOSH_HOOK_BENCH_CALLS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5)
}

fn bench_hook_lookup_filter(c: &mut Criterion) {
    // This benchmark is meant to reproduce pathological behavior and can be slow for large
    // excludes. Keep it opt-in so `cargo bench` stays usable by default.
    if std::env::var_os("JOSH_ENABLE_HOOK_WIDE_BENCH").is_none() {
        eprintln!("hook_wide_filters: set JOSH_ENABLE_HOOK_WIDE_BENCH=1 to run");
        return;
    }

    let f = fixture();
    let mut group = c.benchmark_group("hook_wide_filters/lookup_filter_hook");

    let sizes = wide_sizes();
    group.sample_size(10);

    // Use a monotonically changing hook name per batch to prevent transaction caches from
    // reusing results across benchmark iterations (cache keys include the hook filter id).
    let mut hook_seq: u64 = 0;
    let calls = calls_per_iter();

    for &n in sizes.iter() {
        let spec = Arc::new(make_many_exclude_spec(n));
        let cached_filter =
            filter::optimize(filter::parse(&spec).expect("parse cached wide exclude"));

        group.bench_function(BenchmarkId::new("parse_opt_each_call", n), |b| {
            b.iter_batched(
                || {
                    hook_seq = hook_seq.wrapping_add(1);
                    let hook_name = format!("wide_{hook_seq}");
                    let tx = open_tx(&f.repo_gitdir, f.cache.clone())
                        .with_filter_hook(Arc::new(ParsingHook { spec: spec.clone() }));
                    (tx, hook_name)
                },
                |(tx, hook_name)| {
                    for _ in 0..calls {
                        // The hook implementation ignores the commit oid, but use distinct values
                        // anyway to match the call pattern of real filtering runs.
                        let oid =
                            git2::Oid::hash_object(git2::ObjectType::Blob, hook_name.as_bytes())
                                .expect("hash oid");
                        let out = tx
                            .lookup_filter_hook(&hook_name, oid)
                            .expect("lookup_filter_hook");
                        std::hint::black_box(out);
                    }
                },
                BatchSize::SmallInput,
            )
        });

        group.bench_function(BenchmarkId::new("cached_filter", n), |b| {
            b.iter_batched(
                || {
                    hook_seq = hook_seq.wrapping_add(1);
                    let hook_name = format!("wide_{hook_seq}");
                    let tx = open_tx(&f.repo_gitdir, f.cache.clone()).with_filter_hook(Arc::new(
                        CachedHook {
                            filter: cached_filter,
                        },
                    ));
                    (tx, hook_name)
                },
                |(tx, hook_name)| {
                    for _ in 0..calls {
                        let oid =
                            git2::Oid::hash_object(git2::ObjectType::Blob, hook_name.as_bytes())
                                .expect("hash oid");
                        let out = tx
                            .lookup_filter_hook(&hook_name, oid)
                            .expect("lookup_filter_hook");
                        std::hint::black_box(out);
                    }
                },
                BatchSize::SmallInput,
            )
        });
    }

    group.finish();
}

criterion_group!(benches, bench_hook_lookup_filter);
criterion_main!(benches);
