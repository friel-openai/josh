use std::path::Path;
use std::sync::Arc;

use git2::Repository;
use josh_core::cache::{CacheStack, TransactionContext};
use josh_core::filter::tree;

#[test]
fn pathstree_ignores_stale_cached_tree() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = Repository::init(tmp.path()).expect("init repo");

    // Create a simple tree with one file.
    std::fs::write(tmp.path().join("foo"), "hi").expect("write foo");
    {
        let mut idx = repo.index().expect("index");
        idx.add_path(Path::new("foo")).expect("add foo");
        idx.write().expect("write index");
    }
    let tree_id = {
        let mut idx = repo.index().expect("index");
        idx.write_tree().expect("write tree")
    };

    // Initialize sled-backed caches.
    josh_core::cache::sled::sled_load(repo.path()).expect("sled_load");

    let cache = Arc::new(CacheStack::default());
    let context = TransactionContext::new(repo.path(), Arc::clone(&cache));
    let transaction = context.open(None).expect("open transaction");

    // Seed an invalid cached tree OID for (tree_id, "") to simulate a stale sled entry.
    let bogus = git2::Oid::from_bytes(&[0x42; 20]).expect("bogus oid");
    transaction.insert_paths((tree_id, "".to_string()), bogus);

    // Should ignore the stale cache and recompute instead of erroring.
    let result = tree::pathstree("", tree_id, &transaction).expect("pathstree recomputed");

    // Cache should now hold a valid tree OID, not the bogus one.
    let cached = transaction
        .get_paths((tree_id, "".to_string()))
        .expect("cached entry");
    assert_ne!(cached, bogus, "stale cache entry was not replaced");
    repo.find_tree(cached).expect("cached tree exists");

    // And the result includes our file.
    let names: Vec<_> = result
        .iter()
        .filter_map(|e| e.name().map(str::to_string))
        .collect();
    assert!(names.contains(&"foo".to_string()));
}
