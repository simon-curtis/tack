//! Content-addressed object kinds: [`Chunk`], [`Blob`], [`Tree`], [`Snapshot`],
//! [`View`], and [`Op`].
//!
//! Every type implements [`Encode`]/[`Decode`] (the L1 codec from
//! `encoding.rs`) and provides an [`id()`] method that derives the object's
//! content address via [`hash_object`].
//!
//! On-disk format contract — see `DESIGN.md §4`. The encoding and the type
//! tags are part of the compatibility contract; changing either bumps
//! `FORMAT_VERSION`.

use crate::encoding::{Decode, Decoder, Encode, Encoder};
use crate::error::{Error, Result};
use crate::hash::{ObjectId, TypeTag, hash_object};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Encodes `self` into a fresh byte vector.
///
/// Convenience used internally by every `id()` implementation.
fn encode_to_vec(value: &impl Encode) -> Vec<u8> {
    let mut enc = Encoder::new();
    value.encode(&mut enc);
    enc.into_bytes()
}

// ── Chunk — tag 0x02 ──────────────────────────────────────────────────────────

/// A leaf chunk of raw file bytes.
///
/// Canonical payload = the raw bytes with no extra framing; the type tag
/// `0x02` is prepended by [`hash_object`] before hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    data: Vec<u8>,
}

impl Chunk {
    /// Wraps `data` in a `Chunk`.
    pub const fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    /// Returns a reference to the raw chunk bytes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Returns the content-address of this chunk.
    ///
    /// `ChunkId = BLAKE3(0x02 || raw_bytes)`.
    pub fn id(&self) -> ObjectId {
        // Chunk encoding IS the raw bytes; tag provides domain separation.
        hash_object(TypeTag::Chunk, &self.data)
    }
}

impl Encode for Chunk {
    /// Canonical payload = the raw bytes (no extra framing).
    fn encode(&self, encoder: &mut Encoder) {
        encoder.bytes_raw(&self.data);
    }
}

impl Decode for Chunk {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let data = decoder.read_remaining().to_vec();
        Ok(Self { data })
    }
}

// ── ChunkRef — embedded in Blob ───────────────────────────────────────────────

/// A reference to a [`Chunk`] object with its uncompressed byte length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    /// Content address of the referenced [`Chunk`].
    id: ObjectId,
    /// Uncompressed byte count of the chunk.
    len: u32,
}

impl ChunkRef {
    /// Creates a new `ChunkRef`.
    pub const fn new(id: ObjectId, len: u32) -> Self {
        Self { id, len }
    }

    /// Returns the chunk's content address.
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    /// Returns the chunk's uncompressed byte length.
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Returns `true` if the referenced chunk has zero uncompressed bytes.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Encode for ChunkRef {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.object_id(&self.id);
        encoder.u32(self.len);
    }
}

impl Decode for ChunkRef {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let id = decoder.object_id()?;
        let len = decoder.u32()?;
        Ok(Self { id, len })
    }
}

// ── Blob — tag 0x01 ───────────────────────────────────────────────────────────

/// An ordered list of chunk references representing one file's content.
///
/// Files ≤ `MIN_CHUNK` are stored as a single-chunk blob. Identical files
/// dedup at the Blob level; files sharing regions dedup at the Chunk level.
///
/// Encoding:
/// ```text
/// total_len : u64
/// chunks    : array<{ id: ObjectId, len: u32 }>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    total_len: u64,
    chunks: Vec<ChunkRef>,
}

impl Blob {
    /// Creates a new `Blob`.
    pub const fn new(total_len: u64, chunks: Vec<ChunkRef>) -> Self {
        Self { total_len, chunks }
    }

    /// Returns the total uncompressed file size in bytes.
    pub const fn total_len(&self) -> u64 {
        self.total_len
    }

    /// Returns the ordered list of chunk references.
    pub fn chunks(&self) -> &[ChunkRef] {
        &self.chunks
    }

    /// Returns the content-address of this blob.
    ///
    /// `BlobId = BLAKE3(0x01 || canonical_encoding)`.
    pub fn id(&self) -> ObjectId {
        hash_object(TypeTag::Blob, &encode_to_vec(self))
    }
}

impl Encode for Blob {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.u64(self.total_len);
        encoder.array_len(self.chunks.len());
        for chunk_ref in &self.chunks {
            chunk_ref.encode(encoder);
        }
    }
}

impl Decode for Blob {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let total_len = decoder.u64()?;
        let count = decoder.array_len()?;
        let mut chunks = Vec::with_capacity(decoder.reserve_hint(count));
        for _ in 0..count {
            chunks.push(ChunkRef::decode(decoder)?);
        }
        Ok(Self { total_len, chunks })
    }
}

// ── EntryKind ─────────────────────────────────────────────────────────────────

/// The kind of object a [`TreeEntry`] points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EntryKind {
    /// Points at a [`Blob`] object. `u8` tag `0`.
    Blob = 0,
    /// Points at a nested [`Tree`] object (sub-directory). `u8` tag `1`.
    Tree = 1,
    /// Points at a symlink-target blob. `u8` tag `2`.
    Symlink = 2,
}

impl EntryKind {
    /// Returns `true` if this entry represents a directory (sub-tree).
    pub const fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }

    fn from_u8(byte: u8) -> Result<Self> {
        match byte {
            0 => Ok(Self::Blob),
            1 => Ok(Self::Tree),
            2 => Ok(Self::Symlink),
            other => Err(Error::Corruption(format!("unknown entry kind byte: {other}"))),
        }
    }
}

impl Encode for EntryKind {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.u8(*self as u8);
    }
}

impl Decode for EntryKind {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let byte = decoder.u8()?;
        Self::from_u8(byte)
    }
}

// ── TreeEntry ─────────────────────────────────────────────────────────────────

/// One entry in a [`Tree`] directory snapshot.
///
/// Encoding per entry:
/// ```text
/// name : string
/// mode : u32
/// kind : u8
/// id   : ObjectId
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    name: String,
    mode: u32,
    kind: EntryKind,
    id: ObjectId,
}

impl TreeEntry {
    /// Returns the path component name of this entry.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the POSIX-style mode bits (e.g. `0o100644`).
    pub const fn mode(&self) -> u32 {
        self.mode
    }

    /// Returns the kind of object this entry references.
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    /// Returns the content address of the referenced object.
    pub const fn id(&self) -> ObjectId {
        self.id
    }

