//! Working-copy projection and status (`DESIGN.md §8`, §9).
//!
//! Two directions:
//!
//! * [`materialize`] writes a [`Tree`](crate::object::Tree)'s content out to a
//!   destination directory (the plain `restore` / `checkout` projection; the
//!   lazy `ProjFS` projection is a separate module per `DESIGN.md §10`).
//! * [`status`] compares an on-disk working directory against a tree and reports
//!   the added / modified / deleted repo-relative paths.
//!
//! ## Materialize overwrite policy
//!
//! `materialize` is **additive and overwriting, never deleting**: it creates any
//! missing directories, writes every file the tree contains (overwriting a
//! file already at that path), and recreates symlinks. It does **not** remove
//! files or directories that already exist in `dest` but are absent from the
//! tree. This keeps the operation non-destructive of untracked content
//! (constitution §3); callers that want an exact mirror of the tree should
//! materialize into a fresh, empty directory.
//!
//! On Windows, creating a symlink can require elevated privilege. `materialize`
//! attempts a real symlink and, if the OS refuses, falls back to writing the
//! link's target string as a regular file so no content is lost.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::blob::read_blob;
use crate::chunker::chunk_ranges;
use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::ignore::IgnoreRules;
use crate::object::{Blob, Chunk, ChunkRef, EntryKind, TreeEntry};
use crate::store::ObjectStore;

/// The difference between an on-disk working directory and a tree.
///
/// All paths are repo-relative. Each vector is sorted ascending for
/// deterministic output.
///
/// * `added` — files present on disk but absent from the tree
/// * `modified` — files present in both whose on-disk content differs
/// * `deleted` — files present in the tree but absent from disk
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Files on disk that the tree does not contain.
    added: Vec<PathBuf>,
    /// Files in both whose content differs.
    modified: Vec<PathBuf>,
    /// Files in the tree that are missing from disk.
    deleted: Vec<PathBuf>,
}

impl Status {
    /// Files present on disk but not in the tree.
    pub fn added(&self) -> &[PathBuf] {
        &self.added
    }

    /// Files present in both but with changed content.
    pub fn modified(&self) -> &[PathBuf] {
        &self.modified
    }

    /// Files present in the tree but missing from disk.
    pub fn deleted(&self) -> &[PathBuf] {
        &self.deleted
    }

    /// Returns `true` if the working directory matches the tree exactly.
    pub const fn is_clean(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    fn sort(&mut self) {
        self.added.sort();
        self.modified.sort();
        self.deleted.sort();
    }
}

/// Writes the content of the [`Tree`](crate::object::Tree) at `tree_id` into
/// `dest_dir`.
///
/// See the module docs for the **overwrite policy** (additive + overwriting,
/// never deleting). `dest_dir` is created if it does not exist.
///
/// # Errors
///
/// * [`Error::ObjectNotFound`] if the tree or any referenced object is missing.
/// * [`Error::Corruption`] if an object fails verification or decoding.
/// * [`Error::Io`] for any filesystem write failure.
pub fn materialize(store: &ObjectStore, tree_id: &ObjectId, dest_dir: impl AsRef<Path>) -> Result<()> {
    let dest = dest_dir.as_ref();
    std::fs::create_dir_all(dest)?;
    // Canonicalize the root once so the per-entry containment check below
    // compares against a stable, symlink-resolved absolute path.
    let root = std::fs::canonicalize(dest)?;
    materialize_into(store, tree_id, dest, &root)
}

/// Faithfully re-projects `target_tree` over `dest_dir`, deleting tracked files
/// that `prev_tree` had but `target_tree` does not (`DESIGN.md §9` step 4).
///
/// Plain [`materialize`] is additive (it never deletes), which is correct for a
/// checkout into an arbitrary directory but **wrong** for re-projecting the
/// controlled working copy after `restore`/`undo`: a file that existed in the
/// pre-restore tree but is absent from the restored tree would linger on disk,
/// so the working copy would not actually equal the restored tree (and a
/// follow-up auto-snapshot would silently resurrect it).
///
/// This computes the deletion set by diffing `prev_tree` against `target_tree`
/// (the diff's `removed` set is exactly the tracked paths to drop), materializes
/// `target_tree`, then removes those paths and prunes directories left empty.
/// Untracked / ignored files are **never** touched — only paths the previous
/// tree tracked are eligible for deletion, keeping the projection
/// non-destructive of untracked content (`constitution.md §3`).
///
/// # Errors
///
/// * [`Error::ObjectNotFound`] / [`Error::Corruption`] if a tree or referenced
///   object is missing or fails verification.
/// * [`Error::Io`] for any filesystem write or delete failure.
pub fn project(
    store: &ObjectStore,
    prev_tree: &ObjectId,
    target_tree: &ObjectId,
    dest_dir: impl AsRef<Path>,
) -> Result<()> {
    let dest = dest_dir.as_ref();

    // Paths the previous tree tracked but the target does not: these are the
    // tracked deletions. Computed before materialize so the comparison is purely
    // tree-vs-tree (untracked disk files never enter the deletion set).
    let removed = crate::diff::diff_trees(store, prev_tree, target_tree)?
        .removed()
        .to_vec();

    materialize(store, target_tree, dest)?;

    for rel in &removed {
        let path = dest.join(rel);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Already gone (e.g. user deleted it): nothing to do.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::Io(e)),
        }
        prune_empty_parents(dest, rel);
    }
    Ok(())
}

