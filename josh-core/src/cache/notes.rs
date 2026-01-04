use super::transaction::{CacheBackend, CACHE_VERSION};
use crate::filter;
use crate::filter::Filter;
use crate::JoshResult;
use std::collections::HashMap;

const LEGACY_CACHE_VERSION: u64 = 24;

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

fn tip_note_path(version: u64, key: git2::Oid) -> String {
    format!("refs/josh/{}/tip/{}", version, key)
}

fn read_tip_for_version(
    repo: &git2::Repository,
    version: u64,
    key: git2::Oid,
    from: git2::Oid,
) -> Option<git2::Oid> {
    let path = tip_note_path(version, key);
    let note = repo.find_note(Some(&path), from).ok()?;
    let message = note.message().unwrap_or("").trim();
    let Ok(result) = git2::Oid::from_str(message) else {
        return None;
    };

    // `git2::Oid::zero()` is used as a sentinel for "filter produced no content".
    if result == git2::Oid::zero() {
        return Some(result);
    }

    // Notes may be fetched without the corresponding objects being present locally (e.g. partial
    // fetches, stale notes, or corrupted entries). Treat such entries as cache misses so callers
    // can recompute.
    if repo.find_object(result, None).is_err() {
        return None;
    }

    Some(result)
}

pub(crate) fn read_tip(
    repo: &git2::Repository,
    key: git2::Oid,
    from: git2::Oid,
) -> Option<git2::Oid> {
    read_tip_for_version(repo, CACHE_VERSION, key, from)
        .or_else(|| read_tip_for_version(repo, LEGACY_CACHE_VERSION, key, from))
}

pub(crate) fn write_tip(
    repo: &git2::Repository,
    key: git2::Oid,
    from: git2::Oid,
    to: git2::Oid,
) -> JoshResult<()> {
    let signature = super::transaction::josh_commit_signature()?;
    repo.note(
        &signature,
        &signature,
        Some(&tip_note_path(CACHE_VERSION, key)),
        from,
        &to.to_string(),
        true,
    )?;
    repo.note(
        &signature,
        &signature,
        Some(&tip_note_path(LEGACY_CACHE_VERSION, key)),
        from,
        &to.to_string(),
        true,
    )?;
    Ok(())
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
        if !is_note_eligible(&repo, from, sequence_number) {
            return Ok(None);
        }

        let key = filter.id();

        if let Ok(note) = repo.find_note(Some(&note_path(key, sequence_number)), from) {
            let message = note.message().unwrap_or("").trim();
            let Ok(result) = git2::Oid::from_str(message) else {
                // Corrupt / unexpected note content: treat as a cache miss.
                return Ok(None);
            };

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
        } else {
            Ok(None)
        }
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
        if !is_note_eligible(&repo, from, sequence_number) {
            return Ok(());
        }

        let key = filter.id();
        let signature = super::transaction::josh_commit_signature()?;

        repo.note(
            &signature,
            &signature,
            Some(&note_path(key, sequence_number)),
            from,
            &to.to_string(),
            true,
        )?;

        Ok(())
    }
}

/// Legacy notes cache backend for reading/writing Josh notes stored under cache version 24.
///
/// This is intended to support upgrades where the primary notes cache version and/or sequence
/// number semantics change, while still being able to read and maintain the older cache.
pub struct NotesCacheBackendV24 {
    repo: std::sync::Mutex<git2::Repository>,
    sequence_numbers: std::sync::Mutex<HashMap<git2::Oid, u128>>,
}

impl NotesCacheBackendV24 {
    pub fn new(repo_path: impl AsRef<std::path::Path>) -> JoshResult<Self> {
        let repo = git2::Repository::open(repo_path.as_ref())?;
        Ok(Self {
            repo: std::sync::Mutex::new(repo),
            sequence_numbers: std::sync::Mutex::new(HashMap::new()),
        })
    }

    fn compute_sequence_number_v24(
        repo: &git2::Repository,
        sequence_numbers: &mut HashMap<git2::Oid, u128>,
        input: git2::Oid,
    ) -> Option<u128> {
        if let Some(v) = sequence_numbers.get(&input) {
            return Some(*v);
        }

        let mut chain = Vec::new();
        let mut current = input;
        let start_seq: u128;
        loop {
            if let Some(v) = sequence_numbers.get(&current) {
                start_seq = v.saturating_add(1);
                break;
            }
            chain.push(current);
            let commit = repo.find_commit(current).ok()?;
            if let Some(p) = commit.parent_ids().next() {
                current = p;
            } else {
                // Root commit: v24 first-parent numbering starts at 1.
                start_seq = 1;
                break;
            }
        }

        let mut seq = start_seq;
        for oid in chain.iter().rev() {
            sequence_numbers.insert(*oid, seq);
            seq = seq.saturating_add(1);
        }

        sequence_numbers.get(&input).copied()
    }

    fn note_path_v24(key: git2::Oid, sequence_number: u128) -> String {
        // Version 24 is the legacy namespace.
        format!("refs/josh/24/{}/{}", sequence_number / 10000, key)
    }
}

impl CacheBackend for NotesCacheBackendV24 {
    fn read(
        &self,
        filter: Filter,
        from: git2::Oid,
        _sequence_number: u128,
    ) -> JoshResult<Option<git2::Oid>> {
        if filter == filter::sequence_number() {
            return Ok(None);
        }

        let repo = self.repo.lock()?;
        let mut seqs = self.sequence_numbers.lock()?;
        let Some(sequence_number) = Self::compute_sequence_number_v24(&repo, &mut seqs, from)
        else {
            return Ok(None);
        };

        if !is_note_eligible(&repo, from, sequence_number) {
            return Ok(None);
        }

        let key = filter.id();
        let path = Self::note_path_v24(key, sequence_number);

        if let Ok(note) = repo.find_note(Some(&path), from) {
            let message = note.message().unwrap_or("").trim();
            let Ok(result) = git2::Oid::from_str(message) else {
                return Ok(None);
            };

            if result == git2::Oid::zero() {
                return Ok(Some(result));
            }

            if repo.find_object(result, None).is_err() {
                return Ok(None);
            }

            Ok(Some(result))
        } else {
            Ok(None)
        }
    }

    fn write(
        &self,
        filter: Filter,
        from: git2::Oid,
        to: git2::Oid,
        _sequence_number: u128,
    ) -> JoshResult<()> {
        if filter == filter::sequence_number() {
            return Ok(());
        }

        let repo = self.repo.lock()?;
        let mut seqs = self.sequence_numbers.lock()?;
        let Some(sequence_number) = Self::compute_sequence_number_v24(&repo, &mut seqs, from)
        else {
            return Ok(());
        };

        if !is_note_eligible(&*repo, from, sequence_number) {
            return Ok(());
        }

        let key = filter.id();
        let signature = super::transaction::josh_commit_signature()?;

        repo.note(
            &signature,
            &signature,
            Some(&Self::note_path_v24(key, sequence_number)),
            from,
            &to.to_string(),
            true,
        )?;

        Ok(())
    }
}
