//! Directory ⇄ [`Tree`] building and walking (`DESIGN.md §4`, §8).
//!
//! [`build_tree`] walks a working directory bottom-up, honoring an
//! [`IgnoreRules`] set, storing each regular file's bytes as a [`Blob`] (see
//! [`store_file_bytes`](crate::blob::store_file_bytes)) and each symlink's
//! target as a one-blob payload, then assembling nested [`Tree`] objects and
//! returning the root `TreeId`.
//!
//! Because [`Tree`] sorts and hashes its entries canonically, the resulting
//! root ID is **independent of directory-iteration order**: the same directory
//! content always yields the same root tree. Unchanged sub-directories share
//! their sub-tree object across snapshots for free (content addressing).
//!
//! ## File modes (`DESIGN.md §4`)
//!
//! * regular file → `0o100644` (executable bit is best-effort; on Windows there
//!   is no POSIX exec bit, so files are always `0o100644`)
//! * symlink → `0o120000`, stored as [`EntryKind::Symlink`] whose blob holds the
//!   raw link target bytes
//! * directory → `0o040000`, stored as [`EntryKind::Tree`]
//!
//! [`read_tree_path`] resolves a repo-relative path to its [`TreeEntry`].
//! [`list_tree`] returns one tree's immediate entries.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::blob::store_file_bytes;
use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::ignore::IgnoreRules;
use crate::object::{EntryKind, Tree, TreeEntry, tree_entry};
use crate::statcache::StatCache;
use crate::store::ObjectStore;

/// File mode for a regular file (non-executable). Windows has no exec bit, so
/// this is the default for every regular file (`DESIGN.md §4`).
const MODE_FILE: u32 = 0o100_644;

/// File mode for an executable regular file (best-effort; POSIX only).
#[cfg_attr(windows, allow(dead_code))]
const MODE_EXEC: u32 = 0o100_755;

/// File mode for a symbolic link.
const MODE_SYMLINK: u32 = 0o120_000;

/// File mode for a directory.
const MODE_DIR: u32 = 0o040_000;

/// Builds a [`Tree`] DAG from the directory at `dir`, returning the root
/// `TreeId`.
///
/// The walk is recursive and honors `ignore`: any path for which
/// [`IgnoreRules::is_ignored`] returns `true` (and `.tack/` always) is skipped,
/// including its whole subtree. Regular files become [`Blob`](crate::object::Blob)
/// objects; symlinks become symlink-target blobs; sub-directories become nested
/// [`Tree`] objects. Empty directories are preserved as empty sub-trees.
///
/// The returned root ID is deterministic: it depends only on the directory's
/// content, not on filesystem iteration order.
///
/// # Errors
///
/// * [`Error::Io`] for any filesystem read failure.
/// * [`Error::InvalidTreeEntryName`] if a directory entry's name is not a valid
///   single path component (e.g. it is `.` or `..`).
/// * [`Error::Corruption`] if a file name is not valid UTF-8 (tack tree entry
///   names are UTF-8 strings).
pub fn build_tree(store: &ObjectStore, dir: impl AsRef<Path>, ignore: &IgnoreRules) -> Result<ObjectId> {
    build_tree_inner(store, dir.as_ref(), Path::new(""), ignore, None)
}

/// Like [`build_tree`], but consults and updates a [`StatCache`] so files whose
/// on-disk `(mtime, size)` are unchanged skip re-reading and re-chunking.
///
/// The cache is **advisory**: the returned root tree id is identical to
/// [`build_tree`]'s for the same on-disk content — only the work done to reach
/// it differs. Entries for files seen this call are recorded in `cache`; the
/// caller is responsible for persisting it via [`StatCache::save`].
///
/// # Errors
///
/// Same as [`build_tree`].
pub fn build_tree_cached(
    store: &ObjectStore,
    dir: impl AsRef<Path>,
    ignore: &IgnoreRules,
    cache: &mut StatCache,
) -> Result<ObjectId> {
    build_tree_inner(store, dir.as_ref(), Path::new(""), ignore, Some(cache))
}

