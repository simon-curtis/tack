//! Tree-vs-tree diffing (`DESIGN.md §8`).
//!
//! [`diff_trees`] walks two [`Tree`](crate::object::Tree) DAGs in lock-step and
//! reports, at **file granularity**, every repo-relative path that was added,
//! removed, or modified between them. Because trees are content-addressed, an
//! unchanged sub-tree has an identical ID in both sides and is skipped wholesale
//! — diffing two large trees that share most of their structure touches only the
//! parts that actually differ.
//!
//! A path is:
//!
//! * **added** — present in `to`, absent in `from`
//! * **removed** — present in `from`, absent in `to`
//! * **modified** — present in both as files but pointing at different content,
//!   or present in both but with a changed kind (e.g. file → symlink, or
//!   file → directory, in which case the old file is *removed* and the new
//!   directory's files are *added*)
//!
//! Paths use forward slashes and are sorted within each category for
//! determinism.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::hash::ObjectId;
use crate::object::{EntryKind, Tree, TreeEntry};
use crate::store::ObjectStore;

/// The result of diffing two trees: file-level paths grouped by change kind.
///
/// Each vector is sorted ascending by path for deterministic output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    /// Paths present only in the `to` tree.
    added: Vec<PathBuf>,
    /// Paths present only in the `from` tree.
    removed: Vec<PathBuf>,
    /// Paths present in both but with differing content or kind.
    modified: Vec<PathBuf>,
}

impl TreeDiff {
    /// Returns the paths added in the `to` tree.
    pub fn added(&self) -> &[PathBuf] {
        &self.added
    }

    /// Returns the paths removed relative to the `from` tree.
    pub fn removed(&self) -> &[PathBuf] {
        &self.removed
    }

    /// Returns the paths whose content or kind changed.
    pub fn modified(&self) -> &[PathBuf] {
        &self.modified
    }

    /// Returns `true` if the two trees were identical (no changes).
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    /// Sorts every category ascending by path.
    fn sort(&mut self) {
        self.added.sort();
        self.removed.sort();
        self.modified.sort();
    }
}

/// Diffs the tree rooted at `from_tree` against the tree rooted at `to_tree`,
/// returning a file-level [`TreeDiff`].
///
/// If the two roots are the same `ObjectId` the diff is empty without any I/O.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`](crate::Error::ObjectNotFound) if either
/// root or any reachable sub-tree is missing, or
/// [`Error::Corruption`](crate::Error::Corruption) if an object fails
/// verification.
pub fn diff_trees(store: &ObjectStore, from_tree: &ObjectId, to_tree: &ObjectId) -> Result<TreeDiff> {
    let mut diff = TreeDiff::default();
    diff_into(store, from_tree, to_tree, Path::new(""), &mut diff)?;
    diff.sort();
    Ok(diff)
}

/// Recursive worker: diffs two sub-trees rooted at the same `prefix` path.
fn diff_into(
    store: &ObjectStore,
    from_tree: &ObjectId,
    to_tree: &ObjectId,
    prefix: &Path,
    diff: &mut TreeDiff,
) -> Result<()> {
    // Identical sub-trees cannot contain any change — content addressing.
    if from_tree == to_tree {
        return Ok(());
    }

    let from = store.get_tree(from_tree)?;
    let to = store.get_tree(to_tree)?;

    let from_map = entry_map(&from);
    let to_map = entry_map(&to);

    // Removed and modified: iterate the `from` side.
    for (name, from_entry) in &from_map {
        let child = prefix.join(name);
        match to_map.get(*name) {
            None => collect_paths(store, from_entry, &child, &mut diff.removed)?,
            Some(to_entry) => {
                diff_pair(store, from_entry, to_entry, &child, diff)?;
            }
        }
    }

    // Added: entries present only on the `to` side.
    for (name, to_entry) in &to_map {
        if !from_map.contains_key(*name) {
            let child = prefix.join(name);
            collect_paths(store, to_entry, &child, &mut diff.added)?;
        }
    }

    Ok(())
}

/// Diffs two entries with the same name living at `path` in both trees.
fn diff_pair(
    store: &ObjectStore,
    from_entry: &TreeEntry,
    to_entry: &TreeEntry,
    path: &Path,
    diff: &mut TreeDiff,
) -> Result<()> {
    match (from_entry.kind(), to_entry.kind()) {
        // Both directories — recurse (cheap if the sub-tree ids match).
        (EntryKind::Tree, EntryKind::Tree) => {
            diff_into(store, &from_entry.id(), &to_entry.id(), path, diff)
        }
        // Both non-directories (file/symlink): a content or kind change is a
        // modification iff the (kind, id) pair differs.
        (a, b) if a != EntryKind::Tree && b != EntryKind::Tree => {
            if from_entry.id() != to_entry.id() || from_entry.kind() != to_entry.kind() {
                diff.modified.push(path.to_path_buf());
            }
            Ok(())
        }
        // Kind crossed the file/directory boundary: the old thing is removed and
        // the new thing is added, recursively.
        _ => {
            collect_paths(store, from_entry, path, &mut diff.removed)?;
            collect_paths(store, to_entry, path, &mut diff.added)?;
            Ok(())
        }
    }
}

