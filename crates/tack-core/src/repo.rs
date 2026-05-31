//! [`Repository`] — the high-level engine API (`DESIGN.md §6`, §7, §9, §12).
//!
//! A `Repository` ties the whole engine together: the content-addressed
//! [`ObjectStore`], the operation log ([`oplog`](crate::oplog)), and the
//! working directory. It is the surface the CLI and the agent API drive; every
//! mutating method appends exactly one [`Op`] (`DESIGN.md §6`) and is
//! non-destructive (`constitution.md §3`).
//!
//! ## The jj model, as implemented here (`DESIGN.md §7`)
//!
//! The working copy *is* a [`Snapshot`] — there is no staging area.
//!
//! * [`Repository::snapshot_working_copy`] auto-snapshots: it re-chunks the work
//!   dir into a new root [`Tree`](crate::object::Tree) and, if anything changed,
//!   produces a new working-copy snapshot that **amends** the previous one (same
//!   `change_id`, same parents, new id) and records a `"snapshot working copy"`
//!   op. Unchanged trees are a no-op.
//! * [`Repository::named_cut`] is the analog of a git commit: it auto-snapshots,
//!   *finalizes* the current working-copy snapshot with a message and author
//!   (the closed cut), then starts a **fresh** child working-copy snapshot with a
//!   new `change_id` on top, and records a `"named cut"` op.
//!
//! ## State pointers
//!
//! All durable state is content-addressed and immutable except the single
//! mutable pointer `.tack/op-head`. The current [`View`] (working copy, heads,
//! bookmarks, tags) is recovered by reading the op at the head; the repository
//! holds no other mutable cursor.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::claims::Claim;
use crate::error::{Error, Result};
use crate::hash::{ObjectId, TypeTag};
use crate::ignore::IgnoreRules;
use crate::linediff::FilePatch;
use crate::object::{Identity, Op, Snapshot, View};
use crate::oplog::{self, TACK_DIR};
use crate::statcache::StatCache;
use crate::store::ObjectStore;
use crate::tree::{
    FileNode, build_tree_cached, flatten_tree_full, normalize_repo_path, path_to_slash,
    tree_from_files,
};
use crate::workcopy::{Status, project, status};

/// The on-disk format version this binary understands (`DESIGN.md §15`).
///
/// Written into `.tack/config` at `init`. A repository whose recorded version
/// is **newer** than this is refused rather than silently misread.
pub const FORMAT_VERSION: u32 = 1;

/// Basename of the JSON configuration file under `.tack/`.
///
/// The config is **not** content-addressed (it is mutable repo metadata, never
/// hashed), so plain `serde_json` is appropriate here — unlike the object
/// encoding, which must be canonical (`DESIGN.md §3`).
const CONFIG_FILE: &str = "config";

/// The persisted `.tack/config` contents (`DESIGN.md §15`).
///
/// Records the format version plus the `FastCDC` parameters that are part of the
/// on-disk format contract (`DESIGN.md §5`), so a future binary can detect a
/// repository whose chunking parameters differ from its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Config {
    /// The on-disk format version (`DESIGN.md §15`).
    format_version: u32,
    /// `FastCDC` minimum chunk size in bytes.
    min_chunk: u32,
    /// `FastCDC` average (target) chunk size in bytes.
    avg_chunk: u32,
    /// `FastCDC` maximum chunk size in bytes.
    max_chunk: u32,
}

impl Config {
    /// Builds the config for the current binary's format.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corruption`] if a chunker size constant does not fit in
    /// `u32`. This is impossible for the configured `FastCDC` parameters (all
    /// ≤ 64 KiB), but the bound is checked rather than cast unconditionally.
    fn current() -> Result<Self> {
        let to_u32 = |value: usize| {
            u32::try_from(value)
                .map_err(|_| Error::Corruption("chunker size constant exceeds u32".to_string()))
        };
        Ok(Self {
            format_version: FORMAT_VERSION,
            min_chunk: to_u32(crate::chunker::MIN_CHUNK)?,
            avg_chunk: to_u32(crate::chunker::AVG_CHUNK)?,
            max_chunk: to_u32(crate::chunker::MAX_CHUNK)?,
        })
    }
}

/// A monotonically-increasing counter that, mixed with the wall clock and the
/// process id, gives every freshly-born `change_id` a distinct seed even when
/// several are created within the same clock tick.
static CHANGE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A content-addressed tack repository rooted at a working directory.
///
/// Construct with [`Repository::init`] (creates `.tack/` and the root op) or
/// [`Repository::open`] (discovers an existing repository from a path). Hold one
/// per workspace; the type carries no mutable cursor beyond what lives on disk.
#[derive(Debug)]
pub struct Repository {
    /// The working directory (the parent of `.tack/`).
    work_dir: PathBuf,
    /// The `.tack/` control directory.
    tack_dir: PathBuf,
    /// The content-addressed object store rooted at `.tack/`.
    store: ObjectStore,
}

impl Repository {
    // ── construction ────────────────────────────────────────────────────────

    /// Initializes a brand-new repository at `path`.
    ///
    /// Creates `.tack/`, writes `.tack/config`, initializes the object store,
    /// and lays down the initial history: an empty root [`Tree`](crate::object::Tree),
    /// an initial empty working-copy [`Snapshot`] (with a fresh random
    /// `change_id` and no parents), an initial [`View`], and the root [`Op`]
    /// (`DESIGN.md §12`).
    ///
    /// # Errors
    ///
    /// * [`Error::Io`] if `.tack/` already contains a config (the repository is
    ///   already initialized) or any filesystem write fails.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        let work_dir = path.as_ref().to_path_buf();
        let tack_dir = work_dir.join(TACK_DIR);

