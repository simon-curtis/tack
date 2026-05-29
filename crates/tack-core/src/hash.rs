//! Content-addressing primitives: [`ObjectId`], [`TypeTag`], and [`hash_object`].
//!
//! Every object is named by `BLAKE3(TYPE_TAG_BYTE || canonical_encoding(object))`
//! as specified in `DESIGN.md §2`.

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

// ── ObjectId ─────────────────────────────────────────────────────────────────

/// A 32-byte BLAKE3-256 content address.
///
/// Rendered as lowercase hex (64 chars) for display; stored and compared as
/// raw bytes. The short form used in human output is the first 12 hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    /// Returns the raw 32-byte digest.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Constructs an `ObjectId` from a raw 32-byte array.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the first 12 hex characters, suitable for human-facing output.
    pub fn short(&self) -> String {
        self.to_string().chars().take(12).collect()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({self})")
    }
}

impl FromStr for ObjectId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 64 {
            return Err(Error::InvalidObjectId(s.to_string()));
        }
        let mut bytes = [0u8; 32];
        for (index, chunk) in s.as_bytes().chunks(2).enumerate() {
            let high = hex_digit(chunk[0], s)?;
            let low = hex_digit(chunk[1], s)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

/// Decodes a single ASCII hex nibble, returning `Error::InvalidObjectId` on
/// any non-hex character.
fn hex_digit(byte: u8, full_str: &str) -> Result<u8> {
    let value = match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => return Err(Error::InvalidObjectId(full_str.to_string())),
    };
    Ok(value)
}

// ── TypeTag ───────────────────────────────────────────────────────────────────

/// Domain-separation tag prepended to every object before hashing.
///
/// The tag ensures that two objects of different kinds with the same payload
/// produce different IDs. Values are fixed in `DESIGN.md §2` and are part of
/// the on-disk format contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TypeTag {
    /// A file-content blob (chunk-list). Tag byte `0x01`.
    Blob = 0x01,
    /// A raw chunk of file bytes. Tag byte `0x02`.
    Chunk = 0x02,
    /// A directory snapshot (sorted entries). Tag byte `0x03`.
    Tree = 0x03,
    /// A working-copy or named-cut snapshot. Tag byte `0x04`.
    Snapshot = 0x04,
    /// One entry in the operation log. Tag byte `0x05`.
    Op = 0x05,
    /// The complete repo state at the end of an operation. Tag byte `0x06`.
    View = 0x06,
}

// ── hash_object ───────────────────────────────────────────────────────────────

/// Hashes an object using the tack domain-separated scheme:
/// `BLAKE3(TYPE_TAG_BYTE || canonical_bytes)`.
///
/// The `canonical_bytes` must be the deterministic binary encoding produced by
/// the codec in `encoding.rs`.
pub fn hash_object(tag: TypeTag, canonical_bytes: &[u8]) -> ObjectId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[tag as u8]);
    hasher.update(canonical_bytes);
    let digest = hasher.finalize();
    ObjectId(*digest.as_bytes())
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ObjectId Display / FromStr round-trip ─────────────────────────────

    #[test]
    fn display_is_64_lowercase_hex_chars() {
        let id = ObjectId::from_bytes([0xab; 32]);
        let hex = id.to_string();
        assert_eq!(hex.len(), 64, "expected 64 hex chars, got {}", hex.len());
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "expected lowercase hex, got: {hex}"
        );
    }

    #[test]
    fn from_str_round_trips_display() -> crate::Result<()> {
        let original = ObjectId::from_bytes([0x1a; 32]);
        let hex = original.to_string();
        let parsed = ObjectId::from_str(&hex)?;
        assert_eq!(original, parsed);
        Ok(())
    }

    #[test]
    fn from_str_accepts_mixed_case() -> crate::Result<()> {
        // Our parser is case-insensitive; Display always emits lowercase.
        let upper = "AB".repeat(32);
        let id = ObjectId::from_str(&upper)?;
        let lower = "ab".repeat(32);
        let expected = ObjectId::from_str(&lower)?;
        assert_eq!(id, expected);
        Ok(())
    }

    #[test]
    fn from_str_rejects_too_short() {
        let result = ObjectId::from_str("abc");
        assert!(
            matches!(result, Err(Error::InvalidObjectId(_))),
            "expected InvalidObjectId, got {result:?}"
        );
    }

    #[test]
    fn from_str_rejects_too_long() {
        let long = "a".repeat(65);
        let result = ObjectId::from_str(&long);
        assert!(matches!(result, Err(Error::InvalidObjectId(_))));
    }

    #[test]
    fn from_str_rejects_non_hex_chars() {
        // 63 valid chars + 1 invalid.
        let bad = format!("{}z", "a".repeat(63));
        let result = ObjectId::from_str(&bad);
        assert!(matches!(result, Err(Error::InvalidObjectId(_))));
    }

    #[test]
    fn short_returns_first_12_hex_chars() {
        let id = ObjectId::from_bytes([0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let short = id.short();
        assert_eq!(short.len(), 12);
        assert_eq!(&short, "deadbeefcafe");
    }

    // ── TypeTag ──────────────────────────────────────────────────────────────

    #[test]
    fn type_tag_discriminants_match_design() {
        assert_eq!(TypeTag::Blob as u8, 0x01);
        assert_eq!(TypeTag::Chunk as u8, 0x02);
        assert_eq!(TypeTag::Tree as u8, 0x03);
        assert_eq!(TypeTag::Snapshot as u8, 0x04);
        assert_eq!(TypeTag::Op as u8, 0x05);
        assert_eq!(TypeTag::View as u8, 0x06);
    }

    // ── hash_object ──────────────────────────────────────────────────────────

    #[test]
    fn hash_object_is_deterministic() {
        let a = hash_object(TypeTag::Blob, b"hello");
        let b = hash_object(TypeTag::Blob, b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn hash_object_differs_by_tag() {
        let payload = b"same payload";
        let blob_id = hash_object(TypeTag::Blob, payload);
        let chunk_id = hash_object(TypeTag::Chunk, payload);
        assert_ne!(
            blob_id, chunk_id,
            "different type tags must produce different IDs"
        );
    }

    #[test]
    fn hash_object_differs_by_payload() {
        let a = hash_object(TypeTag::Blob, b"aaa");
        let b = hash_object(TypeTag::Blob, b"bbb");
        assert_ne!(a, b);
    }

    #[test]
    fn hash_object_empty_payload_is_stable() {
        // Must not panic and must return a 32-byte id.
        let id = hash_object(TypeTag::Chunk, &[]);
        assert_eq!(id.as_bytes().len(), 32);
    }

    #[test]
    fn hash_object_matches_manual_blake3() {
        let tag = TypeTag::Blob;
        let payload = b"tack test";
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[tag as u8]);
        hasher.update(payload);
        let expected: [u8; 32] = *hasher.finalize().as_bytes();
        let got = hash_object(tag, payload);
        assert_eq!(got.as_bytes(), &expected);
    }
}