/// Removes directories left empty after deleting `rel` under `dest`, walking up
/// toward `dest` but never removing `dest` itself.
///
/// A non-empty directory (still holding untracked or sibling tracked files)
/// stops the walk: `remove_dir` only succeeds on an empty directory, so untracked
/// content is preserved automatically. Best-effort — a failure to prune a
/// directory is not fatal to the projection.
fn prune_empty_parents(dest: &Path, rel: &Path) {
    let mut current = rel.parent();
    while let Some(parent) = current {
        if parent.as_os_str().is_empty() {
            break;
        }
        let dir = dest.join(parent);
        if std::fs::remove_dir(&dir).is_err() {
            // Non-empty or otherwise unremovable → stop pruning up this branch.
            break;
        }
        current = parent.parent();
    }
}

/// Recursive worker for [`materialize`].
///
/// `dest` is the directory the current tree level writes into; `root` is the
/// once-canonicalized destination the whole materialize is confined to. Every
/// resolved write path is confirmed to stay under `root` before any filesystem
/// write, so even a tree whose name validation was somehow bypassed cannot
/// escape the destination.
fn materialize_into(store: &ObjectStore, tree_id: &ObjectId, dest: &Path, root: &Path) -> Result<()> {
    let tree = store.get_tree(tree_id)?;
    for entry in tree.entries() {
        let path = dest.join(entry.name());
        confirm_under_root(&path, root)?;
        match entry.kind() {
            EntryKind::Tree => {
                std::fs::create_dir_all(&path)?;
                materialize_into(store, &entry.id(), &path, root)?;
            }
            EntryKind::Blob => {
                let bytes = read_blob(store, &entry.id())?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, bytes)?;
            }
            EntryKind::Symlink => {
                let target_bytes = read_blob(store, &entry.id())?;
                let target = String::from_utf8(target_bytes)
                    .map_err(|_| Error::Corruption("symlink target is not utf-8".to_string()))?;
                write_symlink(&path, &target, root)?;
            }
        }
    }
    Ok(())
}

/// Confirms that `path` (a child of an in-root directory, not yet created)
/// resolves to a location under `root`.
///
/// The leaf may not exist yet, so we canonicalize the nearest existing ancestor
/// (which `materialize_into` always keeps inside `root`) and re-join the
/// not-yet-created tail. A path that escapes — via a `..` component, an absolute
/// component, or a drive/UNC prefix that `Path::join` lets drop the base — is
/// reported as [`Error::Corruption`].
fn confirm_under_root(path: &Path, root: &Path) -> Result<()> {
    // Canonicalize the deepest ancestor that already exists; everything below
    // it is brand-new and cannot itself be a pre-existing symlink escape.
    let mut existing = path;
    let tail = loop {
        if existing.exists() {
            break path.strip_prefix(existing).ok();
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break None,
        }
    };

    let canonical_base = std::fs::canonicalize(existing)?;
    let resolved = match tail {
        Some(tail) => canonical_base.join(tail),
        None => canonical_base,
    };

    if resolved.starts_with(root) {
        Ok(())
    } else {
        Err(Error::Corruption(format!(
            "tree entry resolves to {} which escapes the destination root {}",
            resolved.display(),
            root.display()
        )))
    }
}

