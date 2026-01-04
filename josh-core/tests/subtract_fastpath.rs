use std::path::Path;
use std::sync::Arc;

use git2::Repository;
use josh_core::cache::{sled_load, CacheStack, TransactionContext};
use josh_core::filter;

fn create_repo_with_files() -> (tempfile::TempDir, Repository, git2::Oid) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = Repository::init(tmp.path()).expect("init repo");
    let sig = git2::Signature::now("test", "test@example.com").expect("signature");

    std::fs::write(tmp.path().join("a.txt"), "a").expect("write a");
    std::fs::write(tmp.path().join("b.txt"), "b").expect("write b");
    std::fs::create_dir_all(tmp.path().join("dir/sub")).expect("create dirs");
    std::fs::write(tmp.path().join("dir/x.txt"), "x").expect("write x");
    std::fs::write(tmp.path().join("dir/y.txt"), "y").expect("write y");
    std::fs::write(tmp.path().join("dir/sub/z.txt"), "z").expect("write z");

    {
        let mut idx = repo.index().expect("index");
        idx.add_path(Path::new("a.txt")).expect("add a");
        idx.add_path(Path::new("b.txt")).expect("add b");
        idx.add_path(Path::new("dir/x.txt")).expect("add x");
        idx.add_path(Path::new("dir/y.txt")).expect("add y");
        idx.add_path(Path::new("dir/sub/z.txt")).expect("add z");
        idx.write().expect("write index");
    }

    let tree_oid = {
        let mut idx = repo.index().expect("index");
        idx.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_oid).expect("find tree");
    let commit = repo
        .commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .expect("commit");
    drop(tree);

    (tmp, repo, commit)
}

#[test]
fn subtract_removes_files_and_directories() {
    let (_tmp, repo, commit_oid) = create_repo_with_files();
    sled_load(repo.path()).expect("sled_load");

    let cache = Arc::new(CacheStack::default());
    let context = TransactionContext::new(repo.path(), Arc::clone(&cache));
    let transaction = context.open(None).expect("open transaction");

    let commit = transaction
        .repo()
        .find_commit(commit_oid)
        .expect("find commit");
    let input_tree = commit.tree().expect("commit tree");

    let mask_paths = vec![
        Path::new("a.txt").to_path_buf(),
        Path::new("dir/y.txt").to_path_buf(),
        // Removing a directory is represented by a terminal marker at that path.
        Path::new("dir/sub").to_path_buf(),
    ];
    let mask = filter::tree::mask_tree_from_paths(&transaction, &mask_paths)
        .expect("mask_tree_from_paths");

    let out_id = filter::tree::subtract(&transaction, input_tree.id(), mask).expect("subtract");
    let out_tree = transaction.repo().find_tree(out_id).expect("find out tree");

    assert!(out_tree.get_path(Path::new("a.txt")).is_err());
    assert!(out_tree.get_path(Path::new("dir/y.txt")).is_err());
    assert!(out_tree.get_path(Path::new("dir/sub/z.txt")).is_err());

    assert!(out_tree.get_path(Path::new("b.txt")).is_ok());
    assert!(out_tree.get_path(Path::new("dir/x.txt")).is_ok());
}

