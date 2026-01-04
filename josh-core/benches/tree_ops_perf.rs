use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use josh_core::{cache, filter};
use josh_core::cache::{CacheStack, SledCacheBackend};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

// How to run:
// - Optimized (default-on `tree_perf_opt` + `tree_perf_opt_fast_digest`):
//   `cargo bench -p josh-core --features bench --bench tree_ops_perf`
// - Baseline (keep `tree_perf_opt` but disable `tree_perf_opt_fast_digest`):
//   `cargo bench -p josh-core --no-default-features --features fast_quote_if,pathset_builders,tree_perf_opt,bench --bench tree_ops_perf`
// - Baseline (disable `tree_perf_opt` entirely):
//   `cargo bench -p josh-core --no-default-features --features fast_quote_if,pathset_builders,bench --bench tree_ops_perf`

const N_COMMITS: usize = 100;

const COLD_FILES: usize = 6_000;
const WARM_FILES: usize = 400;
const HOT_FILES: usize = 200;

const HOT_CHANGES_PER_COMMIT: usize = 20;
const WARM_CHANGES_PER_WARM_COMMIT: usize = 20;

const PIN_BASE_SIZE: usize = 5_000;
const PIN_DELTA_PER_COMMIT: usize = 10;
const PIN_EXTRA_POOL: usize = 1_000;

const EXPECTED_COMPOSE_DIGEST_HEX: &str = "b5d4cdf69f2d8fe87aed47ac197c0bacb91f3b954e9efc736b456bd79170d8f4";
const EXPECTED_MASK_DIGEST_HEX: &str = "37c16202ba8ef04c14ec1b96f1920f0beab23c63cc95edbcbd4dcecd773e4ed9";
const EXPECTED_SUBTRACT_DIGEST_HEX: &str = "8eba01de125592730cf82a9e6bc1de786ce3717d33ff2bfa144fcb215c130b01";
const EXPECTED_INSERT_DIGEST_HEX: &str = "b7d0b595d2387497f524402210789485eec260ad5251c892e4cfa25ced0e992e";

struct RepoFixture {
    repo_gitdir: std::path::PathBuf,
    tree_ids: Vec<git2::Oid>,
    pin_sets: Vec<Vec<PathBuf>>,
    insert_path: PathBuf,
    cache: std::sync::Arc<CacheStack>,
}

