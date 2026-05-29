//! File-bytes ⇄ [`Blob`] ⇄ [`Chunk`] assembly (`DESIGN.md §4`, §5).
//!
//! A file's content is stored as a list of [`Chunk`] objects (split with
//! `FastCDC` — see [`chunker`](crate::chunker)) plus one [`Blob`] object that
//! records the ordered chunk references and the total length. Identical files
//! dedup at the blob level; files sharing byte regions dedup at the chunk
//! level, automatically, because [`ObjectStore::put_chunk`] is content-addressed
//! and idempotent.
//!
//! Files no larger than [`MIN_CHUNK`](crate::chunker::MIN_CHUNK) become a
//! single-chunk blob — there is no special small-file object, keeping the
//! model uniform. An empty file becomes a zero-chunk blob with `total_len = 0`.
//!
//! [`read_blob_range`] fetches only the chunks overlapping the requested byte
//! range, which is what the future `ProjFS` lazy hydration path needs to serve a
//! partial read without materializing the whole file.

use crate::chunker::chunk_ranges;
use crate::error::{Error, Result};
use crate::hash::ObjectId;
use crate::object::{Blob, Chunk, ChunkRef};
use crate::store::ObjectStore;

/// Stores `bytes` as chunk objects plus a [`Blob`] chunk-list, returning the
/// `BlobId`.
///
/// The bytes are split with `FastCDC`, each chunk is stored (deduplicating
/// automatically), and a `Blob` referencing them in order is stored. A file no
/// larger than [`MIN_CHUNK`](crate::chunker::MIN_CHUNK) yields a single chunk;
/// an empty input yields a zero-chunk blob.
///
/// # Errors
///
/// Returns [`Error::Io`] if any underlying store write fails.
pub fn store_file_bytes(store: &ObjectStore, bytes: &[u8]) -> Result<ObjectId> {
    let mut chunk_refs = Vec::new();
    for range in chunk_ranges(bytes) {
        let chunk = Chunk::new(bytes[range].to_vec());
        let id = store.put_chunk(&chunk)?;
        // A single chunk never exceeds MAX_CHUNK (64 KiB), so the length fits a
        // u32 with room to spare.
        let len = u32::try_from(chunk.data().len())
            .map_err(|_| Error::Corruption("chunk length exceeds u32".to_string()))?;
        chunk_refs.push(ChunkRef::new(id, len));
    }

    let total_len = bytes.len() as u64;
    let blob = Blob::new(total_len, chunk_refs);
    store.put_blob(&blob)
}

/// Reassembles and returns the full content of the [`Blob`] at `blob_id`.
///
/// # Errors
///
/// - [`Error::ObjectNotFound`] if the blob or any referenced chunk is missing.
/// - [`Error::Corruption`] if reassembled bytes do not match the recorded
///   `total_len`, or any object fails store-level verification.
/// - [`Error::Io`] on read failure.
pub fn read_blob(store: &ObjectStore, blob_id: &ObjectId) -> Result<Vec<u8>> {
    let blob = store.get_blob(blob_id)?;
    let total = usize::try_from(blob.total_len())
        .map_err(|_| Error::Corruption("blob total_len exceeds usize".to_string()))?;

    // `total_len` is an attacker/corruption-controlled field: the store's
    // re-hash-on-read proves the bytes match the id, NOT that `total_len` agrees
    // with the chunk list. Pre-allocating `Vec::with_capacity(total)` straight
    // from it lets a crafted blob (e.g. `total_len = 1 << 40`, no chunks) abort
    // the whole process with an allocation failure before the integrity check
    // below can run. Validate `total_len` against the summed chunk lengths first
    // and only allocate the proven size, so a lie is a recoverable `Corruption`.
    let mut chunk_total: usize = 0;
    for chunk_ref in blob.chunks() {
        chunk_total = chunk_total
            .checked_add(chunk_ref.len() as usize)
            .ok_or_else(|| {
                Error::Corruption(format!("blob {} chunk lengths overflow usize", blob_id.short()))
            })?;
    }
    if chunk_total != total {
        return Err(Error::Corruption(format!(
            "blob {} records total_len {} but its chunks sum to {}",
            blob_id.short(),
            total,
            chunk_total
        )));
    }

    let mut out = Vec::with_capacity(total);
    for chunk_ref in blob.chunks() {
        let chunk = store.get_chunk(&chunk_ref.id())?;
        out.extend_from_slice(chunk.data());
    }

    if out.len() != total {
        return Err(Error::Corruption(format!(
            "blob {} reassembled to {} bytes, expected {}",
            blob_id.short(),
            out.len(),
            total
        )));
    }
    Ok(out)
}