    /// Returns the sort key for this entry per the DESIGN.md §4 sort rule.
    ///
    /// Directories compare with a trailing `'/'` appended; all other entries
    /// compare by raw name bytes. This is the git convention and makes `foo`
    /// (file) and `foo` (dir) order deterministically.
    pub(crate) fn sort_key(&self) -> Vec<u8> {
        if self.kind.is_tree() {
            let mut key = self.name.as_bytes().to_vec();
            key.push(b'/');
            key
        } else {
            self.name.as_bytes().to_vec()
        }
    }
}

impl Encode for TreeEntry {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.str(&self.name);
        encoder.u32(self.mode);
        self.kind.encode(encoder);
        encoder.object_id(&self.id);
    }
}

impl Decode for TreeEntry {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let name = decoder.str()?.to_owned();
        let mode = decoder.u32()?;
        let kind = EntryKind::decode(decoder)?;
        let id = decoder.object_id()?;
        Ok(Self { name, mode, kind, id })
    }
}

// ── Tree — tag 0x03 ───────────────────────────────────────────────────────────

/// A directory snapshot: a sorted list of [`TreeEntry`] values.
///
/// The sort rule (from DESIGN.md §4) is bytewise ascending on `name`, with a
/// trailing `'/'` appended to directory entries **for comparison only**. This
/// is the git convention: a file `foo` and a directory `foo` always order
/// deterministically with respect to each other.
///
/// Constructors validate that no entry name is `""`, `"."`, `".."`, or
/// contains `'/'` or `'\\'`.
///
/// Encoding:
/// ```text
/// entries : array<{ name: string, mode: u32, kind: u8, id: ObjectId }>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

/// Validates a single tree entry name, returning `Err` if it is illegal.
///
/// Illegal names: empty string, `"."`, `".."`, any name containing `'/'`
/// or `'\\'`.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidTreeEntryName(name.to_owned()));
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidTreeEntryName(name.to_owned()));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(Error::InvalidTreeEntryName(name.to_owned()));
    }
    Ok(())
}

/// Validates a decoded entry name at the read trust boundary.
///
/// Identical rules to [`validate_name`], but a violation here means the on-disk
/// object is malformed rather than the caller passing a bad name, so it maps to
/// [`Error::Corruption`]. A tree object whose bytes hash to its id can still
/// carry a path-traversing name (`".."`, an embedded separator); rejecting it
/// here closes that escape for every read consumer (`materialize`,
/// `flatten_tree`, `read_tree_path`, `diff`, the `ProjFS` projection).
fn validate_decoded_name(name: &str) -> Result<()> {
    validate_name(name).map_err(|_| {
        Error::Corruption(format!("decoded tree entry name is illegal: {name:?}"))
    })
}

impl Tree {
    /// Creates a `Tree` from a list of entries, sorting them and validating
    /// all names.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTreeEntryName`] if any entry name is `""`,
    /// `"."`, `".."`, or contains `'/'` or `'\\'`.
    pub fn new(mut entries: Vec<TreeEntry>) -> Result<Self> {
        for entry in &entries {
            validate_name(&entry.name)?;
        }
        entries.sort_by_key(TreeEntry::sort_key);
        Ok(Self { entries })
    }

    /// Creates a `Tree` from already-sorted, already-validated entries
    /// (e.g. during decoding). Callers are responsible for correctness.
    pub(crate) const fn from_entries_unchecked(entries: Vec<TreeEntry>) -> Self {
        Self { entries }
    }

    /// Returns the sorted list of tree entries.
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// Returns the content-address of this tree.
    ///
    /// `TreeId = BLAKE3(0x03 || canonical_encoding)`.
    pub fn id(&self) -> ObjectId {
        hash_object(TypeTag::Tree, &encode_to_vec(self))
    }
}

/// Convenience constructor for a [`TreeEntry`].
///
/// Used in tests and by the tree-builder in higher layers. Validates the name.
///
/// # Errors
///
/// Returns [`Error::InvalidTreeEntryName`] for illegal names.
pub fn tree_entry(
    name: impl Into<String>,
    mode: u32,
    kind: EntryKind,
    id: ObjectId,
) -> Result<TreeEntry> {
    let name = name.into();
    validate_name(&name)?;
    Ok(TreeEntry { name, mode, kind, id })
}

impl Encode for Tree {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.array_len(self.entries.len());
        for entry in &self.entries {
            entry.encode(encoder);
        }
    }
}

impl Decode for Tree {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let count = decoder.array_len()?;
        let mut entries = Vec::with_capacity(decoder.reserve_hint(count));
        for _ in 0..count {
            let entry = TreeEntry::decode(decoder)?;
            // The store's re-hash-on-read proves byte integrity, NOT that a
            // decoded name is path-safe. Re-validate every name at this trust
            // boundary so a crafted/corrupt tree object cannot smuggle a
            // traversing name (`".."`, an embedded separator) into a materialize
            // or projection sink. Treat a violation as on-disk corruption.
            validate_decoded_name(&entry.name)?;
            entries.push(entry);
        }
        Ok(Self::from_entries_unchecked(entries))
    }
}

// ── Identity ──────────────────────────────────────────────────────────────────

/// A person's identity: name and e-mail address.
///
/// Used in [`Snapshot`] for author and committer fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    name: String,
    email: String,
}

impl Identity {
    /// Creates a new `Identity`.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    /// Returns the display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the e-mail address.
    pub fn email(&self) -> &str {
        &self.email
    }
}

impl Encode for Identity {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.str(&self.name);
        encoder.str(&self.email);
    }
}

impl Decode for Identity {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let name = decoder.str()?.to_owned();
        let email = decoder.str()?.to_owned();
        Ok(Self { name, email })
    }
}

// ── Timestamp ─────────────────────────────────────────────────────────────────

/// A point in time with a UTC offset.
///
/// `unix_secs` is signed to represent dates before 1970.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    unix_secs: i64,
    tz_offset_mins: i32,
}

impl Timestamp {
    /// Creates a new `Timestamp`.
    pub const fn new(unix_secs: i64, tz_offset_mins: i32) -> Self {
        Self { unix_secs, tz_offset_mins }
    }

    /// Returns the number of seconds since the Unix epoch (negative = before 1970).
    pub const fn unix_secs(&self) -> i64 {
        self.unix_secs
    }

    /// Returns the UTC offset in minutes (e.g. `+60` for UTC+1).
    pub const fn tz_offset_mins(&self) -> i32 {
        self.tz_offset_mins
    }
}

