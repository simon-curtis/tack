//! The agent-native JSON-RPC API (`DESIGN.md §11`).
//!
//! `tack-core` *is* the SDK (`constitution.md §5`); this module is the
//! out-of-process face of it. Every CLI verb maps to exactly one [`Request`]
//! variant, so agents driving `tack serve` and humans driving the CLI exercise
//! the same [`Repository`] methods.
//!
//! The wire protocol is **line-delimited JSON-RPC**: one JSON [`Request`] per
//! line in, one JSON [`Response`] per line out ([`serve`]). A request operates
//! on an *already-open* repository — `init` is deliberately not an API method,
//! because you need a repository before you can serve one.
//!
//! ## Payloads are JSON-friendly
//!
//! Object ids cross the wire as lowercase hex strings, never raw bytes.
//! Per the organization data rule, [`Response`] payloads carry author **names**
//! but omit author e-mail by default — `named_cut` takes an email as input, but
//! no response field echoes it back.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::claims::Claim;
use crate::diff::TreeDiff;
use crate::error::Result;
use crate::hash::ObjectId;
use crate::linediff::{FilePatch, FileStat};
use crate::object::{Identity, Op, Snapshot};
use crate::repo::{
    AdmissionOutcome, BackportOutcome, BackportProvenance, BackportRecord, BackportSettlement,
    Lane, Repository,
};
use crate::tree::list_tree;
use crate::workcopy::Status;

// ── Request ────────────────────────────────────────────────────────────────────

/// One agent-API request against an already-open [`Repository`].
///
/// Internally tagged on a `method` field, so the wire form is a flat JSON
/// object such as `{"method":"diff","from":"ab12","to":null}`. Id-bearing
/// fields accept either a full 64-char hex id or a unique hex prefix; they are
/// resolved through [`Repository::resolve_prefix`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    /// Report the working directory's status against the working-copy snapshot.
    Status,
    /// Auto-snapshot the working copy, returning the resulting snapshot id.
    Snapshot,
    /// Create a named cut (the analog of a commit) with `message` and author.
    NamedCut {
        /// The cut message.
        message: String,
        /// The author's display name.
        author_name: String,
        /// The author's e-mail address (input only; never echoed back).
        author_email: String,
    },
    /// List the named cuts in the working copy's ancestry, newest-first.
    Log,
    /// Return the operation log, newest-first.
    OpLog,
    /// Diff two snapshots (hex ids/prefixes); `None` applies the engine defaults
    /// (`to` = working copy, `from` = its parent cut).
    ///
    /// With neither flag this returns the file-level [`DiffData`] summary
    /// (back-compatible). `stat` adds per-file line counts; `patch` adds full
    /// per-file content hunks and takes precedence over `stat`.
    Diff {
        /// The "from" snapshot id or prefix, or `None` for the default base.
        from: Option<String>,
        /// The "to" snapshot id or prefix, or `None` for the working copy.
        to: Option<String>,
        /// Emit per-file line-count stats ([`Response::DiffStat`]).
        #[serde(default)]
        stat: bool,
        /// Emit per-file content hunks ([`Response::DiffPatch`]); takes
        /// precedence over `stat`.
        #[serde(default)]
        patch: bool,
    },
    /// Non-destructively restore the working copy to `target` (an op or
    /// snapshot id/prefix).
    Restore {
        /// The op or snapshot id/prefix to restore to.
        target: String,
    },
    /// Reverse the most recent operation by reinstating its parent's view.
    Undo,
    /// Return the raw type tag and byte length of the object at `id`.
    Cat {
        /// The object id or prefix to inspect.
        id: String,
    },
    /// List the immediate entries of a tree; `None` lists the **live** working
    /// copy's root (the current files on disk, including un-snapshotted changes).
    Ls {
        /// The tree (or snapshot) id/prefix to list, or `None` for the working
        /// copy's root tree.
        tree: Option<String>,
    },
    /// Describe the agent API itself: every method, its parameters, and the
    /// shape of its response (so an agent need not infer method shapes).
    Help,
    /// Report where the working copy currently is: the current op (and whether
    /// it came from a restore), the working-copy snapshot, the base cut, and the
    /// heads. The orientation an agent needs after a non-linear operation.
    Current,
    /// List **every** named cut across all lineages, newest-first — including
    /// cuts that the current `log` lineage hides (e.g. after a restore).
    Cuts,
    /// List the currently-held advisory claims.
    Claims,
    /// Record an advisory claim on `path` by `holder` (advisory only; never
    /// enforced). Returns any overlapping claims held by other actors.
    Claim {
        /// The repo-relative path to claim.
        path: String,
        /// The actor holding the claim (an agent id / username; never an e-mail).
        holder: String,
        /// An optional advisory note.
        #[serde(default)]
        note: String,
    },
    /// Release any advisory claim on `path` held by `holder`.
    Release {
        /// The repo-relative path to release.
        path: String,
        /// The actor releasing the claim.
        holder: String,
    },
    /// Create a scoped named cut capturing only the working-copy content under
    /// `paths`, taking everything else from `base` (the current base cut by
    /// default). Leaves the working copy and filesystem untouched.
    ScopedCut {
        /// The repo-relative path selectors to capture.
        paths: Vec<String>,
        /// The cut message.
        message: String,
        /// The author's display name.
        author_name: String,
        /// The author's e-mail address (input only; never echoed back).
        author_email: String,
        /// The base cut/op id or prefix to overlay onto; `None` = the current
        /// base cut.
        #[serde(default)]
        base: Option<String>,
    },
    /// List op-derived team lanes and their current admitted cuts.
    Lanes,
    /// Admit a cut to a team/release lane.
    Admit {
        /// The cut id or prefix to admit.
        cut: String,
        /// The lane to admit the cut into.
        lane: String,
        /// Optional admission reason.
        #[serde(default)]
        reason: String,
    },
    /// Create a backport proposal from a source cut to a target lane.
    Backport {
        /// The source fix cut id or prefix.
        source: String,
        /// The target lane name.
        target_lane: String,
        /// Optional target cut message.
        #[serde(default)]
        message: Option<String>,
        /// Optional backport reason.
        #[serde(default)]
        reason: String,
        /// The author's display name.
        author_name: String,
        /// The author's e-mail address (input only; never echoed back).
        author_email: String,
        /// Also admit the created cut when the backport is clean/manual.
        #[serde(default)]
        admit: bool,
    },
    /// Finish the currently materialized manual backport settlement.
    BackportContinue {
        /// The author's display name.
        author_name: String,
        /// The author's e-mail address (input only; never echoed back).
        author_email: String,
        /// Also admit the resulting cut.
        #[serde(default)]
        admit: bool,
    },
}

// ── Response payload DTOs ───────────────────────────────────────────────────────

/// A status report: changed paths grouped by kind (forward-slashed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusData {
    /// Paths present on disk but not in the working-copy snapshot.
    pub added: Vec<String>,
    /// Paths present in both but with changed content.
    pub modified: Vec<String>,
    /// Paths present in the snapshot but missing from disk.
    pub deleted: Vec<String>,
    /// `true` if the working directory matches the snapshot exactly.
    pub clean: bool,
}

impl StatusData {
    /// Builds a [`StatusData`] from an engine [`Status`].
    fn from_status(status: &Status) -> Self {
        Self {
            added: paths_to_strings(status.added()),
            modified: paths_to_strings(status.modified()),
            deleted: paths_to_strings(status.deleted()),
            clean: status.is_clean(),
        }
    }
}

/// A one-line summary of a named cut, for [`Request::Log`].
///
/// Carries the author **name** but not e-mail (organization data rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutSummary {
    /// The cut's content-address (hex).
    pub id: String,
    /// The first 12 hex chars of [`id`](Self::id), for compact display/logging.
    pub id_short: String,
    /// The cut's change id (hex) — stable across amends.
    pub change_id: String,
    /// The first 12 hex chars of [`change_id`](Self::change_id).
    pub change_id_short: String,
    /// The cut message.
    pub message: String,
    /// The author's display name (e-mail intentionally omitted).
    pub author_name: String,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    /// Parent snapshot ids (hex), newest cut's ancestry.
    pub parents: Vec<String>,
}

impl CutSummary {
    /// Builds a [`CutSummary`] from a [`Snapshot`].
    fn from_snapshot(snap: &Snapshot) -> Self {
        Self {
            id: snap.id().to_string(),
            id_short: snap.id().short(),
            change_id: snap.change_id().to_string(),
            change_id_short: snap.change_id().short(),
            message: snap.message().to_owned(),
            author_name: snap.author().name().to_owned(),
            timestamp: snap.timestamp().unix_secs(),
            parents: ids_to_strings(snap.parents()),
        }
    }
}

