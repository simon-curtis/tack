# tack — architecture & on-disk format

This is the implementation contract. The principles it serves live in
[`constitution.md`](./constitution.md); this document says *how*. Anything here
that is part of the **on-disk format** is a compatibility contract: changing it
changes object IDs and must bump `FORMAT_VERSION`.

> Status: v0, local-only, single-user, Windows-first. North-star items
> (capabilities, signing, FUSE) are called out as such and are not implemented in
> v0, but the format reserves room for them.

---

## 1. Crates & module map

Two crates. `tack-core` is the product; `tack-cli` is one client of it
(constitution §5).

```
crates/
  tack-core/                 # all version-control logic — the agent SDK
    src/
      lib.rs                 # crate root, lint config, pub re-exports
      error.rs               # Error enum (thiserror), Result<T> alias
      hash.rs                # ObjectId (BLAKE3-256), type tags, hash_object()
      encoding.rs            # canonical binary codec: Encoder / Decoder primitives
      object.rs              # Object kinds + canonical encode/decode + id()
      store.rs               # ObjectStore: content-addressed store on disk (zstd)
      chunker.rs             # FastCDC content-defined chunking
      blob.rs                # file bytes <-> Blob (chunk-list) <-> Chunk objects
      ignore.rs              # .tackignore matching
      tree.rs                # directory <-> Tree; build/walk
      workcopy.rs            # working-copy snapshot + materialize + status
      diff.rs                # tree-vs-tree and tree-vs-workdir diff
      oplog.rs               # Op, View, op-head; append-only operation DAG
      repo.rs                # Repository: the high-level public API
      api.rs                 # agent API: request/response DTOs + dispatch
      projfs.rs              # ProjFS projection (cfg(all(windows, feature="projfs")))
  tack-cli/                  # the thin `tack` binary
    src/
      main.rs                # clap parser -> tack-core calls
```

`tack-core` re-exports its key types at the crate root (`pub use`). Internal
helpers are `pub(crate)`. Struct fields are private with accessors.

---

## 2. Content addressing

- **Hash:** BLAKE3-256, 32-byte digest. Rendered as lowercase hex (64 chars) for
  display; stored and compared as raw `[u8; 32]`.
- **`ObjectId([u8; 32])`** — a `#[repr(transparent)]` newtype. `Display`/`FromStr`
  do hex. Short form = first 12 hex chars for human output.
- **Domain separation by type tag.** Every object is hashed as:

  ```
  ObjectId = BLAKE3( TYPE_TAG_BYTE || canonical_encoding(object) )
  ```

  Type tags: `Chunk=0x02`, `Blob=0x01`, `Tree=0x03`, `Snapshot=0x04`, `Op=0x05`,
  `View=0x06`. The tag is part of the hashed bytes, so two different kinds with
  coincidentally-equal payloads get different IDs. (BLAKE3 `derive_key` keyed mode
  is reserved for a future true key-separation need; the in-band tag is sufficient
  and cheaper for v0.)

---

## 3. Canonical encoding (the determinism contract)

`serde_json` is **banned from the hash path** — JSON map order, number forms,
whitespace, and string escaping are all non-canonical, so one logical object has
many JSON byte strings → many hashes. Instead `tack-core` owns a **deterministic
binary codec** (`encoding.rs`), git-style: structurally there is exactly one valid
encoding of any value.

Primitives (all integers **little-endian**):

| Primitive | Wire form |
|---|---|
| `u8` / `u32` / `u64` / `i64` / `i32` | fixed-width LE |
| `varint` (lengths/counts) | unsigned LEB128 |
| `bytes` | `varint len` then `len` raw bytes |
| `string` | `bytes` of UTF-8 |
| `ObjectId` | 32 raw bytes (no length prefix — fixed) |
| `array<T>` | `varint count` then `count` encoded `T` |

`serde`/`serde_json` are used **only** for things that are never hashed: the
JSON-RPC wire protocol (`api.rs`) and human-facing `--json` rendering.

A CI-grade test asserts `decode(encode(x)) == x` and `encode(decode(bytes)) ==
bytes` (round-trip byte-stability) for every object kind.

---

## 4. Object model