/// Recursive worker for [`build_tree`] / [`build_tree_cached`].
///
/// `abs` is the directory on disk; `rel` is its repo-relative path (empty for
/// the root) — `rel` is what the ignore rules are matched against, and (slashed)
/// the key into `cache` when present.
fn build_tree_inner(
    store: &ObjectStore,
    abs: &Path,
    rel: &Path,
    ignore: &IgnoreRules,
    mut cache: Option<&mut StatCache>,
) -> Result<ObjectId> {
    let mut entries: Vec<TreeEntry> = Vec::new();

    for dir_entry in std::fs::read_dir(abs)? {
        let dir_entry = dir_entry?;
        let file_name = dir_entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| {
                Error::Corruption(format!("non-utf8 file name: {}", file_name.display()))
            })?;

        let child_rel = rel.join(name);

        // symlink_metadata does NOT follow links — we must record the link
        // itself, not its target.
        let meta = std::fs::symlink_metadata(dir_entry.path())?;
        let file_type = meta.file_type();
        let is_dir = file_type.is_dir();

        if ignore.is_ignored(&child_rel, is_dir) {
            continue;
        }

        let entry = if file_type.is_symlink() {
            symlink_entry(store, &dir_entry.path(), name)?
        } else if is_dir {
            let sub_id =
                build_tree_inner(store, &dir_entry.path(), &child_rel, ignore, cache.as_deref_mut())?;
            tree_entry(name, MODE_DIR, EntryKind::Tree, sub_id)?
        } else if file_type.is_file() {
            let rel_slash = path_to_slash(&child_rel);
            file_entry(store, &dir_entry.path(), name, &rel_slash, &meta, cache.as_deref_mut())?
        } else {
            // FIFOs, sockets, devices, etc. have no representation in the tree
            // model — skip them rather than fail the whole build.
            continue;
        };

        entries.push(entry);
    }

    // Tree::new sorts canonically and validates names; store and return the id.
    let tree = Tree::new(entries)?;
    store.put_tree(&tree)
}

/// Builds a [`TreeEntry`] for a regular file, storing its bytes as a blob.
///
/// When `cache` is present, an unchanged, non-racy file (`rel_slash`'s `(mtime,
/// size)` match the cache) reuses its recorded blob id without reading the file;
/// otherwise the file is read and chunked. Either way the fresh fingerprint is
/// recorded so it persists. The mode is always recomputed from disk (it is
/// content-independent), so a permission-only change is never masked.
fn file_entry(
    store: &ObjectStore,
    path: &Path,
    name: &str,
    rel_slash: &str,
    meta: &std::fs::Metadata,
    cache: Option<&mut StatCache>,
) -> Result<TreeEntry> {
    let size = meta.len();
    let mtime = meta.modified().ok();
    let mode = file_mode(path);

    // Fast path: reuse a cached blob id when the file is provably unchanged.
    // Resolve the lookup first (immutable borrow) so it ends before recording.
    let reused = if let (Some(c), Some(mt)) = (cache.as_deref(), mtime) {
        c.reuse(rel_slash, mt, size)
    } else {
        None
    };

    let blob_id = if let Some(id) = reused {
        id
    } else {
        let bytes = std::fs::read(path)?;
        store_file_bytes(store, &bytes)?
    };

    // Record the fresh fingerprint so it persists (consumes `cache`, last use).
    if let (Some(c), Some(mt)) = (cache, mtime) {
        c.record(rel_slash, mt, size, blob_id);
    }
    tree_entry(name, mode, EntryKind::Blob, blob_id)
}

/// Builds a [`TreeEntry`] for a symlink, storing its target path as a blob.
fn symlink_entry(store: &ObjectStore, path: &Path, name: &str) -> Result<TreeEntry> {
    let target = std::fs::read_link(path)?;
    // The link target is stored as its raw string bytes (forward/back slashes
    // preserved verbatim; we do not canonicalize a link target).
    let target_bytes = target.to_string_lossy().into_owned().into_bytes();
    let blob_id = store_file_bytes(store, &target_bytes)?;
    tree_entry(name, MODE_SYMLINK, EntryKind::Symlink, blob_id)
}

/// Determines a regular file's mode. On Windows there is no executable bit, so
/// this always returns [`MODE_FILE`]; on Unix it inspects the owner-exec bit.
#[cfg(unix)]
fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::metadata(path) {
        Ok(meta) if meta.permissions().mode() & 0o100 != 0 => MODE_EXEC,
        _ => MODE_FILE,
    }
}

