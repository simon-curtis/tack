//! Content-level (line) diffing between two trees (`DESIGN.md §8`).
//!
//! The file-level [`diff`](crate::diff) module answers *which paths changed*;
//! this module answers *how each file's lines changed*, which is what an agent
//! reviewing another agent's work needs. It builds on the file-level diff for
//! path discovery, then for each changed file reassembles both blobs and runs a
//! line diff via the [`similar`] crate.
//!
//! Two shapes are produced from the same computation:
//!
//! * [`tree_patch`] — full per-file hunks (the `--patch` view).
//! * [`FileStat`] (via [`FileStat::from_patch`]) — just the added/removed line
//!   counts per file (the `--stat` view).
//!
//! Binary files (those containing a NUL byte) and files larger than
//! [`MAX_DIFF_BYTES`] are reported with `binary = true` and no hunks: tack does
//! not attempt a line diff of non-text or very large content, but it still
//! reports that the path changed so the orientation is never silently lost.

use std::path::Path;

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::blob::read_blob;
use crate::diff::diff_trees;
use crate::error::Result;
use crate::hash::ObjectId;
use crate::store::ObjectStore;
use crate::tree::read_tree_path;

/// Upper bound on the byte size of a single side of a file diff. Beyond this the
/// file is reported as `binary` (not line-diffed) to bound the cost of a diff.
pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

/// The number of unchanged context lines kept around each change in a hunk.
const CONTEXT_RADIUS: usize = 3;

/// One line of a [`Hunk`], tagged by how it changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// `"context"` (unchanged), `"insert"` (only in the new file), or
    /// `"delete"` (only in the old file).
    pub tag: String,
    /// The line content, with its trailing newline stripped.
    pub content: String,
}

/// A contiguous block of changes with surrounding context, addressed by 1-based
/// start line and line count on each side (the unified-diff `@@` coordinates).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// 1-based start line in the old file.
    pub old_start: usize,
    /// Number of old-file lines the hunk spans.
    pub old_lines: usize,
    /// 1-based start line in the new file.
    pub new_start: usize,
    /// Number of new-file lines the hunk spans.
    pub new_lines: usize,
    /// The tagged lines of the hunk, in order.
    pub lines: Vec<DiffLine>,
}

/// A full content diff for one changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePatch {
    /// The repo-relative path (forward-slashed).
    pub path: String,
    /// `"added"`, `"removed"`, or `"modified"`.
    pub change: String,
    /// `true` if the file was not line-diffed (binary content or oversize).
    pub binary: bool,
    /// Count of inserted lines.
    pub added_lines: usize,
    /// Count of deleted lines.
    pub removed_lines: usize,
    /// The diff hunks (empty when `binary`).
    pub hunks: Vec<Hunk>,
}

/// A per-file line-count summary (the `--stat` projection of a [`FilePatch`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    /// The repo-relative path (forward-slashed).
    pub path: String,
    /// `"added"`, `"removed"`, or `"modified"`.
    pub change: String,
    /// `true` if the file was not line-diffed (binary content or oversize).
    pub binary: bool,
    /// Count of inserted lines.
    pub added_lines: usize,
    /// Count of deleted lines.
    pub removed_lines: usize,
}

impl FileStat {
    /// Projects a [`FilePatch`] to its line-count summary (drops the hunks).
    #[must_use]
    pub fn from_patch(patch: &FilePatch) -> Self {
        Self {
            path: patch.path.clone(),
            change: patch.change.clone(),
            binary: patch.binary,
            added_lines: patch.added_lines,
            removed_lines: patch.removed_lines,
        }
    }
}

