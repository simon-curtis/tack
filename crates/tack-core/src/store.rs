//! The content-addressed object store (`DESIGN.md §4`).
//!
//! An [`ObjectStore`] is rooted at a repository's `.tack/` directory. Objects
//! live under `objects/<aa>/<rest-of-hex>`, where `aa` is the first two hex
//! characters of the object's ID (git-style fan-out) and `<rest-of-hex>` is the
//! remaining 62 characters.
//!
//! # On-disk file layout
//!
//! Each object file is:
//!
//! ```text
//! [ 1-byte type tag ] [ zstd frame of the canonical encoding ]
//! ```
//!
//! The **hash** is always taken over the *uncompressed* canonical bytes with
//! the type tag prepended (`hash_object(tag, canonical)`), exactly as L1/L2
//! define it. zstd is purely a storage transform; it never touches the hash.
//! The leading tag byte lets a reader recover the object's kind without first
//! decompressing or consulting any external index.
//!
//! Writes are **idempotent**: storing an object whose ID already exists on disk
//! is a no-op (content addressing guarantees identical bytes), so concurrent or
//! repeated puts never corrupt the store.
//!
//! Every read **verifies integrity**: after decompression the bytes are
//! re-hashed under the recovered tag and compared against the requested ID; a
//! mismatch yields [`Error::Corruption`]. This catches bit-rot, truncated
//! writes, and tampering.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::encoding::{Decode, Decoder};
use crate::error::{Error, Result};
use crate::hash::{ObjectId, TypeTag, hash_object};
use crate::object::{Blob, Chunk, Op, Snapshot, Tree, View};

/// zstd compression level used for stored objects.
///
/// Level 3 is zstd's default — a balanced speed/ratio trade-off appropriate
/// for a write-often content store. The level is *not* part of the format
/// contract: it only affects the compressed bytes, never the hash, so it may
/// change freely without a `FORMAT_VERSION` bump.
const ZSTD_LEVEL: i32 = 3;

/// Sub-directory of `.tack/` holding the fanned-out object files.
const OBJECTS_DIR: &str = "objects";

/// A content-addressed object store rooted at a `.tack/` directory.
///
/// Construct with [`ObjectStore::init`] (creates the layout) or
/// [`ObjectStore::open`] (expects it to exist).
#[derive(Debug, Clone)]
pub struct ObjectStore {
    /// The `.tack/` root directory.
    root: PathBuf,
}