/// A one-line summary of an operation, for [`Request::OpLog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpSummary {
    /// The op's content-address (hex).
    pub id: String,
    /// The first 12 hex chars of [`id`](Self::id), for compact display/logging.
    pub id_short: String,
    /// The human-readable operation description.
    pub description: String,
    /// The argv (or agent-API call) that triggered the operation.
    pub command: Vec<String>,
    /// Operation start time, seconds since the Unix epoch.
    pub timestamp: i64,
    /// Parent op ids (hex).
    pub parents: Vec<String>,
}

impl OpSummary {
    /// Builds an [`OpSummary`] from an [`Op`].
    fn from_op(op: &Op) -> Self {
        Self {
            id: op.id().to_string(),
            id_short: op.id().short(),
            description: op.description().to_owned(),
            command: op.metadata().command().to_vec(),
            timestamp: op.metadata().start().unix_secs(),
            parents: ids_to_strings(op.parents()),
        }
    }
}

/// A file-level diff summary: paths grouped by change kind (forward-slashed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffData {
    /// Paths present only in the `to` tree.
    pub added: Vec<String>,
    /// Paths present only in the `from` tree.
    pub removed: Vec<String>,
    /// Paths present in both but with differing content or kind.
    pub modified: Vec<String>,
}

impl DiffData {
    /// Builds a [`DiffData`] from a [`TreeDiff`].
    fn from_tree_diff(diff: &TreeDiff) -> Self {
        Self {
            added: paths_to_strings(diff.added()),
            removed: paths_to_strings(diff.removed()),
            modified: paths_to_strings(diff.modified()),
        }
    }
}

/// What the `to` side of a [`Request::Diff`] resolved to.
///
/// Reported on every diff response (`to_kind`) so an agent's logs are
/// self-explanatory about whether the right-hand side was the live working copy
/// or an explicit recorded snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffTarget {
    /// The default `to`: the current files on disk (the live working copy) — the
    /// same on-disk bytes `status` reads. Serializes as `"live_working_copy"`.
    LiveWorkingCopy,
    /// An explicit `to` snapshot/cut id — a *recorded* tree, not dirty disk.
    /// Serializes as `"snapshot"`.
    Snapshot,
}

impl DiffTarget {
    /// Classifies a resolved `to` argument: `None` is the live working copy, a
    /// snapshot id is a recorded snapshot.
    const fn from_to(to: Option<ObjectId>) -> Self {
        if to.is_some() {
            Self::Snapshot
        } else {
            Self::LiveWorkingCopy
        }
    }
}

/// Raw object info returned by [`Request::Cat`].
///
/// The bytes themselves are not embedded (they can be large and binary); the
/// type tag and length identify the object for an agent that will fetch or
/// stream it separately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectInfo {
    /// The resolved object id (hex).
    pub id: String,
    /// The first 12 hex chars of [`id`](Self::id).
    pub id_short: String,
    /// The object's type tag (e.g. `"blob"`, `"tree"`, `"snapshot"`).
    pub kind: String,
    /// The length of the object's canonical bytes.
    pub size: usize,
}

/// One immediate entry of a tree, for [`Request::Ls`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntryData {
    /// The entry's path-component name.
    pub name: String,
    /// The entry kind: `"blob"`, `"tree"`, or `"symlink"`.
    pub kind: String,
    /// POSIX-style mode bits (e.g. `0o100644`).
    pub mode: u32,
    /// The referenced object's content-address (hex).
    pub id: String,
    /// The first 12 hex chars of [`id`](Self::id).
    pub id_short: String,
}

/// One parameter of an API method, for [`Request::Help`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamInfo {
    /// The JSON field name.
    pub name: String,
    /// The JSON type (`"string"`, `"bool"`, `"string?"` for optional, `"[string]"`).
    #[serde(rename = "type")]
    pub ty: String,
    /// Whether the field is required.
    pub required: bool,
    /// A one-line description of the parameter.
    pub description: String,
}

/// A self-description of one agent-API method, for [`Request::Help`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodInfo {
    /// The `method` tag value (e.g. `"diff"`).
    pub method: String,
    /// A one-line summary of what the method does.
    pub summary: String,
    /// The method's parameters (empty for no-arg methods).
    pub params: Vec<ParamInfo>,
    /// A one-line description of the success response shape.
    pub returns: String,
}

/// Where the working copy currently is, for [`Request::Current`].
///
/// This is the orientation an agent needs after a non-linear operation: the
/// current op (and whether it came from a restore), the working-copy snapshot,
/// the base cut it sits on, and the heads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentData {
    /// The current op id (hex).
    pub op: String,
    /// The first 12 hex chars of [`op`](Self::op).
    pub op_short: String,
    /// The current op's human-readable description (e.g. `"restore to ab12…"`).
    pub operation: String,
    /// `true` if the current state was produced by a `restore`.
    pub from_restore: bool,
    /// The current view id (hex).
    pub view: String,
    /// The current working-copy snapshot id (hex).
    pub working_copy: String,
    /// The first 12 hex chars of [`working_copy`](Self::working_copy).
    pub working_copy_short: String,
    /// The working copy's change id (hex) — stable across amends.
    pub change_id: String,
    /// The first 12 hex chars of [`change_id`](Self::change_id).
    pub change_id_short: String,
    /// The named cut the working copy currently represents or sits on, if any.
    pub base_cut: Option<CutSummary>,
    /// The view's anonymous heads (hex ids).
    pub heads: Vec<String>,
    /// `true` if the working directory matches the working-copy snapshot.
    pub clean: bool,
}

/// The result of a [`Request::ScopedCut`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedCutData {
    /// The new scoped cut id (hex).
    pub cut: String,
    /// The first 12 hex chars of [`cut`](Self::cut).
    pub cut_short: String,
    /// The op that recorded the scoped cut (hex).
    pub op: String,
    /// The first 12 hex chars of [`op`](Self::op).
    pub op_short: String,
    /// The base cut the scoped paths were overlaid onto (hex), if any.
    pub base: Option<String>,
    /// The in-scope paths captured by this cut (those that differed from base).
    pub captured: Vec<String>,
    /// Out-of-scope paths that differ from base — uncaptured concurrent work.
    pub outside_changes: Vec<String>,
}

/// One op-derived lane for [`Request::Lanes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneData {
    /// The lane name.
    pub name: String,
    /// The currently admitted cut id (hex).
    pub cut: String,
    /// The first 12 hex chars of [`cut`](Self::cut).
    pub cut_short: String,
    /// The op that admitted the current cut (hex).
    pub admission: String,
    /// The first 12 hex chars of [`admission`](Self::admission).
    pub admission_short: String,
    /// The admission reason, if any.
    pub reason: String,
    /// Admission timestamp, seconds since the Unix epoch.
    pub timestamp: i64,
}

/// The result of an admission operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionData {
    /// The lane name.
    pub lane: String,
    /// The admitted cut id (hex).
    pub cut: String,
    /// The first 12 hex chars of [`cut`](Self::cut).
    pub cut_short: String,
    /// The op that recorded the admission (hex).
    pub op: String,
    /// The first 12 hex chars of [`op`](Self::op).
    pub op_short: String,
    /// The admission reason, if any.
    pub reason: String,
}

/// A source admission referenced by backport provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAdmissionData {
    /// The lane that admitted the source cut.
    pub lane: String,
    /// The source admission op id (hex).
    pub op: String,
    /// The first 12 hex chars of [`op`](Self::op).
    pub op_short: String,
}

/// Provenance for a backport result or settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackportProvenanceData {
    /// The source fix cut id (hex).
    pub source_cut: String,
    /// The first 12 hex chars of [`source_cut`](Self::source_cut).
    pub source_cut_short: String,
    /// The source logical change id (hex).
    pub source_change_id: String,
    /// The first 12 hex chars of [`source_change_id`](Self::source_change_id).
    pub source_change_id_short: String,
    /// The source lane admission, if discoverable.
    pub source_admission: Option<SourceAdmissionData>,
    /// The target lane.
    pub target_lane: String,
    /// The target base cut id (hex).
    pub target_base: String,
    /// The first 12 hex chars of [`target_base`](Self::target_base).
    pub target_base_short: String,
    /// The backport reason, if any.
    pub reason: String,
}

/// The result of [`Request::Backport`] or [`Request::BackportContinue`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackportData {
    /// `created`, `settlement`, or `already_ported`.
    pub outcome: String,
    /// The backport provenance.
    pub provenance: BackportProvenanceData,
    /// The result cut id when a cut exists (hex).
    pub cut: Option<String>,
    /// The first 12 hex chars of [`cut`](Self::cut).
    pub cut_short: Option<String>,
    /// The op that recorded the backport or settlement (hex).
    pub op: Option<String>,
    /// The first 12 hex chars of [`op`](Self::op).
    pub op_short: Option<String>,
    /// `clean`, `manual`, or `settlement`.
    pub method: String,
    /// Conflicted paths for settlement outcomes.
    pub conflicts: Vec<String>,
    /// Admission data when `admit` was requested and a cut was admitted.
    pub admission: Option<AdmissionData>,
}

