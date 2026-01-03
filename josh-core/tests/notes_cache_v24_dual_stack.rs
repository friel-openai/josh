use josh_core::cache::{CacheBackend, CacheStack, NotesCacheBackendV24};
use josh_core::{JoshResult, filter};
use std::sync::Mutex;

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

struct NotesCacheBackendV25Stub {
    repo: Mutex<git2::Repository>,
}

impl NotesCacheBackendV25Stub {
    fn new(repo_path: impl AsRef<std::path::Path>) -> JoshResult<Self> {
        Ok(Self {
            repo: Mutex::new(git2::Repository::open(repo_path.as_ref())?),
        })
    }

    fn note_path_v25(key: git2::Oid, sequence_number: u128) -> String {
        format!("refs/josh/25/{}/{}", sequence_number / 10000, key)
    }
}

impl CacheBackend for NotesCacheBackendV25Stub {
    fn read(
        &self,
        filter: josh_core::filter::Filter,
        from: git2::Oid,
        sequence_number: u128,
    ) -> JoshResult<Option<git2::Oid>> {
        if filter == filter::sequence_number() {
            return Ok(None);
        }
        let repo = self.repo.lock().unwrap();
        let key = filter.id();
        let path = Self::note_path_v25(key, sequence_number);
        if let Ok(note) = repo.find_note(Some(&path), from) {
            let message = note.message().unwrap_or("").trim();
            let Ok(result) = git2::Oid::from_str(message) else {
                return Ok(None);
            };
            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    fn write(
        &self,
        filter: josh_core::filter::Filter,
        from: git2::Oid,
        to: git2::Oid,
        sequence_number: u128,
    ) -> JoshResult<()> {
        if filter == filter::sequence_number() {
            return Ok(());
        }
        let repo = self.repo.lock().unwrap();
        let sig = git2::Signature::now("test", "test@example.com")?;
        let key = filter.id();
        repo.note(
            &sig,
            &sig,
            Some(&Self::note_path_v25(key, sequence_number)),
            from,
            &to.to_string(),
            true,
        )?;
        Ok(())
    }
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
    let v25 = NotesCacheBackendV25Stub::new(repo.path()).expect("v25 stub");
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