        if tack_dir.join(CONFIG_FILE).is_file() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("repository already initialized at {}", tack_dir.display()),
            )));
        }

        let store = ObjectStore::init(&tack_dir)?;
        write_config(&tack_dir, &Config::current()?)?;

        // Empty root tree → initial working-copy snapshot → initial view → root op.
        let empty_tree = crate::object::Tree::new(vec![])?;
        let root_tree = store.put_tree(&empty_tree)?;

        let identity = default_identity();
        let working = Snapshot::new(
            root_tree,
            Vec::new(),
            new_change_id(),
            identity.clone(),
            identity,
            String::new(),
            oplog::now_timestamp(),
        );
        let working_id = store.put_snapshot(&working)?;

        let view = View::new(working_id, Vec::new(), Vec::new(), vec![working_id]);
        let view_id = store.put_view(&view)?;

        oplog::append_op(
            &store,
            &tack_dir,
            Vec::new(),
            view_id,
            "initialize repository",
            vec!["tack".to_string(), "init".to_string()],
        )?;

        Ok(Self {
            work_dir,
            tack_dir,
            store,
        })
    }

    /// Opens an existing repository by discovering `.tack/` from `path` upward.
    ///
    /// Searches `path` and each ancestor directory for a `.tack/config`. Refuses
    /// to open a repository whose recorded `FORMAT_VERSION` is newer than this
    /// binary supports (`DESIGN.md §15`).
    ///
    /// # Errors
    ///
    /// * [`Error::Io`] if no `.tack/` is found in `path` or any ancestor.
    /// * [`Error::FormatVersionMismatch`] if the repository's format is newer.
    /// * [`Error::Corruption`] if the config cannot be parsed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let start = path.as_ref();
        let work_dir = discover_root(start).ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "no tack repository found in {} or any parent",
                    start.display()
                ),
            ))
        })?;
        let tack_dir = work_dir.join(TACK_DIR);

        let config = read_config(&tack_dir)?;
        if config.format_version > FORMAT_VERSION {
            return Err(Error::FormatVersionMismatch {
                repo: config.format_version,
                binary: FORMAT_VERSION,
            });
        }

        let store = ObjectStore::open(&tack_dir)?;
        Ok(Self {
            work_dir,
            tack_dir,
            store,
        })
    }

    // ── accessors ─────────────────────────────────────────────────────────────

    /// Returns the object store backing this repository.
    pub const fn store(&self) -> &ObjectStore {
        &self.store
    }

    /// Returns the working directory (the parent of `.tack/`).
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Returns the `.tack/` control directory.
    pub fn tack_dir(&self) -> &Path {
        &self.tack_dir
    }

    // ── current state helpers ─────────────────────────────────────────────────

    /// Loads the [`View`] at the current op head.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corruption`] if there is no op head, or
    /// [`Error::ObjectNotFound`] if the op or its view is missing.
    pub fn current_view(&self) -> Result<View> {
        let op = oplog::current_op(&self.store, &self.tack_dir)?;
        self.store.get_view(&op.view())
    }

    /// Loads the [`Op`] at the current op head.
    ///
    /// This is the operation that produced the current [`View`]; its description
    /// and command identify how the repository reached its present state (used by
    /// the `current` API method to report e.g. an in-effect restore).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corruption`] if there is no op head, or
    /// [`Error::ObjectNotFound`] if the head op is missing.
    pub fn current_op(&self) -> Result<Op> {
        oplog::current_op(&self.store, &self.tack_dir)
    }

    /// Loads the current working-copy [`Snapshot`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Corruption`] / [`Error::ObjectNotFound`] if the view or
    /// the working-copy snapshot it names cannot be read.
    pub fn working_copy(&self) -> Result<Snapshot> {
        let view = self.current_view()?;
        self.store.get_snapshot(&view.working_copy())
    }

    /// Builds the **live** working-copy root tree from the current on-disk files.
    ///
    /// Like [`status`](Self::status) and the default [`diff`](Self::diff), this
    /// reflects dirty edits not yet snapshotted. It writes content-addressed
    /// blob/tree objects to the store (deduplicated — unchanged content writes
    /// nothing new) but appends **no op** and never moves the op-head, so it is
    /// read-only with respect to history. It is the default tree for listing, so
    /// `ls` shows on-disk reality rather than a stale recorded snapshot.
    ///
    /// Capturing the *same* on-disk content twice is idempotent (content
    /// addressing → no new objects). Objects written for a dirty state that is
    /// never snapshotted stay unreferenced until a future GC pass — the same
    /// objects a later `snap` would have written. Files whose `(mtime, size)` are
    /// unchanged are reused from the working-copy [`StatCache`] without re-reading
    /// or chunking them; only changed files are re-chunked.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem read failures, or store errors if a
    /// referenced object is missing or corrupt.
    pub fn live_tree(&self) -> Result<ObjectId> {
        self.build_cached_tree()
    }

    /// Path of the advisory working-copy stat cache (`.tack/wc-cache`).
    fn wc_cache_path(&self) -> PathBuf {
        self.tack_dir.join("wc-cache")
    }

    /// Builds the live working-copy root tree via the [`StatCache`] fast path and
    /// persists the refreshed cache. The result is identical to an uncached
    /// [`build_tree`](crate::tree::build_tree) for the same on-disk content — only
    /// faster.
    ///
    /// Persisting the cache is best-effort: a cache write failure is swallowed
    /// (the cache is advisory and must never fail a capture).
    fn build_cached_tree(&self) -> Result<ObjectId> {
        let ignore = self.ignore_rules()?;
        let cache_path = self.wc_cache_path();
        let mut cache = StatCache::load(&cache_path);
        // Captured BEFORE the walk: an upper bound on every file's read time, so a
        // file edited during the scan is correctly treated as racy next time.
        let scanned_at = SystemTime::now();
        let root = build_tree_cached(&self.store, &self.work_dir, &ignore, &mut cache)?;
        let _ = cache.save(&cache_path, scanned_at);
        Ok(root)
    }

    /// Loads the repository's `.tackignore` rules.
    fn ignore_rules(&self) -> Result<IgnoreRules> {
        IgnoreRules::load(self.work_dir.join(".tackignore"))
    }

    // ── auto-snapshot (continuous snapshot) ───────────────────────────────────

    /// Auto-snapshots the working copy (`DESIGN.md §7`).
    ///
    /// Re-chunks the work dir into a new root tree. If the tree is unchanged
    /// from the current working-copy snapshot, this is a **no-op** and returns
    /// the current working-copy snapshot id without appending an op. Otherwise it
    /// produces a new working-copy snapshot that **amends** the current one (same
    /// `change_id`, same parents, new id), updates the [`View`] (working copy and
    /// the matching head), and appends a `"snapshot working copy"` op.
    ///
    /// Returns the id of the (new or unchanged) working-copy snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem failures, or store errors if any
    /// referenced object is missing or corrupt.
    pub fn snapshot_working_copy(&self) -> Result<ObjectId> {
        let view = self.current_view()?;
        let current_wc_id = view.working_copy();
        let current = self.store.get_snapshot(&current_wc_id)?;

        let new_root_tree = self.build_cached_tree()?;

        // Nothing changed → nothing to record (content addressing makes this a
        // cheap and exact check).
        if new_root_tree == current.root_tree() {
            return Ok(current_wc_id);
        }

        // Amend: keep change_id, parents, author/committer, message; new tree.
        let amended = Snapshot::new(
            new_root_tree,
            current.parents().to_vec(),
            current.change_id(),
            current.author().clone(),
            current.committer().clone(),
            current.message().to_string(),
            oplog::now_timestamp(),
        );
        let amended_id = self.store.put_snapshot(&amended)?;

        let new_view = replace_working_copy(&view, current_wc_id, amended_id);
        let view_id = self.store.put_view(&new_view)?;
        self.append_op_on_head(
            view_id,
            "snapshot working copy",
            vec!["tack".to_string(), "snapshot".to_string()],
        )?;

        Ok(amended_id)
    }

    // ── named cut ─────────────────────────────────────────────────────────────

    /// Creates a named cut — the tack analog of a git commit (`DESIGN.md §7`).
    ///
    /// Auto-snapshots first, then *finalizes* the current working-copy snapshot
    /// with `message` and `author` (the closed cut), and starts a **fresh** child
    /// working-copy snapshot with a new `change_id` on top of it (empty delta).
    /// The [`View`]'s working copy and head advance to the fresh child; the
    /// closed cut stays reachable as its parent. Appends a `"named cut"` op.
    ///
    /// Returns the id of the closed cut (the finalized commit).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem failures, or store errors if any
    /// referenced object is missing or corrupt.
    pub fn named_cut(&self, message: impl Into<String>, author: Identity) -> Result<ObjectId> {
        // Ensure the working-copy snapshot reflects the latest on-disk content.
        self.snapshot_working_copy()?;

        let view = self.current_view()?;
        let wc_id = view.working_copy();
        let working = self.store.get_snapshot(&wc_id)?;

        // The closed cut: same tree and ancestry as the working copy, but now
        // carrying the message/author. It keeps the working copy's change_id —
        // this is the same logical change being given a name (an amend), exactly
        // as DESIGN §7 describes ("finalize the current working-copy snapshot").
        let now = oplog::now_timestamp();
        let closed = Snapshot::new(
            working.root_tree(),
            working.parents().to_vec(),
            working.change_id(),
            author.clone(),
            author,
            message,
            now,
        );
        let closed_id = self.store.put_snapshot(&closed)?;

        // The fresh child working copy: new change_id, empty message, same tree,
        // parented on the closed cut.
        let default_id = default_identity();
        let child = Snapshot::new(
            working.root_tree(),
            vec![closed_id],
            new_change_id(),
            default_id.clone(),
            default_id,
            String::new(),
            now,
        );
        let child_id = self.store.put_snapshot(&child)?;

        // The new head is the fresh child; the closed cut is reachable via parent.
        let new_view = View::new(
            child_id,
            view.bookmarks().to_vec(),
            view.tags().to_vec(),
            vec![child_id],
        );
        let view_id = self.store.put_view(&new_view)?;
        self.append_op_on_head(
            view_id,
            "named cut",
            vec!["tack".to_string(), "snap".to_string()],
        )?;

        Ok(closed_id)
    }

    // ── inspection ────────────────────────────────────────────────────────────

    /// Reports the working directory's status against the current working-copy
    /// snapshot (added / modified / deleted; `DESIGN.md §8`).
    ///
    /// This is read-only: it does not auto-snapshot or write any object.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem failures or store errors if the
    /// current snapshot/tree is missing.
    pub fn status(&self) -> Result<Status> {
        let working = self.working_copy()?;
        let ignore = self.ignore_rules()?;
        status(&self.store, &working.root_tree(), &self.work_dir, &ignore)
    }

    /// Returns the named cuts in the current working copy's ancestry,
    /// newest-first (`DESIGN.md §12`).
    ///
    /// Walks parent links from the working-copy snapshot, collecting every
    /// snapshot with a non-empty message (a named cut). Auto-snapshots — which
    /// carry an empty message — are skipped, matching the human-facing `tack log`
    /// view.
    ///
    /// # Errors
    ///
    /// Returns store errors if any snapshot in the ancestry is missing or corrupt.
    pub fn log(&self) -> Result<Vec<Snapshot>> {
        let working = self.working_copy()?;
        let mut out = Vec::new();
        let mut seen = std::collections::HashMap::new();

        // Breadth-first walk from the working copy's parents, recording each
        // snapshot's generation distance (depth) from the working copy. A
        // descendant is always strictly closer than its ancestors, so depth is
        // a robust newest-first key even when wall-clock timestamps (1-second
        // granularity) tie for cuts made in the same second.
        let mut frontier: Vec<ObjectId> = working.parents().to_vec();
        let mut depth: usize = 0;
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for id in frontier {
                if seen.contains_key(&id) {
                    continue;
                }
                seen.insert(id, depth);
                let snap = self.store.get_snapshot(&id)?;
                if !snap.message().is_empty() {
                    out.push(snap.clone());
                }
                next.extend(snap.parents().iter().copied());
            }
            frontier = next;
            depth += 1;
        }

        // Newest-first: primary key is ancestry depth ascending (a descendant is
        // always strictly shallower than its ancestors, so this is the only key
        // that is always topologically correct). Wall-clock seconds is the
        // human-facing tiebreaker (descending) only when depths are equal — it is
        // deliberately NOT primary because the system clock can step backward (NTP
        // / manual change), which would otherwise sort an ancestor ahead of its
        // own descendant. `id` gives a fully deterministic total order.
        out.sort_by(|a, b| {
            let da = seen.get(&a.id()).copied().unwrap_or(usize::MAX);
            let db = seen.get(&b.id()).copied().unwrap_or(usize::MAX);
            da.cmp(&db)
                .then_with(|| b.timestamp().unix_secs().cmp(&a.timestamp().unix_secs()))
                .then_with(|| b.id().cmp(&a.id()))
        });
        Ok(out)
    }

    /// Returns the **nearest named cut** in the current working copy's ancestry —
    /// the cut the working copy currently sits on — or `None` if the working copy
    /// has no named ancestor (a fresh repo).
    ///
    /// This is the base an agent is working *against*; the `current` API method
    /// surfaces it so an agent always knows its starting point, and `scoped_cut`
    /// defaults to it.
    ///
    /// # Errors
    ///
    /// Returns store errors if any ancestor snapshot is missing or corrupt.
    pub fn base_cut(&self) -> Result<Option<Snapshot>> {
        // `log()` is newest-first by ancestry depth, so its first element is the
        // nearest named ancestor of the working copy.
        Ok(self.log()?.into_iter().next())
    }

    /// Returns **every named cut reachable from any operation in the op-log**,
    /// regardless of the current lineage, newest-first by timestamp
    /// (`DESIGN.md §12`).
    ///
    /// Unlike [`log`](Self::log) — which follows only the working copy's own
    /// ancestry and therefore hides cuts that a `restore` or a `scoped_cut` left
    /// off-lineage — this walks from every view ever recorded (each op's working
    /// copy and heads), so an agent can see a cut that `log` no longer shows. The
    /// op-log makes this fully recoverable; this method surfaces it directly.
    ///
    /// Ordering is by wall-clock timestamp descending (ancestry depth is not a
    /// total order across divergent lineages), with the content id as a
    /// deterministic tiebreaker.
    ///
    /// # Errors
    ///
    /// Returns store errors if any op, view, or snapshot is missing or corrupt.
    pub fn all_cuts(&self) -> Result<Vec<Snapshot>> {
        // Seed the walk from every snapshot any op ever pointed at: each view's
        // working copy plus its heads. This covers all lineages that were ever
        // current, including off-lineage cuts.
        let mut frontier: Vec<ObjectId> = Vec::new();
        for op in self.op_log()? {
            let view = self.store.get_view(&op.view())?;
            frontier.push(view.working_copy());
            frontier.extend(view.heads().iter().copied());
        }

        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<Snapshot> = Vec::new();
        while let Some(id) = frontier.pop() {
            if !seen.insert(id) {
                continue;
            }
            let snap = self.store.get_snapshot(&id)?;
            if !snap.message().is_empty() {
                out.push(snap.clone());
            }
            frontier.extend(snap.parents().iter().copied());
        }

        out.sort_by(|a, b| {
            b.timestamp()
                .unix_secs()
                .cmp(&a.timestamp().unix_secs())
                .then_with(|| b.id().cmp(&a.id()))
        });
        Ok(out)
    }

    /// Returns the operation log, newest-first (`DESIGN.md §6`).
    ///
    /// # Errors
    ///
    /// Returns store errors if any op in the history is missing or corrupt.
    pub fn op_log(&self) -> Result<Vec<Op>> {
        oplog::op_log(&self.store, &self.tack_dir)
    }

    /// Diffs two snapshots at file granularity (`DESIGN.md §8`).
    ///
    /// `from` and `to` are snapshot ids; `None` defaults are:
    ///
    /// * `to = None` → the **live working copy** (the current files on disk, the
    ///   same on-disk bytes [`status`](Self::status) reads); pass an explicit
    ///   snapshot id to diff a *recorded* snapshot instead. See
    ///   [`resolve_diff_trees`](Self::resolve_diff_trees) for how this relates to —
    ///   and can differ from — `status`.
    /// * `from = None` → the working copy's parent (its most recent cut), so the
    ///   default `diff()` shows uncommitted changes since the last cut — **including
    ///   dirty edits not yet snapshotted**.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem read failures while capturing the live
    /// working copy, or store errors if a snapshot or tree is missing or corrupt.
    pub fn diff(
        &self,
        from: Option<ObjectId>,
        to: Option<ObjectId>,
    ) -> Result<crate::diff::TreeDiff> {
        let (from_tree, to_tree) = self.resolve_diff_trees(from, to)?;
        crate::diff::diff_trees(&self.store, &from_tree, &to_tree)
    }

    /// Content-level (line) diff between two snapshots, using the **same default
    /// resolution** as [`diff`](Self::diff) (`DESIGN.md §8`).
    ///
    /// Returns per-file [`FilePatch`]es (hunks + line counts); the `--stat` view
    /// is a projection of these. Binary / oversize files are reported without
    /// hunks (see [`linediff`](crate::linediff)). With the default `to`, the live
    /// working copy is captured so dirty edits appear at line level (the on-disk
    /// bytes [`status`](Self::status) reads; the baseline is the last cut — see
    /// [`diff`](Self::diff)).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem read failures while capturing the live
    /// working copy, or store errors if a snapshot, tree, or blob is missing or
    /// corrupt.
    pub fn diff_patch(
        &self,
        from: Option<ObjectId>,
        to: Option<ObjectId>,
    ) -> Result<Vec<FilePatch>> {
        let (from_tree, to_tree) = self.resolve_diff_trees(from, to)?;
        crate::linediff::tree_patch(&self.store, &from_tree, &to_tree)
    }

    /// Resolves the `from`/`to` arguments to the pair of root tree ids to diff.
    ///
    /// Defaults match [`diff`](Self::diff):
    ///
    /// * `to = None` → the **live working copy** (the current files on disk), so a
    ///   default diff includes edits not yet snapshotted — the same on-disk bytes
    ///   [`status`](Self::status) reads. (It is *not* identical to `status`: this
    ///   diffs disk against the **last cut** — see `from` below — whereas `status`
    ///   compares disk against the *recorded* working-copy snapshot, so the two can
    ///   report different change sets after a manual
    ///   [`snapshot_working_copy`](Self::snapshot_working_copy) amend.) The live
    ///   tree is built from disk via [`live_tree`](Self::live_tree) (the stat-cache
    ///   fast path), which writes content-addressed blob/tree objects to the store
    ///   (deduplicated — unchanged content writes nothing new) but appends **no
    ///   op** and never moves the op-head: diff stays read-only with respect to
    ///   history. Pass an explicit `to` snapshot id to diff a *recorded* snapshot
    ///   instead (e.g. the working-copy snapshot id from
    ///   [`current`](Self::current)).
    /// * `from = None` → the parent cut of `to`. For the live working copy this is
    ///   the recorded working copy's parent — dirty edits amend the working copy
    ///   but never change its parent. A root working copy with no parent diffs
    ///   against the empty tree.
    fn resolve_diff_trees(
        &self,
        from: Option<ObjectId>,
        to: Option<ObjectId>,
    ) -> Result<(ObjectId, ObjectId)> {
        // `to`: an explicit snapshot, or the live on-disk working tree. Resolving
        // it also yields the parent cut used for the default `from`.
        let (to_tree, default_from) = if let Some(id) = to {
            let snap = self.snapshot_for(id)?;
            (snap.root_tree(), snap.parents().first().copied())
        } else {
            let live_tree = self.live_tree()?;
            let parent = self.working_copy()?.parents().first().copied();
            (live_tree, parent)
        };

        // `from`: an explicit snapshot, or the resolved default parent cut. With no
        // parent (a root working copy) diff against the empty tree.
        let from_tree = match from.or(default_from) {
            Some(id) => self.snapshot_for(id)?.root_tree(),
            None => crate::object::Tree::new(vec![])?.id(),
        };
        Ok((from_tree, to_tree))
    }

    // ── restore / undo (non-destructive; DESIGN §9) ───────────────────────────

    /// Non-destructively restores the working copy to `target` (`DESIGN.md §9`).
    ///
    /// `target` may be either an op id or a snapshot id:
    ///
    /// * an **op** → its recorded [`View`] is reinstated (the working copy it
    ///   pointed at).
    /// * a **snapshot** → a fresh [`View`] is synthesized with that snapshot as
    ///   the working copy.
    ///
    /// A **new** op is appended whose parent is the *current* op head and whose
    /// view installs the target tree as the working copy; the head advances to
    /// it. The filesystem is then re-projected from the restored root tree. The
    /// pre-restore state remains fully reachable (it is the new op's parent), so
    /// the restore is non-destructive — there is no `--hard` (`constitution §3`).
    ///
    /// Returns the id of the **new restore op** (the new head), so a caller can
    /// report where the repository now is and how to get back (its parent is the
    /// pre-restore head).
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`] if `target` resolves to neither a known
    /// op nor a known snapshot, or [`Error::Io`] / store errors during
    /// materialization.
    pub fn restore(&self, target: ObjectId) -> Result<ObjectId> {
        // The tree currently on disk (the pre-restore working copy) — needed to
        // compute which tracked files the restore must delete so the filesystem
        // becomes a faithful projection of the restored tree (`DESIGN.md §9`).
        let prev_tree = self.working_copy()?.root_tree();

        let restored_view = self.view_for_target(target)?;
        let view_id = self.store.put_view(&restored_view)?;
        let new_op = self.append_op_on_head(
            view_id,
            format!("restore to {}", target.short()),
            vec![
                "tack".to_string(),
                "restore".to_string(),
                "--to".to_string(),
                target.to_string(),
            ],
        )?;

        let working = self.store.get_snapshot(&restored_view.working_copy())?;
        project(
            &self.store,
            &prev_tree,
            &working.root_tree(),
            &self.work_dir,
        )?;
        Ok(new_op)
    }

    /// Reverses the most recent operation by reinstating its parent's view
    /// (`DESIGN.md §9`).
    ///
    /// Appends a **new** op whose parent is the current head and whose view is the
    /// view of the current op's parent, then advances the head and re-projects the
    /// filesystem. Like [`restore`](Self::restore), `undo` only ever appends — the
    /// undone op stays reachable, and undoing again would itself be undoable.
    ///
    /// Returns the id of the **new undo op** (the new head), so a caller can
    /// report the operation it reversed and where the working copy now points.
    ///
    /// # Errors
    ///
    /// * [`Error::Corruption`] if there is no op head, or the current op is the
    ///   root op (nothing to undo).
    /// * Store errors if any referenced object is missing.
    pub fn undo(&self) -> Result<ObjectId> {
        // The tree currently on disk, captured before the undo advances the head,
        // so the re-projection can delete tracked files the undone state lacked.
        let prev_tree = self.working_copy()?.root_tree();

        let current = oplog::current_op(&self.store, &self.tack_dir)?;
        let parent_id = current.parents().first().copied().ok_or_else(|| {
            Error::Corruption("nothing to undo: at the root operation".to_string())
        })?;
        let parent_op = self.store.get_op(&parent_id)?;
        let restored_view = self.store.get_view(&parent_op.view())?;

        // Re-store the parent's view (content-addressed: this is the same view
        // id) and point a new op at it, parented on the *current* head.
        let view_id = self.store.put_view(&restored_view)?;
        let new_op = self.append_op_on_head(
            view_id,
            format!("undo {}", current.id().short()),
            vec!["tack".to_string(), "undo".to_string()],
        )?;

        let working = self.store.get_snapshot(&restored_view.working_copy())?;
        project(
            &self.store,
            &prev_tree,
            &working.root_tree(),
            &self.work_dir,
        )?;
        Ok(new_op)
    }

    // ── coordination: advisory claims (DESIGN §13) ────────────────────────────

    /// Returns the currently-held advisory [`Claim`]s, derived by folding the
    /// op-log (`DESIGN.md §13`).
    ///
    /// Claims are advisory only — the engine never refuses a write to a claimed
    /// path; they let parallel agents avoid overlapping edits.
    ///
    /// # Errors
    ///
    /// Returns store errors if the op-log cannot be read.
    pub fn claims(&self) -> Result<Vec<Claim>> {
        Ok(crate::claims::current_claims(&self.op_log()?))
    }

    /// Records an advisory claim on `path` by `holder` with an optional `note`,
    /// returning the new op id (`DESIGN.md §13`).
    ///
    /// Recorded as an op whose view is **unchanged** (claiming a path does not
    /// touch the working copy); the held-claim set is recovered by folding the
    /// op-log. Re-claiming the same `(path, holder)` updates the note.
    ///
    /// # Errors
    ///
    /// Returns store / I/O errors if the op cannot be appended.
    pub fn claim(&self, path: &str, holder: &str, note: &str) -> Result<ObjectId> {
        let command = crate::claims::claim_command(path, holder, note);
        let description = format!("claim {} by {}", command[2], command[3]);
        let view_id = self.current_op()?.view();
        self.append_op_on_head(view_id, description, command)
    }

    /// Releases any advisory claim on `path` held by `holder`, returning the new
    /// op id (`DESIGN.md §13`).
    ///
    /// Recorded as an op (view unchanged); releasing a claim that was not held is
    /// a harmless recorded no-op.
    ///
    /// # Errors
    ///
    /// Returns store / I/O errors if the op cannot be appended.
    pub fn release(&self, path: &str, holder: &str) -> Result<ObjectId> {
        let command = crate::claims::release_command(path, holder);
        let description = format!("release {} by {}", command[2], command[3]);
        let view_id = self.current_op()?.view();
        self.append_op_on_head(view_id, description, command)
    }

    // ── scoped cut (DESIGN §13) ────────────────────────────────────────────────

    /// Creates a **scoped named cut**: a cut capturing only the working-copy
    /// content under `paths`, taking everything else verbatim from `base`
    /// (`DESIGN.md §13`).
    ///
    /// This lets one worker checkpoint just its own files without folding in
    /// other agents' concurrent edits to the shared working copy. The result is
    /// a named cut recorded as a side **head** (reachable via
    /// [`all_cuts`](Self::all_cuts)); the working-copy pointer and the filesystem
    /// are left **untouched**, so other workers are unaffected and no staging
    /// area persists between calls. The returned [`ScopedCutOutcome`] reports
    /// which scoped paths were captured and which *out-of-scope* paths differ
    /// from `base` (uncaptured work an agent may want to know about).
    ///
    /// `base` defaults to the cut the working copy currently sits on; pass
    /// `Some(id)` to overlay onto a specific cut (a snapshot id, or an op whose
    /// view's working copy is used).
    ///
    /// # Errors
    ///
    /// * [`Error::InvalidArgument`] if `paths` has no non-empty selector, or
    ///   `base` resolves to neither a snapshot nor an op.
    /// * Store / I/O errors if a tree cannot be built or written.
    pub fn scoped_cut(
        &self,
        paths: &[String],
        message: impl Into<String>,
        author: Identity,
        base: Option<ObjectId>,
    ) -> Result<ScopedCutOutcome> {
        let scope: Vec<String> = paths
            .iter()
            .map(|p| normalize_repo_path(p))
            .filter(|p| !p.is_empty())
            .collect();
        if scope.is_empty() {
            return Err(Error::InvalidArgument(
                "scoped cut requires at least one non-empty path".to_string(),
            ));
        }

        // Resolve the base snapshot to overlay onto: an explicit id (a snapshot,
        // or an op whose view's working copy is used), else the working copy's
        // current base cut (its first parent).
        let base_snapshot = match base {
            Some(id) => Some(self.base_snapshot_for(id)?),
            None => self.working_copy()?.parents().first().copied(),
        };
        let base_tree = match base_snapshot {
            Some(id) => self.store.get_snapshot(&id)?.root_tree(),
            None => crate::object::Tree::new(vec![])?.id(),
        };

        // The live on-disk content — read directly (via the stat cache), WITHOUT
        // advancing the working-copy snapshot, so concurrent edits by other agents
        // are never folded into the working copy.
        let live_tree = self.build_cached_tree()?;

        let base_map = flatten_tree_full(&self.store, &base_tree)?;
        let live_map = flatten_tree_full(&self.store, &live_tree)?;

        // Overlay: base, minus everything under scope, plus the live content
        // under scope.
        let mut result = base_map.clone();
        result.retain(|path, _| !path_in_scope(path, &scope));
        for (path, node) in &live_map {
            if path_in_scope(path, &scope) {
                result.insert(path.clone(), *node);
            }
        }
        let result_tree = tree_from_files(&self.store, &result)?;

        let (captured, outside_changes) = scope_change_report(&base_map, &live_map, &scope);

        // The scoped cut snapshot: parented on the base cut, carrying the message.
        let now = oplog::now_timestamp();
        let parents = base_snapshot.map(|id| vec![id]).unwrap_or_default();
        let cut = Snapshot::new(
            result_tree,
            parents,
            new_change_id(),
            author.clone(),
            author,
            message,
            now,
        );
        let cut_id = self.store.put_snapshot(&cut)?;

        // Record it as a side head; leave the working copy and filesystem alone.
        let view = self.current_view()?;
        let mut heads = view.heads().to_vec();
        if !heads.contains(&cut_id) {
            heads.push(cut_id);
        }
        let new_view = View::new(
            view.working_copy(),
            view.bookmarks().to_vec(),
            view.tags().to_vec(),
            heads,
        );
        let view_id = self.store.put_view(&new_view)?;
        let op = self.append_op_on_head(
            view_id,
            format!("scoped cut [{}]", scope.join(", ")),
            vec![
                "tack".to_string(),
                "snap".to_string(),
                "--only".to_string(),
                scope.join(","),
            ],
        )?;

        Ok(ScopedCutOutcome {
            cut: cut_id,
            op,
            base: base_snapshot,
            captured,
            outside_changes,
        })
    }

    // ── lanes, admission, and backports ───────────────────────────────────────

    /// Returns the current op-derived team lanes.
    ///
    /// Lanes are not stored in [`View`]. They are derived by folding admission
    /// ops from the operation log, so restore/undo do not silently erase team
    /// decisions.
    ///
    /// # Errors
    ///
    /// Returns store errors if the op-log cannot be read.
    pub fn lanes(&self) -> Result<Vec<Lane>> {
        Ok(derive_lanes(&self.op_log()?))
    }

    /// Records that `cut` is admitted to `lane`, leaving the repository view
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] if the lane is empty or `cut` is not a
    /// snapshot id.
    pub fn admit(&self, cut: ObjectId, lane: &str, reason: &str) -> Result<AdmissionOutcome> {
        let lane = normalise_lane(lane)?;
        ensure_snapshot_id(&self.store, cut, "admission cut")?;

        let view_id = self.current_op()?.view();
        let command = admit_command(&lane, cut, reason);
        let op =
            self.append_op_on_head(view_id, format!("admit {} to {lane}", cut.short()), command)?;
        Ok(AdmissionOutcome {
            lane,
            cut,
            op,
            reason: reason.to_owned(),
        })
    }

    /// Creates a target-lane backport proposal from `source_cut`.
    ///
    /// A clean backport creates a side-head cut and leaves the working copy
    /// untouched. A conflicting backport materializes a settlement working copy
    /// parented on the target lane cut; finish it with
    /// [`continue_backport`](Self::continue_backport).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] if the source is not a one-parent
    /// snapshot, the target lane has no admitted cut, or a related backport is
    /// already recorded.
    pub fn backport(
        &self,
        source_cut: ObjectId,
        target_lane: &str,
        message: Option<String>,
        reason: &str,
        author: Identity,
    ) -> Result<BackportOutcome> {
        let target_lane = normalise_lane(target_lane)?;
        let source = self
            .store
            .get_snapshot(&source_cut)
            .map_err(|err| match err {
                Error::Corruption(_) => Error::InvalidArgument(format!(
                    "backport source {} is not a snapshot",
                    source_cut.short()
                )),
                other => other,
            })?;
        let source_parent = *source.parents().first().ok_or_else(|| {
            Error::InvalidArgument("backport source must have exactly one parent".to_string())
        })?;
        if source.parents().len() != 1 {
            return Err(Error::InvalidArgument(
                "backport source must have exactly one parent".to_string(),
            ));
        }

        let lane = self
            .lanes()?
            .into_iter()
            .find(|candidate| candidate.name() == target_lane)
            .ok_or_else(|| {
                Error::InvalidArgument(format!("target lane {target_lane:?} has no admitted cut"))
            })?;
        let target_base = lane.cut();

        if let Some(record) = self.exact_backport(source_cut, &target_lane)? {
            return Ok(BackportOutcome::AlreadyPorted(record));
        }
        if let Some(record) = self.related_backport(source.change_id(), &target_lane)? {
            return Err(Error::InvalidArgument(format!(
                "related backport already exists for change {} on {target_lane}: {}",
                source.change_id().short(),
                record.result_cut().short()
            )));
        }

        let source_base = self.store.get_snapshot(&source_parent)?;
        let target = self.store.get_snapshot(&target_base)?;
        let plan = plan_backport(
            &self.store,
            source_base.root_tree(),
            source.root_tree(),
            target.root_tree(),
        )?;
        let message = message.unwrap_or_else(|| format!("Backport: {}", source.message()));
        let provenance = BackportProvenance::new(
            source_cut,
            source.change_id(),
            self.source_admission_for(source_cut)?,
            target_lane,
            target_base,
            reason.to_owned(),
        );

        if !plan.conflicts.is_empty() {
            return self.start_backport_settlement(&plan, provenance, &message);
        }

        let result_tree = tree_from_files(&self.store, &plan.result)?;
        let now = oplog::now_timestamp();
        let cut = Snapshot::new(
            result_tree,
            vec![target_base],
            source.change_id(),
            author.clone(),
            author,
            message,
            now,
        );
        let cut_id = self.store.put_snapshot(&cut)?;

        let view = self.current_view()?;
        let mut heads = view.heads().to_vec();
        if !heads.contains(&cut_id) {
            heads.push(cut_id);
        }
        let new_view = View::new(
            view.working_copy(),
            view.bookmarks().to_vec(),
            view.tags().to_vec(),
            heads,
        );
        let view_id = self.store.put_view(&new_view)?;
        let command = backport_command(&provenance, cut_id, "clean");
        let op = self.append_op_on_head(
            view_id,
            format!(
                "backport {} to {}",
                source_cut.short(),
                provenance.target_lane()
            ),
            command,
        )?;

        Ok(BackportOutcome::Created(BackportRecord::new(
            provenance, cut_id, op, "clean",
        )))
    }

    /// Finishes the currently materialized manual backport settlement.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] if the current op is not a pending
    /// backport settlement.
    pub fn continue_backport(&self, author: Identity) -> Result<BackportOutcome> {
        let pending_op = self.current_op()?;
        let pending = parse_pending_backport(pending_op.metadata().command()).ok_or_else(|| {
            Error::InvalidArgument("current state is not a pending backport settlement".to_string())
        })?;

        self.snapshot_working_copy()?;
        let view = self.current_view()?;
        let working = self.store.get_snapshot(&view.working_copy())?;
        let now = oplog::now_timestamp();
        let resolved = Snapshot::new(
            working.root_tree(),
            vec![pending.provenance.target_base()],
            pending.provenance.source_change_id(),
            author.clone(),
            author,
            pending.message,
            now,
        );
        let resolved_id = self.store.put_snapshot(&resolved)?;

        let default_id = default_identity();
        let child = Snapshot::new(
            working.root_tree(),
            vec![resolved_id],
            new_change_id(),
            default_id.clone(),
            default_id,
            String::new(),
            now,
        );
        let child_id = self.store.put_snapshot(&child)?;
        let new_view = replace_working_copy(&view, view.working_copy(), child_id);
        let view_id = self.store.put_view(&new_view)?;
        let command = backport_command(&pending.provenance, resolved_id, "manual");
        let op = self.append_op_on_head(
            view_id,
            format!(
                "finish backport {} to {}",
                pending.provenance.source_cut().short(),
                pending.provenance.target_lane()
            ),
            command,
        )?;

        Ok(BackportOutcome::Created(BackportRecord::new(
            pending.provenance,
            resolved_id,
            op,
            "manual",
        )))
    }

    /// Resolves a scoped-cut base id to a snapshot id: a snapshot is used
    /// directly; an op yields its view's working copy.
    fn base_snapshot_for(&self, id: ObjectId) -> Result<ObjectId> {
        self.resolve_to_snapshot_id(id, "scoped cut base")
    }

    /// Resolves an id that should name a tree-bearing state to its snapshot id: a
    /// snapshot id is used directly; an op id yields its view's working-copy
    /// snapshot (so the same id forms `restore`, `scoped_cut`, and `diff` accept
    /// agree). Any other object kind (e.g. a tree or blob id) is a clear
    /// [`Error::InvalidArgument`] labeled by `what`, rather than an opaque
    /// snapshot type-mismatch from the store.
    fn resolve_to_snapshot_id(&self, id: ObjectId, what: &str) -> Result<ObjectId> {
        let (tag, _bytes) = self.store.get_raw(&id)?;
        match tag {
            TypeTag::Snapshot => Ok(id),
            TypeTag::Op => Ok(self
                .store
                .get_view(&self.store.get_op(&id)?.view())?
                .working_copy()),
            _ => Err(Error::InvalidArgument(format!(
                "{what} {} is neither a snapshot nor an op",
                id.short()
            ))),
        }
    }

    // ── object inspection ──────────────────────────────────────────────────────

    /// Returns the raw type tag and canonical bytes of the object at `id`
    /// (`DESIGN.md §12`, `tack cat`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`] or [`Error::Corruption`].
    pub fn cat(&self, id: &ObjectId) -> Result<(TypeTag, Vec<u8>)> {
        self.store.get_raw(id)
    }

    /// Resolves a hex `prefix` (≥ 1 char) to a single full [`ObjectId`].
    ///
    /// Scans the object-store fan-out for ids whose hex representation begins
    /// with `prefix`. A full 64-char id resolves without scanning. The `.tack/`
    /// fan-out layout is `objects/<aa>/<rest>`, so the search reads at most one
    /// fan-out sub-directory.
    ///
    /// # Errors
    ///
    /// * [`Error::InvalidObjectId`] if `prefix` is empty, too long, or non-hex.
    /// * [`Error::PrefixNotFound`] if no object matches.
    /// * [`Error::Corruption`] if the prefix is ambiguous (matches ≥ 2 objects).
    pub fn resolve_prefix(&self, prefix: &str) -> Result<ObjectId> {
        if prefix.is_empty() || prefix.len() > 64 || !prefix.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(Error::InvalidObjectId(prefix.to_string()));
        }
        let prefix = prefix.to_ascii_lowercase();

        // Exact full id: parse directly.
        if prefix.len() == 64 {
            return prefix.parse();
        }

        let objects = self.tack_dir.join("objects");
        let mut matches: Vec<ObjectId> = Vec::new();

        // The first two hex chars name the fan-out sub-directory; if the prefix
        // is at least that long we only need to scan one sub-directory, else we
        // must scan every sub-directory whose name starts with the prefix.
        let sub_dirs: Vec<PathBuf> = if prefix.len() >= 2 {
            vec![objects.join(&prefix[..2])]
        } else {
            collect_matching_subdirs(&objects, &prefix)?
        };

        for sub in sub_dirs {
            let two = sub
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
                .unwrap_or_default();
            let read = match std::fs::read_dir(&sub) {
                Ok(read) => read,
                // A missing fan-out dir simply means no candidates there.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(Error::Io(e)),
            };
            for entry in read {
                let entry = entry?;
                let entry_path = entry.path();
                // Skip in-flight temp files from interrupted writes. The store
                // writes these with a literal `tmp` extension (see store.rs).
                if entry_path.extension().is_some_and(|ext| ext == "tmp") {
                    continue;
                }
                let Some(rest) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let full = format!("{two}{rest}");
                if full.starts_with(&prefix)
                    && let Ok(id) = full.parse::<ObjectId>()
                {
                    matches.push(id);
                }
            }
        }

        match matches.as_slice() {
            [] => Err(Error::PrefixNotFound(prefix)),
            [only] => Ok(*only),
            _ => Err(Error::Corruption(format!(
                "object id prefix {prefix:?} is ambiguous ({} matches)",
                matches.len()
            ))),
        }
    }

    // ── internal helpers ───────────────────────────────────────────────────────

    /// Appends an op parented on the *current* op head and advances the head.
    fn append_op_on_head(
        &self,
        view_id: ObjectId,
        description: impl Into<String>,
        command: Vec<String>,
    ) -> Result<ObjectId> {
        let parent = oplog::op_head(&self.tack_dir)?;
        let parents = parent.into_iter().collect();
        oplog::append_op(
            &self.store,
            &self.tack_dir,
            parents,
            view_id,
            description,
            command,
        )
    }

    /// Loads the [`Snapshot`] a [`diff`](Self::diff) endpoint id names.
    ///
    /// Accepts a snapshot id directly or an op id (via its view's working copy),
    /// matching `restore`/`scoped_cut`, so `diff --from`/`--to` can take either. A
    /// tree or blob id yields a clear [`Error::InvalidArgument`].
    fn snapshot_for(&self, id: ObjectId) -> Result<Snapshot> {
        let snap_id = self.resolve_to_snapshot_id(id, "diff endpoint")?;
        self.store.get_snapshot(&snap_id)
    }

    /// Resolves a restore/undo `target` (op or snapshot) into the [`View`] to
    /// install.
    ///
    /// An op id yields its recorded view; a snapshot id yields a synthesized view
    /// with that snapshot as the working copy (`DESIGN.md §9` step 1).
    fn view_for_target(&self, target: ObjectId) -> Result<View> {
        let (tag, _bytes) = self.store.get_raw(&target)?;
        match tag {
            TypeTag::Op => {
                let op = self.store.get_op(&target)?;
                self.store.get_view(&op.view())
            }
            TypeTag::Snapshot => {
                // Synthesize a view whose working copy is the target snapshot.
                Ok(View::new(target, Vec::new(), Vec::new(), vec![target]))
            }
            _ => Err(Error::Corruption(format!(
                "restore target {} is neither an op nor a snapshot",
                target.short()
            ))),
        }
    }

    fn start_backport_settlement(
        &self,
        plan: &BackportPlan,
        provenance: BackportProvenance,
        message: &str,
    ) -> Result<BackportOutcome> {
        self.snapshot_working_copy()?;
        let view = self.current_view()?;
        let prev_tree = self.store.get_snapshot(&view.working_copy())?.root_tree();
        let result_tree = tree_from_files(&self.store, &plan.result)?;

        let identity = default_identity();
        let settlement = Snapshot::new(
            result_tree,
            vec![provenance.target_base()],
            provenance.source_change_id(),
            identity.clone(),
            identity,
            String::new(),
            oplog::now_timestamp(),
        );
        let settlement_id = self.store.put_snapshot(&settlement)?;
        let new_view = replace_working_copy(&view, view.working_copy(), settlement_id);
        let view_id = self.store.put_view(&new_view)?;
        let command = pending_backport_command(&provenance, message, &plan.conflicts);
        let op = self.append_op_on_head(
            view_id,
            format!(
                "settle backport {} to {}",
                provenance.source_cut().short(),
                provenance.target_lane()
            ),
            command,
        )?;
        project(&self.store, &prev_tree, &result_tree, &self.work_dir)?;

        Ok(BackportOutcome::Settlement(BackportSettlement {
            provenance,
            op,
            conflicts: plan.conflicts.clone(),
        }))
    }

    fn exact_backport(
        &self,
        source_cut: ObjectId,
        target_lane: &str,
    ) -> Result<Option<BackportRecord>> {
        Ok(self.backport_records()?.into_iter().find(|record| {
            record.source_cut() == source_cut && record.target_lane() == target_lane
        }))
    }

    fn related_backport(
        &self,
        source_change_id: ObjectId,
        target_lane: &str,
    ) -> Result<Option<BackportRecord>> {
        Ok(self.backport_records()?.into_iter().find(|record| {
            record.source_change_id() == source_change_id && record.target_lane() == target_lane
        }))
    }

    fn backport_records(&self) -> Result<Vec<BackportRecord>> {
        let mut records = Vec::new();
        for op in self.op_log()? {
            if let Some(mut record) = parse_backport_record(op.metadata().command()) {
                record.op = op.id();
                records.push(record);
            }
        }
        Ok(records)
    }

    fn source_admission_for(&self, cut: ObjectId) -> Result<Option<SourceAdmission>> {
        Ok(derive_admissions(&self.op_log()?)
            .into_iter()
            .rev()
            .find(|admission| admission.cut() == cut)
            .map(|admission| SourceAdmission {
                lane: admission.name().to_owned(),
                op: admission.admission(),
            }))
    }
}

