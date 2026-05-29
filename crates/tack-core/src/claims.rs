//! Advisory path claims — lightweight coordination for parallel agents
//! (`DESIGN.md §13`).
//!
//! A **claim** is a non-binding, advisory assertion that some actor (an agent
//! id or username) intends to work on a repo path, so other workers can avoid
//! overlapping edits without inventing their own filesystem signalling
//! convention. Claims are deliberately *not* locks: nothing in the engine
//! refuses a write to a claimed path. They are a coordination hint.
//!
//! ## Claims are derived from the op-log, not stored separately
//!
//! tack's op-log is already the single source of truth (`constitution.md §1`,
//! `DESIGN.md §6`). Rather than introduce a new mutable side-table or a new
//! object kind, a claim or release is recorded as an ordinary [`Op`] whose
//! recorded command is the verb plus its arguments
//! (`["tack", "claim", <path>, <holder>, <note>]` /
//! `["tack", "release", <path>, <holder>]`). The op carries the *current* view
//! unchanged — claiming a path does not touch the working copy.
//!
//! The set of currently-held claims is then a **derived view**: fold the op-log
//! oldest-to-newest, applying each claim/release in order
//! ([`current_claims`]). Because claims live in the op-log and not in the view,
//! they are intentionally unaffected by `restore`/`undo` of the working copy —
//! restoring an old tree must not silently drop a peer's active claim.

use serde::{Deserialize, Serialize};

use crate::object::Op;
use crate::tree::normalize_repo_path;

/// The recorded command verb for a claim op (`command[1]`).
pub const CLAIM_VERB: &str = "claim";

/// The recorded command verb for a release op (`command[1]`).
pub const RELEASE_VERB: &str = "release";

/// One currently-held advisory claim.
///
/// This is a *derived* value (folded from the op-log), not a content-addressed
/// object, so — like the API response DTOs — it carries plain public fields and
/// serialises directly to the wire. `holder` is an identity *label* (a name or
/// agent id), never an e-mail address (organization data rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The claimed repo-relative path (normalised, forward-slashed). The empty
    /// string is the whole-repo selector.
    pub path: String,
    /// The actor holding the claim (an agent id or username; never an e-mail).
    pub holder: String,
    /// A free-form advisory note (may be empty).
    pub note: String,
    /// Seconds since the Unix epoch when the claim was (re)asserted.
    pub timestamp: i64,
}

/// Builds the op command argv that records a claim on `path` by `holder` with
/// an advisory `note`.
///
/// Path and holder are normalised/trimmed; the shape is fixed at five elements
/// so [`parse_op`] can recognise it unambiguously.
pub fn claim_command(path: &str, holder: &str, note: &str) -> Vec<String> {
    vec![
        "tack".to_string(),
        CLAIM_VERB.to_string(),
        normalize_repo_path(path),
        holder.trim().to_string(),
        note.to_string(),
    ]
}

/// Builds the op command argv that records the release of `path` by `holder`.
pub fn release_command(path: &str, holder: &str) -> Vec<String> {
    vec![
        "tack".to_string(),
        RELEASE_VERB.to_string(),
        normalize_repo_path(path),
        holder.trim().to_string(),
    ]
}

/// A parsed claim/release intent recovered from an op's command.
enum ClaimOp {
    /// A claim assertion carrying the full [`Claim`] (timestamp filled from the op).
    Claim(Claim),
    /// A release of `(path, holder)`.
    Release { path: String, holder: String },
}

/// Recognises a claim/release op by its recorded command, returning the parsed
/// intent or `None` for any other op.
fn parse_op(op: &Op) -> Option<ClaimOp> {
    let command = op.metadata().command();
    let timestamp = op.metadata().start().unix_secs();
    match command {
        [tack, verb, path, holder, note] if tack == "tack" && verb == CLAIM_VERB => {
            Some(ClaimOp::Claim(Claim {
                path: path.clone(),
                holder: holder.clone(),
                note: note.clone(),
                timestamp,
            }))
        }
        [tack, verb, path, holder] if tack == "tack" && verb == RELEASE_VERB => {
            Some(ClaimOp::Release { path: path.clone(), holder: holder.clone() })
        }
        _ => None,
    }
}

