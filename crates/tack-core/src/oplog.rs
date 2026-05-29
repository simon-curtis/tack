//! The operation log: persistence and navigation of the op DAG
//! (`DESIGN.md §6`).
//!
//! The op-log is tack's **source of truth**; snapshots, trees, blobs, and
//! chunks are the immutable content DAG it points into. Every repo-mutating
//! command appends exactly one [`Op`], each embedding a [`View`] of the full
//! repo state after it.
//!
//! The **only mutable pointer in the entire system** is `.tack/op-head` — a
//! tiny file holding a single [`ObjectId`] (the current op head) rendered as
//! lowercase hex. Every other artifact is content-addressed and immutable.
//! Advancing the head is therefore the one and only commit point of any
//! operation: write the new objects (idempotent, harmless if interrupted), then
//! atomically rewrite `op-head`.
//!
//! `restore` and `undo` are themselves new ops whose parent is the current head
//! (`DESIGN.md §9`), so **nothing reachable is ever destroyed**
//! (`constitution.md §3`).

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::{Op, OpMetadata, Timestamp};
use crate::store::ObjectStore;

/// The control directory created by `init` under a repository root.
pub(crate) const TACK_DIR: &str = ".tack";

/// File under `.tack/` that records the current op head as one hex `OpId`.
pub(crate) const OP_HEAD_FILE: &str = "op-head";

/// Returns the path to the `op-head` file under the given `.tack/` root.
fn op_head_path(tack_dir: &Path) -> PathBuf {
    tack_dir.join(OP_HEAD_FILE)
}

/// Reads the current op head from `.tack/op-head`.
///
/// `tack_dir` is the repository's `.tack/` directory. Returns `Ok(None)` if the
/// file does not exist (a freshly-created store before its first op) and
/// `Ok(Some(id))` otherwise.
///
/// # Errors
///
/// * [`Error::Corruption`] if the file contents are not a valid hex `ObjectId`.
/// * [`Error::Io`] for read failures other than the file being absent.
pub fn op_head(tack_dir: impl AsRef<Path>) -> Result<Option<ObjectId>> {
    let path = op_head_path(tack_dir.as_ref());
    match fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            let id = ObjectId::from_str(trimmed).map_err(|_| {
                Error::Corruption(format!("op-head does not contain a valid object id: {trimmed:?}"))
            })?;
            Ok(Some(id))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Writes `id` to `.tack/op-head`, atomically replacing any previous head.
///
/// The write goes to a sibling temporary file that is then renamed into place,
/// so a crash mid-write cannot leave a truncated head pointer.
///
/// # Errors
///
/// Returns [`Error::Io`] if the file cannot be written or renamed.
pub fn set_op_head(tack_dir: impl AsRef<Path>, id: ObjectId) -> Result<()> {
    let tack_dir = tack_dir.as_ref();
    fs::create_dir_all(tack_dir)?;
    let path = op_head_path(tack_dir);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, id.to_string())?;
    // On Windows rename over an existing file succeeds; if it races, the loser's
    // temp is cleaned up. Either way the head ends up pointing at a valid op.
    match fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(Error::Io(e))
        }
    }
}

/// Loads the [`Op`] currently at the head of the op-log.
///
/// # Errors
///
/// * [`Error::Corruption`] if `op-head` is missing or unreadable — a repository
///   always has at least the root op after `init`, so an absent head is a
///   structural error here (unlike [`op_head`], which reports absence as
///   `None`).
/// * [`Error::ObjectNotFound`] if the head op object is missing from the store.
pub fn current_op(store: &ObjectStore, tack_dir: impl AsRef<Path>) -> Result<Op> {
    let head = op_head(tack_dir)?.ok_or_else(|| {
        Error::Corruption("repository has no op-head; was it initialized?".to_string())
    })?;
    store.get_op(&head)
}

/// Builds and stores a new [`Op`], then advances `.tack/op-head` to point at it.
///
/// The op records `parents` (the preceding op heads — empty only for the root
/// op), the `view_id` capturing repo state at the end of the operation, a
/// human-readable `description`, and the literal `command` that triggered it.
/// [`OpMetadata`] is filled with start/end timestamps, the machine hostname, and
/// the current username (`DESIGN.md §6`).
///
/// Returns the new op's [`ObjectId`], which is also the new op head.
///
/// # Errors
///
/// Returns [`Error::Io`] if storing the op or rewriting the head fails.
pub fn append_op(
    store: &ObjectStore,
    tack_dir: impl AsRef<Path>,
    parents: Vec<ObjectId>,
    view_id: ObjectId,
    description: impl Into<String>,
    command: Vec<String>,
) -> Result<ObjectId> {
    // Capture a single timestamp for both start and end: tack's operations are
    // effectively instantaneous from the user's perspective, and recording two
    // wall-clock reads would only introduce non-determinism without insight.
    let now = now_timestamp();
    let metadata = OpMetadata::new(now, now, hostname(), username(), command);
    let op = Op::new(parents, view_id, metadata, description);
    let id = store.put_op(&op)?;
    set_op_head(tack_dir, id)?;
    Ok(id)
}

