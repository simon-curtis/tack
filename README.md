# tack

A post-git version control system for the AI-agent era.

`tack` replaces git's working-copy model with a **capability-scoped, content-addressed working set**: a content-addressed object store, continuous snapshots recorded to a [Jujutsu](https://github.com/jj-vcs/jj)-style operation log, and an **agent-native API** so AI agents can drive version control without ever touching a real filesystem.

**Status:** working **v0** — a local-first, content-addressed VCS with continuous
snapshots, an append-only operation log, non-destructive restore/undo, an
agent-native JSON-RPC interface, and a Windows ProjFS virtual working directory.
See [`DESIGN.md`](./DESIGN.md) for the architecture and [`constitution.md`](./constitution.md)
for the principles.

## Why

Git's working-copy model — one filesystem directory, one identity, a binary public/private bit, and history-as-the-primary-artifact — is the wrong substrate for fleets of AI agents editing code concurrently. `tack`'s primitive is a content-addressed working set with continuous snapshots and API-native access, where history, visibility, and filesystem projection are *derived views* rather than the source of truth.

## Crates

- **`tack-core`** — all version-control logic: content-addressed object store, operation log, snapshots, diff, non-destructive restore, continuous file-watch capture, and the agent API surface.
- **`tack-cli`** — the thin `tack` binary; one client of `tack-core`.

## Quick start

```sh
cargo build --release

tack init                      # create .tack/ and the root operation
tack status                    # working dir vs the current snapshot
tack snap -m "first cut"       # name a cut (close the working-copy snapshot)
tack log                       # named-cut history (current lineage), newest first
tack log --all                 # every cut across ALL lineages (alias: tack cuts)
tack current                   # where am I: op, base cut, heads, from-restore?
tack op log                    # the operation log (every state transition)
tack diff --from <A> --to <B>  # file-level diff between two cuts
tack diff --patch              # line-level hunks (--stat for per-file counts)
tack restore --to <id>         # non-destructive restore (reports new + previous op)
tack undo                      # reverse the last operation (also non-destructive)
tack snap -m "wip" --only src  # scoped cut: capture ONLY these paths
tack claim src/x.rs --as agent # advisory path claim for parallel agents
tack schema                    # self-describing agent API (the JSON `help` method)
tack serve                     # JSON-RPC agent server over stdio (one request/line)
```

Drive it programmatically without the CLI:

```sh
echo '{"method":"log"}' | tack serve
# => {"status":"log","cuts":[ ... ]}
```

Mount a snapshot as a lazily-hydrated virtual directory (Windows, requires the
ProjFS feature build and the `Client-ProjFS` optional feature enabled):

```sh
cargo build --release --features projfs
tack mount C:\work\my-checkout --snapshot <id>
```

## What works in v0

- **Content-addressed store** — BLAKE3-256 object IDs with type-tag domain
  separation; FastCDC chunking for large-file dedup; zstd on disk; integrity
  verified on read.
- **Continuous snapshots** — the working copy *is* an auto-amending snapshot
  (no `add`/staging); `tack snap` names a cut. A `tack watch` daemon
  auto-snapshots on file changes. An advisory stat cache skips re-chunking
  unchanged files, so capture (and the live `diff`/`ls`) scale with what changed,
  not repo size.
- **Append-only operation log** — every command is an op embedding the full
  repo view; `restore`/`undo` append ops, so **no reachable state is ever
  destroyed**.
- **Agent-native API** — `tack-core` is the SDK; `tack serve` exposes the same
  operations as line-delimited JSON-RPC. The CLI is one client of it. The API is
  **self-describing** (`help`/`tack schema`) and every id-bearing response also
  carries a 12-hex `*_short` form.
- **Orientation after non-linear ops** — `current` reports the working copy's op,
  base cut, heads, and a `from_restore` flag; `restore`/`undo` return the new and
  previous op; `cuts` (`log --all`) lists every cut across all lineages, including
  ones a restore left off the current `log`.
- **Content-level diffs** — `diff --patch` emits per-file line hunks and
  `diff --stat` per-file counts (binary/oversize files reported without hunks). A
  bare `diff` compares the **live working copy** (current files on disk — the
  on-disk bytes `status` reads), so uncaptured edits show without a snapshot
  first; it appends no op. Every diff response carries `to_kind`
  (`live_working_copy` / `snapshot`).
- **Coordination for agent fleets** — advisory `claim`/`release`/`claims` (path
  hints folded from the op-log, surviving restore, never enforced) and
  `scoped_cut` (checkpoint only selected paths from disk, leaving peers' edits
  out), both with **no on-disk format change**.
- **ProjFS projection** — a snapshot can be mounted as a virtual working
  directory whose files hydrate lazily from the store.

334 tests; `cargo clippy --all-targets -- -D warnings` clean (default and
`--features projfs`).

## License

TBD.