impl Encode for Timestamp {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.i64(self.unix_secs);
        encoder.i32(self.tz_offset_mins);
    }
}

impl Decode for Timestamp {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let unix_secs = decoder.i64()?;
        let tz_offset_mins = decoder.i32()?;
        Ok(Self { unix_secs, tz_offset_mins })
    }
}

// ── Snapshot — tag 0x04 ───────────────────────────────────────────────────────

/// A snapshot of the working set — the core unit of history.
///
/// The same type is used for continuously-amended working-copy snapshots and
/// for named cuts. Field encoding order per DESIGN.md §4:
///
/// ```text
/// root_tree   : ObjectId
/// parents     : array<ObjectId>
/// change_id   : ObjectId
/// author      : { name: string, email: string }
/// committer   : { name: string, email: string }
/// message     : string
/// timestamp   : { unix_secs: i64, tz_offset_mins: i32 }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    root_tree: ObjectId,
    parents: Vec<ObjectId>,
    change_id: ObjectId,
    author: Identity,
    committer: Identity,
    message: String,
    timestamp: Timestamp,
}

impl Snapshot {
    /// Creates a new `Snapshot`.
    pub fn new(
        root_tree: ObjectId,
        parents: Vec<ObjectId>,
        change_id: ObjectId,
        author: Identity,
        committer: Identity,
        message: impl Into<String>,
        timestamp: Timestamp,
    ) -> Self {
        Self {
            root_tree,
            parents,
            change_id,
            author,
            committer,
            message: message.into(),
            timestamp,
        }
    }

    /// Returns the root tree content address.
    pub const fn root_tree(&self) -> ObjectId {
        self.root_tree
    }

    /// Returns the parent snapshot IDs (0 = root, 1 = normal, ≥2 = merge).
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    /// Returns the change ID — stable across amends, random at birth.
    pub const fn change_id(&self) -> ObjectId {
        self.change_id
    }

    /// Returns the author identity.
    pub const fn author(&self) -> &Identity {
        &self.author
    }

    /// Returns the committer identity.
    pub const fn committer(&self) -> &Identity {
        &self.committer
    }

    /// Returns the snapshot message (empty for unnamed working-copy snapshots).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the snapshot timestamp.
    pub const fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Returns the content-address of this snapshot.
    ///
    /// `SnapshotId = BLAKE3(0x04 || canonical_encoding)`.
    pub fn id(&self) -> ObjectId {
        hash_object(TypeTag::Snapshot, &encode_to_vec(self))
    }
}

impl Encode for Snapshot {
    fn encode(&self, encoder: &mut Encoder) {
        // Field order is the on-disk compatibility contract — do NOT reorder.
        encoder.object_id(&self.root_tree);
        encoder.array_len(self.parents.len());
        for parent in &self.parents {
            encoder.object_id(parent);
        }
        encoder.object_id(&self.change_id);
        self.author.encode(encoder);
        self.committer.encode(encoder);
        encoder.str(&self.message);
        self.timestamp.encode(encoder);
    }
}

impl Decode for Snapshot {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let root_tree = decoder.object_id()?;
        let parent_count = decoder.array_len()?;
        let mut parents = Vec::with_capacity(decoder.reserve_hint(parent_count));
        for _ in 0..parent_count {
            parents.push(decoder.object_id()?);
        }
        let change_id = decoder.object_id()?;
        let author = Identity::decode(decoder)?;
        let committer = Identity::decode(decoder)?;
        let message = decoder.str()?.to_owned();
        let timestamp = Timestamp::decode(decoder)?;
        Ok(Self { root_tree, parents, change_id, author, committer, message, timestamp })
    }
}

// ── NamedRef ── used by View ───────────────────────────────────────────────────

/// A named pointer to an [`ObjectId`] — used for bookmarks and tags in [`View`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRef {
    name: String,
    target: ObjectId,
}

impl NamedRef {
    /// Creates a new `NamedRef`.
    pub fn new(name: impl Into<String>, target: ObjectId) -> Self {
        Self { name: name.into(), target }
    }

    /// Returns the bookmark or tag name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the target content address.
    pub const fn target(&self) -> ObjectId {
        self.target
    }
}

impl Encode for NamedRef {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.str(&self.name);
        encoder.object_id(&self.target);
    }
}

impl Decode for NamedRef {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let name = decoder.str()?.to_owned();
        let target = decoder.object_id()?;
        Ok(Self { name, target })
    }
}

// ── View — tag 0x06 ───────────────────────────────────────────────────────────

/// The complete repo state at the end of one operation.
///
/// Content-addressed so unchanged views are shared between consecutive ops.
///
/// Encoding:
/// ```text
/// working_copy : ObjectId
/// bookmarks    : array<{ name: string, target: ObjectId }>   (sorted by name)
/// tags         : array<{ name: string, target: ObjectId }>   (sorted by name)
/// heads        : array<ObjectId>                             (sorted)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    working_copy: ObjectId,
    bookmarks: Vec<NamedRef>,
    tags: Vec<NamedRef>,
    heads: Vec<ObjectId>,
}

impl View {
    /// Creates a new `View`, sorting bookmarks by name, tags by name, and
    /// heads by their byte representation for determinism.
    pub fn new(
        working_copy: ObjectId,
        mut bookmarks: Vec<NamedRef>,
        mut tags: Vec<NamedRef>,
        mut heads: Vec<ObjectId>,
    ) -> Self {
        bookmarks.sort_by(|a, b| a.name.cmp(&b.name));
        tags.sort_by(|a, b| a.name.cmp(&b.name));
        heads.sort();
        Self { working_copy, bookmarks, tags, heads }
    }

    /// Returns the current working-copy snapshot ID.
    pub const fn working_copy(&self) -> ObjectId {
        self.working_copy
    }

    /// Returns the sorted list of bookmarks.
    pub fn bookmarks(&self) -> &[NamedRef] {
        &self.bookmarks
    }

    /// Returns the sorted list of tags.
    pub fn tags(&self) -> &[NamedRef] {
        &self.tags
    }

    /// Returns the sorted list of anonymous snapshot heads.
    pub fn heads(&self) -> &[ObjectId] {
        &self.heads
    }

    /// Returns the content-address of this view.
    ///
    /// `ViewId = BLAKE3(0x06 || canonical_encoding)`.
    pub fn id(&self) -> ObjectId {
        hash_object(TypeTag::View, &encode_to_vec(self))
    }
}

