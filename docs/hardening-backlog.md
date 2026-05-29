# tack hardening backlog — agent-fleet field findings

> **Source:** the first real multi-agent field trial of tack (documenting & building
> `mock-enterprise-app`, 2026-05-29), captured in
> `tack-feedback/workflow-feedback.md`.
> **Method:** each friction's root cause was traced to the actual source, a fix
> designed and gated against [`constitution.md`](../constitution.md), then the whole
> set was adversarially reviewed for misclassification and north-star foreclosure.
> **Status of this doc:** a prioritized engine backlog, not a commitment. Line
> numbers are as-of this analysis — reconfirm against current source before
> implementing.

The field trial validated the core bet: advisory claims + scoped cuts let a fleet
share one working tree and stay legible. The frictions below are where the *engine*
(not the workflow) should change. The guiding rule throughout is the constitution's:
fix the primitive so the right behavior is structural, never bolt on a workaround
that forecloses the capability / provenance / multi-scale future.

---

## Priority summary

| ID | Item | Priority | Effort | Format bump | Gated √ |
|----|------|----------|--------|-------------|---------|
| **F1** | Concurrent op-head append race → divergent-head merge | **P0 — data loss** | M | none | √ |
| **F4** | Pre-cut preview (`preview_scoped_cut`) | P1 | M | none | √ |
| **F5** | Recursive, ignore-respecting file listing (`ls --recursive`) | P1 | S | none | √ |
| **F2** | Advisory path-presence annotation on claims | P1 (transparency) | M | none | √ |
| **F3** | `status` × `claims` attribution join | P2 (convenience) | S | none | √ |
| **F6** | Semantic reference validation | *out of scope* (documented non-decision) | — | — | √ |

**Dependency ordering:** **F1 lands first.** Until the op-head race is fixed, the
parallel-claim use cases that F2 and F3 exist to serve are themselves unsafe — so
shipping F2/F3 as "fleet coordination" features before F1 would be selling a
guarantee the engine cannot keep.

Every item below was checked to require **no `FORMAT_VERSION` bump** — they are pure
derived views / additive wire DTOs / an allowed case of an existing object shape,
consistent with how claims and scoped cuts were originally added (DESIGN §13).

---

## F1 — Concurrent op-head append race (P0, data loss)

**Friction (field):** parallel `tack claim` calls produced `corrupt object: claim
was not recorded`; serializing all tack metadata ops was the only workaround.

**Root cause (personally verified against source):**

- `run_claim` (`api.rs:788-802`) appends the claim, then **re-reads** the claim set
  and raises `Error::Corruption("claim was not recorded")` (`api.rs:800`) if it
  cannot find its own claim.
- `append_op_on_head` (`repo.rs:1051-1061`) reads `op_head` into `parent`, then
  `append_op` does `put_op` → `set_op_head` (`oplog.rs:119-136`).
- `set_op_head` (`oplog.rs:73-88`) is **last-writer-wins**. Its own doc comment
  concedes: "if it races … the head ends up pointing at a valid op" — valid, but
  only *one* op.

Two concurrent claimers both read `op_head = X`, both build a claim-op parented on
`X`, both rename op-head. The head ends at one of them; the **loser's claim-op is
orphaned** — present in the store, unreachable from the head. The loser's
verification re-read folds from the new head, never sees its own claim, and raises
the exact field error. The winner *silently* drops the loser's claim. Any two
concurrent op-appending commands collide this way (claim, scoped_cut, restore, snap),
not just claims.

**This violates constitution §3 (non-destructive history)** — a reachable state is
lost — and contradicts DESIGN §6's explicit promise: *"Concurrent ops produce
divergent heads that a later op merges (`parents.len() >= 2`)."* That merge logic was
never implemented.

**Proposed design — make DESIGN §6 true (compare-and-swap + auto-merge):**

1. Turn head advancement into a CAS: `cas_op_head(tack_dir, expected, new) ->
   Result<CasOutcome>` returning success or the *actual* current head. Implement with
   an exclusive lock file (`.tack/op-head.lock`) around read-verify-write — the
   correct local-only v0 primitive (flock on Unix).
2. In `append_op`, on CAS mismatch: read the new head `H`, build a **merge op**
   `parents = [my_op, H]`, `put_op`, then CAS again (retry/loop on further races).