// ── scoped-cut result + helpers ────────────────────────────────────────────────

/// The outcome of a [`Repository::scoped_cut`].
///
/// Carries the new cut and op ids, the base it overlaid onto, and the in-scope
/// vs out-of-scope change reports so an agent (or coordinator) can see exactly
/// what the scoped cut captured and what concurrent work it deliberately left
/// out.
#[derive(Debug, Clone)]
pub struct ScopedCutOutcome {
    cut: ObjectId,
    op: ObjectId,
    base: Option<ObjectId>,
    captured: Vec<String>,
    outside_changes: Vec<String>,
}

impl ScopedCutOutcome {
    /// The id of the new scoped named cut.
    pub const fn cut(&self) -> ObjectId {
        self.cut
    }

    /// The id of the op that recorded the scoped cut.
    pub const fn op(&self) -> ObjectId {
        self.op
    }

    /// The base cut the scoped paths were overlaid onto, if any.
    pub const fn base(&self) -> Option<ObjectId> {
        self.base
    }

    /// The in-scope paths captured by this cut (those that differ from `base`).
    pub fn captured(&self) -> &[String] {
        &self.captured
    }

    /// Out-of-scope paths that differ from `base` — uncaptured work an agent may
    /// want to know about.
    pub fn outside_changes(&self) -> &[String] {
        &self.outside_changes
    }
}