All objects are immutable and content-addressed. On disk: `objects/<aa>/<rest>`
where `aa` is the first 2 hex chars of the ID (git-style fan-out). Stored bytes
are **zstd-compressed**, but the hash is always over the **uncompressed**
canonical encoding.

### Chunk — tag `0x02`
A leaf of a (possibly large) file. Canonical payload = the raw chunk bytes.
`ChunkId = BLAKE3(0x02 || raw_bytes)`.

### Blob (chunk-list) — tag `0x01`
One file's content as an ordered list of chunks.

```
total_len   : u64        # uncompressed file size
chunks      : array<{ id: ObjectId, len: u32 }>
```

Files ≤ `MIN_CHUNK` are stored as a single-chunk blob (no special small-file
type — keeps the model uniform). Identical files dedup at the Blob level; files
sharing regions dedup at the Chunk level.

### Tree — tag `0x03`
A directory snapshot. **Sorted** entries.

```
entries : array<{
  name      : string      # one path component; never "", ".", "..", or containing '/' or '\'
  mode      : u32         # 0o100644 file, 0o100755 exec, 0o120000 symlink, 0o040000 dir
  kind      : u8          # 0 = Blob, 1 = Tree, 2 = Symlink
  id        : ObjectId    # Blob/Tree/symlink-target-blob
}>
```