/// Creates a symlink at `path` pointing to `target`, falling back to a plain
/// file containing the target string if the OS refuses to create the link.
///
/// `target` is validated against `root`: an absolute target, or a relative
/// target that — resolved against the link's own directory — escapes `root`, is
/// rejected as [`Error::Corruption`]. v0 tracked symlinks must be intra-repo;
/// recreating an arbitrary out-of-tree link is a redirect/write-through
/// primitive (a later materialize could write a blob *through* a planted link),
/// so we refuse it at this trust boundary rather than plant it.
fn write_symlink(path: &Path, target: &str, root: &Path) -> Result<()> {
    confirm_symlink_target_in_root(path, target, root)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove any existing entry so the create does not fail on overwrite.
    let _ = std::fs::remove_file(path);

    // The OS may refuse (commonly on Windows without privilege): preserve the
    // target by writing it as a regular file so no information is lost.
    if create_symlink(path, target).is_err() {
        std::fs::write(path, target.as_bytes())?;
    }
    Ok(())
}

/// Confirms a symlink `target` stays inside `root` when resolved relative to the
/// directory holding the link at `path`.
///
/// Rejects absolute targets and drive/UNC-prefixed targets outright (they can
/// never be intra-repo and `Path::join` would let them drop the base), then
/// resolves the relative target lexically against the link's parent and confirms
/// it lands under `root`. The check is lexical for the `..`/prefix rules; the
/// final containment uses [`confirm_under_root`] so it matches the canonicalized
/// root the rest of `materialize` is confined to.
fn confirm_symlink_target_in_root(path: &Path, target: &str, root: &Path) -> Result<()> {
    use std::path::Component;
    let target_path = Path::new(target);
    let first = target_path.components().next();
    if target_path.is_absolute()
        || matches!(first, Some(Component::Prefix(_) | Component::RootDir))
    {
        return Err(Error::Corruption(format!(
            "symlink target {target:?} is absolute; v0 tracked symlinks must be intra-repo"
        )));
    }

    // Resolve relative to the link's own directory and confirm containment with
    // the same canonicalize-nearest-ancestor logic used for every write path.
    let base = path.parent().unwrap_or(root);
    let resolved = base.join(target_path);
    confirm_under_root(&resolved, root)
}

#[cfg(unix)]
fn create_symlink(path: &Path, target: &str) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn create_symlink(path: &Path, target: &str) -> std::io::Result<()> {
    // We cannot know whether the target is a dir or file without resolving it;
    // a file symlink is the safe default for v0 (most tracked symlinks point at
    // files, and the fallback covers the rest).
    std::os::windows::fs::symlink_file(target, path)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_path: &Path, _target: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlinks unsupported on this platform",
    ))
}

