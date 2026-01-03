#![cfg(feature = "bench")]

use josh_core::{cache, filter, history, josh_error};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

fn parse_sizes_env() -> Vec<usize> {
    let raw = std::env::var("JOSH_PIN_HISTORY_FILES").ok();
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
        sizes = vec![5_000, 10_000, 15_000];
    }
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

fn commit_count() -> usize {
    std::env::var("JOSH_PIN_HISTORY_COMMITS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(50)
}

fn make_paths(n: usize) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(n);
    for i in 0..n {
        let dir = (i % 1024) as u32;
        paths.push(PathBuf::from(format!("dir{dir:04}/file_{i:08}.txt")));
    }
    paths
}

fn init_repo(paths: &[PathBuf], commit_count: usize) -> (tempfile::TempDir, PathBuf, git2::Oid) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let workdir = tmp.path().join("repo");
    std::fs::create_dir_all(&workdir).expect("mkdirs");
    let repo = git2::Repository::init(&workdir).expect("init repo");

    // Seed all pinned files.
    for (i, p) in paths.iter().enumerate() {
        let abspath = workdir.join(p);
        std::fs::create_dir_all(abspath.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&abspath, format!("c0 {i}\n")).expect("write file");
    }

    let mut index = repo.index().expect("index");
    index
        .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("index add_all");
    let tree_id = index.write_tree().expect("write tree");
    index.write().expect("index write");
    let sig = git2::Signature::now("pin", "pin@example.com").expect("sig");

    let mut tip = repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "c0",
            &repo.find_tree(tree_id).expect("tree"),
            &[],
        )
        .expect("commit");
    let mut parent = repo.find_commit(tip).expect("commit");

    // Touch a rolling slice of files each commit to keep trees distinct without
    // rewriting every file.
    let chunk_size = std::cmp::max(1, paths.len() / commit_count.max(1));

    for commit_idx in 1..commit_count {
        let start = (commit_idx * chunk_size) % paths.len();
        for offset in 0..chunk_size {
            let idx = (start + offset) % paths.len();
            let abspath = workdir.join(&paths[idx]);
            std::fs::write(&abspath, format!("c{commit_idx} {idx}\n")).expect("write file");
        }

        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("index add_all");
        let tree_id = index.write_tree().expect("write tree");
        index.write().expect("index write");
        tip = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &format!("c{commit_idx}"),
                &repo.find_tree(tree_id).expect("tree"),
                &[&parent],
            )
            .expect("commit");
        parent = repo.find_commit(tip).expect("commit");
    }

    (tmp, repo.path().to_path_buf(), tip)
}

struct PinHistoryHook {
    filters: HashMap<usize, filter::Filter>,
}

impl PinHistoryHook {
    fn new(paths: Arc<Vec<PathBuf>>, sizes: &[usize]) -> Self {
        let mut filters = HashMap::new();
        for &size in sizes {
            let n = size.min(paths.len());
            if n == 0 {
                continue;
            }
            let filter = filter::bench_pin_files(&paths[..n]);
            filters.insert(n, filter);
        }
        PinHistoryHook { filters }
    }

    fn filter_for_size(&self, size: usize) -> josh_core::JoshResult<filter::Filter> {
        self.filters
            .get(&size)
            .copied()
            .ok_or_else(|| josh_error(&format!("missing precomputed pin filter for {size}")))
    }
}

fn parse_hook_size(arg: &str) -> Option<usize> {
    let rest = arg.strip_prefix("pin_hist_")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

impl cache::FilterHook for PinHistoryHook {
    fn filter_for_commit(
        &self,
        _commit_oid: git2::Oid,
        arg: &str,
    ) -> josh_core::JoshResult<filter::Filter> {
        let size = parse_hook_size(arg)
            .ok_or_else(|| josh_error(&format!("unrecognized hook name: {arg}")))?;
        self.filter_for_size(size)
    }
}

fn ensure_sled_loaded() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("sled tempdir");
        let path = tmp.keep();
        cache::sled_load(&path).expect("sled_load");
    });
}

fn open_tx(repo_gitdir: &Path, cache_stack: Arc<cache::CacheStack>) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache_stack)
        .open(None)
        .expect("open tx")
}

#[test]
#[ignore]
fn pin_history_scaling_one_shot() {
    // This is a manual perf probe (ignored by default). It prints rough timings
    // for the per-commit :pin history path across different pin set sizes.
    //
    // Run:
    //   cd third-party/josh/josh-core
    //   JOSH_PIN_HISTORY_FILES=5000,10000,15000 JOSH_PIN_HISTORY_COMMITS=50 \
    //     cargo test -q --test pin_history_scaling -- --ignored --nocapture
    ensure_sled_loaded();
    let sizes = parse_sizes_env();
    let commit_count = commit_count();
    let max_pins = *sizes.iter().max().unwrap_or(&0);
    let paths = Arc::new(make_paths(max_pins));

    let (_tmp, repo_gitdir, tip) = init_repo(&paths, commit_count);
    let hook = Arc::new(PinHistoryHook::new(paths.clone(), &sizes));

    // Use sled backend so repeated runs approximate production behavior.
    let cache = Arc::new(cache::CacheStack::new().with_backend(cache::SledCacheBackend::default()));

    println!("pin_history_scaling: commits={commit_count} max_pins={max_pins}");

    for size in sizes {
        let hook_name = format!("pin_hist_{size}");
        let tx = open_tx(&repo_gitdir, cache.clone()).with_filter_hook(hook.clone());
        let start = Instant::now();
        history::walk2(filter::hook(&hook_name), tip, &tx).expect("walk2");
        let elapsed = start.elapsed();
        println!("pins={size} walk2_elapsed_ms={}", elapsed.as_millis());
    }
}
