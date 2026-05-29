//! Content-defined chunking via `FastCDC` (`fastcdc::v2020`).
//!
//! Large-file dedup splits a file's bytes into variable-length chunks at
//! content-defined boundaries, so an insertion early in a file only re-chunks
//! the region around the edit rather than shifting every subsequent boundary.
//!
//! The three size parameters and the normalization level are part of the
//! on-disk **format contract** (`DESIGN.md §5`): changing any of them moves
//! chunk boundaries and therefore changes [`Chunk`](crate::object::Chunk) IDs,
//! which would require a `FORMAT_VERSION` bump.
//!
//! | Param | Value |
//! |---|---|
//! | [`MIN_CHUNK`] | 2 KiB |
//! | [`AVG_CHUNK`] | 8 KiB |
//! | [`MAX_CHUNK`] | 64 KiB |
//!
//! Profile "`FastCDC8KB`", normalized chunking level 2.

use std::ops::Range;

use fastcdc::v2020::{FastCDC, Normalization};

/// Minimum chunk size: 2 KiB. Files no larger than this become a single chunk.
pub const MIN_CHUNK: usize = 2 * 1024;

/// Target ("average") chunk size: 8 KiB.
pub const AVG_CHUNK: usize = 8 * 1024;

/// Maximum chunk size: 64 KiB. No chunk ever exceeds this.
pub const MAX_CHUNK: usize = 64 * 1024;

/// Normalization level for `FastCDC` (`DESIGN.md §5`: level 2 — "most chunks are
/// of the desired size").
const NORMALIZATION: Normalization = Normalization::Level2;

/// Splits `data` into content-defined chunk byte ranges.
///
/// Each yielded [`Range`] is a half-open `[start, end)` slice of `data`; the
/// ranges are contiguous, non-overlapping, and cover the whole input in order.
/// An empty input yields no ranges.
///
/// The ranges are produced lazily; collect them or index `data` with each
/// range to obtain the chunk bytes.
pub fn chunk_ranges(data: &[u8]) -> impl Iterator<Item = Range<usize>> + '_ {
    // FastCDC's constructors require avg_size in (min, max]; for any non-empty
    // input our compile-time constants satisfy that. An empty slice yields no
    // chunks, which the iterator handles directly.
    FastCDC::with_level(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK, NORMALIZATION)
        .map(|chunk| chunk.offset..chunk.offset + chunk.length)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random byte generator (xorshift) so tests do not
    /// depend on an RNG crate and stay reproducible.
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

    #[test]
    fn empty_input_yields_no_ranges() {
        assert!(
            chunk_ranges(&[]).next().is_none(),
            "empty input must yield zero ranges"
        );
    }

    #[test]
    fn small_input_is_single_chunk() {
        let data = vec![7u8; MIN_CHUNK / 2];
        let ranges: Vec<_> = chunk_ranges(&data).collect();
        assert_eq!(ranges.len(), 1, "sub-MIN_CHUNK input must be one chunk");
        assert_eq!(ranges[0], 0..data.len());
    }

    #[test]
    fn exactly_min_chunk_is_single_chunk() {
        let data = vec![3u8; MIN_CHUNK];
        let ranges: Vec<_> = chunk_ranges(&data).collect();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 0..MIN_CHUNK);
    }

    #[test]
    fn ranges_are_contiguous_and_cover_input() {
        let data = pseudo_random(5 * 1024 * 1024, 0x1234_5678);
        let ranges: Vec<_> = chunk_ranges(&data).collect();
        assert!(ranges.len() > 1, "multi-MiB input should split into many chunks");

        let mut expected_start = 0usize;
        for range in &ranges {
            assert_eq!(range.start, expected_start, "ranges must be contiguous");
            assert!(range.end > range.start, "ranges must be non-empty");
            expected_start = range.end;
        }
        assert_eq!(expected_start, data.len(), "ranges must cover entire input");
    }

    #[test]
    fn chunk_sizes_respect_max_bound() {
        let data = pseudo_random(2 * 1024 * 1024, 0xdead_beef);
        for range in chunk_ranges(&data) {
            assert!(
                range.end - range.start <= MAX_CHUNK,
                "no chunk may exceed MAX_CHUNK"
            );
        }
    }

    #[test]
    fn chunking_is_deterministic() {
        let data = pseudo_random(1024 * 1024, 0xabcd);
        let first: Vec<_> = chunk_ranges(&data).collect();
        let second: Vec<_> = chunk_ranges(&data).collect();
        assert_eq!(first, second, "chunking must be deterministic for fixed input");
    }

    #[test]
    fn consts_match_design_contract() {
        assert_eq!(MIN_CHUNK, 2 * 1024);
        assert_eq!(AVG_CHUNK, 8 * 1024);
        assert_eq!(MAX_CHUNK, 64 * 1024);
    }
}
