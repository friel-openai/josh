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
    std::fs::create_dir_all(tmp.path().join("dir")).expect("create dir");
    std::fs::write(tmp.path().join("dir/b.txt"), "b").expect("write b");
    std::fs::write(tmp.path().join("dir/c.txt"), "c").expect("write c");

    {
        let mut idx = repo.index().expect("index");
        idx.add_path(Path::new("a.txt")).expect("add a");
        idx.add_path(Path::new("dir/b.txt")).expect("add b");
        idx.add_path(Path::new("dir/c.txt")).expect("add c");
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
fn compose_many_file_selections_still_produces_expected_tree() {
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

    // Ensure we exceed the fast-path threshold (currently 1024) while only selecting a handful of
    // real files. Missing paths should be ignored.
    let mut selections = Vec::new();
    selections.push(filter::file("a.txt"));
    selections.push(filter::file("dir/b.txt"));
    for i in 0..2048 {
        selections.push(filter::file(format!("missing/{}.txt", i)));
    }
    let composed = filter::compose(&selections);

    let out = filter::apply(
        &transaction,
        composed,
        filter::Rewrite::from_commit(&commit).expect("rewrite"),
    )
    .expect("apply");
    let out_tree = out.tree().clone();

    // Selected entries exist and match the input tree.
    let a_in = input_tree.get_path(Path::new("a.txt")).expect("a in");
    let a_out = out_tree.get_path(Path::new("a.txt")).expect("a out");
    assert_eq!(a_in.id(), a_out.id());
    assert_eq!(a_in.filemode(), a_out.filemode());

    let b_in = input_tree.get_path(Path::new("dir/b.txt")).expect("b in");
    let b_out = out_tree.get_path(Path::new("dir/b.txt")).expect("b out");
    assert_eq!(b_in.id(), b_out.id());
    assert_eq!(b_in.filemode(), b_out.filemode());

    // Unselected entry is absent.
    assert!(out_tree.get_path(Path::new("dir/c.txt")).is_err());
}

