use std::path::Path;
use std::sync::Arc;

use git2::Repository;
use josh_core::cache::{
    compute_sequence_number, sled_load, CacheBackend, CacheStack, NotesCacheBackend,
    TransactionContext,
};
use josh_core::filter;

fn create_repo_with_two_commits() -> (tempfile::TempDir, Repository, git2::Oid, git2::Oid) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = Repository::init(tmp.path()).expect("init repo");
    let sig = git2::Signature::now("test", "test@example.com").expect("signature");

    std::fs::write(tmp.path().join("foo"), "hi").expect("write foo");
    {
        let mut idx = repo.index().expect("index");
        idx.add_path(Path::new("foo")).expect("add foo");
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

    std::fs::write(tmp.path().join("foo"), "hi2").expect("write foo2");
    {
        let mut idx = repo.index().expect("index");
        idx.add_path(Path::new("foo")).expect("add foo");
        idx.write().expect("write index");
    }
    let tree_oid = {
        let mut idx = repo.index().expect("index");
        idx.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_oid).expect("find tree");
    let commit2 = repo
        .commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&repo.find_commit(commit).unwrap()])
        .expect("commit2");
    drop(tree);

    (tmp, repo, commit, commit2)
}

#[test]
fn apply_to_commit_repairs_stale_forced_notes_cache_entry() {
    let (_tmp, repo, _root_oid, commit_oid) = create_repo_with_two_commits();
    sled_load(repo.path()).expect("sled_load");

    let cache = Arc::new(CacheStack::default());
    let context = TransactionContext::new(repo.path(), Arc::clone(&cache));
    let transaction = context.open(None).expect("open transaction");

    let filterobj = filter::Filter::new();
    let filter_id = filterobj.id();
    let seq25 = compute_sequence_number(&transaction, commit_oid).expect("sequence number");
    let note_ref_v25 = format!("refs/josh/25/{}/{}", seq25 / 10000, filter_id);

    // Seed a stale entry pointing at a nonexistent object.
    let bogus = git2::Oid::from_bytes(&[0x13; 20]).expect("bogus oid");
    let sig = git2::Signature::now("test", "test@example.com").expect("signature");
    repo.note(
        &sig,
        &sig,
        Some(&note_ref_v25),
        commit_oid,
        &bogus.to_string(),
        true,
    )
    .expect("write bogus note");

    let commit = transaction
        .repo()
        .find_commit(commit_oid)
        .expect("find commit");
    let out = filter::apply_to_commit(filterobj, &commit, &transaction).expect("apply_to_commit");
    assert_eq!(out, commit_oid);

    let note = repo
        .find_note(Some(&note_ref_v25), commit_oid)
        .expect("read v25 note");
    assert_eq!(note.message().unwrap_or("").trim(), commit_oid.to_string());

    // Also writes to the legacy v24 namespace for dual-stack compatibility.
    // In a linear history v24 first-parent numbering starts at 1, so the second commit is 2.
    let note_ref_v24 = format!("refs/josh/24/0/{}", filter_id);
    let note = repo
        .find_note(Some(&note_ref_v24), commit_oid)
        .expect("read v24 note");
    assert_eq!(note.message().unwrap_or("").trim(), commit_oid.to_string());
}

#[test]
fn notes_cache_backend_roundtrips_zero_oid() {
    let (_tmp, repo, root_oid, _head_oid) = create_repo_with_two_commits();
    let backend = NotesCacheBackend::new(repo.path()).expect("notes cache backend");

    // Root commits are always eligible (parent_count != 1), so sequence number doesn't matter here.
    backend
        .write(filter::empty(), root_oid, git2::Oid::zero(), 1)
        .expect("write");

    let out = backend
        .read(filter::empty(), root_oid, 1)
        .expect("read")
        .expect("hit");
    assert_eq!(out, git2::Oid::zero());
}
