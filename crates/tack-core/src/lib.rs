//! `tack-core` — the content-addressed, agent-native version-control engine.
//!
//! This crate is the product; the `tack` CLI is one client of it
//! (see `constitution.md` §5). The architecture and on-disk format are
//! specified in `DESIGN.md` at the workspace root.
//!
//! Modules are filled in by the layered build (`DESIGN.md` §1): object store,
//! chunker, working copy, diff, operation log, repository, agent API, and the
//! `ProjFS` projection.
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub)]

// ── L1 modules ───────────────────────────────────────────────────────────────

pub mod encoding;
pub mod error;
pub mod hash;

// ── L2 modules ───────────────────────────────────────────────────────────────

pub mod object;

// ── L3 modules ───────────────────────────────────────────────────────────────

pub mod blob;
pub mod chunker;
pub mod store;

// ── L4 modules ───────────────────────────────────────────────────────────────

pub mod claims;
pub mod diff;
pub mod ignore;
pub mod linediff;
pub mod statcache;
pub mod tree;
pub mod workcopy;

// ── L5 modules ───────────────────────────────────────────────────────────────

pub mod oplog;
pub mod repo;

// ── L8 modules ───────────────────────────────────────────────────────────────

pub mod watch;

// ── L9 modules ───────────────────────────────────────────────────────────────

pub mod projfs;

// ── L6 modules ───────────────────────────────────────────────────────────────

pub mod api;

// ── Re-exports ────────────────────────────────────────────────────────────────

pub use encoding::{Decode, Decoder, Encode, Encoder};
pub use error::{Error, Result};
pub use hash::{ObjectId, TypeTag, hash_object};
pub use object::{
    Blob, Chunk, ChunkRef, EntryKind, Identity, NamedRef, Op, OpMetadata, Snapshot, Timestamp,
    Tree, TreeEntry, tree_entry,
};

pub use blob::{read_blob, read_blob_range, store_file_bytes};
pub use chunker::{AVG_CHUNK, MAX_CHUNK, MIN_CHUNK, chunk_ranges};
pub use store::ObjectStore;

pub use claims::Claim;
pub use diff::{TreeDiff, diff_trees};
pub use ignore::IgnoreRules;
pub use linediff::{DiffLine, FilePatch, FileStat, Hunk, tree_patch};
pub use statcache::StatCache;
pub use tree::{
    FileNode, build_tree, build_tree_cached, flatten_tree_full, list_tree, normalize_repo_path,
    path_to_slash, read_tree_path, tree_from_files,
};
pub use workcopy::{Status, materialize, status};

pub use oplog::{append_op, current_op, op_head, op_log, set_op_head};
pub use repo::{
    AdmissionOutcome, BackportOutcome, BackportProvenance, BackportRecord, BackportSettlement,
    FORMAT_VERSION, Lane, Repository, SourceAdmission,
};

pub use api::{Request, Response, handle, serve};

pub use watch::{DEFAULT_DEBOUNCE, Debouncer, WatchOptions, should_snapshot, watch};

// `mount` is always present (a typed-error stub off Windows / without the
// feature); the real source type is only re-exported when the projection is
// actually compiled.
#[cfg(all(windows, feature = "projfs"))]
pub use projfs::TackProjection;
pub use projfs::mount;
