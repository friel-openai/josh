use josh_core::cache::{CacheBackend, CacheStack, NotesCacheBackend, NotesCacheBackendV24};
use josh_core::filter;

fn write_commit(
    repo: &git2::Repository,
    update_ref: Option<&str>,
    message: &str,
    parents: &[&git2::Commit<'_>],
) -> git2::Oid {
    let sig = repo
        .signature()
        .unwrap_or_else(|_| git2::Signature::now("test", "test@example.com").expect("sig"));
    let tree_id = {
        let mut index = repo.index().expect("index");
        index.write_tree().expect("write tree")
    };
    let tree = repo.find_tree(tree_id).expect("tree");
    repo.commit(update_ref, &sig, &sig, message, &tree, parents)
        .expect("commit")
}

#[test]
fn notes_cache_v24_can_backfill_v25_via_cache_stack_propagation() {
    // Simulate a future where the primary notes cache uses version 25 and sequence_number semantics
    // differ, while we still want to reuse existing v24 notes.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = git2::Repository::init(tmp.path()).expect("init repo");

    // Construct a merge commit so the notes eligibility heuristic always applies.
    let a_oid = write_commit(&repo, Some("HEAD"), "a", &[]);
    let a = repo.find_commit(a_oid).expect("a");
    let b_oid = write_commit(&repo, Some("HEAD"), "b", &[&a]);
    let b = repo.find_commit(b_oid).expect("b");

    // Side branch off a (doesn't advance HEAD).
    let x_oid = write_commit(&repo, None, "x", &[&a]);
    let x = repo.find_commit(x_oid).expect("x");

    // Merge: first parent = b, second parent = x.
    let m_oid = write_commit(&repo, Some("HEAD"), "m", &[&b, &x]);

    let f = filter::parse(":/").expect("parse");

    // Populate only the v24 cache.
    let v24 = NotesCacheBackendV24::new(repo.path()).expect("v24 backend");
    CacheBackend::write(&v24, f, m_oid, m_oid, 0).expect("write v24 note");

    // Cache stack with v25 primary and v24 fallback.
    let v25 = NotesCacheBackend::new(repo.path()).expect("v25 backend");
    let stack = CacheStack::new().with_backend(v25).with_backend(v24);

    // Use a sequence number representative of v25 semantics; the v24 backend ignores it and
    // computes v24 first-parent numbering internally.
    let seq_v25 = 1u128;

    let got = stack
        .read_propagate(f, m_oid, seq_v25)
        .expect("read_propagate");
    assert_eq!(got, Some(m_oid));

    // Ensure the v25 namespace was backfilled via propagation.
    let key = f.id();
    let path = format!("refs/josh/25/0/{}", key);
    let note = repo.find_note(Some(&path), m_oid).expect("v25 note");
    assert_eq!(note.message().unwrap_or("").trim(), m_oid.to_string());
}

#[test]
fn notes_cache_stack_dual_write_writes_v24_and_v25() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = git2::Repository::init(tmp.path()).expect("init repo");

    // Construct a merge commit so both backends will consider it eligible regardless of
    // sequence number semantics.
    let a_oid = write_commit(&repo, Some("HEAD"), "a", &[]);
    let a = repo.find_commit(a_oid).expect("a");
    let b_oid = write_commit(&repo, Some("HEAD"), "b", &[&a]);
    let b = repo.find_commit(b_oid).expect("b");
    let x_oid = write_commit(&repo, None, "x", &[&a]);
    let x = repo.find_commit(x_oid).expect("x");
    let m_oid = write_commit(&repo, Some("HEAD"), "m", &[&b, &x]);

    let f = filter::parse(":/").expect("parse");

    let v25 = NotesCacheBackend::new(repo.path()).expect("v25 backend");
    let v24 = NotesCacheBackendV24::new(repo.path()).expect("v24 backend");
    let stack = CacheStack::new().with_backend(v25).with_backend(v24);

    // Sequence number representative of v25 semantics; the v24 backend computes its own.
    stack
        .write_all(f, m_oid, m_oid, 1)
        .expect("write_all");

    let key = f.id();
    let note_v25 = repo
        .find_note(Some(&format!("refs/josh/25/0/{}", key)), m_oid)
        .expect("v25 note");
    assert_eq!(note_v25.message().unwrap_or("").trim(), m_oid.to_string());

    let note_v24 = repo
        .find_note(Some(&format!("refs/josh/24/0/{}", key)), m_oid)
        .expect("v24 note");
    assert_eq!(note_v24.message().unwrap_or("").trim(), m_oid.to_string());
}