// ── Response ────────────────────────────────────────────────────────────────────

/// One agent-API response.
///
/// Tagged on a `status` field: success variants render as
/// `{"status":"ok", ...payload}`; failures render as
/// `{"status":"error","message":"..."}`. Any engine `Err` is mapped to
/// [`Response::Error`] — [`handle`] never panics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// A working-directory status report ([`Request::Status`]).
    Status {
        /// The status payload.
        data: StatusData,
    },
    /// The id of the snapshot produced by [`Request::Snapshot`].
    Snapshot {
        /// The (new or unchanged) working-copy snapshot id (hex).
        snapshot: String,
        /// The first 12 hex chars of `snapshot`.
        snapshot_short: String,
    },
    /// The id of the cut closed by [`Request::NamedCut`].
    NamedCut {
        /// The finalized cut's id (hex).
        cut: String,
        /// The first 12 hex chars of `cut`.
        cut_short: String,
    },
    /// The named-cut history ([`Request::Log`]), newest-first.
    Log {
        /// The cut summaries.
        cuts: Vec<CutSummary>,
    },
    /// The operation log ([`Request::OpLog`]), newest-first.
    OpLog {
        /// The op summaries.
        ops: Vec<OpSummary>,
    },
    /// A file-level diff summary ([`Request::Diff`]).
    Diff {
        /// The diff payload.
        data: DiffData,
        /// What the `to` side was (live working copy vs recorded snapshot).
        to_kind: DiffTarget,
    },
    /// A generic acknowledgement for operations with no payload to report
    /// (e.g. the CLI's `init`, or `watch`/`mount` having started).
    Ok,
    /// Raw object info ([`Request::Cat`]).
    Cat {
        /// The object info payload.
        object: ObjectInfo,
    },
    /// The immediate entries of a tree ([`Request::Ls`]).
    Ls {
        /// The tree entries.
        entries: Vec<TreeEntryData>,
    },
    /// The self-description of the agent API ([`Request::Help`]).
    Help {
        /// One entry per method.
        methods: Vec<MethodInfo>,
    },
    /// Where the working copy currently is ([`Request::Current`]).
    Current {
        /// The current-state payload.
        data: CurrentData,
    },
    /// Every named cut across all lineages ([`Request::Cuts`]), newest-first.
    Cuts {
        /// The cut summaries.
        cuts: Vec<CutSummary>,
    },
    /// The currently-held advisory claims ([`Request::Claims`] /
    /// [`Request::Release`]).
    Claims {
        /// The held claims.
        claims: Vec<Claim>,
    },
    /// Acknowledgement of a recorded claim ([`Request::Claim`]), with any
    /// advisory conflicts.
    Claimed {
        /// The claim that was recorded.
        claim: Claim,
        /// Overlapping claims held by *other* actors (advisory warning).
        conflicts: Vec<Claim>,
        /// The op that recorded the claim (hex).
        op: String,
        /// The first 12 hex chars of `op`.
        op_short: String,
    },
    /// A scoped named cut ([`Request::ScopedCut`]).
    ScopedCut {
        /// The scoped-cut payload.
        data: ScopedCutData,
    },
    /// The op-derived lanes ([`Request::Lanes`]).
    Lanes {
        /// One entry per lane.
        lanes: Vec<LaneData>,
    },
    /// A lane admission ([`Request::Admit`]).
    Admitted {
        /// The admission payload.
        data: AdmissionData,
    },
    /// A backport result ([`Request::Backport`] /
    /// [`Request::BackportContinue`]).
    Backport {
        /// The backport payload.
        data: BackportData,
    },
    /// Per-file line-count stats ([`Request::Diff`] with `stat`).
    DiffStat {
        /// One entry per changed file.
        files: Vec<FileStat>,
        /// Total inserted lines across all files.
        total_added: usize,
        /// Total deleted lines across all files.
        total_removed: usize,
        /// What the `to` side was (live working copy vs recorded snapshot).
        to_kind: DiffTarget,
    },
    /// Per-file content hunks ([`Request::Diff`] with `patch`).
    DiffPatch {
        /// One entry per changed file.
        files: Vec<FilePatch>,
        /// What the `to` side was (live working copy vs recorded snapshot).
        to_kind: DiffTarget,
    },
    /// The outcome of a non-destructive [`Request::Restore`].
    Restored {
        /// The new restore op id (the new head; hex).
        op: String,
        /// The first 12 hex chars of `op`.
        op_short: String,
        /// The restored working-copy snapshot id (hex).
        working_copy: String,
        /// The first 12 hex chars of `working_copy`.
        working_copy_short: String,
        /// The named cut the working copy now represents/sits on, if any.
        restored_cut: Option<CutSummary>,
        /// The op that was current *before* the restore (its parent; hex), if any.
        previous_op: Option<String>,
        /// How to get back (the restore is itself undoable).
        hint: String,
    },
    /// The outcome of a non-destructive [`Request::Undo`].
    Undone {
        /// The new undo op id (the new head; hex).
        op: String,
        /// The first 12 hex chars of `op`.
        op_short: String,
        /// The op that was reversed (hex), if any.
        undone_op: Option<String>,
        /// The working-copy snapshot the undo reinstated (hex).
        working_copy: String,
        /// The first 12 hex chars of `working_copy`.
        working_copy_short: String,
    },
    /// An error: a request failed, or a line could not be parsed. The `message`
    /// is the lowercased engine error text and carries no PII.
    Error {
        /// A human-readable, PII-free error description.
        message: String,
    },
}

impl Response {
    /// Wraps an error message in a [`Response::Error`].
    fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

// ── handle ──────────────────────────────────────────────────────────────────────

/// Dispatches `req` to the matching [`Repository`] method, mapping any `Err`
/// into [`Response::Error`].
///
/// This function never panics: every fallible step is funnelled through
/// [`run`], whose `Result` is converted to a `Response` here.
#[must_use]
pub fn handle(repo: &Repository, req: Request) -> Response {
    run(repo, req).unwrap_or_else(|e| Response::error(e.to_string()))
}

/// The fallible inner worker for [`handle`]; `?` short-circuits to an error
/// response at the call site.
#[expect(
    clippy::too_many_lines,
    reason = "single request dispatch table keeps the public agent API explicit"
)]
fn run(repo: &Repository, req: Request) -> Result<Response> {
    match req {
        Request::Status => {
            let status = repo.status()?;
            Ok(Response::Status {
                data: StatusData::from_status(&status),
            })
        }
        Request::Snapshot => {
            let id = repo.snapshot_working_copy()?;
            Ok(Response::Snapshot {
                snapshot: id.to_string(),
                snapshot_short: id.short(),
            })
        }
        Request::NamedCut {
            message,
            author_name,
            author_email,
        } => {
            let author = Identity::new(author_name, author_email);
            let cut = repo.named_cut(message, author)?;
            Ok(Response::NamedCut {
                cut: cut.to_string(),
                cut_short: cut.short(),
            })
        }
        Request::Log => {
            let cuts = repo.log()?.iter().map(CutSummary::from_snapshot).collect();
            Ok(Response::Log { cuts })
        }
        Request::OpLog => {
            let ops = repo.op_log()?.iter().map(OpSummary::from_op).collect();
            Ok(Response::OpLog { ops })
        }
        Request::Diff {
            from,
            to,
            stat,
            patch,
        } => run_diff(repo, from.as_deref(), to.as_deref(), stat, patch),
        Request::Restore { target } => {
            let id = repo.resolve_prefix(&target)?;
            let new_op = repo.restore(id)?;
            restored_response(repo, new_op)
        }
        Request::Undo => {
            let new_op = repo.undo()?;
            undone_response(repo, new_op)
        }
        Request::Cat { id } => {
            let id = repo.resolve_prefix(&id)?;
            let (tag, bytes) = repo.cat(&id)?;
            Ok(Response::Cat {
                object: ObjectInfo {
                    id: id.to_string(),
                    id_short: id.short(),
                    kind: tag_name(tag),
                    size: bytes.len(),
                },
            })
        }
        Request::Ls { tree } => {
            let tree_id = match tree.as_deref() {
                Some(prefix) => repo.resolve_prefix(prefix)?,
                // Default to the LIVE working tree (current files on disk), so a
                // listing reflects un-snapshotted adds/removes — consistent with
                // `status`/`diff` and not a stale recorded snapshot.
                None => repo.live_tree()?,
            };
            let entries = list_tree(repo.store(), &tree_id)?
                .iter()
                .map(|entry| TreeEntryData {
                    name: entry.name().to_owned(),
                    kind: entry_kind_name(entry.kind()),
                    mode: entry.mode(),
                    id: entry.id().to_string(),
                    id_short: entry.id().short(),
                })
                .collect();
            Ok(Response::Ls { entries })
        }
        Request::Help => Ok(Response::Help {
            methods: api_schema(),
        }),
        Request::Current => Ok(Response::Current {
            data: current_data(repo)?,
        }),
        Request::Cuts => {
            let cuts = repo
                .all_cuts()?
                .iter()
                .map(CutSummary::from_snapshot)
                .collect();
            Ok(Response::Cuts { cuts })
        }
        Request::Claims => Ok(Response::Claims {
            claims: repo.claims()?,
        }),
        Request::Claim { path, holder, note } => run_claim(repo, &path, &holder, &note),
        Request::Release { path, holder } => {
            repo.release(&path, &holder)?;
            Ok(Response::Claims {
                claims: repo.claims()?,
            })
        }
        Request::ScopedCut {
            paths,
            message,
            author_name,
            author_email,
            base,
        } => {
            let author = Identity::new(author_name, author_email);
            let base = resolve_opt(repo, base.as_deref())?;
            let outcome = repo.scoped_cut(&paths, message, author, base)?;
            Ok(Response::ScopedCut {
                data: scoped_cut_data(&outcome),
            })
        }
        Request::Lanes => {
            let lanes = repo.lanes()?.iter().map(lane_data).collect();
            Ok(Response::Lanes { lanes })
        }
        Request::Admit { cut, lane, reason } => {
            let cut = repo.resolve_prefix(&cut)?;
            let outcome = repo.admit(cut, &lane, &reason)?;
            Ok(Response::Admitted {
                data: admission_data(&outcome),
            })
        }
        Request::Backport {
            source,
            target_lane,
            message,
            reason,
            author_name,
            author_email,
            admit,
        } => {
            let source = repo.resolve_prefix(&source)?;
            let author = Identity::new(author_name, author_email);
            run_backport(repo, source, &target_lane, message, &reason, author, admit)
        }
        Request::BackportContinue {
            author_name,
            author_email,
            admit,
        } => {
            let author = Identity::new(author_name, author_email);
            run_backport_continue(repo, author, admit)
        }
    }
}