**Sort rule (must be exact):** entries are ordered by `name` compared **bytewise
ascending, with a trailing `/` appended to directory names for the comparison
only** (git's convention — so file `foo` and dir `foo` order deterministically).
Documented and unit-tested.

Trees reference sub-trees and blobs by ID → a Merkle DAG. Unchanged
subdirectories are shared across snapshots for free.

### Snapshot — tag `0x04`
A **cut** of the working set. The same object type serves both the
continuously-amending working copy and a named cut (see §7).

```
root_tree   : ObjectId
parents     : array<ObjectId>     # 0 = root, 1 = normal, >=2 = merge
change_id   : ObjectId            # STABLE across amends (jj "change id"); random at birth
author      : { name: string, email: string }
committer   : { name: string, email: string }
message     : string              # empty for an un-named working-copy snapshot
timestamp   : { unix_secs: i64, tz_offset_mins: i32 }
# reserved for north-star (§11): signatures/provenance appended as new trailing fields
```

`SnapshotId = BLAKE3(0x04 || canonical)`. Snapshots are immutable; "amending"
yields a *new* `SnapshotId` that keeps the same `change_id`.

### View — tag `0x06`
The complete repo state at the end of an operation. Content-addressed so
unchanged views are shared between consecutive ops.

```
working_copy : ObjectId                 # current working-copy SnapshotId (v0: single workspace)
bookmarks    : array<{ name: string, target: ObjectId }>   # named refs (sorted by name)
tags         : array<{ name: string, target: ObjectId }>   # sorted by name
heads        : array<ObjectId>          # anonymous snapshot heads (sorted), so nothing is lost
```

### Op — tag `0x05`
One entry in the operation log (§6).

```
parents      : array<ObjectId>          # preceding op(s); >1 only when merging divergent heads
view         : ObjectId                 # View at end of this op
metadata     : {
  start      : { unix_secs: i64, tz_offset_mins: i32 },
  end        : { unix_secs: i64, tz_offset_mins: i32 },
  hostname   : string,
  username   : string,
  command    : array<string>            # the literal argv / agent-API call
}
description  : string                    # e.g. "snapshot working copy", "restore to <op>"
```

`OpId = BLAKE3(0x05 || canonical)`.

---

## 5. Chunking — FastCDC

Large-file dedup uses **FastCDC** (`fastcdc` crate, the `v2020` module), profile
"FastCDC8KB", normalized chunking level 2:

| Param | Value |
|---|---|
| `MIN_CHUNK` | 2 KiB |
| `AVG_CHUNK` | 8 KiB |
| `MAX_CHUNK` | 64 KiB |

These three numbers are **part of the on-disk format contract** — changing them
changes chunk boundaries and therefore IDs, so they live in a versioned constant
and are covered by `FORMAT_VERSION`. The gear table and masks are those baked into
`fastcdc::v2020`; we rely on the crate's implementation being stable for a given
crate major version (pinned `fastcdc = "4.0"`).

---

## 6. The operation log

The op-log is tack's **source of truth**; Snapshots/Trees/Blobs/Chunks are the
immutable content DAG it points into. Modeled on Jujutsu:

- Every repo-mutating command appends exactly one `Op`, each embedding a `View`
  of the full repo state after it.
- Ops form a **DAG** with one or more *op heads*. The repo's current op head is
  the **only mutable pointer in the system**: `op-head` (a tiny file holding one
  OpId). Concurrent ops (e.g. two agents) produce divergent heads that a later op
  merges (`parents.len() >= 2`).
- `restore` / `undo` are themselves new ops → **nothing is ever destroyed**
  (constitution §3).

On-disk: ops are normal content-addressed objects in the store; `op-head` lives at
`.tack/op-head`. A human-readable op log is materialized by walking the DAG from
`op-head`.

---

## 7. Continuous snapshots vs named cuts

The jj model, applied to tack:

- **The working copy *is* a `Snapshot`** (the "working-copy commit"). There is **no
  index / staging area.**
- On every `tack` command (and on file-watch events, and on every agent-API
  mutation), tack first **auto-snapshots** the working copy: re-chunk changed
  files (FastCDC), build new Trees, produce a new working-copy `Snapshot` that
  **auto-amends** the previous one — same `change_id`, new `SnapshotId`, same
  parent. This append is recorded as an Op (`"snapshot working copy"`). This is the
  *continuous snapshot*: fine-grained history with no `git add`/`commit`.
- A **named cut** (`tack snap -m "..."`, the analog of a git commit) =
  finalize the current working-copy snapshot with a message/author, then start a
  **fresh** working-copy snapshot on top (new `change_id`, empty delta). A "commit"
  is therefore not a distinct object type — it is a working-copy `Snapshot` that
  has been *closed* and had a child started above it.

Bookmarks point at named cuts; auto-snapshots remain reachable through the op-log
even when no bookmark names them.

---

## 8. Working copy, status, ignore

- **Snapshot (capture):** walk the working dir (honoring `.tackignore`), chunk each
  file, build Blob + Tree objects, produce the working-copy Snapshot. An advisory
  **stat cache** (`.tack/wc-cache`, `statcache.rs`) skips re-chunking files whose
  on-disk `(mtime, size)` are unchanged (with a git-style racy-clean guard),
  reusing the prior blob id — this accelerates `snap` and, since they build the
  live tree, the default `diff`/`ls` too. The cache is purely advisory: a missing,
  stale, or corrupt cache only costs speed, never correctness, and it carries its
  own version independent of the on-disk `FORMAT_VERSION`.
- **Materialize (project):** write a Tree's content out to a directory. v0 has a
  plain materializer (`restore`/`checkout`); the lazy virtual projection is §10.
- **status:** diff the on-disk working dir against the current working-copy
  Snapshot's tree → added / modified / deleted / unchanged.
- **diff:** tree-vs-tree at **file** granularity (added / removed / modified
  paths). A **content-level** mode (`linediff.rs`) reassembles each changed file
  and produces per-file line hunks (`diff --patch`) or line counts
  (`diff --stat`) via the `similar` crate; binary (NUL-bearing) or oversize files
  are reported without hunks so orientation is never silently lost. With the
  default `to`, diff compares the **live working copy** (the current files on
  disk), so dirty edits show without an explicit snapshot first — the same on-disk
  bytes `status` reads. (It is not identical to `status`: the default diff's
  baseline is the **last cut**, whereas `status`'s baseline is the *recorded*
  working-copy snapshot, so the two can report different sets after a manual
  snapshot amend.) Capturing the live tree writes content-addressed objects but
  appends **no op** and never moves the op-head, so diff is read-only with respect
  to history. Pass an explicit `to` snapshot/op id to diff a *recorded* state.
- **`.tackignore`:** gitignore-style globs (one per line, `#` comments, `!`
  negation). Always implicitly ignores `.tack/`.

---

## 9. Non-destructive restore

`tack restore --to <op|snapshot>`:

1. Resolve the target's `View` (a SnapshotId target synthesizes a View with that
   as the working copy).