/// One lane admission derived from the op-log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    name: String,
    cut: ObjectId,
    admission: ObjectId,
    reason: String,
    timestamp: i64,
}

impl Lane {
    /// Returns the lane name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the currently admitted cut.
    pub const fn cut(&self) -> ObjectId {
        self.cut
    }

    /// Returns the op that admitted the current cut.
    pub const fn admission(&self) -> ObjectId {
        self.admission
    }

    /// Returns the admission reason, if any.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns the admission timestamp as Unix seconds.
    pub const fn timestamp(&self) -> i64 {
        self.timestamp
    }
}

/// The outcome of admitting a cut into a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionOutcome {
    lane: String,
    cut: ObjectId,
    op: ObjectId,
    reason: String,
}

impl AdmissionOutcome {
    /// Returns the lane name.
    pub fn lane(&self) -> &str {
        &self.lane
    }

    /// Returns the admitted cut id.
    pub const fn cut(&self) -> ObjectId {
        self.cut
    }

    /// Returns the op that recorded the admission.
    pub const fn op(&self) -> ObjectId {
        self.op
    }

    /// Returns the admission reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// The result of a backport operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackportOutcome {
    /// A new target-lane cut was created.
    Created(BackportRecord),
    /// A settlement working copy was materialized and must be continued.
    Settlement(BackportSettlement),
    /// The same source cut was already ported to the target lane.
    AlreadyPorted(BackportRecord),
}

