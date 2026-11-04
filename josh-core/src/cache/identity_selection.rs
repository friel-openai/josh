use std::collections::{HashMap, VecDeque};

/// A local selector fingerprint deliberately excludes its ancestors: changing a
/// sibling's policy must not invalidate an unchanged subtree's selection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct IdentitySelectionKey {
    pub tree: git2::Oid,
    pub selector: git2::Oid,
    pub remove: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IdentitySelectionCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
}

/// Transaction-local FIFO with fixed-size keys/values and a hard entry limit.
/// No policy strings or Git objects are retained here. Dropping the transaction
/// drops this cache along with its in-memory object database.
pub(crate) struct IdentitySelectionCache {
    capacity: usize,
    entries: HashMap<IdentitySelectionKey, git2::Oid>,
    order: VecDeque<IdentitySelectionKey>,
    stats: IdentitySelectionCacheStats,
}

impl IdentitySelectionCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
            stats: IdentitySelectionCacheStats::default(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn get(&mut self, key: IdentitySelectionKey) -> Option<git2::Oid> {
        let result = self.entries.get(&key).copied();
        if result.is_some() {
            self.stats.hits += 1;
        } else {
            self.stats.misses += 1;
        }
        result
    }

    pub fn insert(&mut self, key: IdentitySelectionKey, output: git2::Oid) {
        if self.capacity == 0 {
            return;
        }
        if let Some(existing) = self.entries.get_mut(&key) {
            *existing = output;
            return;
        }
        if self.entries.len() == self.capacity {
            let oldest = self.order.pop_front().expect("nonempty bounded cache");
            self.entries.remove(&oldest);
            self.stats.evictions += 1;
        }
        self.entries.insert(key, output);
        self.order.push_back(key);
    }

    pub fn stats(&self) -> IdentitySelectionCacheStats {
        IdentitySelectionCacheStats {
            entries: self.entries.len(),
            ..self.stats
        }
    }
}
