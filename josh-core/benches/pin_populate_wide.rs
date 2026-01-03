use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use josh_core::cache;
use josh_core::filter::tree;
use std::path::PathBuf;
use std::sync::OnceLock;

struct RepoFixture {
    repo_gitdir: PathBuf,
    tree_id: git2::Oid,
    cache: std::sync::Arc<cache::CacheStack>,
}

fn file_count_default() -> usize {
    std::env::var("JOSH_PIN_POPULATE_FILES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5_000)
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

        let file_count = file_count_default();
        for i in 0..file_count {
            let dir = (i % 256) as u32;
            let p = PathBuf::from(format!("dir{dir:03}/file_{i:06}.txt"));
            let abspath = workdir.join(&p);
            std::fs::create_dir_all(abspath.parent().expect("parent")).expect("mkdirs");
            std::fs::write(&abspath, format!("v{i}\n")).expect("write file");
        }

        let mut index = repo.index().expect("index");
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .expect("index add_all");
        let tree_id = index.write_tree().expect("write tree");
        index.write().expect("index write");

        // Josh transaction requires sled DB init.
        let repo_gitdir = repo.path().to_path_buf();
        cache::sled_load(&repo_gitdir).expect("sled_load");
        let cache = std::sync::Arc::new(cache::CacheStack::default());

        RepoFixture {
            repo_gitdir,
            tree_id,
            cache,
        }
    })
}

fn open_tx(
    repo_gitdir: &std::path::Path,
    cache: std::sync::Arc<cache::CacheStack>,
) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache)
        .open(None)
        .expect("open tx")
}

fn bench_pin_populate_wide(c: &mut Criterion) {
    // This bench can be expensive; keep it opt-in so `cargo bench` is still usable.
    if std::env::var_os("JOSH_ENABLE_PIN_POPULATE_BENCH").is_none() {
        eprintln!("pin_populate_wide: set JOSH_ENABLE_PIN_POPULATE_BENCH=1 to run");
        return;
    }

    let f = fixture();
    let file_count = file_count_default();

    let mut group = c.benchmark_group("pin_wide/tree_build");
    group.sample_size(10);

    // Vary the pathstree root per iteration to avoid sled/global cache hits (keyed by (tree_id, root)).
    let mut root_seq: u64 = 0;

    group.bench_function(BenchmarkId::new("paths_tree", file_count), |b| {
        b.iter_batched(
            || {
                root_seq = root_seq.wrapping_add(1);
                let root = format!("bench_{root_seq}");
                let tx = open_tx(&f.repo_gitdir, f.cache.clone());
                (tx, root)
            },
            |(tx, root)| {
                let paths_tree = tree::pathstree(&root, f.tree_id, &tx).expect("pathstree");
                let out = tree::populate(&tx, paths_tree.id(), f.tree_id).expect("populate");
                std::hint::black_box(out);
            },
            BatchSize::LargeInput,
        )
    });

    group.bench_function(BenchmarkId::new("mask_tree", file_count), |b| {
        b.iter_batched(
            || open_tx(&f.repo_gitdir, f.cache.clone()),
            |tx| {
                let out =
                    tree::populate_from_mask_tree(&tx, f.tree_id, f.tree_id).expect("populate");
                std::hint::black_box(out);
            },
            BatchSize::LargeInput,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_pin_populate_wide);
criterion_main!(benches);