/// Compares the working directory at `work_dir` against the tree at
/// `current_tree_id`, returning the [`Status`].
///
/// The walk honors `ignore` (and always skips `.tack/`). A file's identity is
/// its [`Blob`] id, computed by chunking the on-disk bytes exactly as
/// [`store_file_bytes`](crate::blob::store_file_bytes) would — **without writing
/// anything to the store**, so `status` is read-only.
///
/// Only regular files and symlinks are compared; the directory structure itself
/// is implied by its files. A symlink on disk is compared against a tree
/// [`EntryKind::Symlink`] entry by hashing its target the same way the builder
/// does.
///
/// # Errors
///
/// * [`Error::ObjectNotFound`] / [`Error::Corruption`] if the tree cannot be
///   read.
/// * [`Error::Io`] for filesystem read failures.
/// * [`Error::Corruption`] if a working-copy file name is not valid UTF-8.
pub fn status(
    store: &ObjectStore,
    current_tree_id: &ObjectId,
    work_dir: impl AsRef<Path>,
    ignore: &IgnoreRules,
) -> Result<Status> {
    // Flatten the tree to repo-relative path -> blob id (files + symlinks).
    let mut tree_files: BTreeMap<PathBuf, ObjectId> = BTreeMap::new();
    flatten_tree(store, current_tree_id, Path::new(""), &mut tree_files)?;

    // Flatten the working dir to repo-relative path -> on-disk blob id.
    let mut disk_files: BTreeMap<PathBuf, ObjectId> = BTreeMap::new();
    flatten_dir(work_dir.as_ref(), Path::new(""), ignore, &mut disk_files)?;

    let mut status = Status::default();

    for (path, disk_id) in &disk_files {
        match tree_files.get(path) {
            None => status.added.push(path.clone()),
            Some(tree_id) if tree_id != disk_id => status.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in tree_files.keys() {
        if !disk_files.contains_key(path) {
            status.deleted.push(path.clone());
        }
    }

    status.sort();
    Ok(status)
}

/// Flattens a tree into `out`, mapping every file/symlink path to its blob id.
fn flatten_tree(
    store: &ObjectStore,
    tree_id: &ObjectId,
    prefix: &Path,
    out: &mut BTreeMap<PathBuf, ObjectId>,
) -> Result<()> {
    let tree = store.get_tree(tree_id)?;
    for entry in tree.entries() {
        let path = prefix.join(entry.name());
        match entry.kind() {
            EntryKind::Tree => flatten_tree(store, &entry.id(), &path, out)?,
            EntryKind::Blob | EntryKind::Symlink => {
                out.insert(path, entry.id());
            }
        }
    }
    Ok(())
}

/// Flattens a working directory into `out`, mapping every non-ignored file/
/// symlink to the blob id its content *would* hash to, without storing anything.
fn flatten_dir(
    abs: &Path,
    rel: &Path,
    ignore: &IgnoreRules,
    out: &mut BTreeMap<PathBuf, ObjectId>,
) -> Result<()> {
    for dir_entry in std::fs::read_dir(abs)? {
        let dir_entry = dir_entry?;
        let file_name = dir_entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| {
                Error::Corruption(format!("non-utf8 file name: {}", file_name.display()))
            })?;
        let child_rel = rel.join(name);

        let meta = std::fs::symlink_metadata(dir_entry.path())?;
        let file_type = meta.file_type();
        let is_dir = file_type.is_dir();

        if ignore.is_ignored(&child_rel, is_dir) {
            continue;
        }

        if file_type.is_symlink() {
            let target = std::fs::read_link(dir_entry.path())?;
            let target_bytes = target.to_string_lossy().into_owned().into_bytes();
            out.insert(child_rel, blob_id_of(&target_bytes)?);
        } else if is_dir {
            flatten_dir(&dir_entry.path(), &child_rel, ignore, out)?;
        } else if file_type.is_file() {
            let bytes = std::fs::read(dir_entry.path())?;
            out.insert(child_rel, blob_id_of(&bytes)?);
        }
        // Other file types (sockets, fifos) have no tree representation; skip.
    }
    Ok(())
}

/// Computes the `BlobId` that `bytes` would receive from
/// [`store_file_bytes`](crate::blob::store_file_bytes), **without** storing any
/// chunk or blob object.
///
/// This must mirror `store_file_bytes` exactly (same chunking, same `ChunkRef`
/// fields, same `Blob` layout) so that a status comparison against stored blobs
/// is correct.
///
/// # Errors
///
/// Returns [`Error::Corruption`] if a single chunk's length exceeds `u32`
/// (impossible for the configured `MAX_CHUNK`, but checked rather than cast).
fn blob_id_of(bytes: &[u8]) -> Result<ObjectId> {
    let mut chunk_refs = Vec::new();
    for range in chunk_ranges(bytes) {
        let chunk = Chunk::new(bytes[range].to_vec());
        // A single chunk never exceeds MAX_CHUNK (64 KiB), so the length fits a
        // u32; treat any overflow as corruption rather than truncating silently.
        let len = u32::try_from(chunk.data().len())
            .map_err(|_| Error::Corruption("chunk length exceeds u32".to_string()))?;
        chunk_refs.push(ChunkRef::new(chunk.id(), len));
    }
    let blob = Blob::new(bytes.len() as u64, chunk_refs);
    Ok(blob.id())
}

/// Returns the repo-relative path of a [`TreeEntry`] under `prefix` (helper for
/// callers needing the path of a single entry).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn entry_path(prefix: &Path, entry: &TreeEntry) -> PathBuf {
    prefix.join(entry.name())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::store_file_bytes;
    use crate::tree::{build_tree, read_tree_path};
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

    fn empty_ignore() -> IgnoreRules {
        IgnoreRules::empty()
    }

    fn rel_strings(slice: &[PathBuf]) -> Vec<String> {
        slice
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    // ── blob_id_of mirrors store_file_bytes ──────────────────────────────────

    #[test]
    fn blob_id_of_matches_stored_blob_id() -> Result<()> {
        let (_d, store) = temp_store();
        // Use a multi-chunk payload to exercise the chunking path.
        let mut bytes = Vec::new();
        let mut state: u64 = 0x00ab_cdef;
        for _ in 0..(300 * 1024) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.push((state & 0xff) as u8);
        }
        let stored = store_file_bytes(&store, &bytes)?;
        assert_eq!(blob_id_of(&bytes)?, stored, "in-memory id must match stored id");
        Ok(())
    }

    // ── materialize round-trips build_tree ───────────────────────────────────

    #[test]
    fn materialize_reproduces_files() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "top.txt", b"top-content");
        write_file(work.path(), "a/b/deep.txt", b"deep-content");
        write_file(work.path(), "a/sibling.txt", b"sib");

        let root = build_tree(&store, work.path(), &empty_ignore())?;

        let out = TempDir::new()?;
        materialize(&store, &root, out.path())?;

        assert_eq!(fs::read(out.path().join("top.txt"))?, b"top-content");
        assert_eq!(fs::read(out.path().join("a/b/deep.txt"))?, b"deep-content");
        assert_eq!(fs::read(out.path().join("a/sibling.txt"))?, b"sib");

        // Round trip: rebuilding the materialized dir yields the same root id.
        let (_d2, store2) = temp_store();
        let root2 = build_tree(&store2, out.path(), &empty_ignore())?;
        assert_eq!(root, root2, "materialize then build must reproduce the tree id");
        Ok(())
    }

    #[test]
    fn materialize_overwrites_existing_file() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "f.txt", b"new");
        let root = build_tree(&store, work.path(), &empty_ignore())?;

        let out = TempDir::new()?;
        write_file(out.path(), "f.txt", b"stale");
        materialize(&store, &root, out.path())?;
        assert_eq!(fs::read(out.path().join("f.txt"))?, b"new");
        Ok(())
    }

    #[test]
    fn materialize_does_not_delete_untracked() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "tracked.txt", b"t");
        let root = build_tree(&store, work.path(), &empty_ignore())?;

        let out = TempDir::new()?;
        write_file(out.path(), "untracked.txt", b"keep me");
        materialize(&store, &root, out.path())?;
        // Per the documented policy, untracked files survive.
        assert!(out.path().join("untracked.txt").is_file());
        assert!(out.path().join("tracked.txt").is_file());
        Ok(())
    }

    // ── status ───────────────────────────────────────────────────────────────

    #[test]
    fn status_clean_when_matching() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a.txt", b"x");
        write_file(work.path(), "sub/b.txt", b"y");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        let st = status(&store, &root, work.path(), &empty_ignore())?;
        assert!(st.is_clean(), "freshly built dir must be clean, got {st:?}");
        Ok(())
    }

    #[test]
    fn status_detects_added() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a.txt", b"x");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        // Add a new file after the snapshot.
        write_file(work.path(), "new.txt", b"new");
        let st = status(&store, &root, work.path(), &empty_ignore())?;
        assert_eq!(rel_strings(st.added()), vec!["new.txt"]);
        assert!(st.modified().is_empty() && st.deleted().is_empty());
        Ok(())
    }

    #[test]
    fn status_detects_modified() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a.txt", b"before");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        write_file(work.path(), "a.txt", b"after");
        let st = status(&store, &root, work.path(), &empty_ignore())?;
        assert_eq!(rel_strings(st.modified()), vec!["a.txt"]);
        assert!(st.added().is_empty() && st.deleted().is_empty());
        Ok(())
    }

    #[test]
    fn status_detects_deleted() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a.txt", b"x");
        write_file(work.path(), "gone.txt", b"bye");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        fs::remove_file(work.path().join("gone.txt"))?;
        let st = status(&store, &root, work.path(), &empty_ignore())?;
        assert_eq!(rel_strings(st.deleted()), vec!["gone.txt"]);
        assert!(st.added().is_empty() && st.modified().is_empty());
        Ok(())
    }

    #[test]
    fn status_honors_ignore() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a.txt", b"x");
        let ignore = IgnoreRules::parse("*.log\n");
        let root = build_tree(&store, work.path(), &ignore)?;
        // Add an ignored file; status must not report it.
        write_file(work.path(), "noise.log", b"noise");
        let st = status(&store, &root, work.path(), &ignore)?;
        assert!(st.is_clean(), "ignored file must not show in status, got {st:?}");
        Ok(())
    }

    #[test]
    fn status_combined() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "stay.txt", b"same");
        write_file(work.path(), "change.txt", b"v1");
        write_file(work.path(), "drop.txt", b"x");
        let root = build_tree(&store, work.path(), &empty_ignore())?;

        write_file(work.path(), "change.txt", b"v2");
        fs::remove_file(work.path().join("drop.txt"))?;
        write_file(work.path(), "fresh.txt", b"new");

        let st = status(&store, &root, work.path(), &empty_ignore())?;
        assert_eq!(rel_strings(st.added()), vec!["fresh.txt"]);
        assert_eq!(rel_strings(st.modified()), vec!["change.txt"]);
        assert_eq!(rel_strings(st.deleted()), vec!["drop.txt"]);
        Ok(())
    }

    // ── entry_path helper ────────────────────────────────────────────────────

    #[test]
    fn entry_path_joins_prefix() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a/x.txt", b"x");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        let dir_entry = read_tree_path(&store, &root, "a")?;
        let p = entry_path(Path::new("a"), &dir_entry);
        assert_eq!(p.to_string_lossy().replace('\\', "/"), "a/a");
        Ok(())
    }

    // ── symlink materialize round-trip (unix only) ───────────────────────────

    #[cfg(unix)]
    #[test]
    fn symlink_materializes_back() -> Result<()> {
        use std::os::unix::fs::symlink;
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "target.txt", b"data");
        symlink("target.txt", work.path().join("link.txt"))?;
        let root = build_tree(&store, work.path(), &empty_ignore())?;

        let out = TempDir::new()?;
        materialize(&store, &root, out.path())?;
        let link_meta = std::fs::symlink_metadata(out.path().join("link.txt"))?;
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(std::fs::read_link(out.path().join("link.txt"))?, Path::new("target.txt"));
        Ok(())
    }

    // ── path-traversal on materialize (findings 1 & 2) ────────────────────────

    use crate::encoding::{Encode as _, Encoder};
    use crate::hash::TypeTag;
    use crate::object::EntryKind;

    /// Stores a hand-encoded single-entry tree whose entry name is `name`
    /// (bypassing `Tree::new` validation) pointing at the blob `target_blob`,
    /// returning the malicious tree's content address.
    fn put_malicious_tree(store: &ObjectStore, name: &str, kind: EntryKind, target_blob: ObjectId) -> ObjectId {
        let mut enc = Encoder::new();
        enc.array_len(1);
        enc.str(name);
        enc.u32(0o100_644);
        kind.encode(&mut enc);
        enc.object_id(&target_blob);
        store.put_raw(TypeTag::Tree, enc.as_bytes()).expect("put raw tree")
    }

    /// Regression (findings 1 & 2): a crafted tree whose entry name is `".."`
    /// must NOT materialize a file outside the destination root. The write of
    /// the attacker payload into the parent directory used to succeed; it must
    /// now fail (the decode-side validation rejects the name as Corruption) and
    /// leave nothing outside `dest`.
    #[test]
    fn materialize_rejects_dotdot_entry_name() -> Result<()> {
        let (_d, store) = temp_store();
        let payload = store_file_bytes(&store, b"escaped!")?;
        let evil_tree = put_malicious_tree(&store, "..", EntryKind::Blob, payload);

        let scratch = TempDir::new()?;
        let dest = scratch.path().join("nested").join("dest");
        let result = materialize(&store, &evil_tree, &dest);

        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "materializing a `..`-named entry must fail, got {result:?}"
        );
        // The parent of dest must NOT have received the payload.
        let escaped = scratch.path().join("nested").join("escaped!");
        assert!(!escaped.exists(), "payload escaped the destination root to {}", escaped.display());
        Ok(())
    }

    /// Regression (findings 1 & 2): a separator-bearing name is likewise rejected.
    #[test]
    fn materialize_rejects_separator_entry_name() -> Result<()> {
        let (_d, store) = temp_store();
        let payload = store_file_bytes(&store, b"x")?;
        // `a/../../b` style escape collapsed into one component with separators.
        let evil_tree = put_malicious_tree(&store, "..\\..\\evil.txt", EntryKind::Blob, payload);

        let scratch = TempDir::new()?;
        let dest = scratch.path().join("a").join("b").join("dest");
        let result = materialize(&store, &evil_tree, &dest);
        assert!(matches!(result, Err(Error::Corruption(_))), "got {result:?}");
        assert!(!scratch.path().join("a").join("evil.txt").exists());
        assert!(!scratch.path().join("a").join("b").join("evil.txt").exists());
        Ok(())
    }

    /// Regression (finding 6): a symlink whose target escapes the root is
    /// refused on materialize rather than planted as a redirect primitive.
    #[test]
    fn materialize_rejects_escaping_symlink_target() -> Result<()> {
        let (_d, store) = temp_store();
        // A symlink entry whose target climbs out of the destination root.
        let target_blob = store_file_bytes(&store, b"../../outside")?;
        let mut enc = Encoder::new();
        enc.array_len(1);
        enc.str("link");
        enc.u32(0o120_000);
        EntryKind::Symlink.encode(&mut enc);
        enc.object_id(&target_blob);
        let tree = store.put_raw(TypeTag::Tree, enc.as_bytes())?;

        let scratch = TempDir::new()?;
        let dest = scratch.path().join("a").join("b").join("dest");
        let result = materialize(&store, &tree, &dest);
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "escaping symlink target must be refused, got {result:?}"
        );
        Ok(())
    }

    /// Regression (finding 6): an absolute symlink target is refused too.
    #[test]
    fn materialize_rejects_absolute_symlink_target() -> Result<()> {
        let (_d, store) = temp_store();
        let abs = if cfg!(windows) { "C:\\Windows\\System32\\evil" } else { "/etc/evil" };
        let target_blob = store_file_bytes(&store, abs.as_bytes())?;
        let mut enc = Encoder::new();
        enc.array_len(1);
        enc.str("link");
        enc.u32(0o120_000);
        EntryKind::Symlink.encode(&mut enc);
        enc.object_id(&target_blob);
        let tree = store.put_raw(TypeTag::Tree, enc.as_bytes())?;

        let dest = TempDir::new()?;
        let result = materialize(&store, &tree, dest.path());
        assert!(matches!(result, Err(Error::Corruption(_))), "got {result:?}");
        Ok(())
    }

    /// A normal intra-repo relative symlink target still materializes fine.
    #[cfg(unix)]
    #[test]
    fn materialize_allows_intra_repo_symlink_target() -> Result<()> {
        use std::os::unix::fs::symlink;
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "sub/target.txt", b"data");
        symlink("../sub/target.txt", work.path().join("sub").join("link.txt"))?;
        let root = build_tree(&store, work.path(), &empty_ignore())?;

        let out = TempDir::new()?;
        // `../sub/target.txt` from within `sub/` resolves to `sub/target.txt`,
        // which stays under the root — must be allowed.
        materialize(&store, &root, out.path())?;
        let link_meta = std::fs::symlink_metadata(out.path().join("sub").join("link.txt"))?;
        assert!(link_meta.file_type().is_symlink());
        Ok(())
    }

    // ── prune-aware re-projection (finding 4) ─────────────────────────────────

    /// Regression (finding 4): `project` must DELETE a tracked file that the
    /// previous tree had but the target tree lacks, so the working copy becomes a
    /// faithful projection of the target. Plain `materialize` (additive) left it
    /// on disk, silently resurrecting it on the next snapshot.
    #[test]
    fn project_removes_tracked_file_absent_from_target() -> Result<()> {
        let (_d, store) = temp_store();

        // v1 tree: only f1.
        let v1_dir = TempDir::new()?;
        write_file(v1_dir.path(), "f1.txt", b"one");
        let v1 = build_tree(&store, v1_dir.path(), &empty_ignore())?;

        // v2 tree: f1 + f2.
        let v2_dir = TempDir::new()?;
        write_file(v2_dir.path(), "f1.txt", b"one");
        write_file(v2_dir.path(), "f2.txt", b"two");
        let v2 = build_tree(&store, v2_dir.path(), &empty_ignore())?;

        // Put the working copy in the v2 state on disk.
        let work = TempDir::new()?;
        materialize(&store, &v2, work.path())?;
        assert!(work.path().join("f2.txt").is_file());

        // Re-project from v2 back to v1: f2 must be removed.
        project(&store, &v2, &v1, work.path())?;
        assert!(work.path().join("f1.txt").is_file(), "restored file must remain");
        assert!(!work.path().join("f2.txt").exists(), "f2 must be deleted on re-projection to v1");
        Ok(())
    }

    /// Regression (finding 4): `project` must NOT touch untracked files (those
    /// the previous tree never tracked) — only tracked deletions are pruned, per
    /// constitution §3.
    #[test]
    fn project_preserves_untracked_file() -> Result<()> {
        let (_d, store) = temp_store();
        let v1_dir = TempDir::new()?;
        write_file(v1_dir.path(), "f1.txt", b"one");
        let v1 = build_tree(&store, v1_dir.path(), &empty_ignore())?;

        let v2_dir = TempDir::new()?;
        write_file(v2_dir.path(), "f1.txt", b"one");
        write_file(v2_dir.path(), "f2.txt", b"two");
        let v2 = build_tree(&store, v2_dir.path(), &empty_ignore())?;

        let work = TempDir::new()?;
        materialize(&store, &v2, work.path())?;
        // An untracked file appears on disk; it was never in any tree.
        write_file(work.path(), "scratch.tmp", b"keep me");

        project(&store, &v2, &v1, work.path())?;
        assert!(!work.path().join("f2.txt").exists(), "tracked deletion pruned");
        assert!(work.path().join("scratch.tmp").is_file(), "untracked file must survive");
        Ok(())
    }

    /// `project` prunes a directory left empty by a tracked deletion, but keeps a
    /// directory that still holds an untracked file.
    #[test]
    fn project_prunes_emptied_dir_but_keeps_nonempty() -> Result<()> {
        let (_d, store) = temp_store();
        let v1_dir = TempDir::new()?;
        write_file(v1_dir.path(), "root.txt", b"r");
        let v1 = build_tree(&store, v1_dir.path(), &empty_ignore())?;

        let v2_dir = TempDir::new()?;
        write_file(v2_dir.path(), "root.txt", b"r");
        write_file(v2_dir.path(), "dir_a/only.txt", b"a");
        write_file(v2_dir.path(), "dir_b/tracked.txt", b"b");
        let v2 = build_tree(&store, v2_dir.path(), &empty_ignore())?;

        let work = TempDir::new()?;
        materialize(&store, &v2, work.path())?;
        // dir_b also holds an untracked file, so it must survive the prune.
        write_file(work.path(), "dir_b/untracked.txt", b"u");

        project(&store, &v2, &v1, work.path())?;
        assert!(!work.path().join("dir_a").exists(), "emptied tracked dir must be pruned");
        assert!(work.path().join("dir_b").is_dir(), "dir with untracked content must remain");
        assert!(work.path().join("dir_b").join("untracked.txt").is_file());
        Ok(())
    }
}