3. Nothing else changes: `op_log` already does a DAG BFS over `parents`
   (`oplog.rs:150-174`) and `current_claims` already folds the whole log — so once
   no op is orphaned, both original claims reappear for free.

**Gate:** §1 ✓ merge ops are ordinary content-addressed ops. §3 ✓ restores the
invariant the race breaks. §4 ✓ divergent heads + merge are a natural DAG property,
not a special case. §6/§7 ✓ no new fields; provenance signs snapshots, not ops;
capability-scoped reads filter on reachability, which becomes *consistent*.
**Forecloses nothing** — a DAG is strictly more general than a linear chain and is the
multi-scale-correct shape. **Format bump: none** (`parents` is already a list that
permits length ≥ 2; the lock file is unversioned metadata).

**Supersedes** the "serialize tack metadata operations" workaround entirely.

**Open question to close before claiming done:** confirm by reproduction that the
op-head race is the *sole* source of concurrent corruption — that no second hazard
exists in the store write path (`put_op`) or in op-head reads observing a mid-rename
state. Write a stress test (N concurrent `claim`s) that asserts the final op-log is
fully connected and every claim survives.

---

## F4 — Pre-cut preview without a staging area (P1)

**Friction (field):** no staging area means losing the "review exactly what I am
about to commit" step unless workers drive scoped cuts + diffs by hand.

**Root cause (analysis):** `scoped_cut` computes its `captured` / `outside_changes`
report only as a side effect of *recording* the cut (writes objects, appends an op,
creates a side head). There is no way to see that report first. Note the framing
correction from review: tack has *never* had a staging area, and `named_cut` already
operates on the live working copy (which `diff`/`status` preview). The gap is
specifically **scoped** cuts.

**Proposed design:** extract the shared overlay computation and expose a read-only
`preview_scoped_cut(paths, base) -> ScopedCutPreview` (API `PreviewScopedCut`,
optional CLI `tack snap --only <p> --preview`). It returns the identical
`captured` + `outside_changes` report but writes **no objects, no op**, touches no
head and no files — like `diff`, it reads the live on-disk tree. A unit test asserts
the preview equals the post-cut report for identical inputs.

A separate method (not a `--dry-run` flag on the recording call) keeps inspection and
mutation as distinct, composable, zero-cost operations — the jj "inspect, then act"
model, and the agent-native path (§5).

**Gate:** §2 ✓ deliberation before a named cut. §3/§4 ✓ pure read-only projection,
no append. §5 ✓ core method first, CLI on top. **Forecloses nothing; format bump: none.**

---

## F5 — Recursive, ignore-respecting file listing (P1)

**Friction (field):** broad listings via shell tools ignore `.tackignore`, so workers
fall back to `rg --glob` hacks. Per §5 the engine owns the file projection but only
exposes one level of it.

**Root cause (analysis):** `ls` / `list_tree` (`api.rs:632`) returns only a tree's
immediate children. The recursive flattener (`flatten_tree_full`, `tree.rs:336`) exists
and is used inside `scoped_cut`, but is not exposed. Ignore rules are applied at
*tree-build* time (`tree.rs:84-91`), so they're already baked into any recorded or
live tree — enumeration never sees ignored entries.

**Proposed design:** expose `Repository::list_files_recursive(tree_id)` via a new
`LsRecursive { tree: Option<String> }` request (default = live working tree) returning
`{ path, mode, kind, id, id_short }` per file, **and** a `tack ls --recursive [TREE]`
CLI verb (review caught that the API-only version still leaves humans shelling out).
`.tackignore` is honored automatically because ignored files were never in the tree.

**Gate:** §5 ✓ removes the shell-out antipattern. §4 ✓ read-only DAG projection. §1 ✓
deterministic, returns content ids. **Forecloses nothing; format bump: none.**

**Stated assumption (per review):** `flatten_tree_full` has only ever run on freshly
*built* trees, not arbitrary recorded ones — document that `LsRecursive` assumes all
reachable sub-trees are present and returns `Error::ObjectNotFound` otherwise (store
verifies on read; no partial-result mode in v0).

---

## F2 — Advisory path-presence annotation on claims (P1, transparency)

**Friction (field):** claims pointed at paths not yet on disk (`src/lib` before the
domain files existed), and a claimed path briefly vanished mid-work (`src/App.tsx`,
`src/styles.css`).