/// Computes per-file content patches between `from_tree` and `to_tree`.
///
/// Path discovery reuses [`diff_trees`]; each changed file is then reassembled
/// from the store and line-diffed. The result is sorted by path for
/// deterministic output.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`](crate::Error::ObjectNotFound) /
/// [`Error::Corruption`](crate::Error::Corruption) if a tree, sub-tree, or blob
/// is missing or fails verification.
pub fn tree_patch(
    store: &ObjectStore,
    from_tree: &ObjectId,
    to_tree: &ObjectId,
) -> Result<Vec<FilePatch>> {
    let diff = diff_trees(store, from_tree, to_tree)?;
    let mut patches: Vec<FilePatch> = Vec::new();

    for path in diff.added() {
        let to = read_side(store, to_tree, path)?;
        patches.push(build_patch(&slash(path), "added", &Side::empty(), &to));
    }
    for path in diff.removed() {
        let from = read_side(store, from_tree, path)?;
        patches.push(build_patch(&slash(path), "removed", &from, &Side::empty()));
    }
    for path in diff.modified() {
        let from = read_side(store, from_tree, path)?;
        let to = read_side(store, to_tree, path)?;
        patches.push(build_patch(&slash(path), "modified", &from, &to));
    }

    patches.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(patches)
}

/// One side of a file diff: either its raw bytes, or a marker that it is too
/// large to line-diff.
enum Side {
    /// The reassembled (in-bounds) bytes of the file.
    Bytes(Vec<u8>),
    /// The file exceeds [`MAX_DIFF_BYTES`] and was deliberately **not** read.
    Oversize,
}

impl Side {
    /// The empty side (an absent file on the added/removed side of a diff).
    const fn empty() -> Self {
        Self::Bytes(Vec::new())
    }
}

/// Resolves one side of a file diff, **bounding memory before any read**.
///
/// The blob's `total_len` is consulted from its (cheap, chunk-data-free) header
/// first: a file larger than [`MAX_DIFF_BYTES`] returns [`Side::Oversize`]
/// without ever reassembling its bytes, so the line-diff cost is bounded by the
/// cap rather than by the file size. A `total_len` that lies *small* is still
/// caught by [`read_blob`]'s own chunk-sum integrity check.
fn read_side(store: &ObjectStore, tree: &ObjectId, path: &Path) -> Result<Side> {
    let entry = read_tree_path(store, tree, path)?;
    // `get_blob` decodes only the chunk-list metadata, not the chunk bytes.
    let blob = store.get_blob(&entry.id())?;
    if blob.total_len() > MAX_DIFF_BYTES as u64 {
        return Ok(Side::Oversize);
    }
    Ok(Side::Bytes(read_blob(store, &entry.id())?))
}

/// Renders a repo-relative path as a forward-slashed string.
fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Builds a [`FilePatch`] from the two resolved sides.
///
/// Falls back to a no-hunk binary report when either side is
/// [`Side::Oversize`] or contains a NUL byte; otherwise runs a line diff.
fn build_patch(path: &str, change: &str, from: &Side, to: &Side) -> FilePatch {
    // Oversize (never read) on either side, or binary (NUL) content → reported
    // but not line-diffed.
    let (from_bytes, to_bytes) = match (from, to) {
        (Side::Bytes(f), Side::Bytes(t)) if !is_binary(f) && !is_binary(t) => {
            (f.as_slice(), t.as_slice())
        }
        _ => return binary_patch(path, change),
    };

    let from_text = String::from_utf8_lossy(from_bytes).into_owned();
    let to_text = String::from_utf8_lossy(to_bytes).into_owned();
    let diff = TextDiff::from_lines(&from_text, &to_text);

    let mut added_lines = 0;
    let mut removed_lines = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added_lines += 1,
            ChangeTag::Delete => removed_lines += 1,
            ChangeTag::Equal => {}
        }
    }

    let mut hunks = Vec::new();
    for group in diff.grouped_ops(CONTEXT_RADIUS) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let old_start = first.old_range().start;
        let new_start = first.new_range().start;
        let old_lines = last.old_range().end - old_start;
        let new_lines = last.new_range().end - new_start;

        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Equal => "context",
                    ChangeTag::Delete => "delete",
                    ChangeTag::Insert => "insert",
                };
                lines.push(DiffLine {
                    tag: tag.to_string(),
                    content: change.value().trim_end_matches(['\n', '\r']).to_string(),
                });
            }
        }

        hunks.push(Hunk {
            // Unified-diff `@@` coordinates are 1-based, EXCEPT a zero-length
            // span reports a start of 0 (there is no line 1 on that side) —
            // matching git's `@@ -0,0 +1,N @@` for an added file and
            // `@@ -1,N +0,0 @@` for a removed file.
            old_start: if old_lines == 0 { 0 } else { old_start + 1 },
            old_lines,
            new_start: if new_lines == 0 { 0 } else { new_start + 1 },
            new_lines,
            lines,
        });
    }

    FilePatch {
        path: path.to_string(),
        change: change.to_string(),
        binary: false,
        added_lines,
        removed_lines,
        hunks,
    }
}