fn fixture() -> &'static RepoFixture {
    static FIXTURE: OnceLock<RepoFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let tmp = Box::leak(Box::new(tmp));
        let workdir = tmp.path().join("repo");
        std::fs::create_dir_all(&workdir).expect("mkdirs");
        let repo = git2::Repository::init(&workdir).expect("init");

        let mut cold_paths = Vec::with_capacity(COLD_FILES);
        for i in 0..COLD_FILES {
            let dir = (i % 256) as u32;
            let path = PathBuf::from(format!("cold/dir{dir:03}/file_{i:06}.txt"));
            let abspath = workdir.join(&path);
            std::fs::create_dir_all(abspath.parent().unwrap()).unwrap();
            std::fs::write(&abspath, format!("cold init {i}\n")).unwrap();
            cold_paths.push(path);
        }

        let mut warm_paths = Vec::with_capacity(WARM_FILES);
        for i in 0..WARM_FILES {
            let dir = (i % 32) as u32;
            let path = PathBuf::from(format!("warm/dir{dir:02}/file_{i:05}.txt"));
            let abspath = workdir.join(&path);
            std::fs::create_dir_all(abspath.parent().unwrap()).unwrap();
            std::fs::write(&abspath, format!("warm init {i}\n")).unwrap();
            warm_paths.push(path);
        }

        let mut hot_paths = Vec::with_capacity(HOT_FILES);
        for i in 0..HOT_FILES {
            let path = PathBuf::from(format!("hot/file_{i:04}.txt"));
            let abspath = workdir.join(&path);
            std::fs::create_dir_all(abspath.parent().unwrap()).unwrap();
            std::fs::write(&abspath, format!("hot init {i}\n")).unwrap();
            hot_paths.push(path);
        }

        let insert_path = cold_paths
            .get(COLD_FILES / 2)
            .expect("cold_paths non-empty")
            .clone();

        let mut index = repo.index().expect("index");
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("add_all");
        let tree0 = index.write_tree().expect("write_tree");
        index.write().expect("index write");

        let mut tree_ids = Vec::with_capacity(N_COMMITS);
        let signature_time_base: i64 = 1_700_000_000;
        let sig0 = git2::Signature::new(
            "bench",
            "bench@example.com",
            &git2::Time::new(signature_time_base, 0),
        )
        .expect("signature");

        let commit0 = repo
            .commit(
                Some("HEAD"),
                &sig0,
                &sig0,
                "c0",
                &repo.find_tree(tree0).unwrap(),
                &[],
            )
            .unwrap();
        tree_ids.push(tree0);
        let mut parent = repo.find_commit(commit0).unwrap();

        for i in 1..N_COMMITS {
            let sig = git2::Signature::new(
                "bench",
                "bench@example.com",
                &git2::Time::new(signature_time_base + (i as i64), 0),
            )
            .expect("signature");

            let mut changed_paths = Vec::new();
            for j in 0..HOT_CHANGES_PER_COMMIT {
                let idx = (i * HOT_CHANGES_PER_COMMIT + j) % HOT_FILES;
                let path = &hot_paths[idx];
                std::fs::write(workdir.join(path), format!("hot {i} {idx}\n")).unwrap();
                changed_paths.push(path.clone());
            }

            if i % 10 == 0 {
                for j in 0..WARM_CHANGES_PER_WARM_COMMIT {
                    let idx = (i * WARM_CHANGES_PER_WARM_COMMIT + j) % WARM_FILES;
                    let path = &warm_paths[idx];
                    std::fs::write(workdir.join(path), format!("warm {i} {idx}\n")).unwrap();
                    changed_paths.push(path.clone());
                }
            }

            for p in &changed_paths {
                index.add_path(p).unwrap();
            }

            let tree_i = index.write_tree().expect("write_tree");
            index.write().expect("index write");

            let commit_i = repo
                .commit(
                    Some("HEAD"),
                    &sig,
                    &sig,
                    &format!("c{i}"),
                    &repo.find_tree(tree_i).unwrap(),
                    &[&parent],
                )
                .unwrap();
            tree_ids.push(tree_i);
            parent = repo.find_commit(commit_i).unwrap();
        }

        let mut base_pins = Vec::with_capacity(PIN_BASE_SIZE);
        base_pins.extend(cold_paths.iter().take(4_400).cloned());
        base_pins.extend(warm_paths.iter().take(400).cloned());
        base_pins.extend(hot_paths.iter().take(200).cloned());
        assert_eq!(base_pins.len(), PIN_BASE_SIZE);

        let extra_pool: Vec<PathBuf> = cold_paths
            .iter()
            .skip(4_400)
            .take(PIN_EXTRA_POOL)
            .cloned()
            .collect();
        assert_eq!(extra_pool.len(), PIN_EXTRA_POOL);

        let mut pin_sets = Vec::with_capacity(N_COMMITS);
        for i in 0..N_COMMITS {
            let mut pins = base_pins.clone();
            let start = (i * PIN_DELTA_PER_COMMIT) % extra_pool.len();
            for j in 0..PIN_DELTA_PER_COMMIT {
                pins[j] = extra_pool[(start + j) % extra_pool.len()].clone();
            }
            pin_sets.push(pins);
        }

        cache::sled_load(repo.path()).expect("sled_load");
        let cache = std::sync::Arc::new(
            CacheStack::new().with_backend(SledCacheBackend::default()),
        );

        RepoFixture {
            repo_gitdir: repo.path().to_path_buf(),
            tree_ids,
            pin_sets,
            insert_path,
            cache,
        }
    })
}

fn open_tx(
    repo_gitdir: &std::path::Path,
    cache: std::sync::Arc<CacheStack>,
) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache)
        .open(None)
        .expect("open tx")
}

fn digest_hex(h: Sha256) -> String {
    let out = h.finalize();
    hex::encode(out)
}

fn compute_compose_digest(tx: &cache::Transaction, tree_ids: &[git2::Oid], pin_sets: &[Vec<PathBuf>]) -> String {
    let repo = tx.repo();
    let mut hasher = Sha256::new();

    for (tree_id, pins) in tree_ids.iter().zip(pin_sets.iter()) {
        let full_tree = repo.find_tree(*tree_id).expect("find_tree");
        let out = filter::tree::compose_file_selections_no_remap(tx, &full_tree, pins)
            .expect("compose_file_selections_no_remap")
            .expect("expected fast-path Some(tree)");
        hasher.update(out.id().as_bytes());
    }

    digest_hex(hasher)
}

fn compute_mask_digest(tx: &cache::Transaction, pin_sets: &[Vec<PathBuf>]) -> String {
    let mut hasher = Sha256::new();
    for pins in pin_sets {
        let mask = filter::tree::mask_tree_from_paths(tx, pins).expect("mask_tree_from_paths");
        hasher.update(mask.as_bytes());
    }
    digest_hex(hasher)
}

