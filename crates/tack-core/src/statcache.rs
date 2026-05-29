//! Working-copy **stat cache** — an advisory fast path for [`build_tree`].
//!
//! Re-chunking every file on each capture dominates the cost of `snap`, and —
//! since the default `diff`/`ls` now build the live tree — of those reads too.
//! Most files are unchanged between captures, so this cache lets
//! [`build_tree_cached`](crate::tree::build_tree_cached) skip reading and
//! chunking a file whose on-disk `(mtime, size)` fingerprint is unchanged,
//! reusing the previously computed blob id.
//!
//! ## Correctness
//!
//! The cache is **purely advisory**: a missing, stale, or corrupt cache only
//! makes capture slower, never wrong. [`build_tree`](crate::tree::build_tree)
//! (no cache) and [`build_tree_cached`](crate::tree::build_tree_cached) always
//! produce the *same* root tree id for the same on-disk content. Two safeguards
//! make a reused blob id trustworthy:
//!
//! * **fingerprint match** — the file's current `(mtime, size)` must equal the
//!   cached pair, and
//! * **racy-clean guard** — the file's `mtime` must be older than the moment the
//!   working copy was *scanned* (an upper bound on every file's read time,
//!   persisted as the cache's `gen` line) by at least one filesystem mtime tick
//!   ([`RACY_MARGIN`]). A same-size in-place edit made during or after the scan
//!   can collide in the same coarse mtime tick as the recorded value; the margin
//!   rejects exactly those files, so a reused blob id can never disagree with the
//!   file's actual content (git's "racily-clean" rule).
//!
//! Only file **content** is cached (the blob id). The tree entry's mode is
//! always recomputed from disk, so a permission-only change (which does not move
//! `mtime`) is still reflected.
//!
//! The on-disk file (`.tack/wc-cache`) is a local optimization, **not** part of
//! the content-addressed format: it carries its own [`CACHE_VERSION`] and is
//! independent of the repository `FORMAT_VERSION`. A version or parse mismatch
//! discards it (rebuilds from scratch). Writes are atomic (unique temp + rename),
//! so concurrent writers degrade to last-writer-wins with no corruption.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::str::FromStr as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Result;
use crate::hash::ObjectId;

/// Magic marker on the first line of a cache file.
const CACHE_MAGIC: &str = "TACKWCCACHE";

/// Cache format version, bumped on any layout change. Independent of the
/// repository `FORMAT_VERSION` — the cache is a disposable local optimization.
const CACHE_VERSION: u32 = 1;

/// Safety margin for the racy-clean guard: a cached entry is trusted only if its
/// `mtime` is at least this far before the recorded scan instant. It must be
/// `>=` the working-copy filesystem's mtime granularity; 2 seconds covers the
/// coarsest common case (FAT/exFAT's 2-second mtime resolution). Files modified
/// within this window of a capture are simply re-hashed on the next one.
const RACY_MARGIN: Duration = Duration::from_secs(2);

/// A single file's fingerprint: if the on-disk `(mtime, size)` still match (and
/// the entry is not racily-clean), the file's content is unchanged and `blob_id`
/// can be reused without reading or chunking the file.
#[derive(Debug, Clone)]
struct Fingerprint {
    mtime: SystemTime,
    size: u64,
    blob_id: ObjectId,
}

/// An advisory working-copy stat cache (see the module docs).
///
/// Loaded entries (`prev`) are consulted via [`reuse`](Self::reuse); entries for
/// files actually seen this capture are accumulated in `next` and are the only
/// ones [`save`](Self::save)d — so files deleted since the last capture are
/// pruned automatically.
#[derive(Debug)]
pub struct StatCache {
    /// When the loaded cache was written; the racy-clean reference point.
    prev_generated_at: SystemTime,
    /// Entries loaded from disk, keyed by repo-relative forward-slashed path.
    prev: HashMap<String, Fingerprint>,
    /// Entries built during the current capture (what [`save`](Self::save) writes).
    next: HashMap<String, Fingerprint>,
    /// Whether anything was recorded (skip writing an unchanged cache).
    dirty: bool,
}

