use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::{FileMode, ObjectType, Repository};
use josh_core::cache::{sled_load, CacheStack, TransactionContext};
use josh_core::filter::tree;
use josh_core::JoshResult;

#[derive(Default)]
struct SelectNode {
    terminal: bool,
    children: BTreeMap<String, SelectNode>,
}

impl SelectNode {
    fn insert(&mut self, path: &Path) -> Option<bool> {
        if path.as_os_str().is_empty() {
            self.terminal = true;
            self.children.clear();
            return Some(true);
        }

        let mut node = self;
        for component in path.components() {
            let comp = component.as_os_str().to_str()?;
            node = node.children.entry(comp.to_string()).or_default();
            if node.terminal {
                // Parent path already selects the subtree.
                return Some(false);
            }
        }

        if node.terminal {
            return Some(false);
        }

        node.terminal = true;
        node.children.clear();
        Some(true)
    }
}

fn build_subset_naive(
    repo: &Repository,
    full: &git2::Tree<'_>,
    node: &SelectNode,
) -> JoshResult<git2::Oid> {
    let mut builder = repo.treebuilder(None)?;
    for (name, child) in &node.children {
        let Some(entry) = full.get_name(name) else {
            continue;
        };

        if child.terminal {
            builder.insert(Path::new(name), entry.id(), entry.filemode())?;
            continue;
        }

        if child.children.is_empty() || entry.kind() != Some(ObjectType::Tree) {
            continue;
        }

        let subtree = repo.find_tree(entry.id())?;
        let subtree_id = build_subset_naive(repo, &subtree, child)?;
        if subtree_id != tree::empty_id() {
            builder.insert(Path::new(name), subtree_id, FileMode::Tree.into())?;
        }
    }

    Ok(builder.write()?)
}

fn compose_file_selections_naive_oid(
    repo: &Repository,
    full_tree: &git2::Tree<'_>,
    paths: &[PathBuf],
) -> JoshResult<Option<git2::Oid>> {
    if paths.is_empty() {
        return Ok(Some(tree::empty(repo).id()));
    }

    let mut root = SelectNode::default();
    for p in paths {
        if root.insert(p).is_none() {
            return Ok(None);
        }
        if root.terminal {
            return Ok(Some(full_tree.id()));
        }
    }

    let id = build_subset_naive(repo, full_tree, &root)?;
    Ok(Some(id))
}

enum MaskNode {
    Terminal,
    Children(BTreeMap<OsString, MaskNode>),
}

impl Default for MaskNode {
    fn default() -> Self {
        MaskNode::Children(BTreeMap::new())
    }
}

impl MaskNode {
    fn insert_path(&mut self, path: &Path) {
        match self {
            MaskNode::Terminal => return,
            MaskNode::Children(children) => {
                let mut components = path.components();
                let Some(first) = components.next() else {
                    *self = MaskNode::Terminal;
                    return;
                };

                let rest = components.as_path();
                let key = first.as_os_str().to_os_string();

                let entry = children
                    .entry(key)
                    .or_insert_with(|| MaskNode::Children(Default::default()));
                if rest.as_os_str().is_empty() {
                    *entry = MaskNode::Terminal;
                } else {
                    entry.insert_path(rest);
                }
            }
        }
    }
}

fn write_mask_node(
    repo: &Repository,
    marker: git2::Oid,
    node: &MaskNode,
) -> JoshResult<git2::Oid> {
    match node {
        MaskNode::Terminal => Ok(marker),
        MaskNode::Children(children) => {
            let mut builder = repo.treebuilder(None)?;
            for (name, child) in children {
                let oid = write_mask_node(repo, marker, child)?;
                let mode = match child {
                    MaskNode::Terminal => FileMode::Blob.into(),
                    MaskNode::Children(_) => FileMode::Tree.into(),
                };
                builder.insert(Path::new(name), oid, mode)?;
            }
            Ok(builder.write()?)
        }
    }
}

fn mask_tree_naive(
    transaction: &josh_core::cache::Transaction,
    paths: &[PathBuf],
) -> JoshResult<git2::Oid> {
    let repo = transaction.repo();
    let marker = transaction.get_or_init_mask_marker()?;

    let mut root = MaskNode::default();
    for p in paths {
        if p.as_os_str().is_empty() {
            root = MaskNode::Terminal;
            break;
        }
        root.insert_path(p);
    }

    write_mask_node(repo, marker, &root)
}