impl Encode for View {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.object_id(&self.working_copy);
        encoder.array_len(self.bookmarks.len());
        for bookmark in &self.bookmarks {
            bookmark.encode(encoder);
        }
        encoder.array_len(self.tags.len());
        for tag in &self.tags {
            tag.encode(encoder);
        }
        encoder.array_len(self.heads.len());
        for head in &self.heads {
            encoder.object_id(head);
        }
    }
}

impl Decode for View {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let working_copy = decoder.object_id()?;
        let bookmark_count = decoder.array_len()?;
        let mut bookmarks = Vec::with_capacity(decoder.reserve_hint(bookmark_count));
        for _ in 0..bookmark_count {
            bookmarks.push(NamedRef::decode(decoder)?);
        }
        let tag_count = decoder.array_len()?;
        let mut tags = Vec::with_capacity(decoder.reserve_hint(tag_count));
        for _ in 0..tag_count {
            tags.push(NamedRef::decode(decoder)?);
        }
        let head_count = decoder.array_len()?;
        let mut heads = Vec::with_capacity(decoder.reserve_hint(head_count));
        for _ in 0..head_count {
            heads.push(decoder.object_id()?);
        }
        Ok(Self { working_copy, bookmarks, tags, heads })
    }
}

// ── OpMetadata ────────────────────────────────────────────────────────────────

/// Metadata embedded in an [`Op`]: timing, machine identity, and the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpMetadata {
    start: Timestamp,
    end: Timestamp,
    hostname: String,
    username: String,
    command: Vec<String>,
}

impl OpMetadata {
    /// Creates a new `OpMetadata`.
    pub fn new(
        start: Timestamp,
        end: Timestamp,
        hostname: impl Into<String>,
        username: impl Into<String>,
        command: Vec<String>,
    ) -> Self {
        Self {
            start,
            end,
            hostname: hostname.into(),
            username: username.into(),
            command,
        }
    }

    /// Returns the operation start time.
    pub const fn start(&self) -> Timestamp {
        self.start
    }

    /// Returns the operation end time.
    pub const fn end(&self) -> Timestamp {
        self.end
    }

    /// Returns the hostname of the machine that ran the operation.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Returns the username of the person (or agent) that ran the operation.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the literal argv or agent-API call that triggered the operation.
    pub fn command(&self) -> &[String] {
        &self.command
    }
}

impl Encode for OpMetadata {
    fn encode(&self, encoder: &mut Encoder) {
        self.start.encode(encoder);
        self.end.encode(encoder);
        encoder.str(&self.hostname);
        encoder.str(&self.username);
        encoder.array_len(self.command.len());
        for arg in &self.command {
            encoder.str(arg);
        }
    }
}

impl Decode for OpMetadata {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let start = Timestamp::decode(decoder)?;
        let end = Timestamp::decode(decoder)?;
        let hostname = decoder.str()?.to_owned();
        let username = decoder.str()?.to_owned();
        let arg_count = decoder.array_len()?;
        let mut command = Vec::with_capacity(decoder.reserve_hint(arg_count));
        for _ in 0..arg_count {
            command.push(decoder.str()?.to_owned());
        }
        Ok(Self { start, end, hostname, username, command })
    }
}

// ── Op — tag 0x05 ─────────────────────────────────────────────────────────────

/// One entry in the operation log.
///
/// Encoding:
/// ```text
/// parents     : array<ObjectId>
/// view        : ObjectId
/// metadata    : { start, end, hostname, username, command }
/// description : string
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    parents: Vec<ObjectId>,
    view: ObjectId,
    metadata: OpMetadata,
    description: String,
}

impl Op {
    /// Creates a new `Op`.
    pub fn new(
        parents: Vec<ObjectId>,
        view: ObjectId,
        metadata: OpMetadata,
        description: impl Into<String>,
    ) -> Self {
        Self {
            parents,
            view,
            metadata,
            description: description.into(),
        }
    }

    /// Returns the preceding operation IDs (>1 only when merging divergent heads).
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    /// Returns the [`View`] content address at the end of this operation.
    pub const fn view(&self) -> ObjectId {
        self.view
    }

    /// Returns the operation metadata.
    pub const fn metadata(&self) -> &OpMetadata {
        &self.metadata
    }

    /// Returns the human-readable description of the operation.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Returns the content-address of this op.
    ///
    /// `OpId = BLAKE3(0x05 || canonical_encoding)`.
    pub fn id(&self) -> ObjectId {
        hash_object(TypeTag::Op, &encode_to_vec(self))
    }
}

impl Encode for Op {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.array_len(self.parents.len());
        for parent in &self.parents {
            encoder.object_id(parent);
        }
        encoder.object_id(&self.view);
        self.metadata.encode(encoder);
        encoder.str(&self.description);
    }
}

