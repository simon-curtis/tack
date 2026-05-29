# The tack constitution

These are the load-bearing principles. Code that violates one of them is wrong,
even if it works. They are ordered: when two principles pull against each other,
the lower number wins.

## 1. Content-addressed, always

Every stored artifact is named by the cryptographic hash of its content. Identity
*is* content. Two byte-identical things have one name and are stored once.
Deduplication is not a feature — it is a consequence of the naming scheme.

## 2. Continuous snapshots, not manual commits

The working set is captured continuously. You never have to "remember to commit"
to avoid losing work, because the working copy is itself a snapshot that
re-captures on every operation (the Jujutsu model). A *named cut* — what git calls
a commit — is just a snapshot you chose to give a name, a message, and (later) a
signature. The snapshot stream is primary; named cuts are a view onto it.

## 3. Non-destructive history

No operation destroys reachable state. Every mutation **appends** to an operation
log. `restore`, `undo`, and `abandon` move pointers or add new operations — they
never erase. There is no `--hard` in tack. If a state was ever real, the operation
log can take you back to it.

## 4. Everything else is a derived view

History, branches, releases, visibility, and the on-disk file layout are all
*projections* computed over the object store and the operation log. They are not
the source of truth and must never be treated as such. In particular: **the
filesystem is one possible projection** (a ProjFS/FUSE mount), not the substrate
tack is built on.

## 5. API-native before human-native

The primary interface is a programmatic API that an agent can drive without a real
operating system or filesystem. The `tack` CLI and the virtual filesystem are both
*clients* of that API, never privileged paths into the core. We ship the agent SDK
first; the human ergonomics are layered on top. (`tack-core` is the product;
`tack-cli` is one consumer.)

## 6. Capability-scoped access — north star

Visibility and access are expressed as attenuable capabilities (macaroon-style),
not a single binary public/private bit. A holder can hand out a strictly weaker
capability without asking a server.

> **v0 status:** single-user, single-identity, all-access. The object and operation
> model is shaped so capabilities can be layered on without a rewrite — but v0 does
> not implement them.

## 7. Provenance is the trust root — north star

Trust derives from cryptographic provenance: named cuts are signed; the chain is
independently verifiable (Sigstore / Rekor / SLSA direction).

> **v0 status:** the snapshot schema reserves room for signatures and verification
> metadata; v0 does not sign or verify.

---

## Scope discipline

v0 is **local-only, single-user, Windows-first**. We are building the *primitive*,
not the product. The wager is that the same primitive scales from one developer to
a 1000-developer monolith with an agent fleet — so we do not bake in assumptions
that only hold at one scale. When a v0 shortcut would foreclose the multi-scale or
capability/provenance future, we take the longer road instead (smallest future
tech debt, never the cheap v0 hack).
