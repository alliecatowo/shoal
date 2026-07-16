//! Hexagonal ports (docs/ROADMAP.md R4, scratch/audit-arch.md §2c).
//!
//! The evaluator (`shoal-eval`) is meant to be the pure domain core, but it
//! historically reached straight into the OS with scattered `std::fs`,
//! `std::process`, `std::time`, and secret-store calls. These traits are the
//! seam: the domain core holds a `dyn Port` for each effect family and the host
//! wires an adapter. The [`StdFs`], [`StdClock`], and [`StdOpener`] adapters
//! defined here perform *exactly* the calls the inline code did, so installing
//! them (the default) is byte-identical to the pre-ports behavior.
//!
//! Adapters that need other workspace crates (`Exec` over `shoal-exec`,
//! `SecretPort` over `shoal-secret`) keep their trait here but implement the
//! `Std*` adapter in `shoal-eval`, so `shoal-value` stays a leaf crate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Fs — filesystem port
// ---------------------------------------------------------------------------

/// Filesystem effects used by the evaluator's builtins, redirects, script
/// loading, and journal snapshots. Every method returns [`io::Result`] so the
/// call-sites keep their existing `io::Error`-based error mapping unchanged.
///
/// [`StdFs`] is the default adapter; it forwards each method to the identical
/// `std::fs` call the inline code used. A test can swap a fake to interpose on
/// reads/writes without touching the real filesystem.
pub trait Fs: Send + Sync {
    /// Read the entire contents of a file into a byte vector (`std::fs::read`).
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Read the entire contents of a file into a `String`
    /// (`std::fs::read_to_string`).
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// Write bytes to a file, truncating it first (`std::fs::write`).
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Append bytes to a file, creating it if absent (`OpenOptions` create +
    /// append + `write_all`).
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Create a file if absent, updating its mtime otherwise — the `touch`
    /// builtin's `OpenOptions::new().create(true).append(true).open`.
    fn touch(&self, path: &Path) -> io::Result<()>;
    /// Metadata following symlinks (`std::fs::metadata`).
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    /// Metadata without following symlinks (`std::fs::symlink_metadata`).
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    /// The (full) paths of a directory's entries (`std::fs::read_dir`, each
    /// entry's `.path()`). Order is unspecified, exactly as `read_dir` yields.
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    /// Create a single directory (`std::fs::create_dir`).
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    /// Create a directory and all parents (`std::fs::create_dir_all`).
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Remove a file (`std::fs::remove_file`).
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Remove a directory and its contents (`std::fs::remove_dir_all`).
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Rename/move a path (`std::fs::rename`).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Copy a file's contents (`std::fs::copy`).
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    /// Create a hard link (`std::fs::hard_link`).
    fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()>;
    /// Create a symbolic link (`std::os::unix::fs::symlink`).
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
}

/// The default [`Fs`] adapter: each method forwards to the identical `std::fs`
/// call the evaluator made inline before the port existed.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFs;

impl Fs for StdFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        fs::write(path, data)
    }
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(data))
    }
    fn touch(&self, path: &Path) -> io::Result<()> {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|_| ())
    }
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            out.push(entry?.path());
        }
        Ok(out)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        fs::copy(from, to)
    }
    fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
        fs::hard_link(src, dst)
    }
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}

// ---------------------------------------------------------------------------
// Clock — time port
// ---------------------------------------------------------------------------

/// Wall-clock time, isolated so journal timestamps are deterministic under test.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the Unix epoch, clamped to `i64::MAX`, matching the
    /// journal's original `SystemTime::now().duration_since(UNIX_EPOCH)` call.
    fn now_ns(&self) -> i64;
}

/// The default [`Clock`]: the system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdClock;

impl Clock for StdClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(i64::MAX as u128) as i64
    }
}

// ---------------------------------------------------------------------------
// Opener — `open <path>` port
// ---------------------------------------------------------------------------

/// The `open <path>` effect: hand a path to the desktop's default handler.
pub trait Opener: Send + Sync {
    /// Open `path` detached, returning an error message string on spawn failure
    /// (the caller wraps it in an `ErrorVal`).
    fn open(&self, path: &Path) -> Result<(), String>;
}

/// The default [`Opener`]: a detached `xdg-open` with null stdio, exactly as the
/// `open` builtin spawned it inline.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdOpener;

impl Opener for StdOpener {
    fn open(&self, path: &Path) -> Result<(), String> {
        std::process::Command::new("xdg-open")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("open: {e}"))
    }
}

// ---------------------------------------------------------------------------
// SecretPort — secret store read port
// ---------------------------------------------------------------------------

/// Read access to the secret store backing `secret.get(name)`. The trait lives
/// here; the concrete adapter (over `shoal-secret`) lives in `shoal-eval` so
/// `shoal-value` keeps no dependency on the secret crate.
pub trait SecretPort: Send + Sync {
    /// Fetch a secret's raw bytes by name. `Ok(None)` means "no such secret";
    /// `Err(msg)` is a store-open/permission failure (the caller maps it to a
    /// `permission` error).
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String>;
}

// ---------------------------------------------------------------------------
// BytesLoad — content-addressed bytes loader port (TDD §317)
// ---------------------------------------------------------------------------

/// Loads the full content behind a lazy, CAS-backed [`crate::Value::CasBytes`]
/// (TDD §317 disk-spill). A value produced when a command's captured output
/// overflowed the RAM cap holds one of these plus a bounded preview; methods
/// that need the whole bytes (`.str()`, `.save`, indexing, …) call [`load`]
/// on demand, while `.len` and `render` stay cheap and never load.
///
/// The trait lives here so `shoal-value` keeps no dependency on `shoal-journal`;
/// the concrete adapter (over `shoal_journal::Cas`) lives in `shoal-eval`. It is
/// `Send + Sync` so a ref-backed value is as freely shareable as any other.
///
/// [`load`]: BytesLoad::load
pub trait BytesLoad: Send + Sync {
    /// Materialize the full content. Errors are I/O or integrity failures
    /// (a missing/corrupt CAS blob); the caller maps them to an `io_error`.
    fn load(&self) -> std::io::Result<Vec<u8>>;
}
