use super::cache::{CACHE_VERSION, CacheBackend};
use crate::JoshResult;
use crate::filter;
use crate::filter::Filter;

pub struct NotesCacheBackend {
    repo: std::sync::Mutex<git2::Repository>,
}

impl NotesCacheBackend {
    pub fn new(repo_path: impl AsRef<std::path::Path>) -> JoshResult<Self> {
        let repo = git2::Repository::open(repo_path.as_ref())?;
        Ok(Self {
            repo: std::sync::Mutex::new(repo),
        })
    }
}

// The notes cache is meant to be sparse. That is, not all entries are actually persisted.
// This makes it smaller and faster to download.
// It is expected that on any node (server, proxy, local repo) a full "dense" local cache
// is used in addition to the sparse note cache.
// The note cache is mostly only used for initial "cold starts" or longer "catch up".
// For incremental filtering it's fine re-filter commits and rely on the local "dense" cache.
// We store entries for 1% of all commits, and additionally all merges and orphans.
fn is_note_eligible(repo: &git2::Repository, oid: git2::Oid, sequence_number: u128) -> bool {
    let parent_count = if let Ok(c) = repo.find_commit(oid) {
        c.parent_ids().count()
    } else {
        return false;
    };

    sequence_number % 100 == 0 || parent_count != 1
}

// To additionally limit the size of the note trees the cache is also sharded by sequence
// number in groups of 10000. Note that this does not limit the number of entried per bucket
// as branches mean many commits share the same sequence number.
fn note_path(key: git2::Oid, sequence_number: u128) -> String {
    format!(
        "refs/josh/{}/{}/{}",
        CACHE_VERSION,
        sequence_number / 10000,
        key,
    )
}

fn tip_note_path(key: git2::Oid) -> String {
    format!("refs/josh/{}/tip/{}", CACHE_VERSION, key)
}

fn parse_note_oid(message: &str) -> Option<git2::Oid> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    git2::Oid::from_str(message).ok()
}

fn read_note(
    repo: &git2::Repository,
    refname: &str,
    from: git2::Oid,
) -> JoshResult<Option<git2::Oid>> {
    let Ok(note) = repo.find_note(Some(refname), from) else {
        return Ok(None);
    };
    let Some(result) = parse_note_oid(note.message().unwrap_or("")) else {
        // Corrupt / unexpected note content: treat as a cache miss.
        return Ok(None);
    };

    // `Oid::zero()` is a valid cached value meaning "filtered out".
    if result == git2::Oid::zero() {
        return Ok(Some(result));
    }

    // Notes may be fetched without the corresponding objects being present locally (e.g.
    // partial fetches, stale notes, or corrupted entries). Treat such entries as cache
    // misses so callers can recompute.
    if repo.find_object(result, None).is_err() {
        return Ok(None);
    }

    Ok(Some(result))
}

fn write_note(
    repo: &git2::Repository,
    refname: &str,
    from: git2::Oid,
    to: git2::Oid,
) -> JoshResult<()> {
    let signature = super::cache::josh_commit_signature()?;
    repo.note(
        &signature,
        &signature,
        Some(refname),
        from,
        &to.to_string(),
        true,
    )?;
    Ok(())
}

/// Persist a mapping for a specific "tip" commit into `refs/josh/*` regardless of the
/// normal sparse eligibility rules.
///
/// This is intended to avoid expensive re-walks after process restarts: once a client
/// has successfully filtered a requested commit, it can explicitly persist that mapping.
pub fn write_tip_mapping(
    repo: &git2::Repository,
    filter: Filter,
    from: git2::Oid,
    to: git2::Oid,
) -> JoshResult<()> {
    if filter == filter::sequence_number() {
        return Ok(());
    }
    // `apply_to_commit` uses the optimized representation as the cache key, so persist the tip
    // mapping under the optimized filter id to guarantee the subsequent read hits.
    let filter = crate::filter::opt::optimize(filter);
    write_note(repo, &tip_note_path(filter.id()), from, to)
}

impl CacheBackend for NotesCacheBackend {
    fn read(
        &self,
        filter: Filter,
        from: git2::Oid,
        sequence_number: u128,
    ) -> JoshResult<Option<git2::Oid>> {
        if filter == filter::sequence_number() {
            return Ok(None);
        }
        let repo = self.repo.lock()?;
        let key = filter.id();

        // First, consult the explicit tip mapping namespace. This is independent of the
        // sparse eligibility rules and is expected to contain at most a small number of
        // entries written by callers for "tip" commits they care about.
        if let Some(result) = read_note(&repo, &tip_note_path(key), from)? {
            return Ok(Some(result));
        }

        if !is_note_eligible(&repo, from, sequence_number) {
            return Ok(None);
        }

        read_note(&repo, &note_path(key, sequence_number), from)
    }

    fn write(
        &self,
        filter: Filter,
        from: git2::Oid,
        to: git2::Oid,
        sequence_number: u128,
    ) -> JoshResult<()> {
        if filter == filter::sequence_number() {
            return Ok(());
        }

        let repo = self.repo.lock()?;
        if !is_note_eligible(&*repo, from, sequence_number) {
            return Ok(());
        }

        write_note(&repo, &note_path(filter.id(), sequence_number), from, to)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter;
    use git2::Repository;
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let tmp = TempDir::new().expect("tmpdir");
        let repo = Repository::init(tmp.path()).expect("init repo");
        (tmp, repo)
    }

    fn write_commit(repo: &Repository, message: &str) -> git2::Oid {
        let sig = repo.signature().unwrap_or_else(|_| {
            git2::Signature::now("Josh", "josh@example.com").expect("signature")
        });
        let tree_id = {
            let mut index = repo.index().expect("index");
            index.write_tree().expect("write tree")
        };
        let tree = repo.find_tree(tree_id).expect("tree");
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
            .expect("commit")
    }

    #[test]
    fn tip_mapping_is_read_without_sparse_eligibility() {
        let (_tmp, repo) = init_repo();
        let from = write_commit(&repo, "c1");
        let to = from;
        let filter = crate::filter::opt::optimize(filter::parse(":/").expect("parse filter"));

        write_tip_mapping(&repo, filter, from, to).expect("write tip");

        let backend = NotesCacheBackend::new(repo.path()).expect("backend");
        // Use a sequence number that is not eligible (not %100 == 0) and a linear commit.
        let read = backend
            .read(filter, from, 1)
            .expect("read")
            .expect("expected hit");
        assert_eq!(read, to);
    }

    #[test]
    fn tip_mapping_allows_zero_oid() {
        let (_tmp, repo) = init_repo();
        let from = write_commit(&repo, "c1");
        let filter = crate::filter::opt::optimize(filter::parse(":/").expect("parse filter"));

        write_tip_mapping(&repo, filter, from, git2::Oid::zero()).expect("write tip");

        let backend = NotesCacheBackend::new(repo.path()).expect("backend");
        let read = backend
            .read(filter, from, 1)
            .expect("read")
            .expect("expected hit");
        assert_eq!(read, git2::Oid::zero());
    }
}