/// Walks the op DAG from the current head via `parents`, returning every
/// reachable op **newest-first**.
///
/// The walk is a breadth-first traversal over parent links with cycle/duplicate
/// suppression (the DAG is acyclic by construction, but a merge op can be
/// reached by multiple paths). Order is a topological "newest first": the head
/// is element zero and an op always appears before its parents.
///
/// # Errors
///
/// * [`Error::Corruption`] if `op-head` is missing or malformed.
/// * [`Error::ObjectNotFound`] if any op in the history is absent from the store.
pub fn op_log(store: &ObjectStore, tack_dir: impl AsRef<Path>) -> Result<Vec<Op>> {
    let Some(head) = op_head(tack_dir)? else {
        return Ok(Vec::new());
    };

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    // A simple work queue preserves "newest first" because we always enqueue an
    // op before its parents and never revisit a node.
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(head);
    seen.insert(head);

    while let Some(id) = queue.pop_front() {
        let op = store.get_op(&id)?;
        for &parent in op.parents() {
            if seen.insert(parent) {
                queue.push_back(parent);
            }
        }
        out.push(op);
    }

    Ok(out)
}

// ── environment / clock helpers ─────────────────────────────────────────────

/// Returns a [`Timestamp`] for the current wall-clock instant in UTC.
///
/// The timezone offset is recorded as zero: the standard library does not
/// expose the local UTC offset without a third-party dependency, and tack's
/// timestamps are always stored as UTC seconds, so a zero offset is correct
/// (the offset field exists for future display localization, `DESIGN.md §4`).
pub(crate) fn now_timestamp() -> Timestamp {
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| {
            i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
        });
    Timestamp::new(unix_secs, 0)
}

/// Returns the machine hostname, or `"unknown-host"` if it cannot be determined.
///
/// Reads `COMPUTERNAME` (Windows) then `HOSTNAME` (Unix) from the environment;
/// avoids a dedicated crate for what is a single best-effort identifier.
pub(crate) fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// Returns the current username, or `"unknown-user"` if it cannot be determined.
///
/// Reads `USERNAME` (Windows) then `USER` (Unix) from the environment.
pub(crate) fn username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown-user".to_string())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::View;
    use tempfile::TempDir;

    /// Creates a temp repo dir with an initialized `.tack/` store, returning the
    /// guard, the store, and the `.tack/` path.
    fn temp_repo() -> (TempDir, ObjectStore, PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let tack = dir.path().join(TACK_DIR);
        let store = ObjectStore::init(&tack).expect("init store");
        (dir, store, tack)
    }

    fn empty_view() -> View {
        View::new(ObjectId::from_bytes([0u8; 32]), vec![], vec![], vec![])
    }

    #[test]
    fn op_head_absent_is_none() -> Result<()> {
        let (_d, _store, tack) = temp_repo();
        assert!(op_head(&tack)?.is_none());
        Ok(())
    }

    #[test]
    fn set_then_get_op_head_round_trips() -> Result<()> {
        let (_d, _store, tack) = temp_repo();
        let id = ObjectId::from_bytes([0xab; 32]);
        set_op_head(&tack, id)?;
        assert_eq!(op_head(&tack)?, Some(id));
        Ok(())
    }

    #[test]
    fn malformed_op_head_is_corruption() -> Result<()> {
        let (_d, _store, tack) = temp_repo();
        fs::write(tack.join(OP_HEAD_FILE), "not-a-valid-id")?;
        assert!(matches!(op_head(&tack), Err(Error::Corruption(_))));
        Ok(())
    }

    #[test]
    fn append_op_advances_head_and_links_parent() -> Result<()> {
        let (_d, store, tack) = temp_repo();
        let view_id = store.put_view(&empty_view())?;

        let root = append_op(&store, &tack, vec![], view_id, "init", vec!["tack".into(), "init".into()])?;
        assert_eq!(op_head(&tack)?, Some(root));

        let second = append_op(&store, &tack, vec![root], view_id, "snapshot working copy", vec![])?;
        assert_eq!(op_head(&tack)?, Some(second));
        assert_ne!(root, second);

        // current_op resolves the head.
        let head_op = current_op(&store, &tack)?;
        assert_eq!(head_op.id(), second);
        assert_eq!(head_op.parents(), &[root]);
        assert_eq!(head_op.description(), "snapshot working copy");
        Ok(())
    }

    #[test]
    fn op_log_walks_newest_first() -> Result<()> {
        let (_d, store, tack) = temp_repo();
        let view_id = store.put_view(&empty_view())?;
        let a = append_op(&store, &tack, vec![], view_id, "a", vec![])?;
        let b = append_op(&store, &tack, vec![a], view_id, "b", vec![])?;
        let c = append_op(&store, &tack, vec![b], view_id, "c", vec![])?;

        let log = op_log(&store, &tack)?;
        let ids: Vec<ObjectId> = log.iter().map(Op::id).collect();
        assert_eq!(ids, vec![c, b, a], "op log must be newest-first");
        Ok(())
    }

    #[test]
    fn op_log_empty_without_head() -> Result<()> {
        let (_d, store, tack) = temp_repo();
        assert!(op_log(&store, &tack)?.is_empty());
        Ok(())
    }

    #[test]
    fn current_op_without_head_is_corruption() {
        let (_d, store, tack) = temp_repo();
        assert!(matches!(current_op(&store, &tack), Err(Error::Corruption(_))));
    }

    #[test]
    fn metadata_records_host_and_user() -> Result<()> {
        let (_d, store, tack) = temp_repo();
        let view_id = store.put_view(&empty_view())?;
        let id = append_op(&store, &tack, vec![], view_id, "init", vec![])?;
        let op = store.get_op(&id)?;
        // host/user are best-effort but always non-empty (a fallback is used).
        assert!(!op.metadata().hostname().is_empty());
        assert!(!op.metadata().username().is_empty());
        Ok(())
    }
}