impl StatCache {
    /// Loads the cache at `path`, tolerating any error or malformed content by
    /// returning an empty cache (capture is then simply uncached / slower).
    #[must_use]
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| Self::parse(&text))
            .unwrap_or_else(Self::empty)
    }

    /// An empty cache: every lookup misses (so every file is hashed fresh).
    #[must_use]
    fn empty() -> Self {
        Self {
            prev_generated_at: UNIX_EPOCH,
            prev: HashMap::new(),
            next: HashMap::new(),
            dirty: false,
        }
    }

    /// Returns the cached blob id for `rel` **iff** the file is provably
    /// unchanged: its `(mtime, size)` match the cached fingerprint **and** its
    /// `mtime` is older than the previous scan instant by at least one filesystem
    /// mtime-granularity tick ([`RACY_MARGIN`]). Otherwise `None` — hash the file.
    ///
    /// The margin is what makes the guard sound. The reference is the moment the
    /// working copy was *scanned* (an upper bound on every file's read time — see
    /// [`save`](Self::save)). A same-size in-place edit that happens during or
    /// after that scan can land in the *same* coarse mtime tick as the recorded
    /// value; requiring `mtime + RACY_MARGIN < scan` rejects exactly those files
    /// (a stale-but-colliding entry can only occur within one granularity tick of
    /// the scan), so a reused blob id can never disagree with the file's content.
    #[must_use]
    pub fn reuse(&self, rel: &str, mtime: SystemTime, size: u64) -> Option<ObjectId> {
        let fp = self.prev.get(rel)?;
        let unmodified_before_scan = mtime
            .checked_add(RACY_MARGIN)
            .is_some_and(|cutoff| cutoff < self.prev_generated_at);
        (fp.size == size && fp.mtime == mtime && unmodified_before_scan).then_some(fp.blob_id)
    }

    /// Records the fingerprint of a file seen this capture (whether its blob id
    /// was reused or freshly computed) so it persists to the next cache.
    pub fn record(&mut self, rel: &str, mtime: SystemTime, size: u64, blob_id: ObjectId) {
        self.next
            .insert(rel.to_owned(), Fingerprint { mtime, size, blob_id });
        self.dirty = true;
    }

    /// Atomically writes the cache to `path` (unique temp + rename). A no-op when
    /// nothing was recorded.
    ///
    /// `scanned_at` MUST be an instant captured **before** the working copy was
    /// walked — an upper bound on every file's read time. It is persisted as the
    /// `gen` line and used as the racy-clean reference on the next load (see
    /// [`reuse`](Self::reuse)); passing the post-walk time instead would widen the
    /// trusted window to include edits made *during* the scan and could yield a
    /// stale blob id.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`](crate::Error::Io) if the temp write or rename fails.
    /// A `scanned_at` before the Unix epoch is treated as "cannot write" (returns
    /// `Ok` without writing — the cache is optional).
    pub fn save(&self, path: &Path, scanned_at: SystemTime) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some((gen_secs, gen_nanos)) = encode_time(scanned_at) else {
            return Ok(());
        };

        let mut buf = format!("{CACHE_MAGIC} {CACHE_VERSION}\ngen {gen_secs} {gen_nanos}\n");
        for (rel, fp) in &self.next {
            // A newline in a path would corrupt the line-based format; such an
            // entry is simply omitted (it will be re-hashed next time).
            if rel.contains('\n') {
                continue;
            }
            let Some((secs, nanos)) = encode_time(fp.mtime) else {
                continue;
            };
            // Writing to a String is infallible.
            let _ = writeln!(buf, "{secs} {nanos} {} {} {rel}", fp.size, fp.blob_id);
        }

        // std `rename` replaces an existing destination on both Windows and Unix;
        // a pid-tagged temp avoids two concurrent writers clobbering one temp.
        let tmp = path.with_file_name(format!("wc-cache.{}.tmp", std::process::id()));
        fs::write(&tmp, buf.as_bytes())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Parses a cache file. A bad header or `gen` line discards the whole cache
    /// (returns `None`); a malformed *entry* line is skipped individually.
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();

        let mut header = lines.next()?.split_whitespace();
        if header.next()? != CACHE_MAGIC || header.next()?.parse::<u32>().ok()? != CACHE_VERSION {
            return None;
        }

        let mut gen_line = lines.next()?.split_whitespace();
        if gen_line.next()? != "gen" {
            return None;
        }
        let gen_secs = gen_line.next()?.parse::<u64>().ok()?;
        let gen_nanos = gen_line.next()?.parse::<u32>().ok()?;
        let prev_generated_at = decode_time(gen_secs, gen_nanos)?;

        let mut prev = HashMap::new();
        for line in lines {
            if let Some((rel, fp)) = parse_entry(line) {
                prev.insert(rel, fp);
            }
        }

        Some(Self {
            prev_generated_at,
            prev,
            next: HashMap::new(),
            dirty: false,
        })
    }
}

/// Parses one `"<secs> <nanos> <size> <blobhex> <rel-path>"` entry line; the
/// path is the rest of the line (may contain spaces). `None` on any malformed
/// field so the caller can skip it.
fn parse_entry(line: &str) -> Option<(String, Fingerprint)> {
    let mut parts = line.splitn(5, ' ');
    let secs = parts.next()?.parse::<u64>().ok()?;
    let nanos = parts.next()?.parse::<u32>().ok()?;
    let size = parts.next()?.parse::<u64>().ok()?;
    let blob_id = ObjectId::from_str(parts.next()?).ok()?;
    let rel = parts.next()?.to_owned();
    Some((rel, Fingerprint { mtime: decode_time(secs, nanos)?, size, blob_id }))
}

/// Encodes a [`SystemTime`] as `(unix_secs, subsec_nanos)`; `None` for times
/// before the Unix epoch (treated as uncacheable).
fn encode_time(t: SystemTime) -> Option<(u64, u32)> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| (d.as_secs(), d.subsec_nanos()))
}