/// Reads `len` bytes starting at `offset` from the [`Blob`] at `blob_id`,
/// fetching only the chunks that overlap the requested range.
///
/// The returned vector is shorter than `len` only when the range runs past the
/// end of the file; reading at or beyond the end yields an empty vector. This
/// is the partial-read primitive used by lazy filesystem hydration.
///
/// # Errors
///
/// - [`Error::ObjectNotFound`] if the blob or a needed chunk is missing.
/// - [`Error::Corruption`] if a chunk's recorded length disagrees with its
///   actual bytes, or `total_len` overflows `usize`.
/// - [`Error::Io`] on read failure.
pub fn read_blob_range(
    store: &ObjectStore,
    blob_id: &ObjectId,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>> {
    let blob = store.get_blob(blob_id)?;
    let total = blob.total_len();

    // A read starting at or past EOF, or a zero-length read, returns nothing.
    if len == 0 || offset >= total {
        return Ok(Vec::new());
    }

    // Clamp the requested window to the file's actual extent.
    let want_end = offset.saturating_add(len as u64).min(total);
    let out_len = usize::try_from(want_end - offset)
        .map_err(|_| Error::Corruption("read range exceeds usize".to_string()))?;
    let mut out = Vec::with_capacity(out_len);

    // Walk chunks, tracking each chunk's absolute byte span, and copy only the
    // portion of each overlapping chunk that falls inside [offset, want_end).
    let mut chunk_start: u64 = 0;
    for chunk_ref in blob.chunks() {
        let chunk_len = u64::from(chunk_ref.len());
        let chunk_end = chunk_start + chunk_len;

        // Skip chunks entirely before the window; stop once past it.
        if chunk_end <= offset {
            chunk_start = chunk_end;
            continue;
        }
        if chunk_start >= want_end {
            break;
        }

        let chunk = store.get_chunk(&chunk_ref.id())?;
        // Defend against a Blob whose ChunkRef.len disagrees with the stored
        // chunk; otherwise the slice maths below could panic.
        if chunk.data().len() as u64 != chunk_len {
            return Err(Error::Corruption(format!(
                "chunk {} length {} disagrees with blob record {}",
                chunk_ref.id().short(),
                chunk.data().len(),
                chunk_len
            )));
        }

        // Intersection of [chunk_start, chunk_end) and [offset, want_end),
        // expressed relative to this chunk's start.
        let copy_from = offset.saturating_sub(chunk_start);
        let copy_to = (want_end - chunk_start).min(chunk_len);
        // Both bounds are < chunk_len <= MAX_CHUNK, so the conversions cannot
        // fail in practice; treat any overflow as corruption rather than panic.
        let from = usize::try_from(copy_from)
            .map_err(|_| Error::Corruption("chunk copy-from offset exceeds usize".to_string()))?;
        let to = usize::try_from(copy_to)
            .map_err(|_| Error::Corruption("chunk copy-to offset exceeds usize".to_string()))?;
        out.extend_from_slice(&chunk.data()[from..to]);

        chunk_start = chunk_end;
    }

    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::MIN_CHUNK;
    use tempfile::TempDir;

    fn temp_store() -> (TempDir, ObjectStore) {
        let dir = TempDir::new().expect("temp dir");
        let store = ObjectStore::init(dir.path().join(".tack")).expect("init store");
        (dir, store)
    }

    /// Deterministic pseudo-random byte generator (xorshift) — reproducible
    /// across runs without an RNG dependency.
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.push((state & 0xff) as u8);
        }
        out
    }

    // ── store/read round-trips ────────────────────────────────────────────────

    #[test]
    fn empty_file_round_trips() -> Result<()> {
        let (_dir, store) = temp_store();
        let id = store_file_bytes(&store, &[])?;
        let back = read_blob(&store, &id)?;
        assert!(back.is_empty());
        // An empty file is a zero-chunk blob.
        assert_eq!(store.get_blob(&id)?.chunks().len(), 0);
        Ok(())
    }

    #[test]
    fn sub_min_chunk_file_is_single_chunk() -> Result<()> {
        let (_dir, store) = temp_store();
        let bytes = pseudo_random(MIN_CHUNK / 2, 1);
        let id = store_file_bytes(&store, &bytes)?;
        assert_eq!(store.get_blob(&id)?.chunks().len(), 1);
        assert_eq!(read_blob(&store, &id)?, bytes);
        Ok(())
    }

    #[test]
    fn multi_mb_file_round_trips() -> Result<()> {
        let (_dir, store) = temp_store();
        let bytes = pseudo_random(3 * 1024 * 1024, 0xcafe);
        let id = store_file_bytes(&store, &bytes)?;
        let blob = store.get_blob(&id)?;
        assert!(blob.chunks().len() > 1, "multi-MiB file should be many chunks");
        assert_eq!(read_blob(&store, &id)?, bytes);
        Ok(())
    }

    #[test]
    fn identical_files_dedup_at_blob_level() -> Result<()> {
        let (_dir, store) = temp_store();
        let bytes = pseudo_random(512 * 1024, 7);
        let id1 = store_file_bytes(&store, &bytes)?;
        let id2 = store_file_bytes(&store, &bytes)?;
        assert_eq!(id1, id2, "identical files must produce the same BlobId");
        Ok(())
    }

    #[test]
    fn identical_chunks_stored_once() -> Result<()> {
        let (_dir, store) = temp_store();
        // Two files made of the same repeated block share chunks.
        let block = pseudo_random(256 * 1024, 99);
        let id1 = store_file_bytes(&store, &block)?;
        let _id2 = store_file_bytes(&store, &block)?;
        // Every chunk referenced by the blob exists exactly once (content
        // addressing). Re-storing the same chunk is a no-op; verify each chunk
        // referenced is present and that re-putting returns the same id.
        let blob = store.get_blob(&id1)?;
        for chunk_ref in blob.chunks() {
            let chunk = store.get_chunk(&chunk_ref.id())?;
            assert_eq!(store.put_chunk(&chunk)?, chunk_ref.id());
        }
        Ok(())
    }

    #[test]
    fn shared_prefix_dedups_chunks() -> Result<()> {
        let (_dir, store) = temp_store();
        // File B = file A with a small suffix appended. FastCDC should keep most
        // leading chunk boundaries identical, so the leading chunks dedup.
        let a = pseudo_random(1024 * 1024, 0x5151);
        let mut b = a.clone();
        b.extend_from_slice(&pseudo_random(4096, 0x6262));

        let id_a = store_file_bytes(&store, &a)?;
        let id_b = store_file_bytes(&store, &b)?;
        let blob_a = store.get_blob(&id_a)?;
        let blob_b = store.get_blob(&id_b)?;

        let a_ids: std::collections::HashSet<_> =
            blob_a.chunks().iter().map(ChunkRef::id).collect();
        let shared = blob_b
            .chunks()
            .iter()
            .filter(|c| a_ids.contains(&c.id()))
            .count();
        assert!(shared > 0, "files sharing a prefix should share chunks");
        Ok(())
    }

    // ── read_blob_range ───────────────────────────────────────────────────────

    #[test]
    fn range_matches_full_read_at_edges() -> Result<()> {
        let (_dir, store) = temp_store();
        let bytes = pseudo_random(2 * 1024 * 1024 + 123, 0x9090);
        let id = store_file_bytes(&store, &bytes)?;
        let full = read_blob(&store, &id)?;
        assert_eq!(full, bytes);

        let total = bytes.len();
        // Edge offsets: start, near a chunk boundary, end, past end.
        let cases: &[(u64, usize)] = &[
            (0, 1),
            (0, total),
            (0, MIN_CHUNK),
            (1, 100),
            ((total / 2) as u64, 4096),
            ((total - 1) as u64, 1),
            ((total - 10) as u64, 10),
            ((total - 10) as u64, 1000), // runs past end -> clamps
            (total as u64, 50),          // at EOF -> empty
            ((total + 100) as u64, 50),  // past EOF -> empty
            (12345, 0),                  // zero-length -> empty
        ];
        for &(offset, len) in cases {
            let got = read_blob_range(&store, &id, offset, len)?;
            let start = usize::try_from(offset).expect("offset fits usize").min(total);
            let end = (start + len).min(total);
            let expected = &full[start..end];
            assert_eq!(
                got, expected,
                "range mismatch at offset={offset} len={len}"
            );
        }
        Ok(())
    }

    #[test]
    fn random_ranges_match_full_read() -> Result<()> {
        let (_dir, store) = temp_store();
        let bytes = pseudo_random(1024 * 1024 + 777, 0x3141);
        let id = store_file_bytes(&store, &bytes)?;
        let full = read_blob(&store, &id)?;
        let total = full.len();

        // Deterministic "random" offset/len pairs via the same xorshift.
        let mut state: u64 = 0xfeed_face;
        for _ in 0..200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let offset = (state % (total as u64 + 16)) as u64;
            let len = (state >> 20) as usize % (total + 16);
            let got = read_blob_range(&store, &id, offset, len)?;
            let start = usize::try_from(offset).expect("offset fits usize").min(total);
            let end = (start + len).min(total);
            assert_eq!(got, &full[start..end], "offset={offset} len={len}");
        }
        Ok(())
    }

    #[test]
    fn range_on_empty_blob_is_empty() -> Result<()> {
        let (_dir, store) = temp_store();
        let id = store_file_bytes(&store, &[])?;
        assert!(read_blob_range(&store, &id, 0, 100)?.is_empty());
        Ok(())
    }

    // ── DoS: oversized total_len (finding 3) ──────────────────────────────────

    /// Regression (finding 3): a crafted blob whose `total_len` is enormous but
    /// whose chunk list is empty must return a recoverable `Error::Corruption`,
    /// NOT pre-allocate `Vec::with_capacity(total_len)` and abort the process.
    /// `total_len` is validated against the summed chunk lengths before any
    /// allocation, so the lie is caught cheaply.
    #[test]
    fn read_blob_rejects_oversized_total_len() -> Result<()> {
        use crate::encoding::{Encode as _, Encoder};
        use crate::hash::TypeTag;
        use crate::object::Blob;

        let (_dir, store) = temp_store();
        // total_len = 1 TiB, but zero chunks → reassembly can never reach it and a
        // naive Vec::with_capacity(1<<40) would abort on Windows.
        let evil = Blob::new(1 << 40, Vec::new());
        let mut enc = Encoder::new();
        evil.encode(&mut enc);
        let id = store.put_raw(TypeTag::Blob, enc.as_bytes())?;

        let result = read_blob(&store, &id);
        assert!(
            matches!(result, Err(Error::Corruption(_))),
            "oversized total_len must be Corruption, got {result:?}"
        );
        Ok(())
    }

    /// A blob whose chunks sum to less than its `total_len` is likewise rejected
    /// before allocating the inflated size.
    #[test]
    fn read_blob_rejects_total_len_chunk_mismatch() -> Result<()> {
        use crate::encoding::{Encode as _, Encoder};
        use crate::hash::TypeTag;
        use crate::object::{Blob, ChunkRef};

        let (_dir, store) = temp_store();
        // Store one real 3-byte chunk, then claim total_len = 1 GiB.
        let real_chunk = crate::object::Chunk::new(b"abc".to_vec());
        let chunk_id = store.put_chunk(&real_chunk)?;
        let evil = Blob::new(1 << 30, vec![ChunkRef::new(chunk_id, 3)]);
        let mut enc = Encoder::new();
        evil.encode(&mut enc);
        let id = store.put_raw(TypeTag::Blob, enc.as_bytes())?;

        let result = read_blob(&store, &id);
        assert!(matches!(result, Err(Error::Corruption(_))), "got {result:?}");
        Ok(())
    }

    // ── proptest: round-trip ───────────────────────────────────────────────────

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn prop_read_blob_round_trips(data in prop::collection::vec(any::<u8>(), 0..(300 * 1024))) {
            let dir = TempDir::new().expect("temp dir");
            let store = ObjectStore::init(dir.path().join(".tack")).expect("init");
            let id = store_file_bytes(&store, &data).expect("store");
            let back = read_blob(&store, &id).expect("read");
            prop_assert_eq!(back, data);
        }

        #[test]
        fn prop_range_matches_full(
            data in prop::collection::vec(any::<u8>(), 0..(128 * 1024)),
            offset in any::<u32>(),
            len in 0usize..(64 * 1024),
        ) {
            let dir = TempDir::new().expect("temp dir");
            let store = ObjectStore::init(dir.path().join(".tack")).expect("init");
            let id = store_file_bytes(&store, &data).expect("store");
            let full = read_blob(&store, &id).expect("read");
            let total = full.len();
            let offset = u64::from(offset) % (total as u64 + 1);
            let got = read_blob_range(&store, &id, offset, len).expect("range");
            let start = usize::try_from(offset).expect("offset fits usize").min(total);
            let end = (start + len).min(total);
            prop_assert_eq!(got, full[start..end].to_vec());
        }
    }
}