impl Decode for Op {
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self> {
        let parent_count = decoder.array_len()?;
        let mut parents = Vec::with_capacity(decoder.reserve_hint(parent_count));
        for _ in 0..parent_count {
            parents.push(decoder.object_id()?);
        }
        let view = decoder.object_id()?;
        let metadata = OpMetadata::decode(decoder)?;
        let description = decoder.str()?.to_owned();
        Ok(Self { parents, view, metadata, description })
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    /// POSIX mode for a regular non-executable file (`DESIGN.md §4`).
    const MODE_FILE: u32 = 0o100_644;
    /// POSIX mode for a directory (`DESIGN.md §4`).
    const MODE_DIR: u32 = 0o040_000;
    /// POSIX mode for a symbolic link (`DESIGN.md §4`).
    const MODE_SYMLINK: u32 = 0o120_000;

    fn zero_id() -> ObjectId {
        ObjectId::from_bytes([0u8; 32])
    }

    fn one_id() -> ObjectId {
        ObjectId::from_bytes([1u8; 32])
    }

    fn two_id() -> ObjectId {
        ObjectId::from_bytes([2u8; 32])
    }

    fn sample_identity() -> Identity {
        Identity::new("Alice", "alice@example.com")
    }

    fn sample_timestamp() -> Timestamp {
        Timestamp::new(1_700_000_000, 60)
    }

    /// Encodes `value`, then decodes via `D::decode`, then re-encodes and
    /// asserts byte-stable round-trip.
    fn assert_round_trip<T: Encode + Decode + PartialEq + std::fmt::Debug>(value: &T) {
        let first_bytes = encode_to_vec(value);
        let mut dec = Decoder::new(&first_bytes);
        let value_back = T::decode(&mut dec).expect("decode should succeed");
        dec.finish().expect("no trailing bytes after decode");
        assert_eq!(value, &value_back, "decoded value differs from original");
        let second_bytes = encode_to_vec(&value_back);
        assert_eq!(
            first_bytes, second_bytes,
            "re-encoded bytes differ from first encoding — not byte-stable"
        );
    }

    // ── Chunk ─────────────────────────────────────────────────────────────────

    #[test]
    fn chunk_empty_round_trips() {
        let chunk = Chunk::new(vec![]);
        assert_round_trip(&chunk);
    }

    #[test]
    fn chunk_data_round_trips() {
        let chunk = Chunk::new(b"hello tack".to_vec());
        assert_round_trip(&chunk);
    }

    #[test]
    fn chunk_id_uses_tag_0x02() {
        let data = b"raw bytes";
        let chunk = Chunk::new(data.to_vec());
        let expected = hash_object(TypeTag::Chunk, data);
        assert_eq!(chunk.id(), expected);
    }

    #[test]
    fn chunk_id_is_deterministic() {
        let a = Chunk::new(b"abc".to_vec());
        let b = Chunk::new(b"abc".to_vec());
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn chunk_id_differs_for_different_data() {
        let a = Chunk::new(b"aaa".to_vec());
        let b = Chunk::new(b"bbb".to_vec());
        assert_ne!(a.id(), b.id());
    }

    // ── ChunkRef ──────────────────────────────────────────────────────────────

    #[test]
    fn chunk_ref_round_trips() {
        let chunk_ref = ChunkRef::new(zero_id(), 1024);
        assert_round_trip(&chunk_ref);
    }

    // ── Blob ──────────────────────────────────────────────────────────────────

    #[test]
    fn blob_empty_chunks_round_trips() {
        let blob = Blob::new(0, vec![]);
        assert_round_trip(&blob);
    }

    #[test]
    fn blob_with_chunks_round_trips() {
        let blob = Blob::new(
            2048,
            vec![
                ChunkRef::new(zero_id(), 1024),
                ChunkRef::new(one_id(), 1024),
            ],
        );
        assert_round_trip(&blob);
    }

    #[test]
    fn blob_id_uses_tag_0x01() {
        let blob = Blob::new(0, vec![]);
        let encoded = encode_to_vec(&blob);
        let expected = hash_object(TypeTag::Blob, &encoded);
        assert_eq!(blob.id(), expected);
    }

    #[test]
    fn blob_id_is_deterministic() {
        let a = Blob::new(512, vec![ChunkRef::new(zero_id(), 512)]);
        let b = Blob::new(512, vec![ChunkRef::new(zero_id(), 512)]);
        assert_eq!(a.id(), b.id());
    }

    // ── EntryKind ─────────────────────────────────────────────────────────────

    #[test]
    fn entry_kind_round_trips() {
        for kind in [EntryKind::Blob, EntryKind::Tree, EntryKind::Symlink] {
            assert_round_trip(&kind);
        }
    }

    #[test]
    fn entry_kind_from_unknown_byte_is_err() {
        let mut dec = Decoder::new(&[0xff]);
        let result = EntryKind::decode(&mut dec);
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "expected Corruption for unknown kind byte, got {result:?}"
        );
    }

    // ── TreeEntry ─────────────────────────────────────────────────────────────

    #[test]
    fn tree_entry_round_trips() {
        let entry = tree_entry("foo.txt", 0o100_644, EntryKind::Blob, zero_id())
            .expect("valid name");
        assert_round_trip(&entry);
    }

    // ── Tree sort rule ────────────────────────────────────────────────────────

    /// DESIGN.md §4: entries are sorted bytewise on name, with a trailing '/'
    /// appended to directory names **for comparison only**.
    ///
    /// Key test cases from the spec:
    ///
    /// 1. File `foo` vs dir `foo` — dir sorts AFTER file because `"foo/"` > `"foo"`.
    /// 2. File `foo.txt` vs dir `foo` — `"foo.txt"` > `"foo/"` because `'.'` (0x2e)
    ///    > `'/'` (0x2f) is false: `'.'` is 0x2e, `'/'` is 0x2f, so `"foo."` <
    ///    > `"foo/"`, meaning `foo.txt` sorts BEFORE `foo/` (dir).
    ///
    /// Let's verify: `'.'` = 0x2e, `'/'` = 0x2f. So `"foo." < "foo/"`. Therefore
    /// `foo.txt` sorts before dir `foo`.
    #[test]
    fn tree_sort_file_foo_before_dir_foo() -> Result<()> {
        // file "foo" should sort before dir "foo" (compare "foo" < "foo/")
        let file_foo = tree_entry("foo", 0o100_644, EntryKind::Blob, zero_id())?;
        let dir_foo = tree_entry("foo", 0o040_000, EntryKind::Tree, one_id())?;
        let tree = Tree::new(vec![dir_foo, file_foo])?;
        assert_eq!(tree.entries()[0].name(), "foo");
        assert_eq!(tree.entries()[0].kind(), EntryKind::Blob, "file foo should be first");
        assert_eq!(tree.entries()[1].kind(), EntryKind::Tree, "dir foo should be second");
        Ok(())
    }

    #[test]
    fn tree_sort_foo_txt_before_dir_foo() -> Result<()> {
        // "foo.txt" key = "foo.txt" (0x66 0x6f 0x6f 0x2e 0x74 0x78 0x74)
        // dir "foo" key = "foo/"    (0x66 0x6f 0x6f 0x2f)
        // Compare byte 3: '.' (0x2e) < '/' (0x2f) -> foo.txt sorts BEFORE dir foo
        let file_foo_txt = tree_entry("foo.txt", 0o100_644, EntryKind::Blob, zero_id())?;
        let dir_foo = tree_entry("foo", 0o040_000, EntryKind::Tree, one_id())?;
        let tree = Tree::new(vec![dir_foo, file_foo_txt])?;
        assert_eq!(tree.entries()[0].name(), "foo.txt", "foo.txt should be first");
        assert_eq!(tree.entries()[1].name(), "foo", "dir foo should be second");
        Ok(())
    }

    #[test]
    fn tree_sort_is_bytewise_ascending() -> Result<()> {
        // Plain alphabetical order plus mixed kinds.
        let entry_a = tree_entry("alpha", 0o100_644, EntryKind::Blob, zero_id())?;
        let entry_b = tree_entry("beta", 0o040_000, EntryKind::Tree, one_id())?;
        let entry_c = tree_entry("gamma", 0o100_644, EntryKind::Blob, two_id())?;
        // Insert in reverse order; constructor must sort.
        let tree = Tree::new(vec![entry_c, entry_a, entry_b])?;
        let names: Vec<&str> = tree.entries().iter().map(TreeEntry::name).collect();
        // "alpha" < "beta/" < "gamma": 'a'=0x61 < 'b'=0x62 < 'g'=0x67
        assert_eq!(names, ["alpha", "beta", "gamma"]);
        Ok(())
    }

    // ── Tree name validation ───────────────────────────────────────────────────

    #[test]
    fn tree_rejects_empty_name() {
        let entry = TreeEntry {
            name: String::new(),
            mode: MODE_FILE,
            kind: EntryKind::Blob,
            id: zero_id(),
        };
        let result = Tree::new(vec![entry]);
        assert!(
            matches!(result, Err(Error::InvalidTreeEntryName(_))),
            "expected InvalidTreeEntryName for empty name"
        );
    }

    #[test]
    fn tree_rejects_dot() {
        let entry = TreeEntry {
            name: ".".to_owned(),
            mode: MODE_FILE,
            kind: EntryKind::Blob,
            id: zero_id(),
        };
        let result = Tree::new(vec![entry]);
        assert!(matches!(result, Err(Error::InvalidTreeEntryName(_))));
    }

    #[test]
    fn tree_rejects_dotdot() {
        let entry = TreeEntry {
            name: "..".to_owned(),
            mode: MODE_FILE,
            kind: EntryKind::Blob,
            id: zero_id(),
        };
        let result = Tree::new(vec![entry]);
        assert!(matches!(result, Err(Error::InvalidTreeEntryName(_))));
    }

    #[test]
    fn tree_rejects_name_with_forward_slash() {
        let entry = TreeEntry {
            name: "foo/bar".to_owned(),
            mode: MODE_FILE,
            kind: EntryKind::Blob,
            id: zero_id(),
        };
        let result = Tree::new(vec![entry]);
        assert!(matches!(result, Err(Error::InvalidTreeEntryName(_))));
    }

    #[test]
    fn tree_rejects_name_with_backslash() {
        let entry = TreeEntry {
            name: "foo\\bar".to_owned(),
            mode: MODE_FILE,
            kind: EntryKind::Blob,
            id: zero_id(),
        };
        let result = Tree::new(vec![entry]);
        assert!(matches!(result, Err(Error::InvalidTreeEntryName(_))));
    }

    /// Hand-encodes a single-entry tree carrying `name` (bypassing `Tree::new`'s
    /// validation) so the decode path can be exercised against a hostile name.
    fn encode_tree_with_raw_name(name: &str) -> Vec<u8> {
        let mut enc = Encoder::new();
        enc.array_len(1);
        // One TreeEntry: name, mode, kind, id.
        enc.str(name);
        enc.u32(MODE_FILE);
        EntryKind::Blob.encode(&mut enc);
        enc.object_id(&zero_id());
        enc.into_bytes()
    }

    /// Regression (findings 1 & 2): a crafted tree whose decoded entry name is a
    /// traversal component (`".."`) must be rejected at the READ trust boundary,
    /// not silently trusted. Before the fix `Tree::decode` called
    /// `from_entries_unchecked` and accepted any name, letting it flow into the
    /// `materialize`/projection sinks.
    #[test]
    fn decode_rejects_dotdot_name_as_corruption() {
        let bytes = encode_tree_with_raw_name("..");
        let mut dec = Decoder::new(&bytes);
        let result = Tree::decode(&mut dec);
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "decode of a `..` entry name must be Corruption, got {result:?}"
        );
    }