2. Build a **new** `Op` whose parent is the *current* op head and whose `view` is
   the target view. `description = "restore to <target>"`.
3. Append it; advance `op-head`.
4. Re-project the filesystem from the restored root Tree.

The pre-restore state stays fully reachable (it is the new op's parent; its
content objects are immutable and still in the CAS). `undo` adds yet another op.
There is no `--hard`. GC may only remove objects unreachable from **every** op.

`restore`/`undo` return **structured** outcomes: the new op id, the restored
working copy, the named cut it now represents (or sits on), and the *previous*
op — so an agent always knows where it landed and how to get back. Note that
`log` follows only the current lineage, so a cut left off-lineage by a restore
no longer appears there; `cuts` (§13) lists it, and the op-log still reaches it.

---

## 10. Filesystem projection (ProjFS)

The filesystem is one *projection* (constitution §4). On Windows, the virtual
working directory is served by **ProjFS** via the `windows-projfs` crate
(`dynamic-import` feature, so the binary links even where ProjFS is not installed
and fails cleanly at mount time).

- `tack-core` exposes a `Projection` source backed by the object store; the
  `projfs` module implements `windows_projfs::ProjectedFileSystemSource`:
  - `list_directory(path)` → children of the current Tree at `path`.
  - `stream_file_content(path, offset, len)` → lazily hydrate bytes from the CAS.
  - Edit observation (feeding the auto-snapshotter) is **not** done via a ProjFS
    notification callback in v0; it is owned by the separate `watch` module (a
    `notify`/`ReadDirectoryChanges` debounced daemon). The projection itself is
    read-only lazy hydration.
- Gated `#[cfg(all(windows, feature = "projfs"))]`; a stub returns a typed
  `ProjfsUnavailable` error elsewhere so all call sites compile cross-platform.
- **Runtime requirement:** the *Client-ProjFS* optional feature must be enabled
  once per machine (admin): `Enable-WindowsOptionalFeature -Online -FeatureName
  Client-ProjFS`. Mounting itself needs neither admin nor Developer Mode. tack
  detects absence and prints that command. Min OS: Windows 10 1809.
- Known hazards engineered around: case-insensitive name compare
  (`PrjFileNameCompare` order for enumeration), persistent placeholder/tombstone
  reparse state across mounts (needs a clean/reset path keyed on a stored
  virtualization-instance GUID), hydration latency on the ProjFS pool thread, and
  AV/indexer-forced mass hydration.

FUSE (Linux/macOS) is the same `Projection` source behind a different backend —
north star, not v0.

---

## 11. Agent-native API

`tack-core` *is* the SDK (in-process). For out-of-process agents, `tack serve`
runs a **line-delimited JSON-RPC** server over stdio (one JSON request per line,
one JSON response per line). The DTOs live in `api.rs` and use `serde`. Every CLI
verb maps to exactly one API method, so agents and humans drive the same core.
This is the "ship the agent SDK first" bet (constitution §5).

Methods (request `method` tag → response `status` tag):

| Method | Purpose |
|---|---|
| `status` / `snapshot` / `named_cut` | working-copy status; auto-snapshot; close a named cut |
| `log` | named cuts in the current lineage, newest-first |
| `cuts` | **every** named cut across all lineages (off-lineage cuts `log` hides) |
| `op_log` | the operation log |
| `current` | where the working copy is: op, base cut, heads, `from_restore` flag |
| `diff` | file-level summary, or per-file `stat` / `patch` hunks |
| `restore` / `undo` | non-destructive; return the new op, restored cut, previous op |
| `cat` / `ls` | object inspection |
| `help` | self-description of every method (params + response shape) |
| `claims` / `claim` / `release` | advisory path claims (§13) |
| `scoped_cut` | a cut capturing only selected paths (§13) |

Responses carry both full ids and `*_short` (12-hex) forms. The API is
**self-describing** via `help`, so an agent need not infer method shapes. Author
e-mail is accepted as input but **never echoed** in any response (org data rule).

---

## 12. CLI surface (`tack`)