fn create_repo_with_layout() -> (tempfile::TempDir, Repository, git2::Oid) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = Repository::init(tmp.path()).expect("init repo");
    let sig = git2::Signature::now("test", "test@example.com").expect("signature");

    let files = [
        ("readme.txt", "readme"),
        ("foo/skip.txt", "skip"),
        ("foo/bar/b.txt", "b"),
        ("foo/bar/nested/c.txt", "c"),
        ("keep/leaf.txt", "leaf"),
        ("other/omit.txt", "omit"),
    ];

    for (rel, contents) in &files {
        if let Some(parent) = Path::new(rel).parent() {
            std::fs::create_dir_all(tmp.path().join(parent)).expect("create dirs");
        }
        std::fs::write(tmp.path().join(rel), contents).expect("write file");
    }

    let mut idx = repo.index().expect("index");
    for (rel, _) in &files {
        idx.add_path(Path::new(rel)).expect("add path");
    }
    idx.write().expect("write index");

    let tree_id = idx.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let commit_id = repo
        .commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .expect("commit");
    drop(tree);

    (tmp, repo, commit_id)
}

fn open_transaction(repo: &Repository, cache: &Arc<CacheStack>) -> josh_core::cache::Transaction {
    let context = TransactionContext::new(repo.path(), Arc::clone(cache));
    context.open(None).expect("open transaction")
}

#[test]
fn compose_file_selections_match_naive_and_reuse_cache() -> JoshResult<()> {
    let (_tmp, repo, commit_oid) = create_repo_with_layout();
    sled_load(repo.path()).expect("sled_load");

    let cache = Arc::new(CacheStack::default());
    let transaction = open_transaction(&repo, &cache);

    let commit = transaction.repo().find_commit(commit_oid)?;
    let input_tree = commit.tree()?;

    let paths = vec![
        PathBuf::from("readme.txt"),
        PathBuf::from("foo/bar"),
        PathBuf::from("foo/bar/nested/c.txt"),
        PathBuf::from("keep/leaf.txt"),
        PathBuf::from("missing/ghost.txt"),
    ];

    let subset = tree::compose_file_selections_no_remap(&transaction, &input_tree, &paths)?
        .expect("compose produced tree");
    let expected_oid =
        compose_file_selections_naive_oid(transaction.repo(), &input_tree, &paths)?
            .expect("naive produced tree");
    assert_eq!(subset.id(), expected_oid);

    assert!(subset.get_path(Path::new("readme.txt")).is_ok());
    assert!(subset.get_path(Path::new("foo/bar/b.txt")).is_ok());
    assert!(subset
        .get_path(Path::new("foo/bar/nested/c.txt"))
        .is_ok());
    assert!(subset.get_path(Path::new("keep/leaf.txt")).is_ok());
    assert!(subset.get_path(Path::new("foo/skip.txt")).is_err());
    assert!(subset.get_path(Path::new("other/omit.txt")).is_err());
    assert!(subset.get_path(Path::new("missing/ghost.txt")).is_err());

    // Second call should exercise the transaction-local cache and produce the identical tree id.
    let cached_subset =
        tree::compose_file_selections_no_remap(&transaction, &input_tree, &paths)?
            .expect("cached compose produced tree");
    assert_eq!(cached_subset.id(), subset.id());

    Ok(())
}

#[test]
fn mask_tree_matches_naive_and_reuses_cache() -> JoshResult<()> {
    let (_tmp, repo, _commit_oid) = create_repo_with_layout();
    sled_load(repo.path()).expect("sled_load");

    let cache = Arc::new(CacheStack::default());
    let transaction = open_transaction(&repo, &cache);

    let paths = vec![
        PathBuf::from("foo/bar/b.txt"),
        PathBuf::from("foo/bar"),
        PathBuf::from("unshared/leaf.txt"),
        PathBuf::from("top/dir/nested"),
    ];

    let mask = tree::mask_tree_from_paths(&transaction, &paths)?;
    let expected = mask_tree_naive(&transaction, &paths)?;
    assert_eq!(mask, expected);

    let mask_tree = transaction.repo().find_tree(mask)?;
    let foo_bar = mask_tree.get_path(Path::new("foo/bar"))?;
    assert_eq!(foo_bar.kind(), Some(ObjectType::Blob));
    let unshared = mask_tree.get_path(Path::new("unshared/leaf.txt"))?;
    assert_eq!(unshared.kind(), Some(ObjectType::Blob));

    // Calling again in the same transaction should hit the cache and return the identical OID.
    let cached = tree::mask_tree_from_paths(&transaction, &paths)?;
    assert_eq!(cached, mask);

    Ok(())
}