**Reframe (per review):** this is **not** "validate claims" and claims must stay
advisory (§3, DESIGN §13) — claiming an absent path is a *legitimate* pre-claim of a
file you're about to create. The fix is **transparency**: let a claim *report* whether
its path currently exists, without ever blocking on it.

**Root cause (analysis):** `current_claims` (`claims.rs:118`) folds the op-log with no
reference to any tree, so a claim carries no presence signal.

**Proposed design:** add `present: Option<bool>` to `Claim`
(`#[serde(skip_serializing_if = "Option::is_none")]`), populated at query time by
checking the path against the current working-copy tree; `None` if the tree can't be
read (degrade, never fail). No op-log change, no new stored state.

**Gate:** §3 ✓ stays advisory, never blocks. §4 ✓ presence computed at query time, not
stored. §1 ✓ ordinary tree lookup. **Format bump: none.**

**Design debt to note (per review):** presence is measured against *the* working copy.
Reserve an optional `tree_id` parameter so a future multi-workspace / capability-scoped
view can ask "present relative to *which* tree" (§6) without an API break. **Depends on
F1** before it's safe under concurrent claims.

---

## F3 — `status` × `claims` attribution join (P2, convenience)

**Friction (field):** `tack status` is concise but can't say *which worker* changed a
dirty file — it must be cross-referenced with `claims` by hand.

**Root cause (analysis):** `status` and `claims` are independent derived views with no
join; the agent must call both and correlate (both already normalize via
`normalize_repo_path`, so the join is mechanical).

**Proposed design:** a convenience endpoint `status_attributed()` (API
`StatusAttributed`, CLI `tack status --attributed`) that annotates each dirty path with
its overlapping claim holder/note. Plain `status` stays the cheap default; this is an
opt-in single call instead of two. Keep it as a *separate* method so the common path
pays nothing.

**Gate:** §4 ✓ a join of two existing views, nothing stored. §5 ✓ core method, CLI on
top. §3 ✓ read-only. **Forecloses nothing; format bump: none.**

**Priority note:** review argued P2 — it's UX convenience, not a correctness fix, and
sits below F1 (data loss) and the workflow-unblockers F4/F5. Listed P2 here for that
reason, though it directly answers a named friction and is cheap (S), so it's a strong
P2. **Depends on F1** for trustworthy attribution under concurrency.

---

## F6 — Semantic reference validation (documented non-decision)

**Friction (field):** the first integrated build failed because `src/pages/index.ts`
exported pages before all page modules existed — caught immediately by tests.

**Decision: this is workflow discipline, not an engine concern — by design.** tack is a
content-addressed *file* VCS; it has zero semantic understanding of imports/exports
(verified: nothing in `repo.rs`/`tree.rs`/`object.rs`/`api.rs` parses content), and it
cannot know whether a missing path is a bug, a not-yet-written stub, or an optional
dependency. Baking language-aware validation into the engine would violate §4 (it's a
per-stack projection, not a universal primitive), risk format coupling, foreclose
multi-scale (different scales want different semantic models), and breed false
confidence from a check that can't be complete.

The engine **already** gives a coordinator everything needed to gate integration:
`scoped_cut`'s `outside_changes`, `diff`/`status`, the immutable append-only op-log, and
advisory `claims`. The right place for "do all referenced files exist / do tests pass"
is the coordinator's integration gate — exactly where the field trial caught it. **No
engine change.** Recorded here so the boundary is explicit, not rediscovered.

---

## By design — not bugs (record so they aren't refiled)

- **Claims are advisory and never enforced.** Workers still need discipline to avoid
  editing a peer's claimed path. This is the intended §13 contract; F2 makes state
  *visible*, it does not add enforcement. Enforcement would need capabilities (§6),
  not locks.
- **Claiming an absent path is allowed** (pre-claiming a file you'll create). See F2.

## Out of scope — environmental, not tack

- **Vite `EPERM` emptying a locked `dist`.** A build-tool/AV/file-lock interaction;
  `.tackignore` already excludes `dist/`. Not a tack issue.
- **In-app browser backend unavailable** for visual smoke testing — tooling
  availability, unrelated to the VCS.

---

## Suggested sequencing

1. **F1** (P0) — unblocks *all* parallel-agent safety; prerequisite for F2/F3.
2. **F5** then **F4** (P1) — cheap, high-leverage agent-native projections.
3. **F2** (P1) — coordination transparency, once F1 makes concurrent claims safe.
4. **F3** (P2) — convenience join, last.