impl BackportOutcome {
    /// Returns the target-lane result cut when one exists.
    pub const fn result_cut(&self) -> Option<ObjectId> {
        match self {
            Self::Created(record) | Self::AlreadyPorted(record) => Some(record.result_cut),
            Self::Settlement(_) => None,
        }
    }
}

/// Provenance recorded for a target-lane backport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackportProvenance {
    source_cut: ObjectId,
    source_change_id: ObjectId,
    source_admission: Option<SourceAdmission>,
    target_lane: String,
    target_base: ObjectId,
    reason: String,
}

impl BackportProvenance {
    const fn new(
        source_cut: ObjectId,
        source_change_id: ObjectId,
        source_admission: Option<SourceAdmission>,
        target_lane: String,
        target_base: ObjectId,
        reason: String,
    ) -> Self {
        Self {
            source_cut,
            source_change_id,
            source_admission,
            target_lane,
            target_base,
            reason,
        }
    }

    /// Returns the source fix cut id.
    pub const fn source_cut(&self) -> ObjectId {
        self.source_cut
    }

    /// Returns the logical source change id.
    pub const fn source_change_id(&self) -> ObjectId {
        self.source_change_id
    }

    /// Returns the source admission, if the source cut was admitted to a lane.
    pub const fn source_admission(&self) -> Option<&SourceAdmission> {
        self.source_admission.as_ref()
    }

    /// Returns the target lane name.
    pub fn target_lane(&self) -> &str {
        &self.target_lane
    }

    /// Returns the target base cut id.
    pub const fn target_base(&self) -> ObjectId {
        self.target_base
    }

    /// Returns the reason supplied for the backport.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A source lane admission referenced by backport provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAdmission {
    lane: String,
    op: ObjectId,
}

impl SourceAdmission {
    /// Returns the source lane name.
    pub fn lane(&self) -> &str {
        &self.lane
    }

    /// Returns the source admission op id.
    pub const fn op(&self) -> ObjectId {
        self.op
    }
}

/// A completed clean or manual backport record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackportRecord {
    provenance: BackportProvenance,
    result_cut: ObjectId,
    op: ObjectId,
    method: String,
}

impl BackportRecord {
    fn new(
        provenance: BackportProvenance,
        result_cut: ObjectId,
        op: ObjectId,
        method: impl Into<String>,
    ) -> Self {
        Self {
            provenance,
            result_cut,
            op,
            method: method.into(),
        }
    }

    /// Returns the backport provenance.
    pub const fn provenance(&self) -> &BackportProvenance {
        &self.provenance
    }

    /// Returns the source fix cut id.
    pub const fn source_cut(&self) -> ObjectId {
        self.provenance.source_cut
    }

    /// Returns the source logical change id.
    pub const fn source_change_id(&self) -> ObjectId {
        self.provenance.source_change_id
    }

    /// Returns the target lane name.
    pub fn target_lane(&self) -> &str {
        &self.provenance.target_lane
    }

    /// Returns the resulting target-lane cut.
    pub const fn result_cut(&self) -> ObjectId {
        self.result_cut
    }

    /// Returns the op that recorded the completed backport.
    pub const fn op(&self) -> ObjectId {
        self.op
    }

    /// Returns the completion method (`clean` or `manual`).
    pub fn method(&self) -> &str {
        &self.method
    }
}

/// A materialized backport settlement that needs manual continuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackportSettlement {
    provenance: BackportProvenance,
    op: ObjectId,
    conflicts: Vec<String>,
}

impl BackportSettlement {
    /// Returns the pending backport provenance.
    pub const fn provenance(&self) -> &BackportProvenance {
        &self.provenance
    }

    /// Returns the op that materialized the settlement.
    pub const fn op(&self) -> ObjectId {
        self.op
    }

    /// Returns the conflicted paths.
    pub fn conflicts(&self) -> &[String] {
        &self.conflicts
    }
}

#[derive(Debug, Clone)]
struct BackportPlan {
    result: BTreeMap<PathBuf, FileNode>,
    conflicts: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingBackport {
    provenance: BackportProvenance,
    message: String,
}

fn normalise_lane(lane: &str) -> Result<String> {
    let lane = lane.trim();
    if lane.is_empty() {
        return Err(Error::InvalidArgument("lane must not be empty".to_string()));
    }
    Ok(lane.to_owned())
}

fn ensure_snapshot_id(store: &ObjectStore, id: ObjectId, what: &str) -> Result<()> {
    let (tag, _) = store.get_raw(&id)?;
    if tag == TypeTag::Snapshot {
        Ok(())
    } else {
        Err(Error::InvalidArgument(format!(
            "{what} {} is not a snapshot",
            id.short()
        )))
    }
}

fn admit_command(lane: &str, cut: ObjectId, reason: &str) -> Vec<String> {
    vec![
        "tack".to_string(),
        "admit".to_string(),
        format!("lane={lane}"),
        format!("cut={cut}"),
        format!("reason={reason}"),
    ]
}

fn backport_command(
    provenance: &BackportProvenance,
    result_cut: ObjectId,
    method: &str,
) -> Vec<String> {
    let mut command = provenance_command("backport", provenance);
    command.push(format!("result_cut={result_cut}"));
    command.push(format!("method={method}"));
    command
}

fn pending_backport_command(
    provenance: &BackportProvenance,
    message: &str,
    conflicts: &[String],
) -> Vec<String> {
    let mut command = provenance_command("backport_settle", provenance);
    command.push(format!("message={message}"));
    for conflict in conflicts {
        command.push(format!("conflict={conflict}"));
    }
    command
}

fn provenance_command(verb: &str, provenance: &BackportProvenance) -> Vec<String> {
    let mut command = vec![
        "tack".to_string(),
        verb.to_string(),
        format!("source_cut={}", provenance.source_cut),
        format!("source_change_id={}", provenance.source_change_id),
        format!("target_lane={}", provenance.target_lane),
        format!("target_base={}", provenance.target_base),
        format!("reason={}", provenance.reason),
    ];
    if let Some(source) = &provenance.source_admission {
        command.push(format!("source_lane={}", source.lane));
        command.push(format!("source_admission={}", source.op));
    }
    command
}

fn parse_key<'a>(command: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    command.iter().find_map(|arg| arg.strip_prefix(&prefix))
}

fn parse_object_key(command: &[String], key: &str) -> Option<ObjectId> {
    parse_key(command, key).and_then(|value| value.parse().ok())
}

fn parse_admission(op: &Op) -> Option<Lane> {
    let command = op.metadata().command();
    if command.get(1).is_none_or(|verb| verb != "admit") {
        return None;
    }
    let name = parse_key(command, "lane")?.to_owned();
    let cut = parse_object_key(command, "cut")?;
    let reason = parse_key(command, "reason").unwrap_or("").to_owned();
    Some(Lane {
        name,
        cut,
        admission: op.id(),
        reason,
        timestamp: op.metadata().start().unix_secs(),
    })
}

fn derive_admissions(ops: &[Op]) -> Vec<Lane> {
    ops.iter().rev().filter_map(parse_admission).collect()
}

fn derive_lanes(ops: &[Op]) -> Vec<Lane> {
    let mut lanes = BTreeMap::<String, Lane>::new();
    for admission in derive_admissions(ops) {
        lanes.insert(admission.name.clone(), admission);
    }
    lanes.into_values().collect()
}

fn parse_provenance(command: &[String]) -> Option<BackportProvenance> {
    let source_cut = parse_object_key(command, "source_cut")?;
    let source_change_id = parse_object_key(command, "source_change_id")?;
    let target_lane = parse_key(command, "target_lane")?.to_owned();
    let target_base = parse_object_key(command, "target_base")?;
    let reason = parse_key(command, "reason").unwrap_or("").to_owned();
    let source_lane = parse_key(command, "source_lane").map(str::to_owned);
    let source_admission = parse_object_key(command, "source_admission");
    let source_admission = match (source_lane, source_admission) {
        (Some(lane), Some(op)) => Some(SourceAdmission { lane, op }),
        _ => None,
    };
    Some(BackportProvenance::new(
        source_cut,
        source_change_id,
        source_admission,
        target_lane,
        target_base,
        reason,
    ))
}

fn parse_backport_record(command: &[String]) -> Option<BackportRecord> {
    if command.get(1).is_none_or(|verb| verb != "backport") {
        return None;
    }
    let provenance = parse_provenance(command)?;
    let result_cut = parse_object_key(command, "result_cut")?;
    let method = parse_key(command, "method").unwrap_or("clean").to_owned();
    Some(BackportRecord::new(
        provenance,
        result_cut,
        ObjectId::from_bytes([0; 32]),
        method,
    ))
}

fn parse_pending_backport(command: &[String]) -> Option<PendingBackport> {
    if command.get(1).is_none_or(|verb| verb != "backport_settle") {
        return None;
    }
    Some(PendingBackport {
        provenance: parse_provenance(command)?,
        message: parse_key(command, "message")?.to_owned(),
    })
}

fn plan_backport(
    store: &ObjectStore,
    source_base_tree: ObjectId,
    source_tree: ObjectId,
    target_tree: ObjectId,
) -> Result<BackportPlan> {
    let source_base = flatten_tree_full(store, &source_base_tree)?;
    let source = flatten_tree_full(store, &source_tree)?;
    let target = flatten_tree_full(store, &target_tree)?;
    let mut result = target.clone();
    let mut conflicts = Vec::new();

    let mut paths = BTreeSet::new();
    paths.extend(source_base.keys().cloned());
    paths.extend(source.keys().cloned());

    for path in paths {
        let before = source_base.get(&path);
        let after = source.get(&path);
        if before == after {
            continue;
        }
        let target_node = target.get(&path);
        match (before, after, target_node) {
            (None, Some(new), None) => {
                result.insert(path, *new);
            }
            (None, Some(new), Some(existing)) if existing == new => {}
            (Some(old), None, Some(existing)) if existing == old => {
                result.remove(&path);
            }
            (Some(_old), None, None) => {}
            (Some(old), Some(new), Some(existing)) if existing == old => {
                result.insert(path, *new);
            }
            (Some(_old), Some(new), Some(existing)) if existing == new => {}
            _ => conflicts.push(path_to_slash(&path)),
        }
    }

    conflicts.sort();
    Ok(BackportPlan { result, conflicts })
}