impl ObjectStore {
    /// Opens an existing object store rooted at `root` (a `.tack/` directory).
    ///
    /// Does not create anything; use [`ObjectStore::init`] for that. The
    /// `objects/` sub-directory is created lazily on first write, so a freshly
    /// `init`-ed store with no objects opens cleanly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `root` does not exist or is not a directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("object store root does not exist: {}", root.display()),
            )));
        }
        Ok(Self { root })
    }

    /// Creates the object-store layout under `root` and opens it.
    ///
    /// Creates `root` and `root/objects/` if they do not already exist;
    /// re-running on an existing store is harmless.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the directories cannot be created.
    pub fn init(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join(OBJECTS_DIR))?;
        Ok(Self { root })
    }

    /// Returns the store's root (`.tack/`) directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Computes the on-disk path for an object ID using git-style fan-out.
    ///
    /// `objects/<first-2-hex>/<remaining-62-hex>`.
    fn object_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_string();
        // ObjectId always renders to exactly 64 hex chars, so this split is safe.
        let (prefix, rest) = hex.split_at(2);
        self.root.join(OBJECTS_DIR).join(prefix).join(rest)
    }

    /// Returns `true` if an object with the given ID is present on disk.
    pub fn has(&self, id: &ObjectId) -> bool {
        self.object_path(id).is_file()
    }

    /// Stores raw canonical bytes under the given type tag, returning the
    /// object's content address.
    ///
    /// The ID is `hash_object(tag, canonical_bytes)`. The bytes are written
    /// zstd-compressed behind a 1-byte tag header. If the object already
    /// exists, the write is skipped (idempotent).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be written.
    pub fn put_raw(&self, tag: TypeTag, canonical_bytes: &[u8]) -> Result<ObjectId> {
        let id = hash_object(tag, canonical_bytes);
        if self.has(&id) {
            return Ok(id);
        }

        let compressed = zstd::encode_all(canonical_bytes, ZSTD_LEVEL)?;

        let path = self.object_path(&id);
        // The fan-out sub-directory may not exist yet.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write to a temporary file in the same directory, then atomically
        // rename into place, so a crash mid-write never leaves a half-written
        // object that would later fail verification.
        let tmp = path.with_extension("tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&[tag as u8])?;
            file.write_all(&compressed)?;
            file.sync_all()?;
        }
        // On Windows rename fails if the destination exists; another writer may
        // have raced us. Since content is addressed, an existing target is
        // already correct — drop our temp and treat it as success.
        match fs::rename(&tmp, &path) {
            Ok(()) => {}
            Err(_) if path.is_file() => {
                let _ = fs::remove_file(&tmp);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(Error::Io(e));
            }
        }
        Ok(id)
    }

    /// Reads the raw object at `id`, returning its recovered type tag and the
    /// decompressed canonical bytes.
    ///
    /// The stored bytes are re-hashed under the recovered tag and checked
    /// against `id`; any mismatch is reported as corruption.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] if no object with that ID exists.
    /// - [`Error::Corruption`] if the tag byte is unrecognized, decompression
    ///   fails, or the bytes do not re-hash to `id`.
    /// - [`Error::Io`] for other read failures.
    pub fn get_raw(&self, id: &ObjectId) -> Result<(TypeTag, Vec<u8>)> {
        let path = self.object_path(id);
        let stored = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::ObjectNotFound(*id));
            }
            Err(e) => return Err(Error::Io(e)),
        };

        let Some((&tag_byte, frame)) = stored.split_first() else {
            return Err(Error::Corruption(format!(
                "object {} is empty (missing tag header)",
                id.short()
            )));
        };
        let tag = tag_from_byte(tag_byte)?;

        let canonical = zstd::decode_all(frame)
            .map_err(|e| Error::Corruption(format!("zstd decode failed for {}: {e}", id.short())))?;

        let actual = hash_object(tag, &canonical);
        if actual != *id {
            return Err(Error::Corruption(format!(
                "object {} failed verification: re-hashed to {}",
                id.short(),
                actual.short()
            )));
        }

        Ok((tag, canonical))
    }

    /// Reads the raw object at `id`, asserting it has the expected `tag`.
    ///
    /// Shared by the typed getters; surfaces a clear corruption error when a
    /// stored object is of the wrong kind for the caller's request.
    fn get_raw_typed(&self, id: &ObjectId, expected: TypeTag) -> Result<Vec<u8>> {
        let (tag, canonical) = self.get_raw(id)?;
        if tag != expected {
            return Err(Error::Corruption(format!(
                "object {} has tag {:#04x}, expected {:#04x}",
                id.short(),
                tag as u8,
                expected as u8
            )));
        }
        Ok(canonical)
    }

    // ── typed put helpers ──────────────────────────────────────────────────

    /// Encodes and stores a [`Chunk`], returning its ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the write fails.
    pub fn put_chunk(&self, chunk: &Chunk) -> Result<ObjectId> {
        self.put_encoded(TypeTag::Chunk, chunk)
    }

    /// Encodes and stores a [`Blob`], returning its ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the write fails.
    pub fn put_blob(&self, blob: &Blob) -> Result<ObjectId> {
        self.put_encoded(TypeTag::Blob, blob)
    }

    /// Encodes and stores a [`Tree`], returning its ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the write fails.
    pub fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
        self.put_encoded(TypeTag::Tree, tree)
    }

    /// Encodes and stores a [`Snapshot`], returning its ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the write fails.
    pub fn put_snapshot(&self, snapshot: &Snapshot) -> Result<ObjectId> {
        self.put_encoded(TypeTag::Snapshot, snapshot)
    }

    /// Encodes and stores an [`Op`], returning its ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the write fails.
    pub fn put_op(&self, op: &Op) -> Result<ObjectId> {
        self.put_encoded(TypeTag::Op, op)
    }

    /// Encodes and stores a [`View`], returning its ID.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the write fails.
    pub fn put_view(&self, view: &View) -> Result<ObjectId> {
        self.put_encoded(TypeTag::View, view)
    }

    /// Encodes `value` to its canonical form and stores it under `tag`.
    fn put_encoded(&self, tag: TypeTag, value: &impl crate::encoding::Encode) -> Result<ObjectId> {
        let mut encoder = crate::encoding::Encoder::new();
        value.encode(&mut encoder);
        self.put_raw(tag, encoder.as_bytes())
    }

    // ── typed get helpers ──────────────────────────────────────────────────

    /// Reads and decodes the [`Chunk`] at `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`], [`Error::Corruption`] (wrong tag,
    /// failed verification, or malformed encoding), or [`Error::Io`].
    pub fn get_chunk(&self, id: &ObjectId) -> Result<Chunk> {
        let canonical = self.get_raw_typed(id, TypeTag::Chunk)?;
        decode_object(&canonical)
    }

    /// Reads and decodes the [`Blob`] at `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`], [`Error::Corruption`], or
    /// [`Error::Io`].
    pub fn get_blob(&self, id: &ObjectId) -> Result<Blob> {
        let canonical = self.get_raw_typed(id, TypeTag::Blob)?;
        decode_object(&canonical)
    }

    /// Reads and decodes the [`Tree`] at `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`], [`Error::Corruption`], or
    /// [`Error::Io`].
    pub fn get_tree(&self, id: &ObjectId) -> Result<Tree> {
        let canonical = self.get_raw_typed(id, TypeTag::Tree)?;
        decode_object(&canonical)
    }

    /// Reads and decodes the [`Snapshot`] at `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`], [`Error::Corruption`], or
    /// [`Error::Io`].
    pub fn get_snapshot(&self, id: &ObjectId) -> Result<Snapshot> {
        let canonical = self.get_raw_typed(id, TypeTag::Snapshot)?;
        decode_object(&canonical)
    }

    /// Reads and decodes the [`Op`] at `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`], [`Error::Corruption`], or
    /// [`Error::Io`].
    pub fn get_op(&self, id: &ObjectId) -> Result<Op> {
        let canonical = self.get_raw_typed(id, TypeTag::Op)?;
        decode_object(&canonical)
    }

    /// Reads and decodes the [`View`] at `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ObjectNotFound`], [`Error::Corruption`], or
    /// [`Error::Io`].
    pub fn get_view(&self, id: &ObjectId) -> Result<View> {
        let canonical = self.get_raw_typed(id, TypeTag::View)?;
        decode_object(&canonical)
    }
}

