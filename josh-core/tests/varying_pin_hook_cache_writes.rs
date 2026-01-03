use josh_core::{cache, filter};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Default)]
struct CountingInner {
    writes: AtomicU64,
}

#[derive(Clone, Default)]
struct CountingCacheBackend {
    inner: Arc<CountingInner>,
}

impl CountingCacheBackend {
    fn count(&self) -> u64 {
        self.inner.writes.load(Ordering::Relaxed)
    }
}

impl cache::CacheBackend for CountingCacheBackend {
    fn read(
        &self,
        _filter: filter::Filter,
        _from: git2::Oid,
        _sequence_number: u128,
    ) -> josh_core::JoshResult<Option<git2::Oid>> {
        Ok(None)
    }

    fn write(
        &self,
        _filter: filter::Filter,
        _from: git2::Oid,
        _to: git2::Oid,
        _sequence_number: u128,
    ) -> josh_core::JoshResult<()> {
        self.inner.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn write_commit(repo: &git2::Repository, message: &str, parents: &[&git2::Commit<'_>]) -> git2::Oid {
    let sig = repo
        .signature()
        .unwrap_or_else(|_| git2::Signature::now("test", "test@example.com").expect("sig"));
    let tree_id = {
        let mut index = repo.index().expect("index");
        index.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_id).expect("tree");
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, parents)
        .expect("commit")
}

fn init_linear_repo(commit_count: usize) -> (tempfile::TempDir, PathBuf, Vec<git2::Oid>) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = git2::Repository::init(tmp.path()).expect("init repo");

    let mut commits = Vec::with_capacity(commit_count);
    let mut parent: Option<git2::Commit<'_>> = None;
    for i in 0..commit_count {
        let oid = match &parent {
            Some(p) => write_commit(&repo, &format!("c{i}"), &[p]),
            None => write_commit(&repo, &format!("c{i}"), &[]),
        };
        commits.push(oid);
        parent = Some(repo.find_commit(oid).expect("commit"));
    }

    (tmp, repo.path().to_path_buf(), commits)
}

fn make_paths(n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let dir = (i % 256) as u32;
            PathBuf::from(format!("dir{dir:04}/file_{i:06}.txt"))
        })
        .collect()
}

struct StaticPinHook {
    filter: filter::Filter,
}

impl cache::FilterHook for StaticPinHook {
    fn filter_for_commit(
        &self,
        _commit_oid: git2::Oid,
        _arg: &str,
    ) -> josh_core::JoshResult<filter::Filter> {
        Ok(self.filter)
    }
}

struct VaryingPinHook {
    by_commit: HashMap<git2::Oid, filter::Filter>,
}

impl cache::FilterHook for VaryingPinHook {
    fn filter_for_commit(
        &self,
        commit_oid: git2::Oid,
        _arg: &str,
    ) -> josh_core::JoshResult<filter::Filter> {
        self.by_commit
            .get(&commit_oid)
            .copied()
            .ok_or_else(|| josh_core::josh_error("missing commit filter"))
    }
}

fn open_tx(repo_gitdir: &Path, cache_stack: Arc<cache::CacheStack>) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache_stack)
        .open(None)
        .expect("open tx")
}

fn ensure_sled_loaded() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // Josh's internal path/invert/trigram caches use a single global sled DB per process.
        // In test runs, repeatedly calling `sled_load` for different repos will fight over
        // file locks; initialize once with a dedicated temp directory.
        let tmp = tempfile::TempDir::new().expect("sled tempdir");
        let path = tmp.keep();
        cache::sled_load(&path).expect("sled_load");
    });
}

fn run_apply_and_count(
    repo_gitdir: &Path,
    tip: git2::Oid,
    hook: Arc<dyn cache::FilterHook + Send + Sync>,
) -> u64 {
    ensure_sled_loaded();
    let counting = CountingCacheBackend::default();
    let cache_stack = Arc::new(cache::CacheStack::new().with_backend(counting.clone()));
    let tx = open_tx(repo_gitdir, cache_stack).with_filter_hook(hook);
    let commit = tx.repo().find_commit(tip).expect("tip commit");
    let hook_filter = filter::hook("varypin");
    let _ = filter::apply_to_commit(hook_filter, &commit, &tx).expect("apply_to_commit");
    counting.count()
}

fn parse_sizes_env(var: &str, default: &[usize]) -> Vec<usize> {
    let raw = std::env::var(var).ok();
    let mut sizes = Vec::new();
    if let Some(raw) = raw {
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Ok(v) = part.parse::<usize>() {
                if v > 0 {
                    sizes.push(v);
                }
            }
        }
    }
    if sizes.is_empty() {
        sizes = default.to_vec();
    }
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

