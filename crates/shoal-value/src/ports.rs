//! Hexagonal ports. See `site/content/internals/effects-plans-security.md`
//! and `site/content/internals/intercrate-protocol-contracts.md`.
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

use crate::{Record, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static ATOMIC_REPLACE_SEQ: AtomicU64 = AtomicU64::new(1);

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
    /// Open a file for streaming, seekable reads (`std::fs::File::open`). Backs
    /// the `tail` source's incremental read loop, which seeks to EOF / a saved
    /// byte offset and then reads whole lines as they arrive.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>>;
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
    /// [`StdFs`] returns the real appended `File`, preserving the open-once /
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
    /// so it is byte-identical to `Path::is_file` under [`StdFs`]; an in-memory
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
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
        Ok(Box::new(fs::File::open(path)?))
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
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Ok(Box::new(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        ))
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
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
    fn atomic_replace(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use io::Write as _;

        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
        let mut last_collision = None;
        for _ in 0..128 {
            let seq = ATOMIC_REPLACE_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = parent.join(format!(
                ".shoal-atomic-replace-{}-{seq}",
                std::process::id()
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = match options.open(&tmp) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let result = (|| {
                file.write_all(data)?;
                file.sync_all()?;
                drop(file);
                fs::rename(&tmp, path)?;
                // Persist the directory entry as well as the file contents.
                // Unix permits opening a directory for fsync; other platforms
                // do not expose one uniform directory-sync primitive.
                #[cfg(unix)]
                fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            return result;
        }
        Err(last_collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate atomic replacement temporary file",
            )
        }))
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
    #[cfg(unix)]
    fn create_private_dir_all(&self, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fn create_private_dir_all(&self, path: &Path) -> io::Result<()> {
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

#[cfg(test)]
mod fs_tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let seq = ATOMIC_REPLACE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "shoal-value-atomic-replace-test-{}-{seq}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create atomic-replace test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn replacement_temps(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .expect("read test directory")
            .map(|entry| entry.expect("read directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    name.as_encoded_bytes()
                        .starts_with(b".shoal-atomic-replace-")
                })
            })
            .collect()
    }

    #[test]
    fn atomic_replace_succeeds_and_failed_rename_cleans_its_temp() {
        let root = TestDir::new();
        let target = root.0.join("target");
        fs::write(&target, b"old").expect("write original");
        StdFs
            .atomic_replace(&target, b"new")
            .expect("replace and sync");
        assert_eq!(fs::read(&target).expect("read replacement"), b"new");
        assert!(replacement_temps(&root.0).is_empty());

        let directory_target = root.0.join("directory-target");
        fs::create_dir(&directory_target).expect("create rename-error target");
        StdFs
            .atomic_replace(&directory_target, b"must-not-land")
            .expect_err("a file cannot atomically replace a directory");
        assert!(directory_target.is_dir());
        assert!(
            replacement_temps(&root.0).is_empty(),
            "failed atomic replacement leaked a temporary file"
        );
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
/// `open` builtin spawned it inline. A bounded background owner reaps the
/// process (or kills it after a generous dispatch timeout), so repeated opens
/// cannot accumulate zombies or permanent waiter threads.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdOpener;

const OPENER_REAPER_TIMEOUT: Duration = Duration::from_secs(30);
type ChildOwner = Arc<Mutex<Option<std::process::Child>>>;

impl Opener for StdOpener {
    fn open(&self, path: &Path) -> Result<(), String> {
        let mut command = std::process::Command::new("xdg-open");
        command
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        spawn_detached_with(&mut command, OPENER_REAPER_TIMEOUT, |owner, timeout| {
            std::thread::Builder::new()
                .name("shoal-open-reaper".into())
                .spawn(move || reap_open_child(&owner, timeout))
                .map(|_| ())
        })
        .map(|_| ())
    }
}

fn spawn_detached_with(
    command: &mut std::process::Command,
    timeout: Duration,
    launch_reaper: impl FnOnce(ChildOwner, Duration) -> io::Result<()>,
) -> Result<u32, String> {
    let child = command.spawn().map_err(|error| format!("open: {error}"))?;
    let pid = child.id();
    let owner = Arc::new(Mutex::new(Some(child)));
    if let Err(error) = launch_reaper(owner.clone(), timeout) {
        // A failed thread launch never ran the closure, so ownership remains
        // here. Kill and reap this exact child synchronously before reporting
        // failure; dropping Child alone would leave a zombie after it exits.
        if let Some(mut child) = take_child(&owner) {
            kill_and_reap_exact(&mut child);
        }
        return Err(format!("open: cannot launch child reaper: {error}"));
    }
    Ok(pid)
}

fn reap_open_child(owner: &ChildOwner, timeout: Duration) {
    let Some(mut child) = take_child(owner) else {
        return;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if start.elapsed() >= timeout => {
                kill_and_reap_exact(&mut child);
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                kill_and_reap_exact(&mut child);
                return;
            }
        }
    }
}

fn take_child(owner: &ChildOwner) -> Option<std::process::Child> {
    match owner.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

fn kill_and_reap_exact(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod opener_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn process_is_gone(pid: u32) -> bool {
        // SAFETY: signal 0 only checks whether the recorded process exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn wait_until_gone(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !process_is_gone(pid) {
            assert!(
                Instant::now() < deadline,
                "opener child {pid} was not reaped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn quick_exit_is_reaped_by_background_owner() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let pid = spawn_detached_with(&mut command, Duration::from_secs(1), |owner, timeout| {
            std::thread::Builder::new()
                .spawn(move || reap_open_child(&owner, timeout))
                .map(|_| ())
        })
        .expect("spawn quick opener");
        wait_until_gone(pid);
    }

    #[test]
    fn bounded_reaper_kills_and_reaps_a_hung_opener() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let pid = spawn_detached_with(&mut command, Duration::from_millis(50), |owner, timeout| {
            std::thread::Builder::new()
                .spawn(move || reap_open_child(&owner, timeout))
                .map(|_| ())
        })
        .expect("spawn hung opener");
        wait_until_gone(pid);
    }

    #[test]
    fn reaper_launch_failure_kills_and_reaps_exact_spawned_child() {
        let observed_pid = Arc::new(AtomicU32::new(0));
        let observer = observed_pid.clone();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let error = spawn_detached_with(&mut command, Duration::from_secs(1), move |owner, _| {
            let pid = match owner.lock() {
                Ok(guard) => guard.as_ref().map(std::process::Child::id),
                Err(poisoned) => poisoned.into_inner().as_ref().map(std::process::Child::id),
            }
            .expect("launcher observes owned child");
            observer.store(pid, Ordering::SeqCst);
            Err(io::Error::other("injected thread exhaustion"))
        })
        .expect_err("reaper launch must fail closed");
        assert!(error.contains("child reaper"));
        let pid = observed_pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0);
        assert!(process_is_gone(pid));
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
// BytesLoad — content-addressed bytes loader port (site/content/internals/language-conformance-contract.md)
// ---------------------------------------------------------------------------

/// Loads the full content behind a lazy, CAS-backed [`crate::Value::CasBytes`]
/// (site/content/internals/language-conformance-contract.md disk-spill). A value produced when a command's captured output
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

    /// Open a bounded-memory reader over the content. Adapters backed by a
    /// real blob store override this to stream; the compatibility default
    /// materializes once and wraps a cursor, preserving existing embedders.
    fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        self.load()
            .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn std::io::Read + Send>)
    }
}