/// Recovers a [`TypeTag`] from its 1-byte on-disk header.
///
/// # Errors
///
/// Returns [`Error::Corruption`] if the byte is not a known tag.
fn tag_from_byte(byte: u8) -> Result<TypeTag> {
    match byte {
        0x01 => Ok(TypeTag::Blob),
        0x02 => Ok(TypeTag::Chunk),
        0x03 => Ok(TypeTag::Tree),
        0x04 => Ok(TypeTag::Snapshot),
        0x05 => Ok(TypeTag::Op),
        0x06 => Ok(TypeTag::View),
        other => Err(Error::Corruption(format!("unknown object type tag: {other:#04x}"))),
    }
}

/// Decodes a fully-consumed canonical object from `bytes`.
///
/// Asserts (via [`Decoder::finish`]) that no trailing bytes remain.
fn decode_object<T: Decode>(bytes: &[u8]) -> Result<T> {
    let mut decoder = Decoder::new(bytes);
    let value = T::decode(&mut decoder)?;
    decoder.finish()?;
    Ok(value)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{ChunkRef, EntryKind, Identity, OpMetadata, Timestamp, tree_entry};
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, ObjectStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = ObjectStore::init(dir.path().join(".tack")).expect("init store");
        (dir, store)
    }

    fn zero_id() -> ObjectId {
        ObjectId::from_bytes([0u8; 32])
    }

    fn one_id() -> ObjectId {
        ObjectId::from_bytes([1u8; 32])
    }

    fn sample_tree() -> Tree {
        Tree::new(vec![
            tree_entry("alpha.txt", 0o100_644, EntryKind::Blob, zero_id()).expect("name"),
            tree_entry("subdir", 0o040_000, EntryKind::Tree, one_id()).expect("name"),
        ])
        .expect("tree")
    }

    fn sample_snapshot() -> Snapshot {
        Snapshot::new(
            zero_id(),
            vec![one_id()],
            ObjectId::from_bytes([2u8; 32]),
            Identity::new("Alice", "alice@example.com"),
            Identity::new("Bot", "bot@ci.example.com"),
            "initial",
            Timestamp::new(1_700_000_000, 60),
        )
    }

    fn sample_op() -> Op {
        Op::new(
            vec![zero_id()],
            one_id(),
            OpMetadata::new(
                Timestamp::new(1_700_000_000, 0),
                Timestamp::new(1_700_000_001, 0),
                "host",
                "user",
                vec!["tack".to_owned(), "snap".to_owned()],
            ),
            "snapshot working copy",
        )
    }

    fn sample_view() -> View {
        View::new(
            zero_id(),
            vec![crate::object::NamedRef::new("main", one_id())],
            vec![],
            vec![one_id()],
        )
    }

    // ── init / open ─────────────────────────────────────────────────────────

    #[test]
    fn init_creates_objects_dir() -> Result<()> {
        let dir = TempDir::new()?;
        let root = dir.path().join(".tack");
        let store = ObjectStore::init(&root)?;
        assert!(store.root().join(OBJECTS_DIR).is_dir());
        Ok(())
    }

    #[test]
    fn open_nonexistent_root_errors() {
        let result = ObjectStore::open("definitely-not-a-real-path-xyz");
        assert!(matches!(result, Err(Error::Io(_))));
    }

    #[test]
    fn init_is_idempotent() -> Result<()> {
        let dir = TempDir::new()?;
        let root = dir.path().join(".tack");
        ObjectStore::init(&root)?;
        // Second init over the same dir must not fail.
        ObjectStore::init(&root)?;
        Ok(())
    }

    // ── round-trip per kind ───────────────────────────────────────────────────

    #[test]
    fn chunk_round_trip() -> Result<()> {
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(b"hello tack".to_vec());
        let id = store.put_chunk(&chunk)?;
        assert_eq!(id, chunk.id());
        assert_eq!(store.get_chunk(&id)?, chunk);
        Ok(())
    }

    #[test]
    fn blob_round_trip() -> Result<()> {
        let (_dir, store) = temp_store();
        let blob = Blob::new(2048, vec![ChunkRef::new(zero_id(), 1024), ChunkRef::new(one_id(), 1024)]);
        let id = store.put_blob(&blob)?;
        assert_eq!(store.get_blob(&id)?, blob);
        Ok(())
    }

    #[test]
    fn tree_round_trip() -> Result<()> {
        let (_dir, store) = temp_store();
        let tree = sample_tree();
        let id = store.put_tree(&tree)?;
        assert_eq!(store.get_tree(&id)?, tree);
        Ok(())
    }

    #[test]
    fn snapshot_round_trip() -> Result<()> {
        let (_dir, store) = temp_store();
        let snap = sample_snapshot();
        let id = store.put_snapshot(&snap)?;
        assert_eq!(store.get_snapshot(&id)?, snap);
        Ok(())
    }

    #[test]
    fn op_round_trip() -> Result<()> {
        let (_dir, store) = temp_store();
        let op = sample_op();
        let id = store.put_op(&op)?;
        assert_eq!(store.get_op(&id)?, op);
        Ok(())
    }

    #[test]
    fn view_round_trip() -> Result<()> {
        let (_dir, store) = temp_store();
        let view = sample_view();
        let id = store.put_view(&view)?;
        assert_eq!(store.get_view(&id)?, view);
        Ok(())
    }

    // ── has / idempotency ─────────────────────────────────────────────────────

    #[test]
    fn has_reflects_presence() -> Result<()> {
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(b"data".to_vec());
        assert!(!store.has(&chunk.id()));
        let id = store.put_chunk(&chunk)?;
        assert!(store.has(&id));
        Ok(())
    }

    #[test]
    fn put_is_idempotent_and_dedups() -> Result<()> {
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(b"same content".to_vec());
        let id1 = store.put_chunk(&chunk)?;
        let id2 = store.put_chunk(&chunk)?;
        assert_eq!(id1, id2);

        // Exactly one file on disk for this chunk.
        let path = store.object_path(&id1);
        assert!(path.is_file());
        let hex = id1.to_string();
        let fanout = store.root().join(OBJECTS_DIR).join(&hex[..2]);
        let count = fs::read_dir(&fanout)?.count();
        assert_eq!(count, 1, "duplicate chunk must be stored exactly once");
        Ok(())
    }

    // ── not found ─────────────────────────────────────────────────────────────

    #[test]
    fn get_missing_object_errors() {
        let (_dir, store) = temp_store();
        let missing = ObjectId::from_bytes([0xaa; 32]);
        let result = store.get_chunk(&missing);
        assert!(matches!(result, Err(Error::ObjectNotFound(_))));
    }

    // ── corruption detection ──────────────────────────────────────────────────

    #[test]
    fn tampered_payload_is_detected() -> Result<()> {
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(vec![0u8; 100]);
        let id = store.put_chunk(&chunk)?;

        // Flip a byte in the stored (compressed) file body, keeping the tag.
        let path = store.object_path(&id);
        let mut bytes = fs::read(&path)?;
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, &bytes)?;

        let result = store.get_chunk(&id);
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "tampered object must be detected, got {result:?}"
        );
        Ok(())
    }

    #[test]
    fn empty_object_file_is_corruption() -> Result<()> {
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(b"x".to_vec());
        let id = store.put_chunk(&chunk)?;
        fs::write(store.object_path(&id), [])?;
        assert!(matches!(store.get_chunk(&id), Err(Error::Corruption(_))));
        Ok(())
    }

    #[test]
    fn wrong_tag_header_is_corruption() -> Result<()> {
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(b"payload".to_vec());
        let id = store.put_chunk(&chunk)?;
        // Overwrite the tag byte with an unknown value.
        let path = store.object_path(&id);
        let mut bytes = fs::read(&path)?;
        bytes[0] = 0x7f;
        fs::write(&path, &bytes)?;
        assert!(matches!(store.get_raw(&id), Err(Error::Corruption(_))));
        Ok(())
    }

    #[test]
    fn getting_wrong_kind_is_corruption() -> Result<()> {
        let (_dir, store) = temp_store();
        // Store a Blob then ask for it as a Chunk.
        let blob = Blob::new(0, vec![]);
        let id = store.put_blob(&blob)?;
        let result = store.get_chunk(&id);
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "requesting wrong kind must error, got {result:?}"
        );
        Ok(())
    }

    // ── get_raw recovers tag ───────────────────────────────────────────────────

    #[test]
    fn get_raw_recovers_tag() -> Result<()> {
        let (_dir, store) = temp_store();
        let tree = sample_tree();
        let id = store.put_tree(&tree)?;
        let (tag, _bytes) = store.get_raw(&id)?;
        assert_eq!(tag, TypeTag::Tree);
        Ok(())
    }

    #[test]
    fn stored_file_is_compressed_with_tag_header() -> Result<()> {
        // A highly-compressible payload must be smaller on disk than raw.
        let (_dir, store) = temp_store();
        let chunk = Chunk::new(vec![0u8; 64 * 1024]);
        let id = store.put_chunk(&chunk)?;
        let on_disk =
            usize::try_from(fs::metadata(store.object_path(&id))?.len()).expect("size fits usize");
        assert!(
            on_disk < chunk.data().len(),
            "zstd-compressed zeros ({on_disk}) should be far smaller than raw ({})",
            chunk.data().len()
        );
        // First byte is the Chunk tag.
        let first = fs::read(store.object_path(&id))?[0];
        assert_eq!(first, TypeTag::Chunk as u8);
        Ok(())
    }

    // ── ignores stray .tmp ──────────────────────────────────────────────────────

    #[test]
    fn put_after_open_existing_store() -> Result<()> {
        let dir = TempDir::new()?;
        let root = dir.path().join(".tack");
        {
            let store = ObjectStore::init(&root)?;
            store.put_chunk(&Chunk::new(b"first".to_vec()))?;
        }
        // Re-open and confirm the prior object is readable + new puts work.
        let store = ObjectStore::open(&root)?;
        let first = Chunk::new(b"first".to_vec());
        assert!(store.has(&first.id()));
        let second = Chunk::new(b"second".to_vec());
        let id = store.put_chunk(&second)?;
        assert_eq!(store.get_chunk(&id)?, second);
        Ok(())
    }
}
