//! Windows `ProjFS` lazy virtual-filesystem projection (`DESIGN.md §10`).
//!
//! The filesystem is one *projection* of the content-addressed store, never the
//! substrate (`constitution.md §4`). On Windows this projection is served by the
//! kernel's **Projected File System** (`ProjFS`) through the `windows-projfs`
//! crate (`dynamic-import` feature, so the binary links even where `ProjFS` is
//! not installed and fails cleanly at mount time).
//!
//! [`TackProjection`] implements [`windows_projfs::ProjectedFileSystemSource`]
//! backed by a snapshot's root [`Tree`](crate::object::Tree):
//!
//! * `list_directory(path)` resolves `path` within the root tree and maps each
//!   immediate [`TreeEntry`](crate::object::TreeEntry) to a `DirectoryEntry`. It
//!   is **metadata only** — it never reads blob bytes (`DESIGN.md §10`). Children
//!   are handed back *unsorted*; the crate sorts them with `PrjFileNameCompare`
//!   so case-insensitive ordering matches Windows.
//! * `stream_file_content(path, offset, len)` is **lazy hydration**: it resolves
//!   `path` to its blob id and reads exactly that byte range from the store via
//!   [`read_blob_range`](crate::blob::read_blob_range). This runs on a `ProjFS`
//!   pool thread.
//!
//! [`mount`] marks a directory as a virtualization root and starts virtualizing,
//! returning a guard (the live [`windows_projfs::ProjectedFileSystem`]); keep it
//! alive to stay mounted — dropping it stops virtualization (RAII).
//!
//! ## Reset note (placeholder/tombstone persistence)
//!
//! `ProjFS` writes a reparse point onto the root and persists placeholder and
//! tombstone state under it across mounts. A directory that already carries a
//! reparse point cannot be re-marked as a virtualization root. If a mount root
//! becomes wedged, delete the directory and recreate it (an empty directory with
//! no reparse point is required) before mounting again.
//!
//! ## Feature gating
//!
//! The real implementation is compiled only for
//! `#[cfg(all(windows, feature = "projfs"))]`. Everywhere else, a stub [`mount`]
//! returns [`Error::ProjfsUnavailable`](crate::error::Error::ProjfsUnavailable)
//! so call sites compile on every platform and in the default (feature-off)
//! build.

#[cfg(all(windows, feature = "projfs"))]
mod imp {
    use std::io::{self, Cursor, Read};
    use std::path::Path;

    use windows_projfs::{
        DirectoryEntry, DirectoryInfo, FileInfo, ProjectedFileSystem,
        ProjectedFileSystemSource,
    };

    use crate::blob::read_blob_range;
    use crate::error::{Error, Result};
    use crate::hash::ObjectId;
    use crate::object::EntryKind;
    use crate::store::ObjectStore;
    use crate::tree::{list_tree, read_tree_path};

    /// A `ProjFS` data source that projects a single snapshot tree out of the
    /// content-addressed [`ObjectStore`].
    ///
    /// Construct one with [`TackProjection::new`] and hand it to [`mount`]. It is
    /// `'static` and self-contained (it owns its [`ObjectStore`] handle, which is
    /// just a path), so it can outlive the call that built it as the `ProjFS`
    /// runtime requires.
    #[derive(Debug, Clone)]
    pub struct TackProjection {
        /// The store the projected blobs and trees are read from.
        store: ObjectStore,
        /// The root tree being projected (a snapshot's `root_tree`).
        root_tree: ObjectId,
    }

    impl TackProjection {
        /// Builds a projection of `root_tree` backed by `store`.
        pub const fn new(store: ObjectStore, root_tree: ObjectId) -> Self {
            Self { store, root_tree }
        }

        /// Resolves a `ProjFS` repo-relative `path` to the object id and kind it
        /// names within the projected root tree.
        ///
        /// An empty path names the root tree itself (a directory).
        fn resolve(&self, path: &Path) -> Result<(ObjectId, EntryKind)> {
            if is_root(path) {
                return Ok((self.root_tree, EntryKind::Tree));
            }
            let entry = read_tree_path(&self.store, &self.root_tree, path)?;
            Ok((entry.id(), entry.kind()))
        }