/// Returns `true` if a repo-relative `path` falls under any scope selector
/// (equal to it, or nested beneath it as a directory).
fn path_in_scope(path: &Path, scope: &[String]) -> bool {
    let p = path_to_slash(path);
    scope
        .iter()
        .any(|sel| p == *sel || p.starts_with(&format!("{sel}/")))
}

/// Compares `base` vs `live` file maps, returning `(captured, outside_changes)`:
/// the in-scope paths that differ (what a scoped cut captures) and the
/// out-of-scope paths that differ (uncaptured work), both sorted forward-slashed.
fn scope_change_report(
    base: &BTreeMap<PathBuf, FileNode>,
    live: &BTreeMap<PathBuf, FileNode>,
    scope: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut keys: Vec<&PathBuf> = base.keys().chain(live.keys()).collect();
    keys.sort();
    keys.dedup();

    let mut captured = Vec::new();
    let mut outside = Vec::new();
    for key in keys {
        if base.get(key) == live.get(key) {
            continue; // unchanged on both sides
        }
        let slashed = path_to_slash(key);
        if path_in_scope(key, scope) {
            captured.push(slashed);
        } else {
            outside.push(slashed);
        }
    }
    (captured, outside)
}

// ── free helpers ─────────────────────────────────────────────────────────────

/// The default identity for un-named (working-copy) snapshots and the root op.
///
/// v0 is single-user with no configured identity; this is a stable placeholder
/// that a future config/identity layer will replace (`constitution.md §6`).
fn default_identity() -> Identity {
    Identity::new("tack", "tack@localhost")
}

/// Mints a fresh, effectively-unique `change_id` (`DESIGN.md §4`).
///
/// A `change_id` must be random at birth and stable across amends. v0 has no
/// `rand` dependency, so we derive 32 bytes by hashing a high-entropy seed —
/// the current time in nanoseconds, the process id, and a per-process monotonic
/// counter — through BLAKE3. Collisions are vanishingly unlikely; this is a
/// nonce, never a content address, so it is hashed outside the typed object
/// scheme deliberately.
fn new_change_id() -> ObjectId {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let counter = CHANGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tack-change-id-v1");
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    hasher.update(&pid.to_le_bytes());
    ObjectId::from_bytes(*hasher.finalize().as_bytes())
}

/// Builds a new [`View`] identical to `view` but with the working-copy snapshot
/// (and the matching anonymous head, if present) swapped from `old` to `new`.
fn replace_working_copy(view: &View, old: ObjectId, new: ObjectId) -> View {
    let heads: Vec<ObjectId> = view
        .heads()
        .iter()
        .map(|&h| if h == old { new } else { h })
        .collect();
    // If the old working copy was not among the heads, still ensure the new one
    // is tracked so it stays reachable.
    let heads = if heads.contains(&new) {
        heads
    } else {
        let mut heads = heads;
        heads.push(new);
        heads
    };
    View::new(new, view.bookmarks().to_vec(), view.tags().to_vec(), heads)
}