/// Collects every file-level path under `entry` (rooted at `path`) into `out`.
///
/// For a file or symlink this is just `path`; for a directory it recurses into
/// the whole sub-tree so a wholesale add/remove is reported at file granularity.
fn collect_paths(
    store: &ObjectStore,
    entry: &TreeEntry,
    path: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    if entry.kind() == EntryKind::Tree {
        let tree = store.get_tree(&entry.id())?;
        for child in tree.entries() {
            collect_paths(store, child, &path.join(child.name()), out)?;
        }
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}

/// Builds a name → entry lookup for a tree (entries are already sorted; a map
/// gives O(1) presence checks during the lock-step walk).
fn entry_map(tree: &Tree) -> BTreeMap<&str, &TreeEntry> {
    tree.entries().iter().map(|e| (e.name(), e)).collect()
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore::IgnoreRules;
    use crate::tree::build_tree;
    use std::fs;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, ObjectStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = ObjectStore::init(dir.path().join(".tack")).expect("init store");
        (dir, store)
    }

    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdirs");
        }
        fs::write(path, content).expect("write");
    }

    /// Builds a tree from a closure that populates a fresh working dir.
    fn tree_of(store: &ObjectStore, populate: impl FnOnce(&Path)) -> ObjectId {
        let work = TempDir::new().expect("work dir");
        populate(work.path());
        build_tree(store, work.path(), &IgnoreRules::empty()).expect("build tree")
    }

    fn paths(slice: &[PathBuf]) -> Vec<String> {
        slice
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    // ── identical ────────────────────────────────────────────────────────────

    #[test]
    fn identical_trees_diff_empty() -> Result<()> {
        let (_d, store) = temp_store();
        let a = tree_of(&store, |r| {
            write_file(r, "a.txt", b"x");
            write_file(r, "sub/b.txt", b"y");
        });
        let diff = diff_trees(&store, &a, &a)?;
        assert!(diff.is_empty());
        Ok(())
    }

    // ── add ──────────────────────────────────────────────────────────────────

    #[test]
    fn added_file_detected() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "a.txt", b"x"));
        let to = tree_of(&store, |r| {
            write_file(r, "a.txt", b"x");
            write_file(r, "b.txt", b"new");
        });
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.added()), vec!["b.txt"]);
        assert!(diff.removed().is_empty());
        assert!(diff.modified().is_empty());
        Ok(())
    }

    #[test]
    fn added_directory_reports_files() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "a.txt", b"x"));
        let to = tree_of(&store, |r| {
            write_file(r, "a.txt", b"x");
            write_file(r, "newdir/c.txt", b"c");
            write_file(r, "newdir/d.txt", b"d");
        });
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.added()), vec!["newdir/c.txt", "newdir/d.txt"]);
        Ok(())
    }

    // ── remove ───────────────────────────────────────────────────────────────

    #[test]
    fn removed_file_detected() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| {
            write_file(r, "a.txt", b"x");
            write_file(r, "gone.txt", b"bye");
        });
        let to = tree_of(&store, |r| write_file(r, "a.txt", b"x"));
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.removed()), vec!["gone.txt"]);
        assert!(diff.added().is_empty());
        Ok(())
    }

    // ── modify ───────────────────────────────────────────────────────────────

    #[test]
    fn modified_file_detected() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "a.txt", b"old"));
        let to = tree_of(&store, |r| write_file(r, "a.txt", b"new"));
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.modified()), vec!["a.txt"]);
        assert!(diff.added().is_empty());
        assert!(diff.removed().is_empty());
        Ok(())
    }

    #[test]
    fn nested_modify_detected() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| {
            write_file(r, "keep.txt", b"k");
            write_file(r, "a/b/c.txt", b"v1");
        });
        let to = tree_of(&store, |r| {
            write_file(r, "keep.txt", b"k");
            write_file(r, "a/b/c.txt", b"v2");
        });
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.modified()), vec!["a/b/c.txt"]);
        Ok(())
    }

    // ── combined ─────────────────────────────────────────────────────────────

    #[test]
    fn combined_add_remove_modify() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| {
            write_file(r, "stay.txt", b"same");
            write_file(r, "change.txt", b"before");
            write_file(r, "remove.txt", b"x");
        });
        let to = tree_of(&store, |r| {
            write_file(r, "stay.txt", b"same");
            write_file(r, "change.txt", b"after");
            write_file(r, "add.txt", b"new");
        });
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.added()), vec!["add.txt"]);
        assert_eq!(paths(diff.removed()), vec!["remove.txt"]);
        assert_eq!(paths(diff.modified()), vec!["change.txt"]);
        Ok(())
    }

    // ── unchanged subtree is skipped (correctness, not just perf) ─────────────

    #[test]
    fn unchanged_subtree_yields_no_paths() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| {
            write_file(r, "lib/a.txt", b"shared");
            write_file(r, "lib/b.txt", b"shared2");
            write_file(r, "main.txt", b"one");
        });
        let to = tree_of(&store, |r| {
            write_file(r, "lib/a.txt", b"shared");
            write_file(r, "lib/b.txt", b"shared2");
            write_file(r, "main.txt", b"two");
        });
        let diff = diff_trees(&store, &from, &to)?;
        assert_eq!(paths(diff.modified()), vec!["main.txt"]);
        assert!(diff.added().is_empty());
        assert!(diff.removed().is_empty());
        Ok(())
    }
}