/// A "reported but not line-diffed" patch (binary or oversize content): the path
/// change is still visible, but with no hunks or line counts.
fn binary_patch(path: &str, change: &str) -> FilePatch {
    FilePatch {
        path: path.to_string(),
        change: change.to_string(),
        binary: true,
        added_lines: 0,
        removed_lines: 0,
        hunks: Vec::new(),
    }
}

/// Returns `true` if `bytes` look binary (contain a NUL byte) and should not be
/// line-diffed. The **size** bound is enforced earlier, in [`read_side`], so it
/// is intentionally not re-checked here.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ignore::IgnoreRules;
    use crate::tree::build_tree;
    use std::path::Path;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, ObjectStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = ObjectStore::init(dir.path().join(".tack")).expect("init store");
        (dir, store)
    }

    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdirs");
        }
        std::fs::write(path, content).expect("write");
    }

    fn tree_of(store: &ObjectStore, populate: impl FnOnce(&Path)) -> ObjectId {
        let work = TempDir::new().expect("work");
        populate(work.path());
        build_tree(store, work.path(), &IgnoreRules::empty()).expect("build tree")
    }

    fn patch_for<'a>(patches: &'a [FilePatch], path: &str) -> &'a FilePatch {
        patches
            .iter()
            .find(|p| p.path == path)
            .unwrap_or_else(|| panic!("no patch for {path}"))
    }

    #[test]
    fn modified_file_reports_inserts_and_deletes() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "a.txt", b"line1\nline2\nline3\n"));
        let to = tree_of(&store, |r| {
            write_file(r, "a.txt", b"line1\nCHANGED\nline3\n");
        });
        let patches = tree_patch(&store, &from, &to)?;
        let p = patch_for(&patches, "a.txt");
        assert_eq!(p.change, "modified");
        assert!(!p.binary);
        assert_eq!(p.added_lines, 1, "one inserted line");
        assert_eq!(p.removed_lines, 1, "one deleted line");
        assert_eq!(p.hunks.len(), 1, "one contiguous hunk");
        // The hunk contains the deleted old line and the inserted new line.
        let tags: Vec<&str> = p.hunks[0].lines.iter().map(|l| l.tag.as_str()).collect();
        assert!(tags.contains(&"delete") && tags.contains(&"insert"));
        Ok(())
    }

    #[test]
    fn added_file_is_all_inserts() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |_| {});
        let to = tree_of(&store, |r| write_file(r, "new.txt", b"a\nb\nc\n"));
        let patches = tree_patch(&store, &from, &to)?;
        let p = patch_for(&patches, "new.txt");
        assert_eq!(p.change, "added");
        assert_eq!(p.added_lines, 3);
        assert_eq!(p.removed_lines, 0);
        assert!(p.hunks[0].lines.iter().all(|l| l.tag == "insert"));
        Ok(())
    }

    #[test]
    fn removed_file_is_all_deletes() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "gone.txt", b"x\ny\n"));
        let to = tree_of(&store, |_| {});
        let patches = tree_patch(&store, &from, &to)?;
        let p = patch_for(&patches, "gone.txt");
        assert_eq!(p.change, "removed");
        assert_eq!(p.removed_lines, 2);
        assert_eq!(p.added_lines, 0);
        Ok(())
    }

    #[test]
    fn binary_file_is_reported_without_hunks() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "b.bin", b"\x00\x01\x02"));
        let to = tree_of(&store, |r| write_file(r, "b.bin", b"\x00\x03\x04"));
        let patches = tree_patch(&store, &from, &to)?;
        let p = patch_for(&patches, "b.bin");
        assert!(p.binary, "a NUL-bearing file must be flagged binary");
        assert!(p.hunks.is_empty());
        Ok(())
    }

    #[test]
    fn stat_projection_matches_patch_counts() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "a.txt", b"one\n"));
        let to = tree_of(&store, |r| write_file(r, "a.txt", b"one\ntwo\nthree\n"));
        let patches = tree_patch(&store, &from, &to)?;
        let stat = FileStat::from_patch(&patches[0]);
        assert_eq!(stat.added_lines, patches[0].added_lines);
        assert_eq!(stat.added_lines, 2);
        assert_eq!(stat.removed_lines, 0);
        assert_eq!(stat.change, "modified");
        Ok(())
    }

    #[test]
    fn identical_trees_produce_no_patches() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "a.txt", b"same\n"));
        let patches = tree_patch(&store, &from, &from)?;
        assert!(patches.is_empty());
        Ok(())
    }

    // A symlink-target change is a content change and diffs as one line.
    #[test]
    fn hunk_line_coordinates_are_one_based() -> Result<()> {
        let (_d, store) = temp_store();
        let from = tree_of(&store, |r| write_file(r, "f", b"a\nb\nc\nd\ne\n"));
        let to = tree_of(&store, |r| write_file(r, "f", b"a\nb\nC\nd\ne\n"));
        let patches = tree_patch(&store, &from, &to)?;
        let p = patch_for(&patches, "f");
        // A modified file has non-empty spans on both sides, so both starts are
        // 1-based (>= 1).
        assert!(p.hunks[0].old_start >= 1 && p.hunks[0].new_start >= 1);
        Ok(())
    }

    /// Regression: a side that spans zero lines (an added or removed file) must
    /// report a `@@` *start* of 0, matching git (`-0,0 +1,N` / `-1,N +0,0`), not
    /// the off-by-one `-1,0` / `+1,0` the unconditional `+1` produced.
    #[test]
    fn added_and_removed_file_hunk_headers_zero_the_empty_side() -> Result<()> {
        let (_d, store) = temp_store();
        let empty = tree_of(&store, |_| {});
        let three = tree_of(&store, |r| write_file(r, "f.txt", b"a\nb\nc\n"));

        // Added: the old side spans zero lines → old_start must be 0.
        let added = tree_patch(&store, &empty, &three)?;
        let h = &patch_for(&added, "f.txt").hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines),
            (0, 0),
            "added file old side must be -0,0"
        );
        assert_eq!(
            (h.new_start, h.new_lines),
            (1, 3),
            "added file new side must be +1,3"
        );

        // Removed: the new side spans zero lines → new_start must be 0.
        let removed = tree_patch(&store, &three, &empty)?;
        let h = &patch_for(&removed, "f.txt").hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines),
            (1, 3),
            "removed file old side must be -1,3"
        );
        assert_eq!(
            (h.new_start, h.new_lines),
            (0, 0),
            "removed file new side must be +0,0"
        );
        Ok(())
    }

    /// Regression: a file larger than `MAX_DIFF_BYTES` must be reported as binary
    /// (no hunks) — and `read_side` must reach that verdict from the blob header
    /// without reassembling the whole file. Here we only assert the observable
    /// result; the memory bound is structural (see `read_side`).
    #[test]
    fn oversize_text_file_is_reported_binary_without_diffing() -> Result<()> {
        let (_d, store) = temp_store();
        // Just over the cap, all ASCII (no NUL) so this exercises the size gate,
        // not the NUL heuristic.
        let big = "x\n".repeat((MAX_DIFF_BYTES / 2) + 1);
        assert!(big.len() > MAX_DIFF_BYTES, "fixture must exceed the cap");
        let from = tree_of(&store, |_| {});
        let to = tree_of(&store, |r| write_file(r, "big.txt", big.as_bytes()));
        let patches = tree_patch(&store, &from, &to)?;
        let p = patch_for(&patches, "big.txt");
        assert!(
            p.binary,
            "a file over MAX_DIFF_BYTES must be reported binary"
        );
        assert!(p.hunks.is_empty(), "oversize file must have no hunks");
        assert_eq!(p.added_lines, 0);
        Ok(())
    }
}