/// Discovers the repository root (the directory *containing* `.tack/`) by
/// walking `start` and its ancestors upward.
fn discover_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(TACK_DIR).join(CONFIG_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Writes `config` to `.tack/config` as pretty JSON (not content-addressed;
/// `DESIGN.md §3`, §15).
fn write_config(tack_dir: &Path, config: &Config) -> Result<()> {
    let json = serde_json::to_vec_pretty(config)
        .map_err(|e| Error::Corruption(format!("failed to serialize config: {e}")))?;
    std::fs::write(tack_dir.join(CONFIG_FILE), json)?;
    Ok(())
}

/// Reads and parses `.tack/config`.
fn read_config(tack_dir: &Path) -> Result<Config> {
    let bytes = std::fs::read(tack_dir.join(CONFIG_FILE))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Corruption(format!("failed to parse config: {e}")))
}

/// Returns fan-out sub-directories of `objects/` whose two-char name begins with
/// the (sub-two-char) `prefix`. Used only when a prefix is shorter than the
/// fan-out width.
fn collect_matching_subdirs(objects: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let read = match std::fs::read_dir(objects) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in read {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with(prefix)
        {
            out.push(entry.path());
        }
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn alice() -> Identity {
        Identity::new("Alice", "alice@example.com")
    }

    /// Writes `content` to `root/rel`, creating parent directories.
    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdirs");
        }
        fs::write(path, content).expect("write");
    }

    // ── init / open ────────────────────────────────────────────────────────────

    #[test]
    fn init_creates_root_op_and_view() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        // Config exists and records the current format version.
        let config = read_config(repo.tack_dir())?;
        assert_eq!(config.format_version, FORMAT_VERSION);

        // There is exactly one op (the root) with no parents.
        let ops = repo.op_log()?;
        assert_eq!(ops.len(), 1);
        assert!(ops[0].parents().is_empty());

        // The working copy is an empty, un-named snapshot.
        let wc = repo.working_copy()?;
        assert!(wc.message().is_empty());
        assert!(wc.parents().is_empty());
        Ok(())
    }

    #[test]
    fn init_then_open_round_trip() -> Result<()> {
        let dir = TempDir::new()?;
        let created = Repository::init(dir.path())?;
        let created_wc = created.working_copy()?.id();

        let opened = Repository::open(dir.path())?;
        assert_eq!(opened.working_copy()?.id(), created_wc);
        assert_eq!(opened.work_dir(), dir.path());
        Ok(())
    }

    #[test]
    fn open_discovers_from_subdirectory() -> Result<()> {
        let dir = TempDir::new()?;
        Repository::init(dir.path())?;
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested)?;
        let opened = Repository::open(&nested)?;
        assert_eq!(opened.work_dir(), dir.path());
        Ok(())
    }

    #[test]
    fn open_refuses_newer_format() -> Result<()> {
        let dir = TempDir::new()?;
        Repository::init(dir.path())?;
        // Hand-write a config with a newer format version.
        let newer = Config {
            format_version: FORMAT_VERSION + 1,
            min_chunk: 2048,
            avg_chunk: 8192,
            max_chunk: 65536,
        };
        write_config(&dir.path().join(TACK_DIR), &newer)?;
        let result = Repository::open(dir.path());
        assert!(
            matches!(result, Err(Error::FormatVersionMismatch { .. })),
            "expected FormatVersionMismatch, got {result:?}"
        );
        Ok(())
    }

    #[test]
    fn init_refuses_double_init() -> Result<()> {
        let dir = TempDir::new()?;
        Repository::init(dir.path())?;
        assert!(matches!(Repository::init(dir.path()), Err(Error::Io(_))));
        Ok(())
    }

    #[test]
    fn open_without_repo_is_error() {
        let dir = TempDir::new().expect("temp");
        assert!(matches!(Repository::open(dir.path()), Err(Error::Io(_))));
    }

    // ── snapshot_working_copy ────────────────────────────────────────────────────

    #[test]
    fn snapshot_captures_written_file() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let before = repo.working_copy()?;

        write_file(dir.path(), "hello.txt", b"world");
        let new_wc = repo.snapshot_working_copy()?;
        assert_ne!(
            new_wc,
            before.id(),
            "writing a file must change the working copy"
        );

        // The captured tree contains the file.
        let wc = repo.store().get_snapshot(&new_wc)?;
        let entry = crate::tree::read_tree_path(repo.store(), &wc.root_tree(), "hello.txt")?;
        assert_eq!(crate::blob::read_blob(repo.store(), &entry.id())?, b"world");

        // change_id is preserved across the amend.
        assert_eq!(wc.change_id(), before.change_id(), "amend keeps change_id");
        Ok(())
    }

    #[test]
    fn snapshot_no_op_when_unchanged() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let ops_before = repo.op_log()?.len();
        let first = repo.snapshot_working_copy()?;
        let second = repo.snapshot_working_copy()?;
        assert_eq!(first, second, "unchanged snapshot must be a no-op");
        assert_eq!(
            repo.op_log()?.len(),
            ops_before,
            "no-op must not append an op"
        );
        Ok(())
    }

    // ── stat cache (advisory build_tree fast path) ───────────────────────────────

    /// The stat cache must be **invisible to correctness**: a capture that uses
    /// the cache produces the same root tree as a capture with no cache file.
    #[test]
    fn stat_cache_matches_a_cacheless_rebuild() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"alpha\n");
        write_file(dir.path(), "sub/b.txt", b"beta\n");

        // First build writes the cache; the file must now exist.
        let with_cache = repo.live_tree()?;
        assert!(
            repo.wc_cache_path().is_file(),
            "a capture must persist the stat cache"
        );

        // Wipe the cache → the next build cannot reuse anything (full rebuild).
        std::fs::remove_file(repo.wc_cache_path())?;
        let rebuilt = repo.live_tree()?;

        assert_eq!(
            with_cache, rebuilt,
            "the cache must never change the captured tree"
        );
        Ok(())
    }

    /// A cache hit on unchanged content is fine, but a real edit must still change
    /// the tree — the cache must never mask a modification.
    #[test]
    fn cached_capture_still_detects_edits() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one\n");

        let t1 = repo.live_tree()?; // populates the cache
        let t1_again = repo.live_tree()?; // cache hit, identical content
        assert_eq!(
            t1, t1_again,
            "unchanged content must yield the same tree (cache hit)"
        );

        write_file(dir.path(), "a.txt", b"one and two\n"); // edit (different size)
        let t2 = repo.live_tree()?;
        assert_ne!(
            t2, t1,
            "an edit must change the tree even through the cache"
        );

        // And a cacheless rebuild of the edited tree agrees.
        std::fs::remove_file(repo.wc_cache_path())?;
        assert_eq!(
            repo.live_tree()?,
            t2,
            "edited tree must match a cacheless rebuild"
        );
        Ok(())
    }

    // ── named_cut ────────────────────────────────────────────────────────────────

    #[test]
    fn two_named_cuts_show_in_log_in_order() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "a.txt", b"one");
        let cut1 = repo.named_cut("first cut", alice())?;

        write_file(dir.path(), "b.txt", b"two");
        let cut2 = repo.named_cut("second cut", alice())?;

        let log = repo.log()?;
        assert_eq!(log.len(), 2, "two named cuts expected");
        // Newest-first.
        assert_eq!(log[0].id(), cut2);
        assert_eq!(log[0].message(), "second cut");
        assert_eq!(log[1].id(), cut1);
        assert_eq!(log[1].message(), "first cut");

        // Parent linkage: cut2's ancestry reaches cut1.
        assert_eq!(log[0].parents().first().copied(), Some(cut1));
        Ok(())
    }

    #[test]
    fn cut_starts_new_change_id_amend_keeps_it() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "a.txt", b"one");
        // Amend: change_id stable.
        let wc1 = repo.snapshot_working_copy()?;
        let cid_before = repo.store().get_snapshot(&wc1)?.change_id();
        write_file(dir.path(), "a.txt", b"one-edited");
        let wc2 = repo.snapshot_working_copy()?;
        assert_eq!(
            repo.store().get_snapshot(&wc2)?.change_id(),
            cid_before,
            "amend must keep change_id"
        );

        // Cut: the fresh child working copy gets a NEW change_id.
        let closed = repo.named_cut("cut", alice())?;
        let new_wc = repo.working_copy()?;
        assert_ne!(
            new_wc.change_id(),
            repo.store().get_snapshot(&closed)?.change_id(),
            "a cut must start a new change_id for the child working copy"
        );
        Ok(())
    }

    #[test]
    fn named_cut_appends_one_op_each() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let base = repo.op_log()?.len();
        write_file(dir.path(), "a.txt", b"x");
        repo.named_cut("c1", alice())?;
        // named_cut auto-snapshots (1 op) then cuts (1 op) = 2 new ops.
        assert_eq!(repo.op_log()?.len(), base + 2);
        Ok(())
    }

    // ── status ───────────────────────────────────────────────────────────────────

    #[test]
    fn status_reports_added_file() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "new.txt", b"hi");
        let st = repo.status()?;
        let added: Vec<String> = st
            .added()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(added, vec!["new.txt"]);
        Ok(())
    }

    // ── diff ───────────────────────────────────────────────────────────────────

    #[test]
    fn default_diff_shows_uncommitted_changes() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one");
        repo.named_cut("cut", alice())?;
        // Edit after the cut → diff (working vs its parent cut) shows it.
        write_file(dir.path(), "a.txt", b"one-changed");
        repo.snapshot_working_copy()?;
        let diff = repo.diff(None, None)?;
        let modified: Vec<String> = diff
            .modified()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(modified, vec!["a.txt"]);
        Ok(())
    }

    /// Regression: a default `diff` must reflect the **live** working copy — dirty
    /// edits that have *not* been snapshotted — exactly as `status` does, and must
    /// not append an op or move the op-head while doing so (it stays read-only with
    /// respect to history). Before the fix, the default `to` read the stale
    /// *recorded* working-copy snapshot, so `diff` was blind to dirty disk while
    /// `status` saw it — the two contradicted each other.
    #[test]
    fn default_diff_reflects_dirty_disk_without_snapshot() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one\n");
        repo.named_cut("cut", alice())?;

        // Edit on disk only — deliberately NO snapshot_working_copy().
        write_file(dir.path(), "a.txt", b"one\ntwo\n");

        let ops_before = repo.op_log()?.len();
        let head_before = oplog::op_head(repo.tack_dir())?;

        // File-level diff sees the uncaptured edit.
        let diff = repo.diff(None, None)?;
        let modified: Vec<String> = diff
            .modified()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            modified,
            vec!["a.txt"],
            "default diff must reflect dirty disk"
        );

        // Line-level diff sees the added line at the hunk level.
        let patch = repo.diff_patch(None, None)?;
        assert_eq!(patch.len(), 1, "exactly one file changed");
        assert_eq!(patch[0].path, "a.txt");
        let added_lines: Vec<&str> = patch[0]
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.tag == "insert")
            .map(|l| l.content.as_str())
            .collect();
        assert!(
            added_lines.contains(&"two"),
            "line-level diff must show the added line, got {added_lines:?}"
        );

        // Read-only with respect to history: no op appended, head unmoved.
        assert_eq!(
            repo.op_log()?.len(),
            ops_before,
            "diff must not append an op"
        );
        assert_eq!(
            oplog::op_head(repo.tack_dir())?,
            head_before,
            "diff must not move the op-head"
        );
        Ok(())
    }

    fn sorted_slashed(paths: &[PathBuf]) -> Vec<String> {
        let mut v: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        v.sort();
        v
    }

    /// `status` and a default `diff` report the same change set in the common
    /// workflow — edits made since the last cut with **no intervening manual
    /// snapshot**. (They share the live-disk `to` side; here the recorded working
    /// copy still equals its parent cut, so the baselines coincide too. The
    /// post-amend case where they deliberately diverge is pinned by
    /// [`default_diff_and_status_diverge_after_snapshot_amend`].)
    #[test]
    fn default_diff_agrees_with_status_without_intervening_snapshot() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "keep.txt", b"k");
        write_file(dir.path(), "edit.txt", b"before");
        write_file(dir.path(), "gone.txt", b"x");
        repo.named_cut("cut", alice())?;

        // A mix of changes, none snapshotted.
        write_file(dir.path(), "edit.txt", b"after");
        write_file(dir.path(), "new.txt", b"fresh");
        fs::remove_file(dir.path().join("gone.txt"))?;

        let st = repo.status()?;
        let df = repo.diff(None, None)?;
        assert_eq!(
            sorted_slashed(st.added()),
            sorted_slashed(df.added()),
            "added must agree"
        );
        assert_eq!(
            sorted_slashed(st.modified()),
            sorted_slashed(df.modified()),
            "modified must agree"
        );
        // status calls deletions `deleted`; the tree diff calls them `removed`.
        assert_eq!(
            sorted_slashed(st.deleted()),
            sorted_slashed(df.removed()),
            "deletions must agree"
        );
        Ok(())
    }

    /// `status` and a default `diff` answer *different questions* and are NOT
    /// equivalent in general: `status` compares disk to the **recorded** working
    /// copy ("is there anything to snapshot?"), while the default `diff` compares
    /// disk to the **last cut** ("what changed since my last checkpoint?"). After a
    /// manual `snapshot_working_copy` amends the working copy to match disk, status
    /// is clean yet diff still reports the change relative to the cut. This pins
    /// that intended divergence (the adversarial review flagged docs that wrongly
    /// claimed exact equivalence).
    #[test]
    fn default_diff_and_status_diverge_after_snapshot_amend() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one\n");
        repo.named_cut("cut", alice())?;

        // Edit, then explicitly amend the recorded working copy to match disk.
        write_file(dir.path(), "a.txt", b"two\n");
        repo.snapshot_working_copy()?;

        // status: disk == recorded working copy → clean.
        assert!(
            repo.status()?.is_clean(),
            "status compares disk to the recorded WC → clean after amend"
        );

        // diff: disk ('two') vs the last cut ('one') → still modified.
        let df = repo.diff(None, None)?;
        assert_eq!(
            sorted_slashed(df.modified()),
            vec!["a.txt"],
            "diff compares disk to the last cut → modified"
        );
        Ok(())
    }

    /// An explicit `to` snapshot id diffs that **recorded** snapshot, never the
    /// dirty disk — so the escape hatch (`diff --to <wc-snapshot-id>`) still lets a
    /// caller compare a captured state while the live default reflects disk.
    #[test]
    fn explicit_to_snapshot_ignores_dirty_disk() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one");
        repo.named_cut("cut", alice())?;
        let recorded_wc = repo.working_copy()?.id();

        // Edit on disk only — no snapshot.
        write_file(dir.path(), "a.txt", b"two");

        // Explicit `to` = the recorded working-copy snapshot → matches its parent
        // cut (both hold "one"), so no change is reported despite the dirty disk.
        let recorded = repo.diff(None, Some(recorded_wc))?;
        assert!(
            recorded.added().is_empty()
                && recorded.modified().is_empty()
                && recorded.removed().is_empty(),
            "explicit recorded-snapshot diff must ignore dirty disk, got {recorded:?}"
        );

        // The default still reflects disk.
        let live = repo.diff(None, None)?;
        let modified: Vec<String> = live
            .modified()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(modified, vec!["a.txt"]);
        Ok(())
    }

    /// A `diff` endpoint accepts an **op** id (resolved to its working copy, like
    /// `restore`/`scoped_cut`), and rejects a non-state id (a tree) with a clear
    /// `InvalidArgument` rather than an opaque store type-mismatch. (Adversarial
    /// review, low severity, pre-existing: now consistent with the sibling
    /// resolvers.)
    #[test]
    fn diff_endpoint_accepts_op_and_rejects_tree() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one\n");
        repo.named_cut("cut", alice())?;

        // An op id resolves to that op's working copy → diff succeeds (no error).
        let head_op = oplog::op_head(repo.tack_dir())?.expect("head");
        repo.diff(None, Some(head_op))?;

        // A tree id is neither a snapshot nor an op → clear InvalidArgument.
        let tree_id = repo.working_copy()?.root_tree();
        let err = repo
            .diff(None, Some(tree_id))
            .expect_err("a tree id is not a diff endpoint");
        assert!(
            matches!(err, Error::InvalidArgument(_)),
            "a tree id must be rejected as InvalidArgument, got {err:?}"
        );
        Ok(())
    }

    // ── restore (non-destructive) ──────────────────────────────────────────────

    #[test]
    fn restore_reverts_workdir_and_keeps_history() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "f.txt", b"v1");
        let cut1 = repo.named_cut("v1", alice())?;

        write_file(dir.path(), "f.txt", b"v2");
        repo.named_cut("v2", alice())?;
        assert_eq!(fs::read(dir.path().join("f.txt"))?, b"v2");

        // The op that is current right before the restore.
        let pre_restore_head = oplog::op_head(repo.tack_dir())?.expect("head");

        // Restore to the earlier cut: the work dir reverts.
        repo.restore(cut1)?;
        assert_eq!(fs::read(dir.path().join("f.txt"))?, b"v1");

        // Non-destructive: the pre-restore op is still reachable in the op log.
        let log = repo.op_log()?;
        assert!(
            log.iter().map(Op::id).any(|id| id == pre_restore_head),
            "pre-restore op must remain reachable after restore"
        );
        // The new head is the restore op; its parent is the pre-restore head.
        assert_eq!(
            log[0].parents().first().copied(),
            Some(pre_restore_head),
            "restore op must be parented on the previous head"
        );
        Ok(())
    }

    /// Regression (finding 4): restoring to an earlier cut must DELETE a file
    /// added after it, so the working dir is a faithful projection of the
    /// restored tree (`DESIGN.md §9`). The non-deleting `materialize` left the
    /// added file on disk; `status` then reported it as added and a follow-up
    /// snapshot resurrected it.
    #[test]
    fn restore_removes_file_added_after_target() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "f1.txt", b"one");
        let cut1 = repo.named_cut("v1", alice())?;

        // v2 ADDS a distinct file (the case the old test never covered).
        write_file(dir.path(), "f2.txt", b"two");
        repo.named_cut("v2", alice())?;
        assert!(dir.path().join("f2.txt").is_file());

        repo.restore(cut1)?;
        // f1 remains, f2 is gone — the working dir matches the restored tree.
        assert_eq!(fs::read(dir.path().join("f1.txt"))?, b"one");
        assert!(
            !dir.path().join("f2.txt").exists(),
            "f2 must be removed by restore"
        );

        // And status against the restored working copy must be clean (no
        // resurrected f2 reported as added).
        assert!(
            repo.status()?.is_clean(),
            "working dir must equal the restored tree"
        );
        Ok(())
    }

    /// Regression (finding 4): an UNTRACKED file present on disk must survive a
    /// restore — only tracked deletions are pruned (`constitution.md §3`).
    #[test]
    fn restore_preserves_untracked_file() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "f1.txt", b"one");
        let cut1 = repo.named_cut("v1", alice())?;
        write_file(dir.path(), "f2.txt", b"two");
        repo.named_cut("v2", alice())?;

        // An untracked file that no cut ever captured.
        write_file(dir.path(), "scratch.tmp", b"keep");

        repo.restore(cut1)?;
        assert!(!dir.path().join("f2.txt").exists(), "tracked file removed");
        assert!(
            dir.path().join("scratch.tmp").is_file(),
            "untracked file preserved"
        );
        Ok(())
    }

    // ── undo ───────────────────────────────────────────────────────────────────

    #[test]
    fn undo_reverses_last_op_and_grows_log() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "f.txt", b"v1");
        repo.snapshot_working_copy()?;
        let view_before_second = repo.current_view()?.id();

        write_file(dir.path(), "g.txt", b"v2");
        repo.snapshot_working_copy()?;
        let count_before_undo = repo.op_log()?.len();

        repo.undo()?;
        // Undo appends a new op (log grows).
        assert_eq!(repo.op_log()?.len(), count_before_undo + 1);
        // The current view equals the view before the second snapshot.
        assert_eq!(repo.current_view()?.id(), view_before_second);
        Ok(())
    }

    // ── log ordering under clock skew (finding 5) ───────────────────────────────

    /// Regression (finding 5): when the system clock steps BACKWARD, a child cut
    /// can carry a smaller `unix_secs` than its parent. `log()` must still order
    /// by ancestry (child first), not by wall-clock seconds, which would
    /// otherwise present the ancestor as "newer" than its own descendant.
    #[test]
    fn log_orders_by_ancestry_not_backward_clock() -> Result<()> {
        use crate::object::{Snapshot, Timestamp};

        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let empty_tree = crate::object::Tree::new(vec![])?;
        let root_tree = repo.store().put_tree(&empty_tree)?;

        // Parent cut: LATER wall-clock time (t = 2000).
        let parent = Snapshot::new(
            root_tree,
            Vec::new(),
            new_change_id(),
            alice(),
            alice(),
            "parent cut",
            Timestamp::new(2000, 0),
        );
        let parent_id = repo.store().put_snapshot(&parent)?;

        // Child cut: EARLIER wall-clock time (t = 1000) — the clock stepped back.
        let child = Snapshot::new(
            root_tree,
            vec![parent_id],
            new_change_id(),
            alice(),
            alice(),
            "child cut",
            Timestamp::new(1000, 0),
        );
        let child_id = repo.store().put_snapshot(&child)?;

        // Working copy (empty message) parented on the child cut.
        let wc = Snapshot::new(
            root_tree,
            vec![child_id],
            new_change_id(),
            alice(),
            alice(),
            String::new(),
            Timestamp::new(1000, 0),
        );
        let wc_id = repo.store().put_snapshot(&wc)?;

        // Install a view whose working copy is wc and append an op for it.
        let view = View::new(wc_id, Vec::new(), Vec::new(), vec![wc_id]);
        let view_id = repo.store().put_view(&view)?;
        repo.append_op_on_head(view_id, "install skewed chain", vec!["tack".to_string()])?;

        let log = repo.log()?;
        assert_eq!(log.len(), 2, "two named cuts in the ancestry");
        // Child (shallower ancestry) must be newest-first despite its EARLIER
        // timestamp; the old timestamp-primary sort inverted this.
        assert_eq!(log[0].message(), "child cut", "descendant must sort first");
        assert_eq!(log[1].message(), "parent cut", "ancestor must sort last");
        Ok(())
    }

    // ── resolve_prefix ───────────────────────────────────────────────────────────

    #[test]
    fn resolve_prefix_finds_unique_object() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let wc = repo.working_copy()?.id();
        let full = wc.to_string();
        let resolved = repo.resolve_prefix(&full[..12])?;
        assert_eq!(resolved, wc);
        Ok(())
    }

    #[test]
    fn resolve_prefix_rejects_non_hex() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        assert!(matches!(
            repo.resolve_prefix("zz"),
            Err(Error::InvalidObjectId(_))
        ));
    }

    #[test]
    fn resolve_prefix_missing_is_not_found() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        // A prefix that cannot match any stored object.
        let result = repo.resolve_prefix("ffffffffffff");
        assert!(matches!(result, Err(Error::PrefixNotFound(_))));
        Ok(())
    }

    #[test]
    fn resolve_prefix_not_found_message_carries_prefix() -> Result<()> {
        // Regression: a no-match prefix used to surface a sentinel all-zero id
        // ("object not found: 0000...0000"); the message must now name the typed
        // prefix instead.
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let err = repo
            .resolve_prefix("ffffffffffff")
            .expect_err("must not match");
        let msg = err.to_string();
        assert!(
            msg.contains("ffffffffffff"),
            "message must carry the prefix: {msg}"
        );
        assert!(
            !msg.contains("0000000000000000"),
            "must not leak a zero-id sentinel: {msg}"
        );
        Ok(())
    }

    // ── cat ───────────────────────────────────────────────────────────────────

    #[test]
    fn cat_returns_tag_and_bytes() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        let wc = repo.working_copy()?.id();
        let (tag, bytes) = repo.cat(&wc)?;
        assert_eq!(tag, TypeTag::Snapshot);
        assert!(!bytes.is_empty());
        Ok(())
    }

    // ── base_cut / all_cuts ──────────────────────────────────────────────────────

    #[test]
    fn base_cut_is_nearest_named_ancestor() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        assert!(repo.base_cut()?.is_none(), "fresh repo has no base cut");

        write_file(dir.path(), "a.txt", b"one");
        let cut1 = repo.named_cut("c1", alice())?;
        // The working copy now sits on cut1.
        assert_eq!(repo.base_cut()?.map(|s| s.id()), Some(cut1));

        write_file(dir.path(), "b.txt", b"two");
        let cut2 = repo.named_cut("c2", alice())?;
        assert_eq!(
            repo.base_cut()?.map(|s| s.id()),
            Some(cut2),
            "base advances to the newest cut"
        );
        Ok(())
    }

    #[test]
    fn all_cuts_surfaces_off_lineage_cut_that_log_hides() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "a.txt", b"one");
        let cut1 = repo.named_cut("c1", alice())?;
        write_file(dir.path(), "b.txt", b"two");
        repo.named_cut("c2", alice())?;

        // Restore to cut1: the working copy's lineage no longer includes c2.
        repo.restore(cut1)?;
        let log_msgs: Vec<String> = repo.log()?.iter().map(|s| s.message().to_owned()).collect();
        assert!(
            !log_msgs.contains(&"c2".to_string()),
            "log follows the restored lineage: {log_msgs:?}"
        );

        // all_cuts still surfaces c2 (reachable from an earlier op's view).
        let all_msgs: Vec<String> = repo
            .all_cuts()?
            .iter()
            .map(|s| s.message().to_owned())
            .collect();
        assert!(
            all_msgs.contains(&"c1".to_string()),
            "all_cuts must include c1: {all_msgs:?}"
        );
        assert!(
            all_msgs.contains(&"c2".to_string()),
            "all_cuts must include the off-lineage c2: {all_msgs:?}"
        );
        Ok(())
    }

    // ── claims (advisory coordination) ────────────────────────────────────────────

    #[test]
    fn claim_then_release_round_trips() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        assert!(repo.claims()?.is_empty());

        repo.claim("src/model.rs", "alice", "refactor")?;
        let held = repo.claims()?;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].path, "src/model.rs");
        assert_eq!(held[0].holder, "alice");
        assert_eq!(held[0].note, "refactor");

        repo.release("src/model.rs", "alice")?;
        assert!(repo.claims()?.is_empty(), "release clears the claim");
        Ok(())
    }

    #[test]
    fn claims_are_unaffected_by_restore() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one");
        let cut1 = repo.named_cut("c1", alice())?;

        repo.claim("a.txt", "bot", "")?;
        write_file(dir.path(), "b.txt", b"two");
        repo.named_cut("c2", alice())?;

        // Restoring an old working copy must NOT silently drop a peer's claim.
        repo.restore(cut1)?;
        let held = repo.claims()?;
        assert_eq!(
            held.len(),
            1,
            "claim survives restore (it lives in the op-log): {held:?}"
        );
        assert_eq!(held[0].holder, "bot");
        Ok(())
    }

    // ── scoped cut ────────────────────────────────────────────────────────────────

    #[test]
    fn scoped_cut_captures_only_scoped_paths() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;

        write_file(dir.path(), "src/model.txt", b"m1");
        write_file(dir.path(), "other/x.txt", b"x1");
        repo.named_cut("base", alice())?;

        // Two "workers" edit different subtrees concurrently on disk.
        write_file(dir.path(), "src/model.txt", b"m2");
        write_file(dir.path(), "other/x.txt", b"x2");

        // Scope the cut to src/ only.
        let outcome = repo.scoped_cut(&["src".to_string()], "scoped src", alice(), None)?;

        let cut = repo.store().get_snapshot(&outcome.cut())?;
        let root = cut.root_tree();
        // src/model.txt is captured at its LIVE content...
        let model = crate::tree::read_tree_path(repo.store(), &root, "src/model.txt")?;
        assert_eq!(crate::blob::read_blob(repo.store(), &model.id())?, b"m2");
        // ...while other/x.txt keeps the BASE content (the peer's edit is excluded).
        let other = crate::tree::read_tree_path(repo.store(), &root, "other/x.txt")?;
        assert_eq!(crate::blob::read_blob(repo.store(), &other.id())?, b"x1");

        assert!(
            outcome.captured().contains(&"src/model.txt".to_string()),
            "captured: {:?}",
            outcome.captured()
        );
        assert!(
            outcome
                .outside_changes()
                .contains(&"other/x.txt".to_string()),
            "outside_changes must flag the uncaptured peer edit: {:?}",
            outcome.outside_changes()
        );
        Ok(())
    }

    #[test]
    fn scoped_cut_leaves_working_copy_and_disk_untouched() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "src/a.txt", b"a1");
        write_file(dir.path(), "other/b.txt", b"b1");
        repo.named_cut("base", alice())?;

        write_file(dir.path(), "src/a.txt", b"a2");
        write_file(dir.path(), "other/b.txt", b"b2");
        let wc_before = repo.working_copy()?.id();

        repo.scoped_cut(&["src".to_string()], "scoped", alice(), None)?;

        // The working-copy pointer did not move and disk is unchanged.
        assert_eq!(
            repo.working_copy()?.id(),
            wc_before,
            "scoped cut must not advance the working copy"
        );
        assert_eq!(
            std::fs::read(dir.path().join("other/b.txt"))?,
            b"b2",
            "disk must be left as-is"
        );
        // The scoped cut is reachable via all_cuts even though it is off-lineage.
        let all_msgs: Vec<String> = repo
            .all_cuts()?
            .iter()
            .map(|s| s.message().to_owned())
            .collect();
        assert!(
            all_msgs.contains(&"scoped".to_string()),
            "scoped cut must be reachable: {all_msgs:?}"
        );
        Ok(())
    }

    #[test]
    fn scoped_cut_requires_a_path() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        let result = repo.scoped_cut(&[], "msg", alice(), None);
        assert!(
            matches!(result, Err(Error::InvalidArgument(_))),
            "empty scope must be rejected"
        );
    }

    // ── lanes and backports ───────────────────────────────────────────────────

    #[test]
    fn admit_derives_current_lane_from_op_log() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"one");
        let cut1 = repo.named_cut("one", alice())?;
        repo.admit(cut1, "team/main", "seed")?;

        write_file(dir.path(), "a.txt", b"two");
        let cut2 = repo.named_cut("two", alice())?;
        let admission = repo.admit(cut2, "team/main", "advance")?;

        let lanes = repo.lanes()?;
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].name(), "team/main");
        assert_eq!(lanes[0].cut(), cut2);
        assert_eq!(lanes[0].admission(), admission.op());
        assert_eq!(lanes[0].reason(), "advance");
        Ok(())
    }

    #[test]
    fn clean_backport_creates_target_lane_cut_with_source_provenance() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"base");
        let base = repo.named_cut("base", alice())?;
        repo.admit(base, "team/main", "seed")?;
        repo.admit(base, "release/7.8.0", "seed")?;

        write_file(dir.path(), "a.txt", b"main fix");
        let fix = repo.named_cut("fix", alice())?;
        let source_admission = repo.admit(fix, "team/main", "fix accepted")?;
        let source = repo.store().get_snapshot(&fix)?;

        let BackportOutcome::Created(record) =
            repo.backport(fix, "release/7.8.0", None, "hotfix", alice())?
        else {
            panic!("expected clean backport");
        };

        let hotfix = repo.store().get_snapshot(&record.result_cut())?;
        assert_eq!(
            hotfix.parents(),
            &[base],
            "release cut must parent target lane base"
        );
        assert_eq!(
            hotfix.change_id(),
            source.change_id(),
            "logical fix id must carry over"
        );
        assert_eq!(record.source_cut(), fix);
        assert_eq!(record.target_lane(), "release/7.8.0");
        assert_eq!(record.method(), "clean");
        let provenance = record.provenance();
        let source_link = provenance.source_admission().expect("source admission");
        assert_eq!(source_link.lane(), "team/main");
        assert_eq!(source_link.op(), source_admission.op());

        let entry = crate::tree::read_tree_path(repo.store(), &hotfix.root_tree(), "a.txt")?;
        assert_eq!(
            crate::blob::read_blob(repo.store(), &entry.id())?,
            b"main fix"
        );
        Ok(())
    }

    #[test]
    fn repeated_backport_reports_already_ported() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"base");
        let base = repo.named_cut("base", alice())?;
        repo.admit(base, "team/main", "seed")?;
        repo.admit(base, "release/7.8.0", "seed")?;
        write_file(dir.path(), "a.txt", b"main fix");
        let fix = repo.named_cut("fix", alice())?;

        let first = repo.backport(fix, "release/7.8.0", None, "hotfix", alice())?;
        let second = repo.backport(fix, "release/7.8.0", None, "hotfix", alice())?;

        let Some(first_cut) = first.result_cut() else {
            panic!("first backport must create a cut");
        };
        let BackportOutcome::AlreadyPorted(record) = second else {
            panic!("second backport must be idempotent");
        };
        assert_eq!(record.result_cut(), first_cut);
        Ok(())
    }

    #[test]
    fn conflicting_backport_materializes_settlement_and_continue_finalizes() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "a.txt", b"base");
        let base = repo.named_cut("base", alice())?;
        repo.admit(base, "team/main", "seed")?;
        repo.admit(base, "release/7.8.0", "seed")?;

        write_file(dir.path(), "a.txt", b"main fix");
        let fix = repo.named_cut("fix", alice())?;
        repo.admit(fix, "team/main", "fix accepted")?;

        repo.restore(base)?;
        write_file(dir.path(), "a.txt", b"release edit");
        let release = repo.named_cut("release edit", alice())?;
        repo.admit(release, "release/7.8.0", "release diverged")?;

        let BackportOutcome::Settlement(settlement) =
            repo.backport(fix, "release/7.8.0", None, "hotfix", alice())?
        else {
            panic!("expected settlement");
        };
        assert_eq!(settlement.conflicts(), &["a.txt".to_string()]);
        assert_eq!(std::fs::read(dir.path().join("a.txt"))?, b"release edit");

        write_file(dir.path(), "a.txt", b"manual resolution");
        let BackportOutcome::Created(record) = repo.continue_backport(alice())? else {
            panic!("expected manual result");
        };
        assert_eq!(record.method(), "manual");
        let hotfix = repo.store().get_snapshot(&record.result_cut())?;
        assert_eq!(
            hotfix.parents(),
            &[release],
            "manual cut must parent target lane base"
        );
        let entry = crate::tree::read_tree_path(repo.store(), &hotfix.root_tree(), "a.txt")?;
        assert_eq!(
            crate::blob::read_blob(repo.store(), &entry.id())?,
            b"manual resolution"
        );
        Ok(())
    }

    // ── restore / undo return the new op id ────────────────────────────────────────

    #[test]
    fn restore_returns_the_new_head_op() -> Result<()> {
        let dir = TempDir::new()?;
        let repo = Repository::init(dir.path())?;
        write_file(dir.path(), "f.txt", b"v1");
        let cut1 = repo.named_cut("v1", alice())?;
        write_file(dir.path(), "f.txt", b"v2");
        repo.named_cut("v2", alice())?;

        let op = repo.restore(cut1)?;
        assert_eq!(
            repo.current_op()?.id(),
            op,
            "restore must return the new head op id"
        );
        Ok(())
    }
}