        /// Lists the immediate children of the tree at `path`, mapping each tree
        /// entry to a `ProjFS` [`DirectoryEntry`].
        ///
        /// Returns an empty list (the `ProjFS` "directory absent or empty"
        /// signal) if `path` does not resolve to a tree. Reads only tree
        /// metadata — never a blob's bytes.
        fn children(&self, path: &Path) -> Result<Vec<DirectoryEntry>> {
            let (id, kind) = self.resolve(path)?;
            if kind != EntryKind::Tree {
                return Ok(Vec::new());
            }

            let mut out = Vec::new();
            for entry in list_tree(&self.store, &id)? {
                let name = entry.name().to_owned();
                let dir_entry = if entry.kind() == EntryKind::Tree {
                    DirectoryEntry::Directory(DirectoryInfo {
                        directory_name: name,
                        ..DirectoryInfo::default()
                    })
                } else {
                    // A blob or a symlink-target blob: project as a file whose
                    // size is the blob's full length (metadata only — the bytes
                    // are hydrated lazily in `stream_file_content`).
                    let file_size = self.store.get_blob(&entry.id())?.total_len();
                    DirectoryEntry::File(FileInfo {
                        file_name: name,
                        file_size,
                        ..FileInfo::default()
                    })
                };
                out.push(dir_entry);
            }
            // Hand children back unsorted: the crate orders them with
            // `PrjFileNameCompare` (case-insensitive, matching Windows). Do NOT
            // Rust-sort here (`DESIGN.md §10`).
            Ok(out)
        }
    }

    impl ProjectedFileSystemSource for TackProjection {
        fn list_directory(&self, path: &Path) -> Vec<DirectoryEntry> {
            // The trait contract maps any failure (missing tree, decode error)
            // to "directory is empty or does not exist" — an empty list. Log so
            // a genuine corruption is still observable.
            self.children(path).unwrap_or_else(|error| {
                tracing::warn!(path = %path.display(), %error, "projfs list_directory failed");
                Vec::new()
            })
        }

        fn stream_file_content(
            &self,
            path: &Path,
            byte_offset: usize,
            length: usize,
        ) -> io::Result<Box<dyn Read>> {
            // Lazy hydration: resolve to the blob and read exactly the requested
            // window. ProjFS sizes its request against the placeholder's declared
            // `file_size`, so it never reads past EOF; the crate then `read_exact`s
            // `length` bytes from this reader, so the buffer must hold exactly
            // that many. Map every failure into an io::Error for the crate.
            let (id, kind) = self
                .resolve(path)
                .map_err(|error| io::Error::other(error.to_string()))?;
            if kind == EntryKind::Tree {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stream_file_content called on a directory",
                ));
            }