/// Folds an op-log into the set of currently-held claims.
///
/// `ops` is expected newest-first (as produced by
/// [`op_log`](crate::oplog::op_log)); this replays them oldest-to-newest so a
/// later release cancels an earlier claim. A claim is keyed by `(path, holder)`:
/// re-claiming the same pair updates the note/timestamp; releasing it removes
/// it. The result is sorted by `(path, holder)` for deterministic output.
#[must_use]
pub fn current_claims(ops: &[Op]) -> Vec<Claim> {
    // Insertion-tracking via a Vec keyed by (path, holder): the set is small
    // (coordination scope), so linear lookup is cheaper than a map plus a
    // separate ordering pass.
    let mut held: Vec<Claim> = Vec::new();
    for op in ops.iter().rev() {
        match parse_op(op) {
            Some(ClaimOp::Claim(claim)) => {
                if let Some(existing) =
                    held.iter_mut().find(|c| c.path == claim.path && c.holder == claim.holder)
                {
                    *existing = claim;
                } else {
                    held.push(claim);
                }
            }
            Some(ClaimOp::Release { path, holder }) => {
                held.retain(|c| !(c.path == path && c.holder == holder));
            }
            None => {}
        }
    }
    held.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.holder.cmp(&b.holder)));
    held
}

/// Returns the held claims that **overlap** `path` but are held by a *different*
/// holder — the advisory conflicts a would-be claimant should be warned about.
///
/// `path` is normalised before comparison. Two paths overlap when they are equal
/// or one is a directory ancestor of the other (so claiming `src/` conflicts
/// with an existing claim on `src/model.rs`, and vice-versa). The whole-repo
/// selector (empty path) overlaps everything.
#[must_use]
pub fn conflicts<'a>(existing: &'a [Claim], path: &str, holder: &str) -> Vec<&'a Claim> {
    let path = normalize_repo_path(path);
    let holder = holder.trim();
    existing
        .iter()
        .filter(|c| c.holder != holder && paths_overlap(&c.path, &path))
        .collect()
}

