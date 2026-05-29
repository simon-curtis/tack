//! Deterministic binary codec for all tack objects.
//!
//! All integers are **little-endian**. Variable-length quantities use
//! unsigned LEB128. The format is specified in `DESIGN.md §3`.
//!
//! Layer L2 implements [`Encode`] and [`Decode`] for each object kind; this
//! module provides only the primitive building blocks.

use crate::error::{Error, Result};
use crate::hash::ObjectId;

// ── Traits ────────────────────────────────────────────────────────────────────

/// Types that can be serialized into the tack canonical binary format.
pub trait Encode {
    /// Appends the canonical encoding of `self` to `encoder`.
    fn encode(&self, encoder: &mut Encoder);
}

/// Types that can be deserialized from the tack canonical binary format.
pub trait Decode: Sized {
    /// Reads `Self` from `decoder`, advancing the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the buffer ends before the value is
    /// complete, or other decode errors for malformed data.
    fn decode(decoder: &mut Decoder<'_>) -> Result<Self>;
}

// ── Encoder ───────────────────────────────────────────────────────────────────

/// A write-only byte buffer for constructing canonical object encodings.
///
/// Wrap and consume with [`Encoder::into_bytes`] when finished.
#[derive(Debug, Default)]
pub struct Encoder {
    buffer: Vec<u8>,
}

impl Encoder {
    /// Creates a new, empty encoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an encoder with a pre-allocated capacity hint.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
        }
    }

    /// Consumes the encoder and returns the accumulated bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }

    /// Returns a view of the accumulated bytes without consuming the encoder.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer
    }

    /// Writes a single `u8`.
    pub fn u8(&mut self, value: u8) {
        self.buffer.push(value);
    }

    /// Writes a `u32` as 4 little-endian bytes.
    pub fn u32(&mut self, value: u32) {
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a `u64` as 8 little-endian bytes.
    pub fn u64(&mut self, value: u64) {
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes an `i64` as 8 little-endian bytes.
    pub fn i64(&mut self, value: i64) {
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes an `i32` as 4 little-endian bytes.
    pub fn i32(&mut self, value: i32) {
        self.buffer.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a `u64` as unsigned LEB128 (variable-length encoding).
    pub fn varint(&mut self, mut value: u64) {
        loop {
            // Masking to 7 bits guarantees the cast to u8 is always safe.
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.buffer.push(byte);
                break;
            }
            self.buffer.push(byte | 0x80);
        }
    }

    /// Writes a byte slice as a varint length followed by the raw bytes.
    pub fn bytes(&mut self, data: &[u8]) {
        // A slice longer than u64::MAX bytes cannot exist in practice.
        self.varint(data.len() as u64);
        self.buffer.extend_from_slice(data);
    }

    /// Writes a UTF-8 string as a varint length followed by the UTF-8 bytes.
    pub fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    /// Writes an [`ObjectId`] as 32 raw bytes (no length prefix — fixed size).
    pub fn object_id(&mut self, id: &ObjectId) {
        self.buffer.extend_from_slice(id.as_bytes());
    }

    /// Writes an array length as a varint.
    ///
    /// Call this before encoding each element of the array.
    pub fn array_len(&mut self, count: usize) {
        // A collection longer than u64::MAX elements cannot exist in practice.
        self.varint(count as u64);
    }

    /// Writes raw bytes directly, without a length prefix.
    ///
    /// Used for object types (e.g. [`Chunk`](crate::object::Chunk)) whose
    /// canonical payload is the raw bytes with no framing.
    pub fn bytes_raw(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }
}

// ── Decoder ───────────────────────────────────────────────────────────────────

/// A read-only cursor over a byte slice for decoding canonical object encodings.
///
/// After reading all expected fields, call [`Decoder::finish`] to assert that
/// no trailing bytes remain.
#[derive(Debug)]
pub struct Decoder<'a> {
    data: &'a [u8],
    cursor: usize,
}

impl<'a> Decoder<'a> {
    /// Creates a new decoder over the given byte slice.
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, cursor: 0 }
    }

    /// Returns the number of bytes remaining to be read.
    pub const fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor)
    }

    /// Reads exactly `n` bytes from the buffer, advancing the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than `n` bytes remain.
    fn read_bytes_raw(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.cursor.checked_add(n).ok_or(Error::Truncated)?;
        if end > self.data.len() {
            return Err(Error::Truncated);
        }
        let slice = &self.data[self.cursor..end];
        self.cursor = end;
        Ok(slice)
    }

    /// Reads a single `u8`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if no bytes remain.
    pub fn u8(&mut self) -> Result<u8> {
        let slice = self.read_bytes_raw(1)?;
        Ok(slice[0])
    }

    /// Reads a `u32` from 4 little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than 4 bytes remain.
    pub fn u32(&mut self) -> Result<u32> {
        let slice = self.read_bytes_raw(4)?;
        let array: [u8; 4] = slice
            .try_into()
            .map_err(|_| Error::Corruption("read_bytes_raw returned wrong length".to_string()))?;
        Ok(u32::from_le_bytes(array))
    }

    /// Reads a `u64` from 8 little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than 8 bytes remain.
    pub fn u64(&mut self) -> Result<u64> {
        let slice = self.read_bytes_raw(8)?;
        let array: [u8; 8] = slice
            .try_into()
            .map_err(|_| Error::Corruption("read_bytes_raw returned wrong length".to_string()))?;
        Ok(u64::from_le_bytes(array))
    }

    /// Reads an `i64` from 8 little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than 8 bytes remain.
    pub fn i64(&mut self) -> Result<i64> {
        let slice = self.read_bytes_raw(8)?;
        let array: [u8; 8] = slice
            .try_into()
            .map_err(|_| Error::Corruption("read_bytes_raw returned wrong length".to_string()))?;
        Ok(i64::from_le_bytes(array))
    }

    /// Reads an `i32` from 4 little-endian bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than 4 bytes remain.
    pub fn i32(&mut self) -> Result<i32> {
        let slice = self.read_bytes_raw(4)?;
        let array: [u8; 4] = slice
            .try_into()
            .map_err(|_| Error::Corruption("read_bytes_raw returned wrong length".to_string()))?;
        Ok(i32::from_le_bytes(array))
    }

    /// Reads a `u64` encoded as unsigned LEB128.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the buffer ends mid-varint, or
    /// [`Error::OverlongVarint`] if more than 10 bytes are consumed (which
    /// would overflow a `u64`).
    pub fn varint(&mut self) -> Result<u64> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        // A u64 fits in at most 10 LEB128 bytes (ceil(64/7) = 10).
        for _ in 0..10 {
            let byte = self.u8()?;
            let low_bits = u64::from(byte & 0x7f);
            result |= low_bits << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(Error::OverlongVarint)
    }

    /// Reads a varint-prefixed byte slice.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if the buffer is too short, or
    /// [`Error::OverlongVarint`] if the length varint is overlong.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len_u64 = self.varint()?;
        let len = usize::try_from(len_u64)
            .map_err(|_| Error::Corruption("byte-string length overflows usize".to_string()))?;
        self.read_bytes_raw(len)
    }

    /// Reads a varint-prefixed UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`], [`Error::OverlongVarint`], or
    /// [`Error::Corruption`] if the bytes are not valid UTF-8.
    pub fn str(&mut self) -> Result<&'a str> {
        let raw = self.bytes()?;
        std::str::from_utf8(raw)
            .map_err(|_| Error::Corruption("invalid utf-8 in string".to_string()))
    }

    /// Reads a fixed 32-byte [`ObjectId`] (no length prefix).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`] if fewer than 32 bytes remain.
    pub fn object_id(&mut self) -> Result<ObjectId> {
        let slice = self.read_bytes_raw(32)?;
        let array: [u8; 32] = slice
            .try_into()
            .map_err(|_| Error::Corruption("read_bytes_raw returned wrong length".to_string()))?;
        Ok(ObjectId::from_bytes(array))
    }

    /// Reads an array length varint.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Truncated`], [`Error::OverlongVarint`], or
    /// [`Error::Corruption`] if the count overflows `usize`.
    pub fn array_len(&mut self) -> Result<usize> {
        let count = self.varint()?;
        usize::try_from(count)
            .map_err(|_| Error::Corruption("array length overflows usize".to_string()))
    }

    /// A pre-allocation hint for a decoded collection of `count` elements,
    /// bounded by the bytes still remaining.
    ///
    /// Every element encodes to at least one byte, so a well-formed
    /// `count`-element array needs at least `count` more bytes. Capping the
    /// reservation at [`remaining`](Self::remaining) means a crafted or corrupt
    /// length can never trigger a huge speculative `Vec::with_capacity`
    /// allocation (which aborts the process rather than returning an error),
    /// while the decode loop still fails cleanly with [`Error::Truncated`] when
    /// the elements run out.
    pub(crate) fn reserve_hint(&self, count: usize) -> usize {
        count.min(self.remaining())
    }

    /// Returns the remaining (not-yet-read) bytes as a slice.
    ///
    /// Used by [`Chunk`](crate::object::Chunk) decode, which consumes all
    /// remaining bytes as its raw payload.
    pub fn read_remaining(&mut self) -> &'a [u8] {
        let slice = &self.data[self.cursor..];
        self.cursor = self.data.len();
        slice
    }

    /// Asserts that the entire input has been consumed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TrailingBytes`] if any bytes remain after the cursor.
    pub const fn finish(&self) -> Result<()> {
        if self.cursor < self.data.len() {
            return Err(Error::TrailingBytes);
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── u8 ────────────────────────────────────────────────────────────────────

    #[test]
    fn u8_round_trips() -> Result<()> {
        for v in [0u8, 1, 127, 128, 255] {
            let mut enc = Encoder::new();
            enc.u8(v);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.u8()?, v);
            dec.finish()?;
        }
        Ok(())
    }

    // ── u32 ───────────────────────────────────────────────────────────────────

    #[test]
    fn u32_round_trips() -> Result<()> {
        for v in [0u32, 1, u32::MAX / 2, u32::MAX] {
            let mut enc = Encoder::new();
            enc.u32(v);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.u32()?, v);
            dec.finish()?;
        }
        Ok(())
    }

    // ── u64 ───────────────────────────────────────────────────────────────────

    #[test]
    fn u64_round_trips() -> Result<()> {
        for v in [0u64, 1, u64::MAX / 2, u64::MAX] {
            let mut enc = Encoder::new();
            enc.u64(v);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.u64()?, v);
            dec.finish()?;
        }
        Ok(())
    }

    // ── i64 ───────────────────────────────────────────────────────────────────

    #[test]
    fn i64_round_trips() -> Result<()> {
        for v in [0i64, 1, -1, i64::MIN, i64::MAX] {
            let mut enc = Encoder::new();
            enc.i64(v);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.i64()?, v);
            dec.finish()?;
        }
        Ok(())
    }

    // ── i32 ───────────────────────────────────────────────────────────────────

    #[test]
    fn i32_round_trips() -> Result<()> {
        for v in [0i32, 1, -1, i32::MIN, i32::MAX] {
            let mut enc = Encoder::new();
            enc.i32(v);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.i32()?, v);
            dec.finish()?;
        }
        Ok(())
    }

    // ── varint LEB128 ─────────────────────────────────────────────────────────

    #[test]
    fn varint_boundary_values() -> Result<()> {
        // 0: single byte 0x00
        // 127: single byte 0x7f (max single-byte value)
        // 128: two bytes 0x80 0x01
        // u64::MAX: 10 bytes
        for v in [0u64, 127, 128, u64::MAX] {
            let mut enc = Encoder::new();
            enc.varint(v);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            let got = dec.varint()?;
            dec.finish()?;
            assert_eq!(got, v, "varint round-trip failed for {v}");
        }
        Ok(())
    }

    #[test]
    fn varint_single_byte_for_values_0_to_127() -> Result<()> {
        for v in [0u64, 1, 63, 127] {
            let mut enc = Encoder::new();
            enc.varint(v);
            let bytes = enc.into_bytes();
            assert_eq!(bytes.len(), 1, "value {v} should encode to 1 byte");
            let mut dec = Decoder::new(&bytes);
            assert_eq!(dec.varint()?, v);
        }
        Ok(())
    }

    #[test]
    fn varint_two_bytes_for_128() {
        let mut enc = Encoder::new();
        enc.varint(128);
        let bytes = enc.into_bytes();
        assert_eq!(bytes, [0x80, 0x01]);
    }

    // ── bytes ─────────────────────────────────────────────────────────────────

    #[test]
    fn bytes_round_trips() -> Result<()> {
        for data in [b"".as_slice(), b"hello", b"\x00\xff\x80"] {
            let mut enc = Encoder::new();
            enc.bytes(data);
            let encoded = enc.into_bytes();
            let mut dec = Decoder::new(&encoded);
            assert_eq!(dec.bytes()?, data);
            dec.finish()?;
        }
        Ok(())
    }

    // ── str ───────────────────────────────────────────────────────────────────

    #[test]
    fn str_round_trips() -> Result<()> {
        for s in ["", "hello", "unicode: \u{1F600}"] {
            let mut enc = Encoder::new();
            enc.str(s);
            let encoded = enc.into_bytes();
            let mut dec = Decoder::new(&encoded);
            assert_eq!(dec.str()?, s);
            dec.finish()?;
        }
        Ok(())
    }

    // ── ObjectId ──────────────────────────────────────────────────────────────

    #[test]
    fn object_id_round_trips() -> Result<()> {
        let id = ObjectId::from_bytes([0xab; 32]);
        let mut enc = Encoder::new();
        enc.object_id(&id);
        let encoded = enc.into_bytes();
        assert_eq!(encoded.len(), 32, "ObjectId must encode to exactly 32 bytes");
        let mut dec = Decoder::new(&encoded);
        let got = dec.object_id()?;
        dec.finish()?;
        assert_eq!(got, id);
        Ok(())
    }

    // ── array_len ─────────────────────────────────────────────────────────────

    #[test]
    fn array_len_round_trips() -> Result<()> {
        for count in [0usize, 1, 127, 128, 1024] {
            let mut enc = Encoder::new();
            enc.array_len(count);
            let encoded = enc.into_bytes();
            let mut dec = Decoder::new(&encoded);
            assert_eq!(dec.array_len()?, count);
            dec.finish()?;
        }
        Ok(())
    }

    // ── Decoder error cases ───────────────────────────────────────────────────

    #[test]
    fn decoder_errors_on_truncated_u8() {
        let mut dec = Decoder::new(&[]);
        assert!(
            matches!(dec.u8(), Err(Error::Truncated)),
            "expected Truncated on empty buffer"
        );
    }

    #[test]
    fn decoder_errors_on_truncated_u32() {
        let mut dec = Decoder::new(&[0x01, 0x02]); // only 2 bytes instead of 4
        assert!(matches!(dec.u32(), Err(Error::Truncated)));
    }

    #[test]
    fn decoder_errors_on_truncated_u64() {
        let mut dec = Decoder::new(&[0x01, 0x02, 0x03]); // only 3 bytes instead of 8
        assert!(matches!(dec.u64(), Err(Error::Truncated)));
    }

    #[test]
    fn decoder_errors_on_truncated_varint() {
        // A continuation byte (high bit set) with nothing following.
        let mut dec = Decoder::new(&[0x80]);
        assert!(matches!(dec.varint(), Err(Error::Truncated)));
    }

    #[test]
    fn decoder_errors_on_overlong_varint() {
        // 11 bytes total: 10 continuation bytes + 1 more — too many for a u64.
        let overlong: Vec<u8> = std::iter::repeat_n(0x80_u8, 10)
            .chain(std::iter::once(0x01_u8))
            .collect();
        let mut dec = Decoder::new(&overlong);
        assert!(
            matches!(dec.varint(), Err(Error::OverlongVarint)),
            "expected OverlongVarint"
        );
    }

    #[test]
    fn decoder_errors_on_truncated_object_id() {
        let mut dec = Decoder::new(&[0u8; 16]); // only 16 bytes instead of 32
        assert!(matches!(dec.object_id(), Err(Error::Truncated)));
    }

    #[test]
    fn finish_errors_on_trailing_bytes() {
        let mut enc = Encoder::new();
        enc.u8(42);
        enc.u8(99); // extra byte
        let bytes = enc.into_bytes();
        let mut dec = Decoder::new(&bytes);
        let _ = dec.u8().unwrap();
        // One byte remains.
        assert!(
            matches!(dec.finish(), Err(Error::TrailingBytes)),
            "expected TrailingBytes"
        );
    }

    #[test]
    fn remaining_tracks_unread_bytes() -> Result<()> {
        let mut enc = Encoder::new();
        enc.u8(1);
        enc.u32(2);
        let bytes = enc.into_bytes();
        let mut dec = Decoder::new(&bytes);
        assert_eq!(dec.remaining(), 5, "all 5 bytes are unread initially");
        dec.u8()?;
        assert_eq!(dec.remaining(), 4, "1 byte consumed");
        dec.u32()?;
        assert_eq!(dec.remaining(), 0, "all bytes consumed");
        Ok(())
    }

    #[test]
    fn finish_succeeds_when_fully_consumed() -> Result<()> {
        let mut enc = Encoder::new();
        enc.u32(1234);
        let bytes = enc.into_bytes();
        let mut dec = Decoder::new(&bytes);
        dec.u32()?;
        dec.finish()
    }

    // ── proptest: arbitrary u64 varint round-trip ─────────────────────────────

    proptest! {
        #[test]
        fn prop_varint_round_trips(value: u64) {
            let mut enc = Encoder::new();
            enc.varint(value);
            let bytes = enc.into_bytes();
            let mut dec = Decoder::new(&bytes);
            let got = dec.varint().expect("varint decode should succeed");
            prop_assert_eq!(got, value);
            dec.finish().expect("no trailing bytes expected");
        }
    }

    // ── Multiple fields in sequence ───────────────────────────────────────────

    #[test]
    fn multiple_fields_in_sequence() -> Result<()> {
        let mut enc = Encoder::new();
        enc.u8(7);
        enc.u32(0x0102_0304);
        enc.i64(-42);
        enc.str("tack");
        enc.varint(300);
        let id = ObjectId::from_bytes([0xcc; 32]);
        enc.object_id(&id);

        let bytes = enc.into_bytes();
        let mut dec = Decoder::new(&bytes);
        assert_eq!(dec.u8()?, 7);
        assert_eq!(dec.u32()?, 0x0102_0304);
        assert_eq!(dec.i64()?, -42);
        assert_eq!(dec.str()?, "tack");
        assert_eq!(dec.varint()?, 300);
        assert_eq!(dec.object_id()?, id);
        dec.finish()
    }
}