/// Decodes `(unix_secs, subsec_nanos)` back to a [`SystemTime`]. `None` for an
/// out-of-range `nanos` or a `secs` so large the [`SystemTime`] would overflow —
/// a corrupt or crafted cache must degrade to "skip this entry", never panic.
fn decode_time(secs: u64, nanos: u32) -> Option<SystemTime> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> ObjectId {
        ObjectId::from_bytes([byte; 32])
    }

    /// `UNIX_EPOCH + secs`, for building deterministic instants in tests.
    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn empty_cache_never_reuses() {
        let cache = StatCache::empty();
        assert!(cache.reuse("a.txt", SystemTime::now(), 10).is_none());
    }

    #[test]
    fn round_trips_through_save_and_load() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("wc-cache");

        // mtime is well before the scan instant (outside RACY_MARGIN) → trustworthy.
        let mtime = t(1_000_000);
        let mut cache = StatCache::empty();
        cache.record("src/a.txt", mtime, 42, id(0xab));
        cache.save(&path, t(1_000_010))?;

        let loaded = StatCache::load(&path);
        assert_eq!(loaded.reuse("src/a.txt", mtime, 42), Some(id(0xab)));
        // A size change misses.
        assert!(loaded.reuse("src/a.txt", mtime, 43).is_none());
        // An mtime change misses.
        assert!(loaded.reuse("src/a.txt", mtime + Duration::from_secs(1), 42).is_none());
        Ok(())
    }

    #[test]
    fn racy_margin_rejects_entries_too_close_to_the_scan() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("wc-cache");
        let scanned = t(1_000_000);

        let mut cache = StatCache::empty();
        // 1s before the scan → inside the 2s margin → racy, must NOT be trusted.
        cache.record("recent.txt", t(999_999), 10, id(0x01));
        // 10s before the scan → outside the margin → trustworthy.
        cache.record("old.txt", t(999_990), 10, id(0x02));
        cache.save(&path, scanned)?;

        let loaded = StatCache::load(&path);
        assert!(
            loaded.reuse("recent.txt", t(999_999), 10).is_none(),
            "an mtime within RACY_MARGIN of the scan must be re-hashed"
        );
        assert_eq!(
            loaded.reuse("old.txt", t(999_990), 10),
            Some(id(0x02)),
            "an mtime comfortably before the scan is trustworthy"
        );
        Ok(())
    }

    #[test]
    fn deleted_files_are_pruned_on_save() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("wc-cache");
        let mtime = t(1_000_000);
        let scanned = t(1_000_010);

        let mut first = StatCache::empty();
        first.record("keep.txt", mtime, 1, id(0x01));
        first.record("gone.txt", mtime, 1, id(0x02));
        first.save(&path, scanned)?;

        // Next capture only sees keep.txt → gone.txt must not survive.
        let mut second = StatCache::load(&path);
        assert_eq!(second.reuse("keep.txt", mtime, 1), Some(id(0x01)));
        second.record("keep.txt", mtime, 1, id(0x01));
        second.save(&path, scanned)?;

        let third = StatCache::load(&path);
        assert_eq!(third.reuse("keep.txt", mtime, 1), Some(id(0x01)));
        assert!(third.reuse("gone.txt", mtime, 1).is_none(), "deleted file must be pruned");
        Ok(())
    }

    #[test]
    fn corrupt_or_wrong_version_cache_loads_empty() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("wc-cache");

        fs::write(&path, b"not a tack cache at all\nrandom bytes\n")?;
        assert!(StatCache::load(&path).reuse("a", UNIX_EPOCH, 0).is_none());

        fs::write(&path, format!("{CACHE_MAGIC} 999\ngen 1 0\n").as_bytes())?;
        let bad_version = StatCache::load(&path);
        assert!(bad_version.prev.is_empty(), "a future version must be discarded");
        Ok(())
    }

    #[test]
    fn overflowing_timestamp_is_tolerated_not_panicked() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("wc-cache");
        let hex = "ab".repeat(32); // a valid 64-char ObjectId

        // An overflowing `gen` secs discards the whole cache (no panic).
        fs::write(&path, format!("{CACHE_MAGIC} {CACHE_VERSION}\ngen {} 0\n", u64::MAX).as_bytes())?;
        assert!(StatCache::load(&path).prev.is_empty());

        // An overflowing entry secs skips just that entry; the cache still loads.
        fs::write(
            &path,
            format!("{CACHE_MAGIC} {CACHE_VERSION}\ngen 1000 0\n{} 0 5 {hex} bad.txt\n", u64::MAX)
                .as_bytes(),
        )?;
        let loaded = StatCache::load(&path);
        assert!(loaded.reuse("bad.txt", UNIX_EPOCH, 5).is_none());
        Ok(())
    }

    #[test]
    fn paths_with_spaces_survive_round_trip() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let path = dir.path().join("wc-cache");
        let mtime = t(1_000_000);

        let mut cache = StatCache::empty();
        cache.record("dir with spaces/a b.txt", mtime, 5, id(0x07));
        cache.save(&path, t(1_000_010))?;

        let loaded = StatCache::load(&path);
        assert_eq!(loaded.reuse("dir with spaces/a b.txt", mtime, 5), Some(id(0x07)));
        Ok(())
    }
}