/// Windows file mode: always non-executable (`DESIGN.md §4`).
#[cfg(not(unix))]
const fn file_mode(_path: &Path) -> u32 {
    MODE_FILE
}

/// Resolves a repo-relative `rel_path` against the tree rooted at `root`,
/// returning the [`TreeEntry`] it names.
///
/// An empty path is not a valid entry (the root tree itself has no entry); use
/// [`list_tree`] for the root's children. Path components are split on both `/`
/// and `\\`. Intermediate components must be directories.
///
/// # Errors
///
/// * [`Error::ObjectNotFound`] if `root` or an intermediate sub-tree is missing.
/// * [`Error::Corruption`] if an intermediate component is not a directory, or
///   the path is empty.
pub fn read_tree_path(
    store: &ObjectStore,
    root: &ObjectId,
    rel_path: impl AsRef<Path>,
) -> Result<TreeEntry> {
    let rel = rel_path.as_ref();
    let rel_str = rel.to_string_lossy();
    let components: Vec<&str> = rel_str
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();

    if components.is_empty() {
        return Err(Error::Corruption(
            "read_tree_path called with an empty path".to_string(),
        ));
    }

    let mut current = *root;
    for (index, component) in components.iter().enumerate() {
        let tree = store.get_tree(&current)?;
        let entry = find_entry(&tree, component).ok_or_else(|| {
            Error::Corruption(format!("path component {component:?} not found in tree"))
        })?;

        let is_last = index == components.len() - 1;
        if is_last {
            return Ok(entry.clone());
        }
        if entry.kind() != EntryKind::Tree {
            return Err(Error::Corruption(format!(
                "path component {component:?} is not a directory"
            )));
        }
        current = entry.id();
    }

    // Unreachable: the loop returns on the last component.
    Err(Error::Corruption("path resolution fell through".to_string()))
}

/// Finds an entry by exact name within a tree (entries are sorted, but a linear
/// scan is fine for the typical directory size).
fn find_entry<'a>(tree: &'a Tree, name: &str) -> Option<&'a TreeEntry> {
    tree.entries().iter().find(|e| e.name() == name)
}

/// Returns the immediate entries of the [`Tree`] at `tree_id`.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] if the tree is missing, or
/// [`Error::Corruption`] if it fails verification or decoding.
pub fn list_tree(store: &ObjectStore, tree_id: &ObjectId) -> Result<Vec<TreeEntry>> {
    Ok(store.get_tree(tree_id)?.entries().to_vec())
}

// ── tree surgery: flatten ⇄ rebuild ─────────────────────────────────────────

/// A single file or symlink leaf of a tree: its mode, kind, and content id.
///
/// Captures everything needed to place the entry back into a rebuilt tree
/// (unlike the id-only flatten in [`workcopy`](crate::workcopy), which only
/// needs blob identity for status comparison). Directories are not `FileNode`s —
/// they are reconstructed from the paths of the files beneath them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileNode {
    mode: u32,
    kind: EntryKind,
    id: ObjectId,
}

impl FileNode {
    /// Creates a new `FileNode`.
    pub const fn new(mode: u32, kind: EntryKind, id: ObjectId) -> Self {
        Self { mode, kind, id }
    }

    /// Returns the POSIX-style mode bits.
    pub const fn mode(&self) -> u32 {
        self.mode
    }

    /// Returns the entry kind ([`EntryKind::Blob`] or [`EntryKind::Symlink`]).
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    /// Returns the content address of the referenced object.
    pub const fn id(&self) -> ObjectId {
        self.id
    }
}

/// Flattens the tree at `tree_id` to a map of every file/symlink's repo-relative
/// path → [`FileNode`], recursing into sub-trees.
///
/// Paths use the platform separator (built via [`Path::join`]); callers that
/// need a stable string compare against scope selectors should normalise with
/// [`path_to_slash`]. The map is a [`BTreeMap`] so iteration is deterministic.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] / [`Error::Corruption`] if the tree or any
/// reachable sub-tree is missing or fails verification.
pub fn flatten_tree_full(store: &ObjectStore, tree_id: &ObjectId) -> Result<BTreeMap<PathBuf, FileNode>> {
    let mut out = BTreeMap::new();
    flatten_full_inner(store, tree_id, Path::new(""), &mut out)?;
    Ok(out)
}