fn compute_subtract_digest(
    tx: &cache::Transaction,
    tree_ids: &[git2::Oid],
    pin_sets: &[Vec<PathBuf>],
) -> String {
    let mut hasher = Sha256::new();
    // Use a stable mask across commits to isolate `subtract()` cost.
    let mask_paths = pin_sets.first().expect("pin_sets non-empty");
    let mask = filter::tree::mask_tree_from_paths(tx, mask_paths).expect("mask_tree_from_paths");
    for tree_id in tree_ids {
        let out = filter::tree::subtract(tx, *tree_id, mask).expect("subtract");
        hasher.update(out.as_bytes());
    }
    digest_hex(hasher)
}

fn compute_insert_digest(tx: &cache::Transaction, tree_ids: &[git2::Oid], insert_path: &PathBuf) -> String {
    let repo = tx.repo();
    let blob = repo.blob(b"bench insert").expect("blob");
    let mut hasher = Sha256::new();

    for tree_id in tree_ids {
        let base = repo.find_tree(*tree_id).expect("find_tree");
        let out = filter::tree::insert(
            repo,
            &base,
            insert_path,
            blob,
            0o0100644,
        )
        .expect("insert");
        hasher.update(out.id().as_bytes());
    }

    digest_hex(hasher)
}

fn bench_tree_ops(c: &mut Criterion) {
    let f = fixture();

    let mut group = c.benchmark_group("tree_ops_perf");
    // Criterion requires a minimum sample size of 10.
    group.sample_size(10);
    // Keep the default run time bounded; this bench is intentionally heavy.
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    // Golden correctness: verify digest once per bench invocation.
    {
        let tx = open_tx(&f.repo_gitdir, f.cache.clone());
        let actual = compute_compose_digest(&tx, &f.tree_ids, &f.pin_sets);
        assert_eq!(
            actual,
            EXPECTED_COMPOSE_DIGEST_HEX,
            "set EXPECTED_COMPOSE_DIGEST_HEX to {actual}"
        );
    }
    {
        let tx = open_tx(&f.repo_gitdir, f.cache.clone());
        let actual = compute_mask_digest(&tx, &f.pin_sets);
        assert_eq!(
            actual,
            EXPECTED_MASK_DIGEST_HEX,
            "set EXPECTED_MASK_DIGEST_HEX to {actual}"
        );
    }
    {
        let tx = open_tx(&f.repo_gitdir, f.cache.clone());
        let actual = compute_subtract_digest(&tx, &f.tree_ids, &f.pin_sets);
        assert_eq!(
            actual,
            EXPECTED_SUBTRACT_DIGEST_HEX,
            "set EXPECTED_SUBTRACT_DIGEST_HEX to {actual}"
        );
    }
    {
        let tx = open_tx(&f.repo_gitdir, f.cache.clone());
        let actual = compute_insert_digest(&tx, &f.tree_ids, &f.insert_path);
        assert_eq!(
            actual,
            EXPECTED_INSERT_DIGEST_HEX,
            "set EXPECTED_INSERT_DIGEST_HEX to {actual}"
        );
    }

    group.bench_function(BenchmarkId::new("compose_file_selections_no_remap", PIN_BASE_SIZE), |b| {
        b.iter_batched(
            || open_tx(&f.repo_gitdir, f.cache.clone()),
            |tx| {
                let digest = compute_compose_digest(&tx, &f.tree_ids, &f.pin_sets);
                std::hint::black_box(digest);
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function(BenchmarkId::new("mask_tree_from_paths", PIN_BASE_SIZE), |b| {
        b.iter_batched(
            || open_tx(&f.repo_gitdir, f.cache.clone()),
            |tx| {
                let digest = compute_mask_digest(&tx, &f.pin_sets);
                std::hint::black_box(digest);
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function(BenchmarkId::new("subtract", PIN_BASE_SIZE), |b| {
        b.iter_batched(
            || open_tx(&f.repo_gitdir, f.cache.clone()),
            |tx| {
                let digest = compute_subtract_digest(&tx, &f.tree_ids, &f.pin_sets);
                std::hint::black_box(digest);
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function(BenchmarkId::new("tree_insert_replace_child", PIN_BASE_SIZE), |b| {
        b.iter_batched(
            || open_tx(&f.repo_gitdir, f.cache.clone()),
            |tx| {
                let digest = compute_insert_digest(&tx, &f.tree_ids, &f.insert_path);
                std::hint::black_box(digest);
            },
            BatchSize::LargeInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_tree_ops);
criterion_main!(benches);