    #[test]
    fn decode_rejects_separator_name_as_corruption() {
        for hostile in ["a/b", "a\\b", ".", ""] {
            let bytes = encode_tree_with_raw_name(hostile);
            let mut dec = Decoder::new(&bytes);
            let result = Tree::decode(&mut dec);
            assert!(
                matches!(result, Err(Error::Corruption(_))),
                "decode of name {hostile:?} must be Corruption, got {result:?}"
            );
        }
    }

    #[test]
    fn decode_accepts_legal_name() {
        // A normal name must still round-trip through decode unharmed.
        let bytes = encode_tree_with_raw_name("ok.txt");
        let mut dec = Decoder::new(&bytes);
        let tree = Tree::decode(&mut dec).expect("legal name decodes");
        assert_eq!(tree.entries()[0].name(), "ok.txt");
    }

    #[test]
    fn decode_bounds_array_capacity_against_remaining() {
        // A crafted object that claims a gigantic element count but supplies no
        // element bytes must fail cleanly with an error — never abort the process
        // via a speculative `Vec::with_capacity(huge)`. The decode loop's first
        // element read hits the end of the buffer and returns Truncated.
        let huge: usize = 1 << 40; // ~1 TiB of "elements"

        // Blob: u64 total_len, then a varint chunk count, then no chunks.
        let mut enc = Encoder::new();
        enc.u64(0);
        enc.array_len(huge);
        let bytes = enc.into_bytes();
        assert!(
            Blob::decode(&mut Decoder::new(&bytes)).is_err(),
            "a huge chunk count with no chunk bytes must error, not abort"
        );

        // Tree: a varint entry count, then no entries.
        let mut enc = Encoder::new();
        enc.array_len(huge);
        let bytes = enc.into_bytes();
        assert!(
            Tree::decode(&mut Decoder::new(&bytes)).is_err(),
            "a huge entry count with no entry bytes must error, not abort"
        );
    }

    #[test]
    fn tree_round_trips() -> Result<()> {
        let entries = vec![
            tree_entry("alpha", MODE_FILE, EntryKind::Blob, zero_id())?,
            tree_entry("beta", MODE_DIR, EntryKind::Tree, one_id())?,
            tree_entry("gamma", MODE_SYMLINK, EntryKind::Symlink, two_id())?,
        ];
        let tree = Tree::new(entries)?;
        assert_round_trip(&tree);
        Ok(())
    }

    #[test]
    fn tree_id_uses_tag_0x03() -> Result<()> {
        let tree = Tree::new(vec![])?;
        let encoded = encode_to_vec(&tree);
        let expected = hash_object(TypeTag::Tree, &encoded);
        assert_eq!(tree.id(), expected);
        Ok(())
    }

    // ── id() type-tag isolation ────────────────────────────────────────────────