/// Recursive worker for [`flatten_tree_full`].
fn flatten_full_inner(
    store: &ObjectStore,
    tree_id: &ObjectId,
    prefix: &Path,
    out: &mut BTreeMap<PathBuf, FileNode>,
) -> Result<()> {
    let tree = store.get_tree(tree_id)?;
    for entry in tree.entries() {
        let path = prefix.join(entry.name());
        match entry.kind() {
            EntryKind::Tree => flatten_full_inner(store, &entry.id(), &path, out)?,
            EntryKind::Blob | EntryKind::Symlink => {
                out.insert(path, FileNode::new(entry.mode(), entry.kind(), entry.id()));
            }
        }
    }
    Ok(())
}

/// Rebuilds a [`Tree`] DAG from a flat map of file/symlink paths → [`FileNode`],
/// returning the root `TreeId`.
///
/// This is the inverse of [`flatten_tree_full`]: intermediate directories are
/// recreated from the path components, and identical sub-trees deduplicate in
/// the store for free (content addressing). An empty map yields the canonical
/// empty root tree. Empty directories are *not* represented (a flattened tree
/// has no record of them); for tack's file-oriented model this is intentional.
///
/// # Errors
///
/// * [`Error::InvalidTreeEntryName`] if a path component is not a legal tree
///   entry name.
/// * [`Error::Io`] / store errors if a sub-tree cannot be written.
pub fn tree_from_files(store: &ObjectStore, files: &BTreeMap<PathBuf, FileNode>) -> Result<ObjectId> {
    let entries: Vec<(Vec<String>, FileNode)> = files
        .iter()
        .map(|(path, node)| (path_components(path), *node))
        .collect();
    build_from_entries(store, &entries)
}

/// Recursive worker for [`tree_from_files`]: builds one tree level from entries
/// expressed as `(remaining-components, node)` pairs.
fn build_from_entries(store: &ObjectStore, entries: &[(Vec<String>, FileNode)]) -> Result<ObjectId> {
    let mut leaves: Vec<TreeEntry> = Vec::new();
    // Sub-directory name → the entries that live beneath it (with the first
    // component stripped). A BTreeMap keeps the recursion deterministic.
    let mut subdirs: BTreeMap<String, Vec<(Vec<String>, FileNode)>> = BTreeMap::new();

    for (components, node) in entries {
        match components.as_slice() {
            [] => {
                return Err(Error::Corruption(
                    "tree_from_files received an entry with an empty path".to_string(),
                ));
            }
            [name] => leaves.push(tree_entry(name.clone(), node.mode(), node.kind(), node.id())?),
            [first, rest @ ..] => {
                subdirs
                    .entry(first.clone())
                    .or_default()
                    .push((rest.to_vec(), *node));
            }
        }
    }

    for (name, sub_entries) in subdirs {
        let sub_id = build_from_entries(store, &sub_entries)?;
        leaves.push(tree_entry(name, MODE_DIR, EntryKind::Tree, sub_id)?);
    }

    let tree = Tree::new(leaves)?;
    store.put_tree(&tree)
}

/// Splits `path` into its `Normal` UTF-8 components, dropping any `.`, `..`,
/// root, or prefix components (none of which can appear in a flattened tree
/// path, but filtering keeps the rebuild robust against either separator).
fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(os) => os.to_str().map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Renders a repo-relative path as a forward-slashed string for stable string
/// comparison (e.g. against a scope selector), independent of the OS separator.
pub fn path_to_slash(path: &Path) -> String {
    path_components(path).join("/")
}

