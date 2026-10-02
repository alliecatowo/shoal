use super::{FsCopyDestination, FsCopySource, FsRemovalTree};
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

/// Identity of one directory entry captured without following its final
/// symbolic link. Unix identities use the kernel `(device, inode, type)`
/// tuple. Other adapters must refuse guarded mutation unless they can provide
/// an equivalent conditional guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    file_type: u32,
    #[cfg(not(unix))]
    len: u64,
    #[cfg(not(unix))]
    modified: Option<std::time::SystemTime>,
}

impl FsEntryIdentity {
    #[must_use]
    #[allow(clippy::unnecessary_cast)]
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                file_type: metadata.mode() & libc::S_IFMT as u32,
            }
        }
        #[cfg(not(unix))]
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }

    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast)]
    pub(crate) fn matches_stat(&self, stat: &libc::stat) -> bool {
        self.device == stat.st_dev as u64
            && self.inode == stat.st_ino as u64
            && self.file_type == (stat.st_mode as u32 & libc::S_IFMT as u32)
    }
}

// ---------------------------------------------------------------------------
// Fs — filesystem port
// ---------------------------------------------------------------------------

/// A readable **and seekable** byte source: the return type of
/// [`Fs::open_read`]. The `tail` stream source both seeks (to EOF, or to a
/// saved byte offset it advances across appends) and reads whole lines as they
/// land, so the port hands back a `Read + Seek` object rather than a bare
/// reader. The blanket impl makes every `Read + Seek` type (a real
/// `std::fs::File`, an in-memory `io::Cursor` in a test) a `ReadSeek` for free.
pub trait ReadSeek: io::Read + io::Seek {}
impl<T: io::Read + io::Seek> ReadSeek for T {}

/// Bounded metadata used to detect file replacement and mutation around a
/// mediated read. `identity` is a stable filesystem object identity on Unix
/// (`st_dev`, `st_ino`); adapters on other platforms may leave it absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsFileSnapshot {
    pub(super) len: u64,
    pub(super) modified: Option<std::time::SystemTime>,
    pub(super) identity: Option<(u64, u64)>,
}

impl FsFileSnapshot {
    pub(super) fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt as _;
            Some((metadata.dev(), metadata.ino()))
        };
        #[cfg(not(unix))]
        let identity = None;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity,
        }
    }

    /// Length observed with this metadata snapshot.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the observed file was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether both snapshots identify the same filesystem object. Platforms
    /// without a stable object identity conservatively return `true`; callers
    /// can still detect truncation by comparing [`len`](Self::len).
    #[must_use]
    pub fn same_file(&self, other: &Self) -> bool {
        match (self.identity, other.identity) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
    }
}

pub(super) fn snapshot_limit_error(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("file exceeds the {limit}-byte stable-read limit"),
    )
}

pub(super) fn snapshot_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "file changed while its stable snapshot was being read",
    )
}

pub(super) fn collect_bounded_stable(
    mut reader: impl io::Read,
    before: &FsFileSnapshot,
    limit: usize,
    after: impl FnOnce() -> io::Result<FsFileSnapshot>,
) -> io::Result<Vec<u8>> {
    if before.len() > limit as u64 {
        return Err(snapshot_limit_error(limit));
    }
    let mut bytes = Vec::with_capacity((before.len() as usize).min(limit));
    (&mut reader)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if before != &after()? {
        return Err(snapshot_changed_error());
    }
    if bytes.len() > limit {
        return Err(snapshot_limit_error(limit));
    }
    Ok(bytes)
}