// ── serve ───────────────────────────────────────────────────────────────────────

/// Runs the line-delimited JSON-RPC loop over `reader`/`writer`.
///
/// Reads one [`Request`] per line. A JSON parse error is reported back as a
/// [`Response::Error`] (the loop is *not* aborted); a well-formed request is
/// dispatched through [`handle`]. Each [`Response`] is written as one JSON line
/// followed by a newline, and the writer is flushed after every response. EOF
/// (a read of zero bytes) ends the loop cleanly.
///
/// # Errors
///
/// Returns [`Error::Io`](crate::Error::Io) if reading a line or writing a
/// response fails. Request-level failures never escape — they become
/// [`Response::Error`] lines.
pub fn serve<R: BufRead, W: Write>(repo: &Repository, mut reader: R, writer: &mut W) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(()); // EOF — clean shutdown.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // Tolerate blank lines between requests.
        }

        let response = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => handle(repo, req),
            Err(e) => Response::error(format!("invalid request: {e}")),
        };
        write_response(writer, &response)?;
    }
}

/// Serializes `response` and writes it as one newline-terminated line, flushing
/// after the write.
fn write_response<W: Write>(writer: &mut W, response: &Response) -> Result<()> {
    // A `Response` is always serializable (no map keys, no non-string keys), so
    // a serialization failure is genuine corruption rather than expected input.
    let json = serde_json::to_string(response)
        .map_err(|e| crate::Error::Corruption(format!("failed to serialize response: {e}")))?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

// ── helpers ─────────────────────────────────────────────────────────────────────

/// Resolves an optional hex id/prefix, leaving `None` untouched so the engine's
/// defaults apply.
fn resolve_opt(repo: &Repository, prefix: Option<&str>) -> Result<Option<ObjectId>> {
    prefix.map(|p| repo.resolve_prefix(p)).transpose()
}

/// Renders a slice of repo-relative paths as forward-slashed strings.
fn paths_to_strings(paths: &[std::path::PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect()
}

/// Renders a slice of object ids as lowercase hex strings.
fn ids_to_strings(ids: &[ObjectId]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
}

/// Returns the lowercase, `snake_case` name of a [`TypeTag`](crate::hash::TypeTag).
fn tag_name(tag: crate::hash::TypeTag) -> String {
    use crate::hash::TypeTag;
    match tag {
        TypeTag::Blob => "blob",
        TypeTag::Chunk => "chunk",
        TypeTag::Tree => "tree",
        TypeTag::Snapshot => "snapshot",
        TypeTag::Op => "op",
        TypeTag::View => "view",
    }
    .to_owned()
}

/// Returns the lowercase name of an [`EntryKind`](crate::object::EntryKind).
fn entry_kind_name(kind: crate::object::EntryKind) -> String {
    use crate::object::EntryKind;
    match kind {
        EntryKind::Blob => "blob",
        EntryKind::Tree => "tree",
        EntryKind::Symlink => "symlink",
    }
    .to_owned()
}

/// Dispatches [`Request::Diff`]: file-level summary by default, per-file stats
/// with `stat`, or full per-file hunks with `patch` (which wins over `stat`).
fn run_diff(
    repo: &Repository,
    from: Option<&str>,
    to: Option<&str>,
    stat: bool,
    patch: bool,
) -> Result<Response> {
    let from = resolve_opt(repo, from)?;
    let to = resolve_opt(repo, to)?;
    let to_kind = DiffTarget::from_to(to);
    if patch {
        Ok(Response::DiffPatch {
            files: repo.diff_patch(from, to)?,
            to_kind,
        })
    } else if stat {
        let files: Vec<FileStat> = repo
            .diff_patch(from, to)?
            .iter()
            .map(FileStat::from_patch)
            .collect();
        let total_added = files.iter().map(|f| f.added_lines).sum();
        let total_removed = files.iter().map(|f| f.removed_lines).sum();
        Ok(Response::DiffStat {
            files,
            total_added,
            total_removed,
            to_kind,
        })
    } else {
        let diff = repo.diff(from, to)?;
        Ok(Response::Diff {
            data: DiffData::from_tree_diff(&diff),
            to_kind,
        })
    }
}

/// Dispatches [`Request::Claim`]: records the claim and reports any overlapping
/// claims held by *other* actors (computed against the pre-claim state).
fn run_claim(repo: &Repository, path: &str, holder: &str, note: &str) -> Result<Response> {
    let existing = repo.claims()?;
    let conflicts: Vec<Claim> = crate::claims::conflicts(&existing, path, holder)
        .into_iter()
        .cloned()
        .collect();
    let op = repo.claim(path, holder, note)?;
    // Recover the just-recorded claim (normalised path / trimmed holder).
    let norm_path = crate::tree::normalize_repo_path(path);
    let trimmed_holder = holder.trim();
    let claim = repo
        .claims()?
        .into_iter()
        .find(|c| c.path == norm_path && c.holder == trimmed_holder)
        .ok_or_else(|| crate::Error::Corruption("claim was not recorded".to_string()))?;
    Ok(Response::Claimed {
        claim,
        conflicts,
        op: op.to_string(),
        op_short: op.short(),
    })
}

fn run_backport(
    repo: &Repository,
    source: ObjectId,
    target_lane: &str,
    message: Option<String>,
    reason: &str,
    author: Identity,
    admit: bool,
) -> Result<Response> {
    let outcome = repo.backport(source, target_lane, message, reason, author)?;
    let admission = admit_result(repo, &outcome, admit, reason)?;
    Ok(Response::Backport {
        data: backport_data(&outcome, admission.as_ref()),
    })
}

fn run_backport_continue(repo: &Repository, author: Identity, admit: bool) -> Result<Response> {
    let outcome = repo.continue_backport(author)?;
    let reason = match &outcome {
        BackportOutcome::Created(record) | BackportOutcome::AlreadyPorted(record) => {
            record.provenance().reason()
        }
        BackportOutcome::Settlement(settlement) => settlement.provenance().reason(),
    };
    let admission = admit_result(repo, &outcome, admit, reason)?;
    Ok(Response::Backport {
        data: backport_data(&outcome, admission.as_ref()),
    })
}

fn admit_result(
    repo: &Repository,
    outcome: &BackportOutcome,
    admit: bool,
    reason: &str,
) -> Result<Option<AdmissionData>> {
    if !admit {
        return Ok(None);
    }
    let Some(cut) = outcome.result_cut() else {
        return Ok(None);
    };
    if matches!(outcome, BackportOutcome::AlreadyPorted(_)) {
        return Ok(None);
    }
    let lane = match outcome {
        BackportOutcome::Created(record) | BackportOutcome::AlreadyPorted(record) => {
            record.target_lane()
        }
        BackportOutcome::Settlement(settlement) => settlement.provenance().target_lane(),
    };
    let outcome = repo.admit(cut, lane, reason)?;
    Ok(Some(admission_data(&outcome)))
}

/// Assembles the [`CurrentData`] orientation payload from the repository.
fn current_data(repo: &Repository) -> Result<CurrentData> {
    let op = repo.current_op()?;
    let view = repo.current_view()?;
    let wc = repo.working_copy()?;
    // The current state "came from a restore" iff the head op's command is a
    // restore (an undo of a restore resets this, which is the intended meaning).
    let from_restore = op
        .metadata()
        .command()
        .get(1)
        .is_some_and(|verb| verb == "restore");
    let base_cut = repo.base_cut()?.as_ref().map(CutSummary::from_snapshot);
    let clean = repo.status()?.is_clean();
    Ok(CurrentData {
        op: op.id().to_string(),
        op_short: op.id().short(),
        operation: op.description().to_owned(),
        from_restore,
        view: view.id().to_string(),
        working_copy: wc.id().to_string(),
        working_copy_short: wc.id().short(),
        change_id: wc.change_id().to_string(),
        change_id_short: wc.change_id().short(),
        base_cut,
        heads: ids_to_strings(view.heads()),
        clean,
    })
}

/// Builds the [`Response::Restored`] payload for the new restore op `new_op`.
fn restored_response(repo: &Repository, new_op: ObjectId) -> Result<Response> {
    let op = repo.store().get_op(&new_op)?;
    let previous_op = op.parents().first().map(ToString::to_string);
    let view = repo.store().get_view(&op.view())?;
    let wc = repo.store().get_snapshot(&view.working_copy())?;
    // If the restored working copy is itself a named cut, report it directly;
    // otherwise report the nearest named ancestor it sits on.
    let restored_cut = if wc.message().is_empty() {
        repo.base_cut()?.as_ref().map(CutSummary::from_snapshot)
    } else {
        Some(CutSummary::from_snapshot(&wc))
    };
    Ok(Response::Restored {
        op: new_op.to_string(),
        op_short: new_op.short(),
        working_copy: wc.id().to_string(),
        working_copy_short: wc.id().short(),
        restored_cut,
        previous_op,
        hint: "run `tack undo` to return to the pre-restore state (it is itself recorded as an op)"
            .to_string(),
    })
}

/// Builds the [`Response::Undone`] payload for the new undo op `new_op`.
fn undone_response(repo: &Repository, new_op: ObjectId) -> Result<Response> {
    let op = repo.store().get_op(&new_op)?;
    // The undo op's parent is the op whose effect was reversed.
    let undone_op = op.parents().first().map(ToString::to_string);
    let view = repo.store().get_view(&op.view())?;
    let wc = repo.store().get_snapshot(&view.working_copy())?;
    Ok(Response::Undone {
        op: new_op.to_string(),
        op_short: new_op.short(),
        undone_op,
        working_copy: wc.id().to_string(),
        working_copy_short: wc.id().short(),
    })
}

/// Maps a [`ScopedCutOutcome`](crate::repo::ScopedCutOutcome) to its wire DTO.
fn scoped_cut_data(outcome: &crate::repo::ScopedCutOutcome) -> ScopedCutData {
    ScopedCutData {
        cut: outcome.cut().to_string(),
        cut_short: outcome.cut().short(),
        op: outcome.op().to_string(),
        op_short: outcome.op().short(),
        base: outcome.base().map(|id| id.to_string()),
        captured: outcome.captured().to_vec(),
        outside_changes: outcome.outside_changes().to_vec(),
    }
}

fn lane_data(lane: &Lane) -> LaneData {
    LaneData {
        name: lane.name().to_owned(),
        cut: lane.cut().to_string(),
        cut_short: lane.cut().short(),
        admission: lane.admission().to_string(),
        admission_short: lane.admission().short(),
        reason: lane.reason().to_owned(),
        timestamp: lane.timestamp(),
    }
}

fn admission_data(outcome: &AdmissionOutcome) -> AdmissionData {
    AdmissionData {
        lane: outcome.lane().to_owned(),
        cut: outcome.cut().to_string(),
        cut_short: outcome.cut().short(),
        op: outcome.op().to_string(),
        op_short: outcome.op().short(),
        reason: outcome.reason().to_owned(),
    }
}

fn provenance_data(provenance: &BackportProvenance) -> BackportProvenanceData {
    let source_admission = provenance
        .source_admission()
        .map(|source| SourceAdmissionData {
            lane: source.lane().to_owned(),
            op: source.op().to_string(),
            op_short: source.op().short(),
        });
    BackportProvenanceData {
        source_cut: provenance.source_cut().to_string(),
        source_cut_short: provenance.source_cut().short(),
        source_change_id: provenance.source_change_id().to_string(),
        source_change_id_short: provenance.source_change_id().short(),
        source_admission,
        target_lane: provenance.target_lane().to_owned(),
        target_base: provenance.target_base().to_string(),
        target_base_short: provenance.target_base().short(),
        reason: provenance.reason().to_owned(),
    }
}

fn record_backport_data(
    outcome: &str,
    record: &BackportRecord,
    admission: Option<&AdmissionData>,
) -> BackportData {
    BackportData {
        outcome: outcome.to_owned(),
        provenance: provenance_data(record.provenance()),
        cut: Some(record.result_cut().to_string()),
        cut_short: Some(record.result_cut().short()),
        op: Some(record.op().to_string()),
        op_short: Some(record.op().short()),
        method: record.method().to_owned(),
        conflicts: Vec::new(),
        admission: admission.cloned(),
    }
}

fn settlement_backport_data(
    settlement: &BackportSettlement,
    admission: Option<&AdmissionData>,
) -> BackportData {
    BackportData {
        outcome: "settlement".to_owned(),
        provenance: provenance_data(settlement.provenance()),
        cut: None,
        cut_short: None,
        op: Some(settlement.op().to_string()),
        op_short: Some(settlement.op().short()),
        method: "settlement".to_owned(),
        conflicts: settlement.conflicts().to_vec(),
        admission: admission.cloned(),
    }
}

fn backport_data(outcome: &BackportOutcome, admission: Option<&AdmissionData>) -> BackportData {
    match outcome {
        BackportOutcome::Created(record) => record_backport_data("created", record, admission),
        BackportOutcome::AlreadyPorted(record) => {
            record_backport_data("already_ported", record, admission)
        }
        BackportOutcome::Settlement(settlement) => settlement_backport_data(settlement, admission),
    }
}

/// Returns the agent-API schema (the same data the `help` method returns).
///
/// Exposed so a client (e.g. the `tack schema` CLI command) can render the API
/// description **without** an open repository — API discovery should never
/// require a repo.
#[must_use]
pub fn schema_methods() -> Vec<MethodInfo> {
    api_schema()
}

/// The hand-maintained self-description of the agent API ([`Request::Help`]).
///
/// One [`MethodInfo`] per [`Request`] variant. The
/// `help_describes_every_dispatched_method` test keeps this in lock-step with
/// the dispatch in [`run`] so a new method cannot ship undocumented.
#[expect(
    clippy::too_many_lines,
    reason = "hand-maintained API schema is intentionally kept as one table"
)]
fn api_schema() -> Vec<MethodInfo> {
    /// Shorthand for a parameter descriptor.
    fn p(name: &str, ty: &str, required: bool, description: &str) -> ParamInfo {
        ParamInfo {
            name: name.to_string(),
            ty: ty.to_string(),
            required,
            description: description.to_string(),
        }
    }
    /// Shorthand for a method descriptor.
    fn m(method: &str, summary: &str, params: Vec<ParamInfo>, returns: &str) -> MethodInfo {
        MethodInfo {
            method: method.to_string(),
            summary: summary.to_string(),
            params,
            returns: returns.to_string(),
        }
    }

    vec![
        m(
            "status",
            "Working directory vs the working-copy snapshot.",
            vec![],
            "{status:\"status\", data:{added,modified,deleted,clean}}",
        ),
        m(
            "snapshot",
            "Auto-snapshot the working copy.",
            vec![],
            "{status:\"snapshot\", snapshot, snapshot_short}",
        ),
        m(
            "named_cut",
            "Close a named cut (the analog of a commit).",
            vec![
                p("message", "string", true, "the cut message"),
                p("author_name", "string", true, "author display name"),
                p(
                    "author_email",
                    "string",
                    true,
                    "author e-mail (never echoed back)",
                ),
            ],
            "{status:\"named_cut\", cut, cut_short}",
        ),
        m(
            "log",
            "Named cuts in the working copy's ancestry, newest-first.",
            vec![],
            "{status:\"log\", cuts:[CutSummary]}",
        ),
        m(
            "op_log",
            "The operation log, newest-first.",
            vec![],
            "{status:\"op_log\", ops:[OpSummary]}",
        ),
        m(
            "diff",
            "Diff the live working copy (or two snapshots); file-level, or per-file stat/patch.",
            vec![
                p(
                    "from",
                    "string?",
                    false,
                    "from id/prefix (default: the to-snapshot's parent cut)",
                ),
                p(
                    "to",
                    "string?",
                    false,
                    "to id/prefix (default: the LIVE working copy — current files on disk, including dirty edits, the same on-disk bytes `status` reads; pass a snapshot id to diff a recorded snapshot instead). Accepts a snapshot OR an op id.",
                ),
                p("stat", "bool", false, "per-file line counts (DiffStat)"),
                p(
                    "patch",
                    "bool",
                    false,
                    "per-file content hunks (DiffPatch); wins over stat",
                ),
            ],
            "{status:\"diff\"|\"diff_stat\"|\"diff_patch\", to_kind:\"live_working_copy\"|\"snapshot\", ...}",
        ),
        m(
            "restore",
            "Non-destructively restore the working copy to an op/snapshot.",
            vec![p(
                "target",
                "string",
                true,
                "op or snapshot id/prefix to restore to",
            )],
            "{status:\"restored\", op, working_copy, restored_cut, previous_op, hint}",
        ),
        m(
            "undo",
            "Reverse the most recent operation (non-destructive).",
            vec![],
            "{status:\"undone\", op, undone_op, working_copy}",
        ),
        m(
            "cat",
            "An object's type tag and byte length.",
            vec![p("id", "string", true, "object id/prefix")],
            "{status:\"cat\", object:{id,id_short,kind,size}}",
        ),
        m(
            "ls",
            "Immediate entries of a tree (default: the LIVE working copy on disk).",
            vec![p(
                "tree",
                "string?",
                false,
                "tree id/prefix (default: the live working copy — current files on disk, including un-snapshotted adds/removes)",
            )],
            "{status:\"ls\", entries:[TreeEntryData]}",
        ),
        m(
            "help",
            "This self-description of every method.",
            vec![],
            "{status:\"help\", methods:[MethodInfo]}",
        ),
        m(
            "current",
            "Where the working copy is: op, base cut, heads, from_restore.",
            vec![],
            "{status:\"current\", data:CurrentData}",
        ),
        m(
            "cuts",
            "Every named cut across all lineages, newest-first.",
            vec![],
            "{status:\"cuts\", cuts:[CutSummary]}",
        ),
        m(
            "claims",
            "The currently-held advisory claims.",
            vec![],
            "{status:\"claims\", claims:[Claim]}",
        ),
        m(
            "claim",
            "Record an advisory claim on a path (never enforced).",
            vec![
                p("path", "string", true, "repo-relative path to claim"),
                p(
                    "holder",
                    "string",
                    true,
                    "actor id/username (never an e-mail)",
                ),
                p("note", "string", false, "optional advisory note"),
            ],
            "{status:\"claimed\", claim, conflicts, op}",
        ),
        m(
            "release",
            "Release an advisory claim on a path.",
            vec![
                p("path", "string", true, "repo-relative path to release"),
                p("holder", "string", true, "actor releasing the claim"),
            ],
            "{status:\"claims\", claims:[Claim]}",
        ),
        m(
            "scoped_cut",
            "A named cut capturing only the given paths from disk.",
            vec![
                p(
                    "paths",
                    "[string]",
                    true,
                    "repo-relative path selectors to capture",
                ),
                p("message", "string", true, "the cut message"),
                p("author_name", "string", true, "author display name"),
                p(
                    "author_email",
                    "string",
                    true,
                    "author e-mail (never echoed back)",
                ),
                p(
                    "base",
                    "string?",
                    false,
                    "base cut/op to overlay onto (default: current base cut)",
                ),
            ],
            "{status:\"scoped_cut\", data:ScopedCutData}",
        ),
        m(
            "lanes",
            "Current op-derived team/release lanes.",
            vec![],
            "{status:\"lanes\", lanes:[LaneData]}",
        ),
        m(
            "admit",
            "Admit a cut to a team/release lane.",
            vec![
                p("cut", "string", true, "cut id/prefix to admit"),
                p("lane", "string", true, "target lane name"),
                p("reason", "string", false, "admission reason"),
            ],
            "{status:\"admitted\", data:AdmissionData}",
        ),
        m(
            "backport",
            "Create a target-lane backport proposal from a source cut.",
            vec![
                p("source", "string", true, "source fix cut id/prefix"),
                p("target_lane", "string", true, "target lane name"),
                p("message", "string?", false, "target cut message"),
                p("reason", "string", false, "backport reason"),
                p("author_name", "string", true, "author display name"),
                p(
                    "author_email",
                    "string",
                    true,
                    "author e-mail (never echoed back)",
                ),
                p(
                    "admit",
                    "bool",
                    false,
                    "also admit the resulting cut when one is created",
                ),
            ],
            "{status:\"backport\", data:BackportData}",
        ),
        m(
            "backport_continue",
            "Finish the current manual backport settlement.",
            vec![
                p("author_name", "string", true, "author display name"),
                p(
                    "author_email",
                    "string",
                    true,
                    "author e-mail (never echoed back)",
                ),
                p("admit", "bool", false, "also admit the resulting cut"),
            ],
            "{status:\"backport\", data:BackportData}",
        ),
    ]
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn alice() -> (String, String) {
        ("Alice".to_owned(), "alice@example.com".to_owned())
    }

    /// Writes `content` to `root/rel`, creating parent directories.
    fn write_file(root: &std::path::Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdirs");
        }
        std::fs::write(path, content).expect("write");
    }

    // ── serde round-trips ──────────────────────────────────────────────────────

    #[test]
    fn request_round_trips_through_json() {
        let requests = [
            Request::Status,
            Request::Snapshot,
            Request::NamedCut {
                message: "first cut".to_owned(),
                author_name: "Alice".to_owned(),
                author_email: "alice@example.com".to_owned(),
            },
            Request::Log,
            Request::OpLog,
            Request::Diff {
                from: Some("ab12".to_owned()),
                to: None,
                stat: false,
                patch: true,
            },
            Request::Restore {
                target: "deadbeef".to_owned(),
            },
            Request::Undo,
            Request::Cat {
                id: "cafe".to_owned(),
            },
            Request::Ls { tree: None },
            Request::Help,
            Request::Current,
            Request::Cuts,
            Request::Claims,
            Request::Claim {
                path: "src/x.rs".to_owned(),
                holder: "alice".to_owned(),
                note: "wip".to_owned(),
            },
            Request::Release {
                path: "src/x.rs".to_owned(),
                holder: "alice".to_owned(),
            },
            Request::ScopedCut {
                paths: vec!["src".to_owned()],
                message: "scoped".to_owned(),
                author_name: "Alice".to_owned(),
                author_email: "alice@example.com".to_owned(),
                base: None,
            },
            Request::Lanes,
            Request::Admit {
                cut: "ab12".to_owned(),
                lane: "team/main".to_owned(),
                reason: "seed".to_owned(),
            },
            Request::Backport {
                source: "cd34".to_owned(),
                target_lane: "release/7.8.0".to_owned(),
                message: Some("hotfix".to_owned()),
                reason: "customer-blocker".to_owned(),
                author_name: "Alice".to_owned(),
                author_email: "alice@example.com".to_owned(),
                admit: true,
            },
            Request::BackportContinue {
                author_name: "Alice".to_owned(),
                author_email: "alice@example.com".to_owned(),
                admit: true,
            },
        ];
        for req in requests {
            let json = serde_json::to_string(&req).expect("serialize");
            let back: Request = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(req, back, "round-trip mismatch for {json}");
        }
    }

    #[test]
    fn request_uses_method_tag() {
        let json = serde_json::to_string(&Request::Status).expect("serialize");
        assert_eq!(json, r#"{"method":"status"}"#);
        let json = serde_json::to_string(&Request::Diff {
            from: None,
            to: None,
            stat: false,
            patch: false,
        })
        .expect("serialize");
        assert_eq!(
            json,
            r#"{"method":"diff","from":null,"to":null,"stat":false,"patch":false}"#
        );
        // The stat/patch flags default, so the legacy two-field form still parses.
        let legacy: Request = serde_json::from_str(r#"{"method":"diff","from":null,"to":null}"#)
            .expect("legacy diff request must still parse");
        assert_eq!(
            legacy,
            Request::Diff {
                from: None,
                to: None,
                stat: false,
                patch: false
            }
        );
    }

    #[test]
    fn response_round_trips_through_json() {
        let responses = [
            Response::Status {
                data: StatusData {
                    added: vec!["a.txt".to_owned()],
                    modified: vec![],
                    deleted: vec![],
                    clean: false,
                },
            },
            Response::Snapshot {
                snapshot: "ab".repeat(32),
                snapshot_short: "abababababab".to_owned(),
            },
            Response::NamedCut {
                cut: "cd".repeat(32),
                cut_short: "cdcdcdcdcdcd".to_owned(),
            },
            Response::Log {
                cuts: vec![CutSummary {
                    id: "ef".repeat(32),
                    id_short: "efefefefefef".to_owned(),
                    change_id: "01".repeat(32),
                    change_id_short: "010101010101".to_owned(),
                    message: "msg".to_owned(),
                    author_name: "Alice".to_owned(),
                    timestamp: 1_700_000_000,
                    parents: vec!["02".repeat(32)],
                }],
            },
            Response::Ok,
            Response::Error {
                message: "boom".to_owned(),
            },
        ];
        for resp in responses {
            let json = serde_json::to_string(&resp).expect("serialize");
            let back: Response = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(resp, back, "round-trip mismatch for {json}");
        }
    }

    #[test]
    fn response_uses_status_tag_and_omits_email() {
        let resp = Response::Log {
            cuts: vec![CutSummary {
                id: "00".repeat(32),
                id_short: "000000000000".to_owned(),
                change_id: "00".repeat(32),
                change_id_short: "000000000000".to_owned(),
                message: "m".to_owned(),
                author_name: "Alice".to_owned(),
                timestamp: 1,
                parents: vec![],
            }],
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(
            json.contains(r#""status":"log""#),
            "expected status tag: {json}"
        );
        assert!(json.contains("Alice"), "author name should be present");
        assert!(
            !json.contains('@'),
            "author e-mail must never appear in a Log response: {json}"
        );
    }

    // ── handle() against a real temp repo ────────────────────────────────────────

    #[test]
    fn handle_status_snapshot_cut_log_against_real_repo() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");

        // Status on a clean repo: no changes.
        let Response::Status { data } = handle(&repo, Request::Status) else {
            panic!("expected Status response");
        };
        assert!(data.clean, "fresh repo should be clean");

        // Write a file → Status reports it as added.
        write_file(dir.path(), "hello.txt", b"world");
        let Response::Status { data } = handle(&repo, Request::Status) else {
            panic!("expected Status response");
        };
        assert_eq!(data.added, vec!["hello.txt"], "new file must show as added");

        // Snapshot returns a hex id.
        let Response::Snapshot { snapshot, .. } = handle(&repo, Request::Snapshot) else {
            panic!("expected Snapshot response");
        };
        assert_eq!(snapshot.len(), 64, "snapshot id should be 64 hex chars");

        // NamedCut closes a cut.
        let (name, email) = alice();
        let cut_req = Request::NamedCut {
            message: "first cut".to_owned(),
            author_name: name,
            author_email: email,
        };
        let Response::NamedCut { cut, .. } = handle(&repo, cut_req) else {
            panic!("expected NamedCut response");
        };
        assert_eq!(cut.len(), 64);

        // Log shows the cut, newest-first, with the author name (no e-mail).
        let Response::Log { cuts } = handle(&repo, Request::Log) else {
            panic!("expected Log response");
        };
        assert_eq!(cuts.len(), 1, "one named cut expected");
        assert_eq!(cuts[0].message, "first cut");
        assert_eq!(cuts[0].author_name, "Alice");
        assert_eq!(cuts[0].id, cut, "log entry id must match the closed cut");
    }

    #[test]
    fn handle_maps_errors_to_error_response() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        // A non-hex prefix is rejected by resolve_prefix → Error response.
        let resp = handle(
            &repo,
            Request::Cat {
                id: "zz".to_owned(),
            },
        );
        assert!(
            matches!(resp, Response::Error { .. }),
            "expected Error, got {resp:?}"
        );
    }

    #[test]
    fn handle_ls_lists_root_tree_entries() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "a.txt", b"x");
        write_file(dir.path(), "b.txt", b"y");
        let _ = handle(&repo, Request::Snapshot);

        let Response::Ls { entries } = handle(&repo, Request::Ls { tree: None }) else {
            panic!("expected Ls response");
        };
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt") && names.contains(&"b.txt"));
        assert!(entries.iter().all(|e| e.kind == "blob"));
    }

    /// Regression (adversarial review, consistency-high): a default `ls` reflects
    /// the **live** working copy, so a file added on disk shows up *without* a
    /// snapshot first — the same dirty-disk fix applied to `status`/`diff`.
    #[test]
    fn handle_ls_default_reflects_dirty_disk() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "a.txt", b"x");
        let _ = handle(&repo, Request::Snapshot);

        // Add a file on disk only — deliberately NO snapshot.
        write_file(dir.path(), "b.txt", b"y");
        let Response::Ls { entries } = handle(&repo, Request::Ls { tree: None }) else {
            panic!("expected Ls response");
        };
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"b.txt"),
            "ls must list the un-snapshotted file, got {names:?}"
        );
    }

    // ── serve() over an in-memory Cursor ─────────────────────────────────────────

    #[test]
    fn serve_handles_multiple_requests_and_survives_a_bad_line() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "f.txt", b"v1");

        // Three lines: a valid snapshot, a malformed line, then a valid status.
        let input = concat!(
            "{\"method\":\"snapshot\"}\n",
            "this is not json\n",
            "{\"method\":\"status\"}\n",
        );
        let reader = Cursor::new(input.as_bytes());
        let mut output: Vec<u8> = Vec::new();

        serve(&repo, reader, &mut output).expect("serve loop");

        let text = String::from_utf8(output).expect("utf8 output");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            3,
            "expected one response line per request line"
        );

        // Line 1: a successful Snapshot.
        let r1: Response = serde_json::from_str(lines[0]).expect("parse line 1");
        assert!(
            matches!(r1, Response::Snapshot { .. }),
            "line 1 should be Snapshot: {r1:?}"
        );

        // Line 2: the malformed line yields an Error, NOT a crash or skipped line.
        let r2: Response = serde_json::from_str(lines[1]).expect("parse line 2");
        assert!(
            matches!(r2, Response::Error { .. }),
            "line 2 should be Error: {r2:?}"
        );

        // Line 3: the loop kept going and answered the next request.
        let r3: Response = serde_json::from_str(lines[2]).expect("parse line 3");
        assert!(
            matches!(r3, Response::Status { .. }),
            "line 3 should be Status: {r3:?}"
        );
    }

    #[test]
    fn serve_stops_cleanly_at_eof() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        // Empty input → immediate EOF → no output, clean return.
        let reader = Cursor::new(Vec::new());
        let mut output: Vec<u8> = Vec::new();
        serve(&repo, reader, &mut output).expect("serve loop");
        assert!(
            output.is_empty(),
            "EOF before any request must produce no output"
        );
    }

    // ── new methods: help / current / cuts / claims / scoped_cut / diffs ─────────

    /// A named-cut request with a pinned author, returning the closed cut id.
    fn cut(repo: &Repository, message: &str) -> String {
        match handle(
            repo,
            Request::NamedCut {
                message: message.to_owned(),
                author_name: "Alice".to_owned(),
                author_email: "alice@example.com".to_owned(),
            },
        ) {
            Response::NamedCut { cut, .. } => cut,
            other => panic!("expected NamedCut, got {other:?}"),
        }
    }

    #[test]
    fn help_describes_every_dispatched_method() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        let Response::Help { methods } = handle(&repo, Request::Help) else {
            panic!("expected Help");
        };
        let names: std::collections::HashSet<&str> =
            methods.iter().map(|m| m.method.as_str()).collect();
        for expected in [
            "status",
            "snapshot",
            "named_cut",
            "log",
            "op_log",
            "diff",
            "restore",
            "undo",
            "cat",
            "ls",
            "help",
            "current",
            "cuts",
            "claims",
            "claim",
            "release",
            "scoped_cut",
            "lanes",
            "admit",
            "backport",
            "backport_continue",
        ] {
            assert!(
                names.contains(expected),
                "help is missing method {expected}"
            );
        }
        assert_eq!(
            names.len(),
            21,
            "help must describe exactly the dispatched methods"
        );
        // Self-description must not leak any PII either.
        let json = serde_json::to_string(&Response::Help { methods }).expect("serialize");
        assert!(
            !json.contains('@'),
            "help payload must not contain an e-mail: {json}"
        );
    }

    #[test]
    fn current_reports_base_cut_clean_and_restore_flag() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "a.txt", b"one");
        let cut1 = cut(&repo, "c1");

        let Response::Current { data } = handle(&repo, Request::Current) else {
            panic!("expected Current");
        };
        assert!(!data.from_restore, "fresh state is not a restore");
        assert!(data.clean, "after a cut the working copy is clean");
        assert_eq!(
            data.base_cut.as_ref().map(|c| c.id.clone()),
            Some(cut1.clone())
        );

        write_file(dir.path(), "b.txt", b"two");
        let _ = cut(&repo, "c2");
        let _ = handle(&repo, Request::Restore { target: cut1 });
        let Response::Current { data } = handle(&repo, Request::Current) else {
            panic!("expected Current");
        };
        assert!(
            data.from_restore,
            "after a restore from_restore must be true"
        );
        let json = serde_json::to_string(&Response::Current { data }).expect("serialize");
        assert!(
            !json.contains('@'),
            "current payload must not carry an e-mail: {json}"
        );
    }

    #[test]
    fn cuts_surfaces_off_lineage_cut_that_log_hides() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "a.txt", b"one");
        let cut1 = cut(&repo, "c1");
        write_file(dir.path(), "b.txt", b"two");
        let _ = cut(&repo, "c2");
        let _ = handle(&repo, Request::Restore { target: cut1 });

        let Response::Cuts { cuts } = handle(&repo, Request::Cuts) else {
            panic!("expected Cuts");
        };
        let msgs: Vec<&str> = cuts.iter().map(|c| c.message.as_str()).collect();
        assert!(
            msgs.contains(&"c1") && msgs.contains(&"c2"),
            "cuts must show both lineages: {msgs:?}"
        );

        let Response::Log { cuts } = handle(&repo, Request::Log) else {
            panic!("expected Log");
        };
        let log_msgs: Vec<&str> = cuts.iter().map(|c| c.message.as_str()).collect();
        assert!(
            !log_msgs.contains(&"c2"),
            "log follows the restored lineage: {log_msgs:?}"
        );
    }

    #[test]
    fn diff_patch_and_stat_report_line_level_changes() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "a.txt", b"l1\nl2\n");
        let _ = cut(&repo, "base");
        write_file(dir.path(), "a.txt", b"l1\nCHANGED\n");
        let _ = handle(&repo, Request::Snapshot);

        let Response::DiffPatch { files, to_kind } = handle(
            &repo,
            Request::Diff {
                from: None,
                to: None,
                stat: false,
                patch: true,
            },
        ) else {
            panic!("expected DiffPatch");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.txt");
        assert!(
            files[0].added_lines >= 1 && files[0].removed_lines >= 1,
            "{files:?}"
        );
        assert!(
            !files[0].hunks.is_empty(),
            "a text change must produce hunks"
        );
        // Default `to` → the live working copy, reported for self-explanatory logs.
        assert_eq!(to_kind, DiffTarget::LiveWorkingCopy);

        let Response::DiffStat {
            files,
            total_added,
            total_removed,
            to_kind,
        } = handle(
            &repo,
            Request::Diff {
                from: None,
                to: None,
                stat: true,
                patch: false,
            },
        )
        else {
            panic!("expected DiffStat");
        };
        assert_eq!(files.len(), 1);
        assert!(total_added >= 1 && total_removed >= 1);
        assert_eq!(to_kind, DiffTarget::LiveWorkingCopy);

        // Default (no flags) must remain the file-level summary (back-compat).
        let resp = handle(
            &repo,
            Request::Diff {
                from: None,
                to: None,
                stat: false,
                patch: false,
            },
        );
        assert!(
            matches!(resp, Response::Diff { .. }),
            "default diff must stay file-level: {resp:?}"
        );
    }

    /// An explicit `to` snapshot id classifies the diff as `to_kind: "snapshot"`,
    /// and the value serializes to the documented `snake_case` strings.
    #[test]
    fn diff_to_kind_distinguishes_snapshot_from_live_working_copy() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "a.txt", b"one\n");
        let cut = cut(&repo, "base");

        // Explicit `to` = a recorded snapshot/cut id → "snapshot".
        let resp = handle(
            &repo,
            Request::Diff {
                from: None,
                to: Some(cut),
                stat: false,
                patch: false,
            },
        );
        let Response::Diff { to_kind, .. } = resp else {
            panic!("expected Diff: {resp:?}")
        };
        assert_eq!(to_kind, DiffTarget::Snapshot);

        // The wire strings match the documented values.
        assert_eq!(
            serde_json::to_string(&DiffTarget::LiveWorkingCopy).unwrap(),
            "\"live_working_copy\""
        );
        assert_eq!(
            serde_json::to_string(&DiffTarget::Snapshot).unwrap(),
            "\"snapshot\""
        );
    }

    #[test]
    fn claim_reports_conflicts_and_release_returns_remaining() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");

        let Response::Claimed {
            claim, conflicts, ..
        } = handle(
            &repo,
            Request::Claim {
                path: "src/x.rs".to_owned(),
                holder: "alice".to_owned(),
                note: "wip".to_owned(),
            },
        )
        else {
            panic!("expected Claimed");
        };
        assert_eq!(claim.path, "src/x.rs");
        assert_eq!(claim.holder, "alice");
        assert!(conflicts.is_empty(), "no prior claim → no conflict");

        // A different holder on the same path is an advisory conflict.
        let Response::Claimed { conflicts, .. } = handle(
            &repo,
            Request::Claim {
                path: "src/x.rs".to_owned(),
                holder: "bob".to_owned(),
                note: String::new(),
            },
        ) else {
            panic!("expected Claimed");
        };
        assert_eq!(conflicts.len(), 1, "bob's claim overlaps alice's");
        assert_eq!(conflicts[0].holder, "alice");

        let Response::Claims { claims } = handle(
            &repo,
            Request::Release {
                path: "src/x.rs".to_owned(),
                holder: "alice".to_owned(),
            },
        ) else {
            panic!("expected Claims from release");
        };
        assert_eq!(claims.len(), 1, "alice released; bob remains");
        assert_eq!(claims[0].holder, "bob");
    }

    #[test]
    fn scoped_cut_via_handle_captures_scope_and_flags_outside() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "src/m.txt", b"m1");
        write_file(dir.path(), "other/o.txt", b"o1");
        let _ = cut(&repo, "base");
        write_file(dir.path(), "src/m.txt", b"m2");
        write_file(dir.path(), "other/o.txt", b"o2");

        let Response::ScopedCut { data } = handle(
            &repo,
            Request::ScopedCut {
                paths: vec!["src".to_owned()],
                message: "scoped".to_owned(),
                author_name: "Alice".to_owned(),
                author_email: "alice@example.com".to_owned(),
                base: None,
            },
        ) else {
            panic!("expected ScopedCut");
        };
        assert!(
            data.captured.contains(&"src/m.txt".to_owned()),
            "captured: {:?}",
            data.captured
        );
        assert!(
            data.outside_changes.contains(&"other/o.txt".to_owned()),
            "outside_changes: {:?}",
            data.outside_changes
        );
        assert_eq!(data.cut.len(), 64);
        let json = serde_json::to_string(&Response::ScopedCut { data }).expect("serialize");
        assert!(
            !json.contains('@'),
            "scoped_cut payload must not carry an e-mail: {json}"
        );
    }

    #[test]
    fn restore_and_undo_return_structured_state() {
        let dir = TempDir::new().expect("temp");
        let repo = Repository::init(dir.path()).expect("init");
        write_file(dir.path(), "f.txt", b"v1");
        let cut1 = cut(&repo, "v1");
        write_file(dir.path(), "f.txt", b"v2");
        let _ = cut(&repo, "v2");

        let Response::Restored {
            restored_cut,
            previous_op,
            op,
            ..
        } = handle(
            &repo,
            Request::Restore {
                target: cut1.clone(),
            },
        )
        else {
            panic!("expected Restored");
        };
        assert_eq!(op.len(), 64);
        assert!(previous_op.is_some(), "restore must report the previous op");
        assert_eq!(
            restored_cut.as_ref().map(|c| c.id.clone()),
            Some(cut1),
            "restoring to a named cut reports that cut"
        );

        let Response::Undone { undone_op, op, .. } = handle(&repo, Request::Undo) else {
            panic!("expected Undone");
        };
        assert_eq!(op.len(), 64);
        assert!(undone_op.is_some(), "undo must report the reversed op");
    }
}