    /// Two objects whose canonical payloads coincidentally match must still get
    /// different IDs because the type tag differs (DESIGN.md §2).
    #[test]
    fn id_differs_between_blob_and_tree_with_equal_payloads() -> Result<()> {
        // Empty blob and empty tree both encode to the same bytes (both are just
        // a u64 zero + varint zero, or just a varint zero for the array). Build
        // them so their encoded payloads are equal and verify ids differ.
        let blob = Blob::new(0, vec![]);
        let tree = Tree::new(vec![])?;

        // They likely won't have equal payloads, but even if they did the ids
        // must differ. Let's just confirm the type-tag separation works:
        let blob_id = hash_object(TypeTag::Blob, &encode_to_vec(&blob));
        let tree_id = hash_object(TypeTag::Tree, &encode_to_vec(&tree));
        assert_ne!(blob_id, tree_id, "type-tag separation must produce different ids");
        Ok(())
    }

    #[test]
    fn id_stability_same_content_same_id() {
        let chunk_a = Chunk::new(b"stable".to_vec());
        let chunk_b = Chunk::new(b"stable".to_vec());
        assert_eq!(chunk_a.id(), chunk_b.id());
    }

    #[test]
    fn id_stability_different_content_different_id() {
        let chunk_a = Chunk::new(b"aaa".to_vec());
        let chunk_b = Chunk::new(b"bbb".to_vec());
        assert_ne!(chunk_a.id(), chunk_b.id());
    }

    // ── Identity ──────────────────────────────────────────────────────────────

    #[test]
    fn identity_round_trips() {
        let id = sample_identity();
        assert_round_trip(&id);
    }

    #[test]
    fn identity_empty_fields_round_trip() {
        let id = Identity::new("", "");
        assert_round_trip(&id);
    }

    #[test]
    fn identity_email_returns_email() {
        let id = Identity::new("Alice", "alice@example.com");
        assert_eq!(id.email(), "alice@example.com");
    }

    // ── Timestamp ─────────────────────────────────────────────────────────────

    #[test]
    fn timestamp_round_trips() {
        let ts = sample_timestamp();
        assert_round_trip(&ts);
    }

    #[test]
    fn timestamp_negative_unix_secs_round_trips() {
        let ts = Timestamp::new(-86400, -480); // UTC-8
        assert_round_trip(&ts);
    }

    // ── Snapshot ──────────────────────────────────────────────────────────────

    fn sample_snapshot() -> Snapshot {
        Snapshot::new(
            zero_id(),
            vec![one_id()],
            two_id(),
            sample_identity(),
            Identity::new("Bot", "bot@ci.example.com"),
            "initial snapshot",
            sample_timestamp(),
        )
    }

    #[test]
    fn snapshot_round_trips() {
        assert_round_trip(&sample_snapshot());
    }

    #[test]
    fn snapshot_root_no_parents_round_trips() {
        let snap = Snapshot::new(
            zero_id(),
            vec![],
            one_id(),
            sample_identity(),
            sample_identity(),
            "",
            sample_timestamp(),
        );
        assert_round_trip(&snap);
    }

    #[test]
    fn snapshot_id_uses_tag_0x04() {
        let snap = sample_snapshot();
        let encoded = encode_to_vec(&snap);
        let expected = hash_object(TypeTag::Snapshot, &encoded);
        assert_eq!(snap.id(), expected);
    }

    #[test]
    fn snapshot_id_is_deterministic() {
        let a = sample_snapshot();
        let b = sample_snapshot();
        assert_eq!(a.id(), b.id());
    }

    // ── NamedRef ──────────────────────────────────────────────────────────────

    #[test]
    fn named_ref_round_trips() {
        let nr = NamedRef::new("main", zero_id());
        assert_round_trip(&nr);
    }

    #[test]
    fn named_ref_target_returns_target() {
        let nr = NamedRef::new("main", one_id());
        assert_eq!(nr.target(), one_id());
    }

    // ── View ──────────────────────────────────────────────────────────────────

    fn sample_view() -> View {
        View::new(
            zero_id(),
            vec![
                NamedRef::new("main", one_id()),
                NamedRef::new("dev", two_id()),
            ],
            vec![NamedRef::new("v1.0", zero_id())],
            vec![two_id(), one_id()],
        )
    }

    #[test]
    fn view_round_trips() {
        assert_round_trip(&sample_view());
    }

    #[test]
    fn view_empty_round_trips() {
        let view = View::new(zero_id(), vec![], vec![], vec![]);
        assert_round_trip(&view);
    }

    #[test]
    fn view_bookmarks_sorted_by_name() {
        let view = View::new(
            zero_id(),
            vec![
                NamedRef::new("zzz", one_id()),
                NamedRef::new("aaa", two_id()),
            ],
            vec![],
            vec![],
        );
        assert_eq!(view.bookmarks()[0].name(), "aaa");
        assert_eq!(view.bookmarks()[1].name(), "zzz");
    }

    #[test]
    fn view_tags_sorted_by_name() {
        let view = View::new(
            zero_id(),
            vec![],
            vec![
                NamedRef::new("v2.0", one_id()),
                NamedRef::new("v1.0", two_id()),
            ],
            vec![],
        );
        assert_eq!(view.tags()[0].name(), "v1.0");
        assert_eq!(view.tags()[1].name(), "v2.0");
    }

    #[test]
    fn view_heads_sorted() {
        let view = View::new(zero_id(), vec![], vec![], vec![two_id(), one_id(), zero_id()]);
        // Sorted by byte representation: [0;32] < [1;32] < [2;32]
        assert_eq!(view.heads()[0], zero_id());
        assert_eq!(view.heads()[1], one_id());
        assert_eq!(view.heads()[2], two_id());
    }

    #[test]
    fn view_id_uses_tag_0x06() {
        let view = sample_view();
        let encoded = encode_to_vec(&view);
        let expected = hash_object(TypeTag::View, &encoded);
        assert_eq!(view.id(), expected);
    }

    // ── OpMetadata ────────────────────────────────────────────────────────────

    fn sample_metadata() -> OpMetadata {
        OpMetadata::new(
            Timestamp::new(1_700_000_000, 0),
            Timestamp::new(1_700_000_001, 0),
            "machine-01",
            "alice",
            vec!["tack".to_owned(), "snap".to_owned(), "-m".to_owned(), "msg".to_owned()],
        )
    }

    #[test]
    fn op_metadata_round_trips() {
        assert_round_trip(&sample_metadata());
    }