/// Filesystem effects used by the evaluator's builtins, redirects, script
/// loading, and journal snapshots. Every method returns [`io::Result`] so the
/// call-sites keep their existing `io::Error`-based error mapping unchanged.
///
/// [`crate::StdFs`] is the default adapter; it forwards each method to the identical
/// `std::fs` call the inline code used. A test can swap a fake to interpose on
/// reads/writes without touching the real filesystem.
pub trait Fs: Send + Sync {
    /// Read the entire contents of a file into a byte vector (`std::fs::read`).
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Read the entire contents of a file into a `String`
    /// (`std::fs::read_to_string`).
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// Open a file for streaming, seekable reads (`std::fs::File::open`). Backs
    /// the `tail` source's incremental read loop, which seeks to EOF / a saved
    /// byte offset and then reads whole lines as they arrive.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>>;
    /// Capture bounded metadata for rotation/change detection. The default is
    /// capability-preserving and uses [`metadata`](Fs::metadata); production
    /// adapters can supply a stable object identity through `FsFileSnapshot`.
    fn file_snapshot(&self, path: &Path) -> io::Result<FsFileSnapshot> {
        self.metadata(path)
            .map(|metadata| FsFileSnapshot::from_metadata(&metadata))
    }
    /// Read at most `limit + 1` bytes and accept the result only when the file
    /// is unchanged across the read. The compatibility default remains fully
    /// mediated through this port. It detects a path swap by comparing the
    /// before/after snapshots; [`crate::StdFs`] strengthens this by taking both main
    /// snapshots from the opened descriptor.
    fn read_bounded_stable(&self, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
        let before = self.file_snapshot(path)?;
        let reader = self.open_read(path)?;
        collect_bounded_stable(reader, &before, limit, || self.file_snapshot(path))
    }
    /// Write bytes to a file, truncating it first (`std::fs::write`).
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Append bytes to a file, creating it if absent (`OpenOptions` create +
    /// append + `write_all`).
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Open a file for buffered, **incremental** appends, creating it if absent
    /// (`OpenOptions::new().create(true).append(true)`). Backs the stream
    /// `.save`/`.append` sink, which opens the file **once** and writes each
    /// item as it arrives (live logging) rather than buffering the whole stream
    /// — so it needs a long-lived writer, not the whole-buffer [`append`] above.
    ///
    /// [`crate::StdFs`] returns the real appended `File`, preserving the open-once /
    /// write-many syscall shape of the pre-port inline `OpenOptions` code. The
    /// default fails **closed** with [`io::ErrorKind::Unsupported`]: an adapter
    /// that mediates filesystem effects (a sandbox, a recording/denying test
    /// fake) must override this to interpose on streamed appends, and one that
    /// has not yet done so refuses the write rather than letting it escape the
    /// port.
    ///
    /// [`append`]: Fs::append
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not mediate streamed appends (open_append)",
        ))
    }
    /// Open a file for buffered, incremental replacement, creating it if
    /// absent and truncating it first. This is the overwrite counterpart to
    /// [`open_append`](Fs::open_append), used when a lazy blob must be copied
    /// without first materializing it in memory.
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not mediate streamed writes (open_write)",
        ))
    }
    /// Create a file if absent, updating its mtime otherwise — the `touch`
    /// builtin's `OpenOptions::new().create(true).append(true).open`.
    fn touch(&self, path: &Path) -> io::Result<()>;
    /// Metadata following symlinks (`std::fs::metadata`).
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    /// Metadata without following symlinks (`std::fs::symlink_metadata`).
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    /// Whether `path` exists, following symlinks — the port form of
    /// `Path::exists`. Never errors: a missing path or an IO failure is
    /// `false`, preserving `Path::exists`' fail-closed behavior.
    fn exists(&self, path: &Path) -> bool {
        self.metadata(path).is_ok()
    }
    /// Whether `path` is an existing regular file, following symlinks — the
    /// port form of `Path::is_file`. Never errors: a missing path or an IO
    /// failure is `false`. The default routes through [`metadata`](Fs::metadata)
    /// so it is byte-identical to `Path::is_file` under [`crate::StdFs`]; an in-memory
    /// adapter overrides it to answer from its own store.
    fn is_file(&self, path: &Path) -> bool {
        self.metadata(path).map(|m| m.is_file()).unwrap_or(false)
    }
    /// Whether `path` is an existing directory, following symlinks — the port
    /// form of `Path::is_dir`. Never errors: a missing path or an IO failure is
    /// `false`.
    fn is_dir(&self, path: &Path) -> bool {
        self.metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    }
    /// Resolve symlinks and normalize a path (`std::fs::canonicalize`). The
    /// default fails closed: an adapter that mediates filesystem probes must
    /// explicitly interpose on canonicalization rather than letting it escape
    /// to the ambient filesystem.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not mediate canonicalization",
        ))
    }
    /// Atomically replace `path` with `data` using a fully-written, fsynced
    /// temporary file in the same directory. The default fails closed because
    /// degrading undo restore to truncate-in-place would lose crash safety and
    /// expose a partial file.
    fn atomic_replace(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let _ = (path, data);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not mediate atomic replacement",
        ))
    }
    /// The (full) paths of a directory's entries (`std::fs::read_dir`, each
    /// entry's `.path()`). Order is unspecified, exactly as `read_dir` yields.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    /// Read at most `max_entries` directory entries, returning `InvalidData`
    /// if another entry exists. The default preserves compatibility for
    /// injected adapters; production [`crate::StdFs`] overrides it to enforce the
    /// wall while iterating rather than after an unbounded collection.
    fn read_dir_bounded(&self, path: &Path, max_entries: usize) -> io::Result<Vec<PathBuf>> {
        self.read_dir_limited(path, max_entries, usize::MAX)
    }
    /// Read a complete directory only when both its entry count and aggregate
    /// encoded path bytes fit. `InvalidData` means another entry would cross a
    /// wall. Production [`crate::StdFs`] checks before retaining each path; the
    /// compatibility default checks an adapter's already-materialized result.
    fn read_dir_limited(
        &self,
        path: &Path,
        max_entries: usize,
        max_path_bytes: usize,
    ) -> io::Result<Vec<PathBuf>> {
        let entries = self.read_dir(path)?;
        if entries.len() > max_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("directory contains more than {max_entries} admitted entries"),
            ));
        }
        let mut path_bytes = 0usize;
        for entry in &entries {
            path_bytes = path_bytes
                .checked_add(entry.as_os_str().as_encoded_bytes().len())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory path accounting overflowed",
                    )
                })?;
            if path_bytes > max_path_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("directory paths exceed {max_path_bytes} admitted bytes"),
                ));
            }
        }
        Ok(entries)
    }
    /// Read only the first `max_entries` directory entries. Unlike
    /// [`read_dir_bounded`](Fs::read_dir_bounded), a larger directory is not an
    /// error: this is for deliberately partial maintenance scans. The default
    /// preserves adapter compatibility but materializes before truncating;
    /// production [`crate::StdFs`] overrides it to stop the iterator at the prefix.
    fn read_dir_prefix(&self, path: &Path, max_entries: usize) -> io::Result<Vec<PathBuf>> {
        let mut entries = self.read_dir(path)?;
        entries.truncate(max_entries);
        Ok(entries)
    }
    /// Create a single directory (`std::fs::create_dir`).
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    /// Create a directory and all parents (`std::fs::create_dir_all`).
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Create a private directory tree for security-sensitive runtime data.
    /// Adapters without permission concepts may use ordinary directory
    /// creation; the production Unix adapter forces mode `0700` for newly
    /// created directories.
    fn create_private_dir_all(&self, path: &Path) -> io::Result<()> {
        self.create_dir_all(path)
    }
    /// Remove a file (`std::fs::remove_file`).
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Permanently remove a preflighted non-directory entry only after an
    /// fd-relative, no-replace move to a private sibling and post-move identity
    /// verification. The default refuses: a pathname `remove_file` would let a
    /// replacement win the final check-to-unlink race.
    fn remove_entry_if_unchanged(&self, path: &Path, expected: &FsEntryIdentity) -> io::Result<()> {
        let _ = (path, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not support conditional permanent leaf removal",
        ))
    }
    /// Remove a directory and its contents (`std::fs::remove_dir_all`).
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Rename/move a path (`std::fs::rename`).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Move a preflighted entry only when the moved object still has the
    /// expected no-follow identity. Implementations pin both parents and roll
    /// back drift (or contain the replacement at `to`). The default refuses;
    /// check-then-rename would recreate the race this method closes.
    fn rename_if_unchanged(
        &self,
        from: &Path,
        to: &Path,
        expected: &FsEntryIdentity,
    ) -> io::Result<()> {
        let _ = (from, to, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not support identity-guarded rename",
        ))
    }
    /// Pin and completely inventory a directory tree before permanent
    /// recursive removal. The returned capability validates the admitted tree
    /// and removes descendants relative to retained directory descriptors.
    /// The default refuses: falling back to pathname recursion would permit a
    /// concurrent descendant replacement to escape the admission decision.
    fn open_removal_tree(
        &self,
        path: &Path,
        expected: &FsEntryIdentity,
    ) -> io::Result<Box<dyn FsRemovalTree>> {
        let _ = (path, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not support pinned recursive removal",
        ))
    }
    /// Copy a file's contents without inheriting source timestamps.
    /// Portable permissions are applied separately through
    /// [`Fs::set_permissions`].
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    /// Pin a recursive-copy source entry to its opened filesystem object.
    ///
    /// The default refuses rather than degrading to a pathname that can be
    /// replaced between inventory and execution. Implementations must open
    /// children relative to a pinned directory and copy regular files from
    /// the retained handle; see [`FsCopySource`].
    fn open_copy_source(&self, path: &Path) -> io::Result<Box<dyn FsCopySource>> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not support pinned recursive-copy sources",
        ))
    }
    /// Pin a recursive-copy destination at its deepest existing directory.
    /// All descendant admission and mutation must remain relative to that
    /// retained descriptor. The default fails closed rather than falling back
    /// to pathname-based creation or overwrite.
    fn open_copy_destination(&self, path: &Path) -> io::Result<Box<dyn FsCopyDestination>> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not support pinned recursive-copy destinations",
        ))
    }
    /// Maximum number of operating-system handles a copy plan may retain,
    /// after reserving room for traversal, publication, and unrelated work.
    /// `None` means the adapter does not retain OS handles or has no finite
    /// descriptor ceiling. Production Unix adapters derive this from
    /// `RLIMIT_NOFILE` and the process's current descriptor count.
    fn copy_capability_budget(&self) -> io::Result<Option<usize>> {
        Ok(None)
    }
    /// Apply a source node's portable permissions after copying. The default
    /// fails closed so a filesystem-backed adapter cannot silently claim the
    /// recursive-copy metadata contract while discarding modes.
    fn set_permissions(&self, path: &Path, permissions: fs::Permissions) -> io::Result<()> {
        let _ = (path, permissions);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter does not mediate permission updates",
        ))
    }
    /// Report whether a path carries portable/user extended attributes.
    /// Recursive copy rejects such nodes because it cannot preserve them.
    /// Host-managed mandatory labels (for example Linux SELinux labels that
    /// are recreated by policy on every new file) are not portable metadata
    /// and the standard adapter excludes them. The default fails closed rather
    /// than treating an unknown adapter as metadata-free.
    fn has_extended_attributes(&self, path: &Path) -> io::Result<bool> {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this Fs adapter cannot inspect extended attributes",
        ))
    }
    /// Create a hard link (`std::fs::hard_link`).
    fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()>;
    /// Create a symbolic link (`std::os::unix::fs::symlink`).
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
}