/// Normalises a free-form, possibly OS-separated repo path string to the
/// canonical forward-slashed, separator-trimmed form used for scope selectors
/// and advisory claims.
///
/// Backslashes become forward slashes; surrounding whitespace, a leading `./`,
/// and surrounding slashes are stripped. The empty string normalises to itself
/// (the whole-repo selector).
pub fn normalize_repo_path(input: &str) -> String {
    let slashed = input.replace('\\', "/");
    let trimmed = slashed.trim();
    let no_dot = trimmed.strip_prefix("./").unwrap_or(trimmed);
    no_dot.trim_matches('/').to_string()
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::read_blob;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, ObjectStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = ObjectStore::init(dir.path().join(".tack")).expect("init store");
        (dir, store)
    }

    /// Writes `content` to `root/rel`, creating parent directories.
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

    // ── determinism ──────────────────────────────────────────────────────────

    #[test]
    fn same_content_same_root_id() -> Result<()> {
        let (_d1, store1) = temp_store();
        let (_d2, store2) = temp_store();
        let work1 = TempDir::new()?;
        let work2 = TempDir::new()?;

        // Build the SAME content in two different on-disk orders by writing the
        // files in different sequences. Tree::new sorts, so the ids must match.
        write_file(work1.path(), "a.txt", b"alpha");
        write_file(work1.path(), "z.txt", b"zulu");
        write_file(work1.path(), "sub/m.txt", b"mike");

        write_file(work2.path(), "sub/m.txt", b"mike");
        write_file(work2.path(), "z.txt", b"zulu");
        write_file(work2.path(), "a.txt", b"alpha");

        let id1 = build_tree(&store1, work1.path(), &empty_ignore())?;
        let id2 = build_tree(&store2, work2.path(), &empty_ignore())?;
        assert_eq!(id1, id2, "identical content must yield identical root tree id");
        Ok(())
    }

    #[test]
    fn different_content_different_root_id() -> Result<()> {
        let (_d, store) = temp_store();
        let work1 = TempDir::new()?;
        let work2 = TempDir::new()?;
        write_file(work1.path(), "a.txt", b"alpha");
        write_file(work2.path(), "a.txt", b"ALPHA");
        let id1 = build_tree(&store, work1.path(), &empty_ignore())?;
        let id2 = build_tree(&store, work2.path(), &empty_ignore())?;
        assert_ne!(id1, id2);
        Ok(())
    }

    #[test]
    fn empty_dir_builds_empty_tree() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        let id = build_tree(&store, work.path(), &empty_ignore())?;
        assert!(list_tree(&store, &id)?.is_empty());
        Ok(())
    }

    // ── ignore honored ───────────────────────────────────────────────────────

    #[test]
    fn build_tree_skips_ignored_files() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "keep.txt", b"keep");
        write_file(work.path(), "skip.log", b"skip");
        write_file(work.path(), "build/out.o", b"obj");

        let ignore = IgnoreRules::parse("*.log\nbuild/\n");
        let id = build_tree(&store, work.path(), &ignore)?;
        let names: Vec<String> = list_tree(&store, &id)?
            .iter()
            .map(|e| e.name().to_owned())
            .collect();
        assert_eq!(names, vec!["keep.txt".to_owned()]);
        Ok(())
    }

    #[test]
    fn build_tree_always_skips_tack_dir() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "file.txt", b"x");
        write_file(work.path(), ".tack/op-head", b"deadbeef");
        let id = build_tree(&store, work.path(), &empty_ignore())?;
        let names: Vec<String> = list_tree(&store, &id)?
            .iter()
            .map(|e| e.name().to_owned())
            .collect();
        assert_eq!(names, vec!["file.txt".to_owned()]);
        Ok(())
    }

    // ── nested structure + read_tree_path ────────────────────────────────────

    #[test]
    fn nested_dirs_are_subtrees() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "top.txt", b"top");
        write_file(work.path(), "a/b/deep.txt", b"deep");

        let root = build_tree(&store, work.path(), &empty_ignore())?;

        // Resolve the nested file.
        let entry = read_tree_path(&store, &root, "a/b/deep.txt")?;
        assert_eq!(entry.kind(), EntryKind::Blob);
        assert_eq!(read_blob(&store, &entry.id())?, b"deep");

        // Resolve an intermediate directory.
        let dir_entry = read_tree_path(&store, &root, "a")?;
        assert_eq!(dir_entry.kind(), EntryKind::Tree);
        Ok(())
    }

    #[test]
    fn read_tree_path_windows_separator() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a/b.txt", b"hi");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        let entry = read_tree_path(&store, &root, "a\\b.txt")?;
        assert_eq!(read_blob(&store, &entry.id())?, b"hi");
        Ok(())
    }

    #[test]
    fn read_tree_path_missing_is_corruption() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "a.txt", b"x");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        let result = read_tree_path(&store, &root, "nope.txt");
        assert!(matches!(result, Err(Error::Corruption(_))));
        Ok(())
    }

    #[test]
    fn read_tree_path_empty_is_error() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        assert!(matches!(read_tree_path(&store, &root, ""), Err(Error::Corruption(_))));
        Ok(())
    }

    // ── shared sub-trees ─────────────────────────────────────────────────────

    #[test]
    fn unchanged_subdir_shares_subtree_id() -> Result<()> {
        let (_d, store) = temp_store();
        let work1 = TempDir::new()?;
        let work2 = TempDir::new()?;
        // Both have an identical "lib/" subdir; only the top file differs.
        write_file(work1.path(), "lib/a.txt", b"shared");
        write_file(work1.path(), "main.txt", b"one");
        write_file(work2.path(), "lib/a.txt", b"shared");
        write_file(work2.path(), "main.txt", b"two");

        let root1 = build_tree(&store, work1.path(), &empty_ignore())?;
        let root2 = build_tree(&store, work2.path(), &empty_ignore())?;
        assert_ne!(root1, root2, "different top file -> different root");

        let lib1 = read_tree_path(&store, &root1, "lib")?;
        let lib2 = read_tree_path(&store, &root2, "lib")?;
        assert_eq!(lib1.id(), lib2.id(), "identical subdir must share its sub-tree id");
        Ok(())
    }

    // ── symlinks (unix only) ─────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn symlink_is_recorded_as_symlink_entry() -> Result<()> {
        use std::os::unix::fs::symlink;
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "target.txt", b"data");
        symlink("target.txt", work.path().join("link.txt"))?;

        let root = build_tree(&store, work.path(), &empty_ignore())?;
        let entry = read_tree_path(&store, &root, "link.txt")?;
        assert_eq!(entry.kind(), EntryKind::Symlink);
        assert_eq!(entry.mode(), MODE_SYMLINK);
        assert_eq!(read_blob(&store, &entry.id())?, b"target.txt");
        Ok(())
    }

    // ── list_tree on missing tree ────────────────────────────────────────────

    #[test]
    fn list_tree_missing_errors() {
        let (_d, store) = temp_store();
        let missing = ObjectId::from_bytes([0x33; 32]);
        assert!(matches!(list_tree(&store, &missing), Err(Error::ObjectNotFound(_))));
    }

    // ── PathBuf is accepted ──────────────────────────────────────────────────

    #[test]
    fn build_tree_accepts_pathbuf() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "x.txt", b"x");
        let p: PathBuf = work.path().to_path_buf();
        let _ = build_tree(&store, p, &empty_ignore())?;
        Ok(())
    }

    // ── flatten_tree_full / tree_from_files round-trip ───────────────────────

    #[test]
    fn flatten_then_rebuild_reproduces_root_id() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "top.txt", b"top");
        write_file(work.path(), "a/b/deep.txt", b"deep");
        write_file(work.path(), "a/sib.txt", b"sib");
        let root = build_tree(&store, work.path(), &empty_ignore())?;

        let flat = flatten_tree_full(&store, &root)?;
        assert_eq!(flat.len(), 3, "three files flattened");
        // The rebuilt tree must be byte-for-byte the same object (same id).
        let rebuilt = tree_from_files(&store, &flat)?;
        assert_eq!(rebuilt, root, "flatten then rebuild must reproduce the root id");
        Ok(())
    }

    #[test]
    fn tree_from_empty_map_is_the_empty_tree() -> Result<()> {
        let (_d, store) = temp_store();
        let empty = Tree::new(vec![])?;
        let built = tree_from_files(&store, &std::collections::BTreeMap::new())?;
        assert_eq!(built, store.put_tree(&empty)?, "empty map yields the empty root tree");
        Ok(())
    }

    #[test]
    fn flatten_full_captures_kind_and_mode() -> Result<()> {
        let (_d, store) = temp_store();
        let work = TempDir::new()?;
        write_file(work.path(), "f.txt", b"x");
        let root = build_tree(&store, work.path(), &empty_ignore())?;
        let flat = flatten_tree_full(&store, &root)?;
        let node = flat.get(Path::new("f.txt")).expect("f.txt present");
        assert_eq!(node.kind(), EntryKind::Blob);
        assert_eq!(node.mode(), MODE_FILE);
        Ok(())
    }

    #[test]
    fn path_to_slash_normalises_separators() {
        assert_eq!(path_to_slash(Path::new("a/b/c.txt")), "a/b/c.txt");
        let joined = PathBuf::from("a").join("b").join("c.txt");
        assert_eq!(path_to_slash(&joined), "a/b/c.txt");
    }
}