fn parse_usize_env(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

#[test]
fn varying_pin_hook_does_not_explode_cache_writes() {
    // This is intentionally small to keep the test fast while still exposing
    // pathological O(N * distinct_filters) behavior if present.
    // Keep small enough for `cargo test` while still exercising the per-commit
    // varying-hook path.
    let commit_count = 80;
    let max_pins = 40;
    let (_tmp, repo_gitdir, commits) = init_linear_repo(commit_count);
    let tip = *commits.last().expect("tip");

    let paths = make_paths(max_pins);

    // Static hook: constant pin filter for all commits.
    let static_pins = filter::pin_paths(paths.iter().cloned());
    let static_filter = filter::Filter::new()
        .chain(static_pins)
        .chain(filter::sequence_number());
    let static_hook: Arc<dyn cache::FilterHook + Send + Sync> =
        Arc::new(StaticPinHook { filter: static_filter });

    // Varying hook: pin set grows monotonically by 1 per commit (mimics dag-union behavior).
    let mut by_commit = HashMap::with_capacity(commits.len());
    for (idx, oid) in commits.iter().enumerate() {
        let pin_len = std::cmp::min(max_pins, 1 + idx / 2);
        let pins = filter::pin_paths(paths[..pin_len].iter().cloned());
        let f = filter::Filter::new().chain(pins).chain(filter::sequence_number());
        by_commit.insert(*oid, f);
    }
    let varying_hook: Arc<dyn cache::FilterHook + Send + Sync> =
        Arc::new(VaryingPinHook { by_commit });

    let static_writes = run_apply_and_count(&repo_gitdir, tip, static_hook);
    let varying_writes = run_apply_and_count(&repo_gitdir, tip, varying_hook);

    eprintln!("static_writes={static_writes} varying_writes={varying_writes}");

    // Regression guard: varying per-commit pins should not cause orders-of-magnitude more writes.
    // This threshold is intentionally generous; tighten once we have stable numbers post-fix.
    assert!(
        varying_writes <= static_writes.saturating_mul(10),
        "cache write explosion: varying={varying_writes} static={static_writes}"
    );
}

#[test]
#[ignore]
fn varying_pin_hook_scaling_one_shot() {
    // Manual perf probe for Copyberry2-like "dag union" per-commit pin growth.
    //
    // Run:
    //   cd third-party/josh/josh-core
    //   JOSH_VARY_PIN_SCALING_FILES=5000,10000,15000 JOSH_VARY_PIN_SCALING_COMMITS=80 \
    //     cargo test -q --test varying_pin_hook_cache_writes -- --ignored --nocapture
    ensure_sled_loaded();
    let sizes = parse_sizes_env(
        "JOSH_VARY_PIN_SCALING_FILES",
        &[5_000usize, 10_000usize, 15_000usize],
    );
    let commit_count = parse_usize_env("JOSH_VARY_PIN_SCALING_COMMITS", 80);

    for &max_pins in &sizes {
        let (_tmp, repo_gitdir, commits) = init_linear_repo(commit_count);
        let tip = *commits.last().expect("tip");

        let paths = make_paths(max_pins);

        // Map commit oid -> index.
        let mut oid_to_idx = HashMap::with_capacity(commits.len());
        for (idx, oid) in commits.iter().enumerate() {
            oid_to_idx.insert(*oid, idx);
        }

        // Precompute per-commit filters so the callback itself doesn't dominate.
        let precompute_start = Instant::now();
        let mut filters_by_idx = Vec::with_capacity(commits.len());
        for idx in 0..commits.len() {
            let pin_len = std::cmp::min(max_pins, 1 + (idx * max_pins) / commits.len().max(1));
            let pins = filter::pin_paths(paths[..pin_len].iter().cloned());
            filters_by_idx.push(filter::Filter::new().chain(pins).chain(filter::sequence_number()));
        }
        let precompute_elapsed = precompute_start.elapsed();

        struct PrecomputedDagUnionHook {
            oid_to_idx: HashMap<git2::Oid, usize>,
            filters_by_idx: Vec<filter::Filter>,
        }

        impl cache::FilterHook for PrecomputedDagUnionHook {
            fn filter_for_commit(
                &self,
                commit_oid: git2::Oid,
                _arg: &str,
            ) -> josh_core::JoshResult<filter::Filter> {
                let idx = *self
                    .oid_to_idx
                    .get(&commit_oid)
                    .ok_or_else(|| josh_core::josh_error("missing commit idx"))?;
                Ok(self.filters_by_idx[idx])
            }
        }

        let hook: Arc<dyn cache::FilterHook + Send + Sync> = Arc::new(PrecomputedDagUnionHook {
            oid_to_idx,
            filters_by_idx,
        });

        let counting = CountingCacheBackend::default();
        let notes = cache::NotesCacheBackend::new(&repo_gitdir).expect("notes backend");
        let cache_stack = Arc::new(
            cache::CacheStack::new()
                .with_backend(counting.clone())
                .with_backend(notes),
        );
        let tx = open_tx(&repo_gitdir, cache_stack).with_filter_hook(hook);
        let commit = tx.repo().find_commit(tip).expect("tip commit");
        let hook_filter = filter::hook("varypin");

        let start = Instant::now();
        let _ = filter::apply_to_commit(hook_filter, &commit, &tx).expect("apply_to_commit");
        let apply_elapsed = start.elapsed();
        let writes = counting.count();

        eprintln!(
            "varying_pin_scaling max_pins={} commits={} precompute_ms={} apply_ms={} cache_writes={}",
            max_pins,
            commit_count,
            precompute_elapsed.as_millis(),
            apply_elapsed.as_millis(),
            writes
        );
    }
}
