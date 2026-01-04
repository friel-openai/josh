use std::path::Path;
use std::sync::Arc;

use git2::Repository;
use josh_core::cache::{
    sled_load, CacheBackend, CacheStack, NotesCacheBackend, TransactionContext,
};
use josh_core::filter;

fn create_repo_with_initial_commit() -> (tempfile::TempDir, Repository, git2::Oid) {
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

    (tmp, repo, commit)
}

#[test]
fn apply_to_commit_repairs_stale_tip_note_cache_entry() {
    let (_tmp, repo, commit_oid) = create_repo_with_initial_commit();
    sled_load(repo.path()).expect("sled_load");

    let cache = Arc::new(CacheStack::default());
    let context = TransactionContext::new(repo.path(), Arc::clone(&cache));
    let transaction = context.open(None).expect("open transaction");

    let filterobj = filter::Filter::new();
    let filter_id = filterobj.id();
    let tip_ref_v25 = format!("refs/josh/25/tip/{}", filter_id);

    // Seed a stale entry pointing at a nonexistent object.
    let bogus = git2::Oid::from_bytes(&[0x13; 20]).expect("bogus oid");
    let sig = git2::Signature::now("test", "test@example.com").expect("signature");
    repo.note(
        &sig,
        &sig,
        Some(&tip_ref_v25),
        commit_oid,
        &bogus.to_string(),
        true,
    )
    .expect("write bogus tip note");

    let commit = transaction
        .repo()
        .find_commit(commit_oid)
        .expect("find commit");
    let out = filter::apply_to_commit(filterobj, &commit, &transaction).expect("apply_to_commit");
    assert_eq!(out, commit_oid);

    let note = repo
        .find_note(Some(&tip_ref_v25), commit_oid)
        .expect("read tip note");
    assert_eq!(note.message().unwrap_or("").trim(), commit_oid.to_string());

    // Also writes to the legacy v24 tip namespace for dual-stack compatibility.
    let tip_ref_v24 = format!("refs/josh/24/tip/{}", filter_id);
    let note = repo
        .find_note(Some(&tip_ref_v24), commit_oid)
        .expect("read v24 tip note");
    assert_eq!(note.message().unwrap_or("").trim(), commit_oid.to_string());
}

#[test]
fn notes_cache_backend_roundtrips_zero_oid() {
    let (_tmp, repo, commit_oid) = create_repo_with_initial_commit();
    let backend = NotesCacheBackend::new(repo.path()).expect("notes cache backend");

    // Root commits are always eligible (parent_count != 1), so sequence number doesn't matter here.
    backend
        .write(filter::empty(), commit_oid, git2::Oid::zero(), 1)
        .expect("write");

    let out = backend
        .read(filter::empty(), commit_oid, 1)
        .expect("read")
        .expect("hit");
    assert_eq!(out, git2::Oid::zero());
}