/// Returns `true` if two normalised repo paths overlap: equal, one an ancestor
/// directory of the other, or either being the whole-repo selector.
fn paths_overlap(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() || a == b {
        return true;
    }
    a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Op, OpMetadata, Timestamp};
    use crate::hash::ObjectId;

    /// Builds an op carrying `command` at wall-clock second `secs`.
    fn op_with_command(command: Vec<String>, secs: i64) -> Op {
        let ts = Timestamp::new(secs, 0);
        let view = ObjectId::from_bytes([0u8; 32]);
        Op::new(vec![], view, OpMetadata::new(ts, ts, "host", "user", command), "test op")
    }

    fn claim_op(path: &str, holder: &str, note: &str, secs: i64) -> Op {
        op_with_command(claim_command(path, holder, note), secs)
    }

    fn release_op(path: &str, holder: &str, secs: i64) -> Op {
        op_with_command(release_command(path, holder), secs)
    }

    /// Folds ops given oldest-first by reversing them into the newest-first
    /// order `current_claims` expects.
    fn fold_oldest_first(oldest_first: Vec<Op>) -> Vec<Claim> {
        let mut newest_first = oldest_first;
        newest_first.reverse();
        current_claims(&newest_first)
    }

    #[test]
    fn single_claim_is_held() {
        let claims = fold_oldest_first(vec![claim_op("src/model.rs", "alice", "wip", 100)]);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].path, "src/model.rs");
        assert_eq!(claims[0].holder, "alice");
        assert_eq!(claims[0].note, "wip");
        assert_eq!(claims[0].timestamp, 100);
    }

    #[test]
    fn release_cancels_claim() {
        let claims = fold_oldest_first(vec![
            claim_op("src/model.rs", "alice", "", 100),
            release_op("src/model.rs", "alice", 200),
        ]);
        assert!(claims.is_empty(), "release must cancel the claim: {claims:?}");
    }

    #[test]
    fn reclaim_updates_note_and_timestamp() {
        let claims = fold_oldest_first(vec![
            claim_op("a.txt", "alice", "first", 100),
            claim_op("a.txt", "alice", "second", 200),
        ]);
        assert_eq!(claims.len(), 1, "same (path, holder) collapses to one claim");
        assert_eq!(claims[0].note, "second");
        assert_eq!(claims[0].timestamp, 200);
    }

    #[test]
    fn different_holders_coexist() {
        let claims = fold_oldest_first(vec![
            claim_op("a.txt", "alice", "", 100),
            claim_op("a.txt", "bob", "", 110),
        ]);
        assert_eq!(claims.len(), 2, "two holders may advisorily claim the same path");
    }

    #[test]
    fn release_only_affects_matching_holder() {
        let claims = fold_oldest_first(vec![
            claim_op("a.txt", "alice", "", 100),
            claim_op("a.txt", "bob", "", 110),
            release_op("a.txt", "alice", 120),
        ]);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].holder, "bob");
    }

    #[test]
    fn non_claim_ops_are_ignored() {
        let claims = fold_oldest_first(vec![
            op_with_command(vec!["tack".into(), "snapshot".into()], 100),
            claim_op("a.txt", "alice", "", 110),
            op_with_command(vec!["tack".into(), "restore".into(), "--to".into(), "ab".into()], 120),
        ]);
        assert_eq!(claims.len(), 1, "only the claim op contributes");
    }

    #[test]
    fn claims_are_sorted_by_path_then_holder() {
        let claims = fold_oldest_first(vec![
            claim_op("z.txt", "bob", "", 100),
            claim_op("a.txt", "zoe", "", 100),
            claim_op("a.txt", "amy", "", 100),
        ]);
        let keys: Vec<(&str, &str)> =
            claims.iter().map(|c| (c.path.as_str(), c.holder.as_str())).collect();
        assert_eq!(keys, vec![("a.txt", "amy"), ("a.txt", "zoe"), ("z.txt", "bob")]);
    }

    // ── conflicts ────────────────────────────────────────────────────────────

    #[test]
    fn conflict_on_same_path_different_holder() {
        let held = fold_oldest_first(vec![claim_op("src/model.rs", "alice", "", 100)]);
        let c = conflicts(&held, "src/model.rs", "bob");
        assert_eq!(c.len(), 1, "different holder on same path conflicts");
        let none = conflicts(&held, "src/model.rs", "alice");
        assert!(none.is_empty(), "same holder is not a conflict (it is a re-claim)");
    }

    #[test]
    fn conflict_on_ancestor_directory() {
        let held = fold_oldest_first(vec![claim_op("src/model.rs", "alice", "", 100)]);
        // Claiming the parent dir conflicts with a child-file claim.
        assert_eq!(conflicts(&held, "src", "bob").len(), 1);
        assert_eq!(conflicts(&held, "src/", "bob").len(), 1, "trailing slash normalised");
    }

    #[test]
    fn no_conflict_on_sibling_paths() {
        let held = fold_oldest_first(vec![claim_op("src/a.rs", "alice", "", 100)]);
        assert!(conflicts(&held, "src/b.rs", "bob").is_empty(), "siblings do not overlap");
    }

    #[test]
    fn whole_repo_claim_conflicts_with_everything() {
        let held = fold_oldest_first(vec![claim_op("", "coordinator", "freeze", 100)]);
        assert_eq!(conflicts(&held, "src/deep/x.rs", "worker").len(), 1);
    }
}