    #[test]
    fn op_metadata_command_returns_argv() {
        let meta = sample_metadata();
        assert_eq!(
            meta.command(),
            ["tack".to_owned(), "snap".to_owned(), "-m".to_owned(), "msg".to_owned()]
        );
    }

    // ── Op ────────────────────────────────────────────────────────────────────

    fn sample_op() -> Op {
        Op::new(
            vec![zero_id()],
            one_id(),
            sample_metadata(),
            "snapshot working copy",
        )
    }

    #[test]
    fn op_round_trips() {
        assert_round_trip(&sample_op());
    }

    #[test]
    fn op_root_no_parents_round_trips() {
        let op = Op::new(vec![], zero_id(), sample_metadata(), "init");
        assert_round_trip(&op);
    }

    #[test]
    fn op_id_uses_tag_0x05() {
        let op = sample_op();
        let encoded = encode_to_vec(&op);
        let expected = hash_object(TypeTag::Op, &encoded);
        assert_eq!(op.id(), expected);
    }

    #[test]
    fn op_id_is_deterministic() {
        let a = sample_op();
        let b = sample_op();
        assert_eq!(a.id(), b.id());
    }

    // ── proptest: byte-stable round-trips ─────────────────────────────────────

    use proptest::prelude::*;

    prop_compose! {
        fn arb_object_id()(bytes: [u8; 32]) -> ObjectId {
            ObjectId::from_bytes(bytes)
        }
    }

    prop_compose! {
        fn arb_chunk()(data: Vec<u8>) -> Chunk {
            Chunk::new(data)
        }
    }

    prop_compose! {
        fn arb_chunk_ref()(id in arb_object_id(), len: u32) -> ChunkRef {
            ChunkRef::new(id, len)
        }
    }

    prop_compose! {
        fn arb_blob()(
            total_len: u64,
            chunks in prop::collection::vec(arb_chunk_ref(), 0..8),
        ) -> Blob {
            Blob::new(total_len, chunks)
        }
    }

    prop_compose! {
        fn arb_identity()(name: String, email: String) -> Identity {
            Identity::new(name, email)
        }
    }

    prop_compose! {
        fn arb_timestamp()(unix_secs: i64, tz_offset_mins: i32) -> Timestamp {
            Timestamp::new(unix_secs, tz_offset_mins)
        }
    }

    prop_compose! {
        fn arb_snapshot()(
            root_tree in arb_object_id(),
            parents in prop::collection::vec(arb_object_id(), 0..4),
            change_id in arb_object_id(),
            author in arb_identity(),
            committer in arb_identity(),
            message: String,
            timestamp in arb_timestamp(),
        ) -> Snapshot {
            Snapshot::new(root_tree, parents, change_id, author, committer, message, timestamp)
        }
    }

    prop_compose! {
        fn arb_named_ref()(name: String, target in arb_object_id()) -> NamedRef {
            NamedRef::new(name, target)
        }
    }

    prop_compose! {
        fn arb_view()(
            working_copy in arb_object_id(),
            bookmarks in prop::collection::vec(arb_named_ref(), 0..4),
            tags in prop::collection::vec(arb_named_ref(), 0..4),
            heads in prop::collection::vec(arb_object_id(), 0..4),
        ) -> View {
            View::new(working_copy, bookmarks, tags, heads)
        }
    }

    prop_compose! {
        fn arb_op_metadata()(
            start in arb_timestamp(),
            end in arb_timestamp(),
            hostname: String,
            username: String,
            command in prop::collection::vec(any::<String>(), 0..4),
        ) -> OpMetadata {
            OpMetadata::new(start, end, hostname, username, command)
        }
    }

    prop_compose! {
        fn arb_op()(
            parents in prop::collection::vec(arb_object_id(), 0..4),
            view in arb_object_id(),
            metadata in arb_op_metadata(),
            description: String,
        ) -> Op {
            Op::new(parents, view, metadata, description)
        }
    }

    proptest! {
        #[test]
        fn prop_chunk_round_trip(chunk in arb_chunk()) {
            let bytes = encode_to_vec(&chunk);
            let mut dec = Decoder::new(&bytes);
            let decoded = Chunk::decode(&mut dec).expect("decode");
            dec.finish().expect("no trailing bytes");
            prop_assert_eq!(&chunk, &decoded);
            prop_assert_eq!(bytes, encode_to_vec(&decoded));
        }

        #[test]
        fn prop_blob_round_trip(blob in arb_blob()) {
            let bytes = encode_to_vec(&blob);
            let mut dec = Decoder::new(&bytes);
            let decoded = Blob::decode(&mut dec).expect("decode");
            dec.finish().expect("no trailing bytes");
            prop_assert_eq!(&blob, &decoded);
            prop_assert_eq!(bytes, encode_to_vec(&decoded));
        }

        #[test]
        fn prop_snapshot_round_trip(snap in arb_snapshot()) {
            let bytes = encode_to_vec(&snap);
            let mut dec = Decoder::new(&bytes);
            let decoded = Snapshot::decode(&mut dec).expect("decode");
            dec.finish().expect("no trailing bytes");
            prop_assert_eq!(&snap, &decoded);
            prop_assert_eq!(bytes, encode_to_vec(&decoded));
        }

        #[test]
        fn prop_view_round_trip(view in arb_view()) {
            let bytes = encode_to_vec(&view);
            let mut dec = Decoder::new(&bytes);
            let decoded = View::decode(&mut dec).expect("decode");
            dec.finish().expect("no trailing bytes");
            prop_assert_eq!(&view, &decoded);
            prop_assert_eq!(bytes, encode_to_vec(&decoded));
        }

        #[test]
        fn prop_op_round_trip(op in arb_op()) {
            let bytes = encode_to_vec(&op);
            let mut dec = Decoder::new(&bytes);
            let decoded = Op::decode(&mut dec).expect("decode");
            dec.finish().expect("no trailing bytes");
            prop_assert_eq!(&op, &decoded);
            prop_assert_eq!(bytes, encode_to_vec(&decoded));
        }

        #[test]
        fn prop_chunk_id_stable(data: Vec<u8>) {
            let a = Chunk::new(data.clone());
            let b = Chunk::new(data);
            prop_assert_eq!(a.id(), b.id());
        }

        #[test]
        fn prop_blob_id_stable(blob in arb_blob()) {
            let a = blob.clone();
            prop_assert_eq!(a.id(), blob.id());
        }
    }
}
