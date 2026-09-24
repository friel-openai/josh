//! Optional incremental construction for evolving exact-path selections.
use std::path::PathBuf;

use crate::{Filter, Op, filter, opt, persist};

/// Reuse unchanged branches while constructing successive exact-path selections.
///
/// This has the same output as [`filter::files`]. Sorted, unique, canonical UTF-8
/// paths use a retained trie; other inputs use `files` and release that state.
/// The retained paths and trie trade memory for less work between similar policies.
#[derive(Default)]
pub struct FileSelection {
    paths: Vec<String>,
    tree: Option<Paths>,
}

#[derive(Default)]
struct Paths {
    terminal: bool,
    children: Vec<(String, Paths)>,
    filter: Option<Filter>,
}

impl FileSelection {
    pub fn update<I, P>(&mut self, paths: I) -> Filter
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let paths: Vec<PathBuf> = paths.into_iter().map(Into::into).collect();
        // Exact byte equality with the last validated input also proves that
        // this input is canonical. Avoid revalidating unchanged policies.
        if let Some(tree) = &mut self.tree
            && self.paths.len() == paths.len()
            && self
                .paths
                .iter()
                .zip(&paths)
                .all(|(old, new)| std::ffi::OsStr::new(old) == new.as_os_str())
        {
            return tree.build();
        }
        let mut previous = None;
        let canonical = paths.iter().all(|path| {
            let Some(text) = path.to_str() else {
                return false;
            };
            if text.is_empty()
                || text.contains('\0')
                || previous.is_some_and(|old| old >= text)
                || text
                    .split('/')
                    .any(|name| name.is_empty() || name == "." || name == "..")
            {
                return false;
            }
            #[cfg(not(unix))]
            if !path
                .components()
                .map(|part| match part {
                    std::path::Component::Normal(name) => name.to_str(),
                    _ => None,
                })
                .eq(text.split('/').map(Some))
            {
                return false;
            }
            previous = Some(text);
            true
        });
        if !canonical {
            self.paths = Vec::new();
            self.tree = None;
            return filter::files(paths);
        }
        let paths: Vec<String> = paths
            .into_iter()
            .map(|path| {
                path.into_os_string()
                    .into_string()
                    .expect("validated UTF-8 path")
            })
            .collect();
        let tree = self.tree.get_or_insert_with(Paths::default);
        let mut old = self.paths.iter().peekable();
        let mut new = paths.iter().peekable();
        while old.peek().is_some() || new.peek().is_some() {
            match (old.peek(), new.peek()) {
                (Some(left), Some(right)) if left == right => {
                    old.next();
                    new.next();
                }
                (Some(left), Some(right)) if left < right => {
                    tree.set(old.next().unwrap().split('/'), false)
                }
                (Some(_), None) => tree.set(old.next().unwrap().split('/'), false),
                (_, Some(_)) => tree.set(new.next().unwrap().split('/'), true),
                (None, None) => unreachable!(),
            }
        }
        self.paths = paths;
        tree.build()
    }
}

impl Paths {
    fn set<'a>(&mut self, mut components: impl Iterator<Item = &'a str>, terminal: bool) {
        self.filter = None;
        if let Some(name) = components.next() {
            let position = self
                .children
                .binary_search_by(|(child_name, _)| child_name.as_str().cmp(name));
            if terminal {
                let index = match position {
                    Ok(index) => index,
                    Err(index) => {
                        self.children
                            .insert(index, (name.to_owned(), Paths::default()));
                        index
                    }
                };
                self.children[index].1.set(components, true);
            } else {
                let index = position.expect("previously selected path");
                let child = &mut self.children[index].1;
                child.set(components, false);
                if !child.terminal && child.children.is_empty() {
                    self.children.remove(index);
                }
            }
        } else {
            self.terminal = terminal;
        }
    }

    fn build(&mut self) -> Filter {
        if let Some(filter) = self.filter {
            return filter;
        }
        let mut branches = Vec::new();
        for (name, child) in &mut self.children {
            let name = name.as_str();
            if child.terminal {
                branches.push(persist::to_filter(Op::File(name.into(), name.into())));
            }
            if !child.children.is_empty() {
                let inner = child.build();
                let mut chain = vec![persist::to_filter(Op::Subdir(name.into()))];
                match persist::to_op_ref(inner) {
                    Op::Chain(nested) => chain.extend(nested.iter().copied()),
                    _ => chain.push(inner),
                }
                chain.push(persist::to_filter(Op::Prefix(name.into())));
                branches.push(opt::remember_optimized(persist::to_filter(Op::Chain(
                    chain,
                ))));
            }
        }
        let result = match branches.as_slice() {
            [] => persist::to_filter(Op::Empty),
            [only] => *only,
            _ => opt::remember_optimized(persist::to_filter(Op::Compose(branches))),
        };
        self.filter = Some(result);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_insertions_and_removals_preserve_canonical_identity() {
        let choices = [
            "a",
            "a/one",
            "a/two",
            "a/deep/three",
            "a-b/x",
            "b/space name",
            "b/[literal]",
            ".hidden/x",
            "unicode/é",
            "unicode/日本語",
            "root",
            "z",
        ];
        let mut selection = FileSelection::default();
        // Gray-code transitions add/remove one path; reversing the sequence
        // exercises both directions through every ancestor/descendant overlap.
        for index in (0..1 << choices.len()).chain((0..1 << choices.len()).rev()) {
            let bits = index ^ (index >> 1);
            let mut paths: Vec<_> = choices
                .iter()
                .enumerate()
                .filter(|(i, _)| bits & (1 << i) != 0)
                .map(|(_, path)| *path)
                .collect();
            paths.sort_unstable();
            let actual = selection.update(paths.clone());
            let expected = filter::files(paths);
            assert_eq!(actual, expected, "transition {index}");
            assert_eq!(actual.id(), expected.id());
        }
    }

    #[test]
    fn fallback_inputs_and_return_to_incremental_mode_preserve_files_behavior() {
        let mut selection = FileSelection::default();
        for paths in [
            vec!["a/one", "a/two"],
            vec!["a/two", "a/one"],
            vec!["a", "a"],
            vec!["a/one", "a"],
            vec!["a", "a/one"],
            vec![],
            vec!["/a"],
            vec![""],
            vec!["a//b"],
            vec!["a/./b"],
            vec!["a/../b"],
            vec!["a/"],
            vec!["a\0b"],
            vec![r"back\slash", "drive:c"],
            vec!["a/new", "b"],
        ] {
            let expected = filter::files(paths.clone());
            let actual = selection.update(paths);
            assert_eq!(actual, expected);
            assert_eq!(actual.id(), expected.id());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = PathBuf::from(std::ffi::OsString::from_vec(b"bad-\xff".to_vec()));
            assert_eq!(selection.update([raw.clone()]), filter::files([raw]));
            assert_eq!(
                selection.update(["again/valid"]),
                filter::files(["again/valid"])
            );
        }
    }
}