            let offset = byte_offset as u64;
            let bytes = read_blob_range(&self.store, &id, offset, length)
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(Box::new(Cursor::new(bytes)))
        }
    }

    /// Returns `true` if `path` names the projection root (the empty relative
    /// path `ProjFS` uses for the virtualization root itself).
    fn is_root(path: &Path) -> bool {
        path.as_os_str().is_empty()
    }

    /// Mounts a lazy `ProjFS` projection of `root_tree` at `root`.
    ///
    /// Creates `root` if it does not exist, marks it as a virtualization root,
    /// and starts virtualizing. The returned [`ProjectedFileSystem`] is a live
    /// guard: keep it alive to stay mounted; dropping it stops virtualization
    /// (RAII) and waits for in-flight callbacks to drain.
    ///
    /// # Errors
    ///
    /// * [`Error::Io`] if `root` cannot be created.
    /// * [`Error::ProjfsUnavailable`] if the Windows *Client-ProjFS* feature is
    ///   not enabled, or virtualization cannot be started (e.g. `root` already
    ///   carries a reparse point from a previous mount).
    pub fn mount(
        root: &Path,
        store: ObjectStore,
        root_tree: ObjectId,
    ) -> Result<ProjectedFileSystem> {
        // The root must be an existing directory with no pre-existing reparse
        // point; create it if absent (`create_dir_all` is a no-op if present).
        std::fs::create_dir_all(root)?;

        let source = TackProjection::new(store, root_tree);
        ProjectedFileSystem::new(root, source).map_err(|error| {
            Error::ProjfsUnavailable(format!("failed to start projfs virtualization: {error}"))
        })
    }

    // ── tests ──────────────────────────────────────────────────────────────────
    //
    // These exercise the projection's data path (directory listing + lazy
    // hydration) without a live ProjFS mount, so they are deterministic and run
    // anywhere the `projfs` feature is built on Windows — they do not require the
    // Client-ProjFS OS feature to be installed. A real end-to-end mount lives in
    // an `#[ignore]`d test that needs that feature.
    #[cfg(test)]
    mod tests {
        use std::io::Read as _;
        use std::path::Path;

        use tempfile::TempDir;

        use windows_projfs::{DirectoryEntry, ProjectedFileSystemSource as _};

        use super::TackProjection;
        use crate::blob::read_blob;
        use crate::error::Result;
        use crate::ignore::IgnoreRules;
        use crate::store::ObjectStore;
        use crate::tree::{build_tree, read_tree_path};

        /// Builds a store + a populated working tree, returning the store and the
        /// built root-tree id.
        fn fixture() -> Result<(TempDir, ObjectStore, crate::ObjectId)> {
            let dir = TempDir::new()?;
            let store = ObjectStore::init(dir.path().join(".tack"))?;
            let work = dir.path().join("work");
            std::fs::create_dir_all(work.join("sub"))?;
            std::fs::write(work.join("a.txt"), b"alpha")?;
            std::fs::write(work.join("b.txt"), b"bravo-content-here")?;
            std::fs::write(work.join("sub").join("deep.txt"), b"deep")?;
            let root = build_tree(&store, &work, &IgnoreRules::empty())?;
            Ok((dir, store, root))
        }

        /// Collects a directory entry's name regardless of variant.
        fn entry_name(entry: &DirectoryEntry) -> &str {
            entry.name()
        }

        #[test]
        fn list_directory_root_maps_entries() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);

            let mut names: Vec<String> = proj
                .list_directory(Path::new(""))
                .iter()
                .map(|e| entry_name(e).to_owned())
                .collect();
            names.sort();
            assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
            Ok(())
        }

        #[test]
        fn list_directory_marks_dirs_and_file_sizes() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);

            for entry in proj.list_directory(Path::new("")) {
                match entry {
                    DirectoryEntry::Directory(info) => assert_eq!(info.directory_name, "sub"),
                    DirectoryEntry::File(info) if info.file_name == "a.txt" => {
                        assert_eq!(info.file_size, b"alpha".len() as u64);
                    }
                    DirectoryEntry::File(info) if info.file_name == "b.txt" => {
                        assert_eq!(info.file_size, b"bravo-content-here".len() as u64);
                    }
                    DirectoryEntry::File(info) => panic!("unexpected file entry: {info:?}"),
                }
            }
            Ok(())
        }

        #[test]
        fn list_directory_descends_into_subtree() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);

            let names: Vec<String> = proj
                .list_directory(Path::new("sub"))
                .iter()
                .map(|e| entry_name(e).to_owned())
                .collect();
            assert_eq!(names, vec!["deep.txt"]);
            Ok(())
        }

        #[test]
        fn list_directory_missing_path_is_empty() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);
            assert!(proj.list_directory(Path::new("nope")).is_empty());
            Ok(())
        }

        #[test]
        fn list_directory_on_a_file_is_empty() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);
            // A file path is not a directory → empty list, not an error.
            assert!(proj.list_directory(Path::new("a.txt")).is_empty());
            Ok(())
        }

        #[test]
        fn stream_file_content_full_read_matches_blob() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let entry = read_tree_path(&store, &root, "b.txt")?;
            let expected = read_blob(&store, &entry.id())?;
            let proj = TackProjection::new(store, root);

            let mut reader = proj.stream_file_content(Path::new("b.txt"), 0, expected.len())?;
            let mut got = Vec::new();
            reader.read_to_end(&mut got)?;
            assert_eq!(got, expected);
            Ok(())
        }

        #[test]
        fn stream_file_content_partial_range_matches_blob() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);

            // b.txt = "bravo-content-here"; read 7 bytes from offset 6.
            let mut reader = proj.stream_file_content(Path::new("b.txt"), 6, 7)?;
            let mut got = Vec::new();
            reader.read_to_end(&mut got)?;
            assert_eq!(got, b"content");
            Ok(())
        }

        #[test]
        fn stream_file_content_in_subdir() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);

            let mut reader = proj.stream_file_content(Path::new("sub\\deep.txt"), 0, 4)?;
            let mut got = Vec::new();
            reader.read_to_end(&mut got)?;
            assert_eq!(got, b"deep");
            Ok(())
        }

        #[test]
        fn stream_file_content_on_directory_errors() -> Result<()> {
            let (_dir, store, root) = fixture()?;
            let proj = TackProjection::new(store, root);
            assert!(proj.stream_file_content(Path::new("sub"), 0, 1).is_err());
            Ok(())
        }

        /// Real end-to-end mount. `#[ignore]`d because it touches the live
        /// `ProjFS` API and requires the Windows *Client-ProjFS* feature to be
        /// installed
        /// (`Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS`).
        /// Run explicitly with `cargo test --features projfs -- --ignored`.
        ///
        /// It also asserts the invariant that holds even when `ProjFS` is absent:
        /// `mount` creates the (missing) root directory before starting
        /// virtualization.
        #[test]
        #[ignore = "requires the Windows Client-ProjFS feature; performs a real mount"]
        fn mount_creates_root_and_projects() {
            let dir = TempDir::new().expect("temp");
            let store = ObjectStore::init(dir.path().join(".tack")).expect("init");
            let work = dir.path().join("work");
            std::fs::create_dir_all(&work).expect("mkdir work");
            std::fs::write(work.join("hello.txt"), b"world").expect("write");
            let root_tree =
                build_tree(&store, &work, &IgnoreRules::empty()).expect("build tree");

            let mount_point = dir.path().join("mnt");
            assert!(!mount_point.exists());
            let guard = super::mount(&mount_point, store, root_tree).expect("mount");
            assert!(mount_point.is_dir(), "mount must create the root directory");

            // The projected file is visible and hydrates on read.
            let projected = std::fs::read(mount_point.join("hello.txt")).expect("read projected");
            assert_eq!(projected, b"world");

            // Dropping the guard stops virtualization.
            drop(guard);
        }
    }
}

