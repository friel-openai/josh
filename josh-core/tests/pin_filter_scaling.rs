use josh_core::{cache, filter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn make_paths(n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let dir = (i % 1024) as u32;
            PathBuf::from(format!("dir{dir:04}/file_{i:06}.txt"))
        })
        .collect()
}

fn init_repo(paths: &[PathBuf], commit_count: usize) -> (tempfile::TempDir, PathBuf, git2::Oid) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let workdir = tmp.path().join("repo");
    std::fs::create_dir_all(&workdir).expect("mkdirs");
    let repo = git2::Repository::init(&workdir).expect("init repo");

    // Seed all files once; later commits touch a small slice to keep trees distinct.
    for (i, p) in paths.iter().enumerate() {
        let abspath = workdir.join(p);
        std::fs::create_dir_all(abspath.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&abspath, format!("c0 {i}\n")).expect("write file");
    }

    let sig = git2::Signature::now("pin", "pin@example.com").expect("sig");
    let mut index = repo.index().expect("index");
    index
        .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("index add_all");
    let tree_id = index.write_tree().expect("write tree");
    index.write().expect("index write");
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

fn ensure_sled_loaded() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // Josh's internal path/invert/trigram caches use a single global sled DB per process.
        // Point it at a dedicated temp dir so multiple repo fixtures don't fight over locks.
        let tmp = tempfile::TempDir::new().expect("sled tempdir");
        let path = tmp.keep();
        cache::sled_load(&path).expect("sled_load");
    });
}

fn open_tx(repo_gitdir: &Path) -> cache::Transaction {
    // Use a persistent backend so scaling reflects Josh's intended cache behavior rather than
    // pathological "always miss" recomputation.
    let cache_stack =
        Arc::new(cache::CacheStack::new().with_backend(cache::SledCacheBackend::default()));
    cache::TransactionContext::new(repo_gitdir, cache_stack)
        .open(None)
        .expect("open tx")
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

fn pretty_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    format!("{ms:.1}ms")
}

#[test]
#[ignore]
fn pin_filter_scaling() {
    // Run with:
    //   cargo test -q --test pin_filter_scaling -- --ignored --nocapture
    //
    // Optional env overrides:
    //   JOSH_PIN_SCALING_FILES=5000,10000,15000
    //   JOSH_PIN_SCALING_COMMITS=25
    let sizes: Vec<usize> = std::env::var("JOSH_PIN_SCALING_FILES")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .filter(|v| *v > 0)
                .collect()
        })
        .filter(|v: &Vec<usize>| !v.is_empty())
        .unwrap_or_else(|| vec![5_000, 10_000, 15_000]);
    let commit_count = std::env::var("JOSH_PIN_SCALING_COMMITS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(25);

    ensure_sled_loaded();

    for &size in &sizes {
        let n = size;
        let paths = make_paths(n);
        let (_tmp, repo_gitdir, tip) = init_repo(&paths, commit_count);
        let f = filter::Filter::new()
            .chain(filter::pin_paths(paths.iter().cloned()))
            .chain(filter::sequence_number());

        let tx = open_tx(&repo_gitdir);
        let commit = tx.repo().find_commit(tip).expect("tip commit");

        let start = Instant::now();
        let _ = filter::apply_to_commit(f, &commit, &tx).expect("apply_to_commit");
        let elapsed = start.elapsed();

        eprintln!(
            "pin_filter_scaling size={} commits={} elapsed={}",
            n,
            commit_count,
            pretty_duration(elapsed)
        );

        // Compare with the :hook path (mimics Copyberry2's ComputedResolver calling Josh hooks).
        drop(commit);
        let tx_hook = tx.with_filter_hook(Arc::new(StaticPinHook { filter: f }));
        let commit = tx_hook.repo().find_commit(tip).expect("tip commit");
        let hook_filter = filter::hook("varypin");
        let start = Instant::now();
        let _ = filter::apply_to_commit(hook_filter, &commit, &tx_hook).expect("apply_to_commit");
        let elapsed = start.elapsed();

        eprintln!(
            "pin_filter_scaling_hook size={} commits={} elapsed={}",
            n,
            commit_count,
            pretty_duration(elapsed)
        );
    }
}
