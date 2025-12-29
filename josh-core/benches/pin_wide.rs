use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use josh_core::{cache, cache_sled, cache_stack, filter};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

struct RepoFixture {
    repo_gitdir: std::path::PathBuf,
    tip: git2::Oid,
    cache: std::sync::Arc<cache_stack::CacheStack>,
}

fn fixture() -> &'static RepoFixture {
    static FIXTURE: OnceLock<RepoFixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let tmp = Box::leak(Box::new(tmp));
        let workdir = tmp.path().join("repo");
        std::fs::create_dir_all(&workdir).expect("mkdirs");
        let repo = git2::Repository::init(&workdir).expect("init");

        // Create a 2-commit history so pinning has a parent to pull from.
        let mut paths_5000 = Vec::with_capacity(5_000);
        for i in 0..5_000usize {
            let dir = (i % 256) as u32;
            let p = PathBuf::from(format!("dir{dir:03}/file_{i:06}.txt"));
            let abspath = workdir.join(&p);
            std::fs::create_dir_all(abspath.parent().unwrap()).unwrap();
            std::fs::write(&abspath, format!("v1 {i}\n")).unwrap();
            paths_5000.push(p);
        }

        // Build pin spec and write stored file pin.josh
        let mut pin_spec = String::from(":pin[");
        for (i, p) in paths_5000.iter().enumerate() {
            if i != 0 {
                pin_spec.push(',');
            }
            pin_spec.push_str("::");
            pin_spec.push_str(&p.to_string_lossy());
        }
        pin_spec.push(']');
        std::fs::write(workdir.join("pin.josh"), &pin_spec).unwrap();

        // Commit 1
        let mut index = repo.index().unwrap();
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree1 = index.write_tree().unwrap();
        index.write().unwrap();
        let sig = git2::Signature::now("pin", "pin@example.com").unwrap();
        let commit1 = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "c1",
                &repo.find_tree(tree1).unwrap(),
                &[],
            )
            .unwrap();

        // Modify the files
        for (i, p) in paths_5000.iter().enumerate() {
            std::fs::write(workdir.join(p), format!("v2 {i}\n")).unwrap();
        }

        // Commit 2
        index
            .add_all(["."].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree2 = index.write_tree().unwrap();
        index.write().unwrap();
        let commit2 = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "c2",
                &repo.find_tree(tree2).unwrap(),
                &[&repo.find_commit(commit1).unwrap()],
            )
            .unwrap();

        cache_sled::sled_load(&repo.path().to_path_buf()).expect("sled_load");
        let cache = std::sync::Arc::new(
            cache_stack::CacheStack::new().with_backend(cache_sled::SledCacheBackend::default()),
        );

        RepoFixture {
            repo_gitdir: repo.path().to_path_buf(),
            tip: commit2,
            cache,
        }
    })
}

fn open_tx(
    repo_gitdir: &std::path::Path,
    cache: std::sync::Arc<cache_stack::CacheStack>,
) -> cache::Transaction {
    cache::TransactionContext::new(repo_gitdir, cache)
        .open(None)
        .expect("open tx")
}

fn bench_pin_apply(c: &mut Criterion) {
    let f = fixture();
    let mut group = c.benchmark_group("pin_wide/apply");

    let use_mempack = std::env::var_os("JOSH_ENABLE_MEMPACK").is_some();

    // Stored filter: read from pin.josh via Op::Stored
    let stored_filter = filter::parse(":+pin").expect("parse stored pin");

    group.sample_size(10);

    // Apply to tip commit.
    group.bench_function(BenchmarkId::from_parameter("pin_5000"), |b| {
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

                let commit = repo.find_commit(f.tip).expect("commit");
                let out = filter::apply(
                    &tx,
                    stored_filter,
                    filter::Apply::from_commit(&commit).unwrap(),
                )
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
    });

    group.finish();
}

criterion_group!(benches, bench_pin_apply);
criterion_main!(benches);