#[cfg(all(windows, feature = "projfs"))]
pub use imp::{TackProjection, mount};

// ── feature-off / non-Windows stub ────────────────────────────────────────────

#[cfg(not(all(windows, feature = "projfs")))]
mod stub {
    use std::path::Path;

    use crate::error::{Error, Result};
    use crate::hash::ObjectId;
    use crate::store::ObjectStore;

    /// Stub [`mount`] for builds without the `projfs` feature (or off Windows).
    ///
    /// Always returns [`Error::ProjfsUnavailable`] with instructions to rebuild
    /// with `--features projfs` and enable the Windows *Client-ProjFS* feature.
    /// This keeps every call site (notably `tack mount`) compiling on every
    /// platform and in the default build (`DESIGN.md §10`).
    ///
    /// # Errors
    ///
    /// Always returns [`Error::ProjfsUnavailable`].
    pub fn mount(_root: &Path, _store: ObjectStore, _root_tree: ObjectId) -> Result<()> {
        Err(Error::ProjfsUnavailable(
            "projfs projection is unavailable in this build: rebuild with `--features projfs` \
             on windows and enable the client-projfs feature with `enable-windowsoptionalfeature \
             -online -featurename client-projfs`"
                .to_string(),
        ))
    }
}

#[cfg(not(all(windows, feature = "projfs")))]
pub use stub::mount;