// ---------------------------------------------------------------------------
// ConfigPort — resolved-config snapshot read port
// ---------------------------------------------------------------------------

/// Read access to the resolved, host-applied configuration backing the
/// in-language `config` namespace (`config.get(key)`, `config.all`). The
/// evaluator holds a `dyn ConfigPort` and reads the snapshot from it instead
/// of walking the filesystem to re-parse `shoal.toml` on its own — which would
/// bypass the host's layering/env-override/validation (all of which live in
/// `shoal-config`, a crate `shoal-value`/`shoal-eval` deliberately do not
/// depend on). The host injects a [`ConfigSnapshot`] built from the *same*
/// resolved `Config` it applies to itself, so in-language `config.get` and the
/// host-applied config can never disagree.
///
/// The default adapter is the **empty** [`ConfigSnapshot`] (`Default`): a
/// kernel-less/`-c`/test evaluator that never had a config injected reports an
/// empty record, so `config.get(key)` degrades to `null` — never a filesystem
/// walk. This mirrors how the other ports degrade to their inert default.
pub trait ConfigPort: Send + Sync {
    /// The whole resolved config as a record [`Value`] (`config.all`);
    /// `config.get(key)` reads one top-level key out of it. An adapter with no
    /// injected config returns an empty record.
    fn snapshot(&self) -> &Value;
}

/// The default [`ConfigPort`] adapter: a plain resolved-config snapshot. Holds
/// the config as a record [`Value`] (what `config.all` returns); the host
/// builds one from `shoal_config::load`'s resolved `Config` and injects it via
/// `Evaluator::set_config`. [`ConfigSnapshot::default`] (an empty record) is
/// the no-config, zero-regression default the evaluator starts with.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    value: Value,
}

impl ConfigSnapshot {
    /// Wrap an already-resolved config record. `value` is normally a
    /// [`Value::Record`] (the serialized `Config`); anything else makes every
    /// `config.get(key)` resolve to `null`, exactly like an empty snapshot.
    pub fn new(value: Value) -> Self {
        Self { value }
    }

    /// The empty snapshot: an empty record. `config.get(key)` on it is always
    /// `null`, and `config.all` is `{}`.
    pub fn empty() -> Self {
        Self {
            value: Value::Record(Record::new()),
        }
    }
}

impl Default for ConfigSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl ConfigPort for ConfigSnapshot {
    fn snapshot(&self) -> &Value {
        &self.value
    }
}