| Command | Meaning |
|---|---|
| `tack init` | create `.tack/` and the root op |
| `tack status` | working dir vs current working-copy snapshot |
| `tack snap [-m MSG]` | named cut (close working-copy snapshot, start a child) |
| `tack snap -m MSG --only <path>… [--base <id>]` | **scoped cut** — capture only the given paths (§13) |
| `tack log [--all]` | named-cut history; `--all` lists every cut across all lineages |
| `tack cuts` | alias for `tack log --all` |
| `tack op log` | operation log |
| `tack current` | where the working copy is (op, base cut, heads, from-restore) |
| `tack diff [--from A] [--to B] [--stat] [--patch]` | file-level, or line-level stat/patch |
| `tack restore --to <op\|snapshot>` | non-destructive restore (reports new/previous op) |
| `tack undo` | append an op reversing the last one |
| `tack cat <id>` / `tack ls <tree>` | inspect objects |
| `tack schema` | print the agent-API self-description (the JSON `help` method) |
| `tack claim <path> [--as H] [--note N]` / `tack release <path> [--as H]` / `tack claims` | advisory claims (§13) |
| `tack serve` | JSON-RPC agent server over stdio |
| `tack mount <dir>` *(feature=projfs)* | project a snapshot as a virtual dir |

`--json` on read commands emits machine-readable output for agents.

---

## 13. Coordination & scoped cuts

Two additions make tack safer for **fleets of agents** sharing one working copy,
both built **without new object kinds or a format bump** — they are pure derived
views over the existing op-log and content DAG.

### Advisory claims (`claims.rs`)

A **claim** is a non-binding hint that an actor (agent id / username) intends to
work on a path, so peers can avoid overlapping edits without inventing their own
filesystem signalling. Claims are **not locks**: the engine never refuses a write
to a claimed path.

A claim/release is recorded as an ordinary `Op` whose command is the verb plus
its arguments (`["tack","claim",<path>,<holder>,<note>]` /
`["tack","release",<path>,<holder>]`) and whose `view` is **unchanged** (claiming
a path does not touch the working copy). The held set is a **derived view**: fold
the op-log oldest-to-newest (`current_claims`). Because claims live in the op-log
and not in any view, `restore`/`undo` of the working copy deliberately do **not**
drop a peer's active claim. `claim` also reports advisory **conflicts** —
overlapping claims (same path, or one a directory ancestor of the other) held by
a *different* actor.

This replaces the "transient signal file accidentally captured in a cut" pattern
with coordination state that lives where the source of truth already is.

### Scoped cuts (`scoped_cut`)

A **scoped cut** lets one worker checkpoint *only its own paths* without folding
in peers' concurrent edits to the shared working copy. It is **not** a staging
area: each call is atomic and names its paths explicitly; nothing accumulates
between calls.

Given path selectors and a `base` (the current base cut by default), it builds a
result tree = `base`'s tree with the selected sub-paths **overlaid** from the
live on-disk content, reads disk directly **without** advancing the working-copy
snapshot, and records the result as a named cut on a side **head** (reachable via
`cuts`). The working copy pointer and the filesystem are left untouched, so other
workers are unaffected. The outcome reports the **captured** in-scope paths and
the **`outside_changes`** — out-of-scope paths that differ from `base` (uncaptured
concurrent work a coordinator may want to know about).

---

## 14. North stars (not v0, but the format leaves room)

1. **Capability-scoped visibility** — attenuable macaroon-style capabilities
   instead of one public/private bit (constitution §6).
2. **Cryptographic provenance** — signed named cuts; Sigstore/Rekor/SLSA
   (constitution §7). Snapshot reserves trailing fields for signatures.
3. **Multi-scale** — the same primitive from solo → 1000-dev monolith + agent
   fleet; no v0 assumption may foreclose it.
4. **FUSE projection** for Linux/macOS.

---

## 15. Format versioning

`FORMAT_VERSION` (a `u32` constant, starts at `1`) is written into `.tack/config`
at `init` and gates: the type tags, the canonical encoding rules, the FastCDC
params, and the object-store layout. A repo with a newer format than the binary
understands is refused with a clear error rather than silently misread.
