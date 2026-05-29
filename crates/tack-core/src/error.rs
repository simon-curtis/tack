//! Error types for `tack-core`.
//!
//! All public errors are `Send + Sync` and carry lowercase, unpunctuated
//! `Display` messages per the C&C style guide.

use thiserror::Error;

/// The canonical result type for all fallible operations in `tack-core`.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors that can be produced by `tack-core`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error from the operating system.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Object data is structurally corrupt or otherwise undecodable.
    #[error("corrupt object: {0}")]
    Corruption(String),

    /// Decoding failed because the input was truncated.
    #[error("decode error: input truncated")]
    Truncated,

    /// Decoding failed because a varint exceeded 10 bytes (overlong).
    #[error("decode error: varint is overlong")]
    OverlongVarint,

    /// Decoding finished but bytes remain in the buffer.
    #[error("decode error: trailing bytes after object")]
    TrailingBytes,

    /// The requested object was not found in the store.
    #[error("object not found: {0}")]
    ObjectNotFound(crate::ObjectId),

    /// The repo's on-disk format version is incompatible with this binary.
    #[error("format version mismatch: repo requires version {repo}, binary supports version {binary}")]
    FormatVersionMismatch {
        /// The version recorded in the repo.
        repo: u32,
        /// The maximum version this binary understands.
        binary: u32,
    },

    /// A string could not be parsed as a valid `ObjectId`.
    #[error("invalid object id: {0}")]
    InvalidObjectId(String),

    /// A hex id prefix matched no object in the store.
    ///
    /// Carries the prefix the user typed (parallel to [`InvalidObjectId`]) so the
    /// message is truthful, rather than rendering a sentinel all-zero id.
    ///
    /// [`InvalidObjectId`]: Error::InvalidObjectId
    #[error("no object matches id prefix: {0}")]
    PrefixNotFound(String),

    /// A tree entry name is invalid (empty, `"."`, `".."`, or contains
    /// `'/'` or `'\\'`).
    #[error("invalid tree entry name: {0:?}")]
    InvalidTreeEntryName(String),

    /// A caller passed an argument the engine cannot act on (e.g. a scoped cut
    /// with no paths). Distinct from [`Corruption`](Error::Corruption), which
    /// signals bad *data* rather than a bad *request*.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The filesystem watcher (`tack watch`) could not be created or failed
    /// while watching.
    ///
    /// Carries the underlying watcher message as a `String` so the public error
    /// surface stays free of the platform-specific `notify` error type.
    #[error("watch error: {0}")]
    Watch(String),

    /// The `ProjFS` projection (`tack mount`) is not available, either because
    /// this binary was built without the `projfs` feature / off Windows, or the
    /// Windows *Client-ProjFS* feature is not enabled, or virtualization could
    /// not be started.
    ///
    /// Carries a lowercase message explaining how to make it available (rebuild
    /// with `--features projfs` and enable the Windows feature).
    #[error("projfs unavailable: {0}")]
    ProjfsUnavailable(String),
}

// Verify Send + Sync at compile time.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    const fn check() {
        assert_send_sync::<Error>();
    }
    let _ = check;
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_display_is_lowercase() {
        let err = Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "file missing"));
        let msg = err.to_string();
        assert!(msg.starts_with("io error:"), "unexpected prefix: {msg}");
    }

    #[test]
    fn corruption_carries_message() {
        let err = Error::Corruption("bad magic bytes".to_string());
        assert_eq!(err.to_string(), "corrupt object: bad magic bytes");
    }

    #[test]
    fn truncated_display() {
        assert_eq!(Error::Truncated.to_string(), "decode error: input truncated");
    }

    #[test]
    fn overlong_varint_display() {
        assert_eq!(
            Error::OverlongVarint.to_string(),
            "decode error: varint is overlong"
        );
    }

    #[test]
    fn trailing_bytes_display() {
        assert_eq!(
            Error::TrailingBytes.to_string(),
            "decode error: trailing bytes after object"
        );
    }

    #[test]
    fn format_version_mismatch_display() {
        let err = Error::FormatVersionMismatch { repo: 2, binary: 1 };
        let msg = err.to_string();
        assert!(
            msg.contains("version mismatch"),
            "unexpected message: {msg}"
        );
        assert!(msg.contains('2') && msg.contains('1'));
    }

    #[test]
    fn invalid_object_id_display() {
        // Build a placeholder ObjectId for display; use the zero id.
        let zero_id = crate::ObjectId::from_bytes([0u8; 32]);
        let err = Error::ObjectNotFound(zero_id);
        let msg = err.to_string();
        assert!(msg.starts_with("object not found:"), "unexpected: {msg}");
    }

    #[test]
    fn invalid_object_id_error_display() {
        let err = Error::InvalidObjectId("not-hex".to_string());
        assert_eq!(err.to_string(), "invalid object id: not-hex");
    }

    #[test]
    fn invalid_argument_display() {
        let err = Error::InvalidArgument("scoped cut requires at least one path".to_string());
        assert_eq!(err.to_string(), "invalid argument: scoped cut requires at least one path");
    }

    #[test]
    fn watch_error_display_is_lowercase_unpunctuated() {
        let err = Error::Watch("failed to start watcher".to_string());
        assert_eq!(err.to_string(), "watch error: failed to start watcher");
    }

    #[test]
    fn from_io_error_converts() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: Error = io.into();
        assert!(matches!(err, Error::Io(_)));
    }
}
