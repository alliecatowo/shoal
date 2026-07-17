//! Capture mode: piped stdout/stderr, no controlling tty, child in its own
//! process group, both pipes drained concurrently (deadlock-free).

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::mem;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cancel::CancelToken;
use crate::status::decode_wait_status;
use crate::watcher::spawn_cancel_watcher;
use crate::which::resolve_program;
use crate::{CaptureSpill, ExecMode, ExecResult, ExecSpec, SpillConfig, StdinSpec};

/// A capture-mode child spawned for streaming consumption (background tasks,
/// `tail -f`, …). The caller drains [`StreamingChild::stdout`] /
/// [`StreamingChild::stderr`] and then calls [`StreamingChild::wait`].
///
/// Dropping a `StreamingChild` without waiting SIGKILLs its process group and
/// reaps it — the crate never leaves zombies behind.
pub struct StreamingChild {
    /// The child's process id (also its process-group id).
    pub pid: u32,
    /// The child's stdout pipe. Read to EOF for the full stream.
    pub stdout: Box<dyn Read + Send>,
    /// The child's stderr pipe. Read to EOF for the full stream.
    pub stderr: Box<dyn Read + Send>,
    inner: StreamInner,
}

impl StreamingChild {
    /// Wait for exit, honoring the same INT → TERM → KILL cancellation
    /// escalation as [`crate::run`] (both the token given at spawn time and
    /// the one given here are watched).
    ///
    /// `stdout`/`stderr` in the result are empty — the caller drained the
    /// readers. Any reader still held by this struct is dropped before
    /// waiting, so an undrained child blocked on a full pipe gets `EPIPE`
    /// rather than deadlocking the wait.
    ///
    /// # Errors
    ///
    /// Propagates OS errors from `waitpid`; these do not occur in normal
    /// operation. The child is reaped either way.
    pub fn wait(self, cancel: &CancelToken) -> io::Result<ExecResult> {
        let StreamingChild {
            stdout,
            stderr,
            mut inner,
            ..
        } = self;
        drop(stdout);
        drop(stderr);
        inner.wait_reap(cancel)
    }
}

/// Owns the OS child plus its helper threads; reaps on drop if needed.
struct StreamInner {
    child: Child,
    pgid: libc::pid_t,
    start: Instant,
    done: Arc<AtomicBool>,
    claimed: Arc<AtomicBool>,
    spawn_token: CancelToken,
    threads: Vec<JoinHandle<()>>,
    reaped: bool,
    /// What [`crate::sandbox::apply`] actually did before this child's exec,
    /// if `ExecSpec::sandbox` was set. Carried through to [`ExecResult`].
    enforcement: Option<shoal_leash::EnforcementStatus>,
}

impl StreamInner {
    fn wait_reap(&mut self, cancel: &CancelToken) -> io::Result<ExecResult> {
        if !self.spawn_token.same(cancel) {
            self.threads.push(spawn_cancel_watcher(
                self.pgid,
                vec![cancel.clone()],
                self.claimed.clone(),
                self.done.clone(),
            ));
        }
        let status = self.child.wait();
        self.reaped = status.is_ok();
        self.done.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let status = status?;
        let (code, signal) = decode_wait_status(status.into_raw());
        Ok(ExecResult {
            status: code,
            signal,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
            stdout_spill: None,
            dur: self.start.elapsed(),
            pid: self.child.id(),
            #[allow(clippy::cast_sign_loss)] // pgids are positive
            pgid: self.pgid as u32,
            // Capture mode has no controlling tty and thus no stop concept.
            stopped: false,
            enforcement: self.enforcement.take(),
        })
    }
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        if !self.reaped {
            // Dropped without wait(): kill the whole group and reap so the
            // crate never leaks zombies or runaway process trees.
            // SAFETY: signalling a process group is memory-safe.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
            let _ = self.child.wait();
        }
        self.done.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Spawn `spec` (which must be [`crate::ExecMode::Capture`]-shaped) with
/// piped stdout/stderr for streaming consumption.
///
/// The cancellation watcher starts immediately: tripping `cancel` while the
/// caller is still draining the pipes interrupts the child (INT → TERM →
/// KILL against its process group), which unblocks the reader at EOF.
///
/// # Errors
///
/// Program resolution failures ([`io::ErrorKind::NotFound`]), stdin-file open
/// failures, and spawn errors (including `E2BIG`) surface here.
pub fn spawn_capture(mut spec: ExecSpec, cancel: &CancelToken) -> io::Result<StreamingChild> {
    if spec.mode != ExecMode::Capture {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spawn_capture requires ExecMode::Capture",
        ));
    }
    let enforcement = crate::sandbox::apply(&mut spec)?;
    let program = resolve_program(&spec.argv, &spec.env, &spec.cwd)?;
    let mut cmd = Command::new(&program);
    cmd.args(&spec.argv[1..]);
    cmd.current_dir(&spec.cwd);
    cmd.env_clear();
    cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut stdin_bytes = None;
    let mut stdin_stream = None;
    match spec.stdin {
        StdinSpec::Null => {
            cmd.stdin(Stdio::null());
        }
        StdinSpec::Inherit => {
            cmd.stdin(Stdio::inherit());
        }
        StdinSpec::Bytes(b) => {
            cmd.stdin(Stdio::piped());
            stdin_bytes = Some(b);
        }
        StdinSpec::File(p) => {
            cmd.stdin(Stdio::from(std::fs::File::open(&p)?));
        }
        StdinSpec::Stream(stream) => {
            cmd.stdin(Stdio::piped());
            stdin_stream = Some(stream.take()?);
        }
    }

    // Child gets its own process group so cancellation can signal the whole
    // tree without touching the shell itself.
    // SAFETY: setpgid is async-signal-safe and allowed between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }

    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let pgid = pid as libc::pid_t;

    let done = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    if let Some(bytes) = stdin_bytes {
        let mut sink = child.stdin.take().expect("stdin was configured as piped");
        threads.push(thread::spawn(move || {
            // EPIPE (child exited before reading everything) is expected.
            let _ = sink.write_all(&bytes);
        }));
    }

    if let Some(rx) = stdin_stream {
        let mut sink = child.stdin.take().expect("stdin was configured as piped");
        let child_done = done.clone();
        threads.push(thread::spawn(move || {
            loop {
                match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(chunk) => {
                        if sink.write_all(&chunk).is_err() {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) if !child_done.load(Ordering::SeqCst) => {}
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
        }));
    }

    let claimed = Arc::new(AtomicBool::new(false));
    threads.push(spawn_cancel_watcher(
        pgid,
        vec![cancel.clone()],
        claimed.clone(),
        done.clone(),
    ));

    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    Ok(StreamingChild {
        pid,
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        inner: StreamInner {
            child,
            pgid,
            start,
            done,
            claimed,
            spawn_token: cancel.clone(),
            threads,
            reaped: false,
            enforcement,
        },
    })
}

/// Blocking capture run: drains stdout and stderr on two threads (so a child
/// filling both pipes can never deadlock), waits, and returns the collected
/// bytes. Each stream is bounded to [`crate::capture_hard_cap`] bytes in memory
/// (site/content/internals/language-conformance-contract.md) so an unbounded producer (`yes`, `cat /dev/zero`) cannot OOM the
/// shell.
///
/// stderr overflow is discarded (with [`ExecResult::truncated`] set), as before.
/// stdout is the value: when the spec requests a spill ([`ExecSpec::spill`]) and
/// stdout exceeds the RAM cap, the **full** stream is streamed to a
/// blake3-addressed disk file instead of being dropped (site/content/internals/language-conformance-contract.md disk-spill
/// promise) and returned as [`ExecResult::stdout_spill`]; the resident stdout is
/// then a bounded preview. With no spill requested, stdout behaves exactly as it
/// did before (bounded + `truncated`).
pub(crate) fn run_capture(spec: ExecSpec, cancel: &CancelToken) -> io::Result<ExecResult> {
    let cap = crate::capture_hard_cap();
    let spill = spec.spill.clone();
    let spill_cap = crate::capture_spill_cap();
    let mut child = spawn_capture(spec, cancel)?;
    let out = mem::replace(&mut child.stdout, Box::new(io::empty()));
    let err = mem::replace(&mut child.stderr, Box::new(io::empty()));
    let t_out = thread::spawn(move || drain_stdout(out, cap, spill, spill_cap));
    let t_err = thread::spawn(move || drain_capped(err, cap));
    let mut res = child.wait(cancel)?;
    let out = t_out.join().unwrap_or_else(|_| DrainOut {
        buf: Vec::new(),
        spill: None,
        truncated: false,
    });
    let (stderr, err_trunc) = t_err.join().unwrap_or_default();
    res.stdout = out.buf;
    res.stderr = stderr;
    res.truncated = out.truncated || err_trunc;
    res.stdout_spill = out.spill;
    Ok(res)
}

/// Read `r` to EOF, buffering at most `cap` bytes. Reading continues past the
/// cap (draining and discarding the overflow) so the child never blocks on a
/// full pipe; the returned flag is `true` when any byte was dropped.
fn drain_capped(mut r: Box<dyn Read + Send>, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (buf, truncated)
}

/// Outcome of draining a value-position stdout stream.
struct DrainOut {
    /// The resident bytes: the whole stream when it fit the RAM cap, else the
    /// bounded preview (the first `cap` bytes).
    buf: Vec<u8>,
    /// `Some` when the stream overflowed the RAM cap and was streamed to disk.
    spill: Option<CaptureSpill>,
    /// `true` when overflow was dropped with no spill (RAM floor, unchanged).
    truncated: bool,
}

/// Drain stdout in value position. Without `spill`, identical to
/// [`drain_capped`] (bounded RAM buffer, overflow dropped + flagged). With
/// `spill`, once the stream exceeds `cap` the **full** stream is streamed to a
/// blake3-addressed file under `spill.dir` (up to `spill_cap` bytes); the RAM
/// buffer is kept as a bounded preview (site/content/internals/language-conformance-contract.md).
///
/// Reading always continues to EOF so the child never blocks on a full pipe.
fn drain_stdout(
    mut r: Box<dyn Read + Send>,
    cap: usize,
    spill: Option<SpillConfig>,
    spill_cap: u64,
) -> DrainOut {
    // No spill requested → exact pre-spill behavior.
    let Some(spill) = spill else {
        let (buf, truncated) = drain_capped(r, cap);
        return DrainOut {
            buf,
            spill: None,
            truncated,
        };
    };

    let mut preview = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut hasher = blake3::Hasher::new();
    // Lazily created on first overflow so sub-cap output never touches disk.
    let mut sink: Option<SpillSink> = None;
    let mut total: u64 = 0; // bytes read from the child
    let mut stored: u64 = 0; // bytes written to the spill file (≤ spill_cap)
    let mut spill_truncated = false;

    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let data = &chunk[..n];
        // Grow the resident preview up to the RAM cap; remember how many of this
        // chunk's front bytes it absorbed so the spill doesn't double-write them.
        let into_preview = if preview.len() < cap {
            (cap - preview.len()).min(n)
        } else {
            0
        };
        if into_preview > 0 {
            preview.extend_from_slice(&data[..into_preview]);
        }
        total += n as u64;

        if sink.is_none() && total > cap as u64 {
            // First overflow: open the spill file and write the whole stream so
            // far. Everything before this chunk fit under the cap, so `preview`
            // holds it verbatim (the first `cap` bytes); the overflow tail is
            // this chunk's bytes past what the preview just absorbed. Together
            // they cover the stream from byte zero, so the hash is exact.
            match SpillSink::create(&spill.dir) {
                Ok(mut s) => {
                    let w1 = write_bounded(&mut s, &preview, spill_cap, &mut stored);
                    hasher.update(&preview[..w1]);
                    let tail = &data[into_preview..];
                    let w2 = write_bounded(&mut s, tail, spill_cap, &mut stored);
                    hasher.update(&tail[..w2]);
                    if w1 < preview.len() || w2 < tail.len() {
                        spill_truncated = true;
                    }
                    sink = Some(s);
                }
                // Spill couldn't be opened: fall back to the RAM floor. Keep
                // draining so the child still finishes, dropping the overflow.
                Err(_) => {
                    return drain_rest_no_spill(r, preview);
                }
            }
        } else if let Some(s) = sink.as_mut() {
            // Sink already open before this chunk: the preview is full, so the
            // whole chunk is overflow.
            let written = write_bounded(s, data, spill_cap, &mut stored);
            hasher.update(&data[..written]);
            if written < n {
                spill_truncated = true;
            }
        }
    }

    match sink {
        None => {
            // Never overflowed: the whole stream is resident, no spill.
            DrainOut {
                buf: preview,
                spill: None,
                truncated: false,
            }
        }
        Some(mut s) => {
            let ok = s.finish().is_ok();
            if !ok {
                // Flushing/closing the spill failed: don't hand back a partial
                // file as durable. Fall back to the preview + truncated flag.
                let _ = std::fs::remove_file(&s.path);
                return DrainOut {
                    buf: preview,
                    spill: None,
                    truncated: true,
                };
            }
            let hash = hasher.finalize().to_hex().to_string();
            DrainOut {
                buf: preview,
                spill: Some(CaptureSpill {
                    path: s.path,
                    hash,
                    len: stored,
                    truncated: spill_truncated,
                }),
                truncated: false,
            }
        }
    }
}

/// Drain the remainder of `r` (discarding it) after a spill failed to open,
/// preserving the already-buffered `preview` and flagging truncation.
fn drain_rest_no_spill(mut r: Box<dyn Read + Send>, preview: Vec<u8>) -> DrainOut {
    let mut chunk = [0u8; 65536];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    DrainOut {
        buf: preview,
        spill: None,
        truncated: true,
    }
}

/// Write `data` to the spill sink, but never let `*stored` exceed `spill_cap`.
/// Returns the number of bytes actually written (== the number that must be fed
/// to the content hasher, so the hash addresses exactly the stored bytes).
fn write_bounded(sink: &mut SpillSink, data: &[u8], spill_cap: u64, stored: &mut u64) -> usize {
    let room = spill_cap.saturating_sub(*stored);
    if room == 0 {
        return 0;
    }
    let take = (room as usize).min(data.len());
    // A write error stops storage (treated as cap reached) rather than aborting
    // the drain — the child must still be drained to EOF.
    if sink.write_all(&data[..take]).is_err() {
        return 0;
    }
    *stored += take as u64;
    take
}

/// A blake3-addressed spill file being written. Buffered so the many small
/// pipe-sized writes coalesce into large disk writes.
struct SpillSink {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl SpillSink {
    fn create(dir: &std::path::Path) -> io::Result<SpillSink> {
        // A unique name in the caller's spill dir; the caller adopts/deletes it.
        let named = tempfile::Builder::new()
            .prefix("capture-spill-")
            .tempfile_in(dir)?;
        let path = named.path().to_path_buf();
        // Detach from tempfile's auto-delete: ownership passes to the caller.
        let file = named.persist(&path).map_err(|e| e.error)?;
        Ok(SpillSink {
            writer: BufWriter::new(file),
            path,
        })
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)
    }

    fn finish(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bounded producer emitting more than `cap` bytes fills the buffer
    /// to exactly the cap and reports truncation — the buffer never grows past
    /// the bound, so an unbounded child can't OOM the shell.
    #[test]
    fn drain_capped_stops_at_cap_and_flags_truncation() {
        let cap = 4096;
        let producer = vec![b'x'; cap + 5000];
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(producer));
        let (buf, truncated) = drain_capped(r, cap);
        assert_eq!(buf.len(), cap, "buffer must stop growing at the cap");
        assert!(buf.iter().all(|&b| b == b'x'));
        assert!(truncated, "dropping overflow must set the truncated flag");
    }

    /// Output at or under the cap is captured whole with no truncation flag.
    #[test]
    fn drain_capped_keeps_output_within_cap() {
        let cap = 4096;
        let producer = vec![b'y'; 1000];
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(producer));
        let (buf, truncated) = drain_capped(r, cap);
        assert_eq!(buf.len(), 1000);
        assert!(!truncated);
    }

    /// Exactly-cap-sized output is not falsely flagged as truncated.
    #[test]
    fn drain_capped_exact_cap_is_not_truncated() {
        let cap = 4096;
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(vec![b'z'; cap]));
        let (buf, truncated) = drain_capped(r, cap);
        assert_eq!(buf.len(), cap);
        assert!(!truncated, "an exact fit is complete, not truncated");
    }

    /// site/content/internals/process-execution.md disk-spill: a stream past the RAM cap streams the FULL content to a
    /// blake3-addressed file, keeps a bounded preview, and reports the true len
    /// and hash — nothing is lost (contrast `drain_capped`, which drops it).
    #[test]
    fn drain_stdout_spills_full_stream_to_disk() {
        let cap = 4096;
        let dir = tempfile::tempdir().unwrap();
        let payload = vec![b'x'; cap + 5000];
        let expect_hash = blake3::hash(&payload).to_hex().to_string();
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload.clone()));
        let out = drain_stdout(
            r,
            cap,
            Some(SpillConfig {
                dir: dir.path().to_path_buf(),
            }),
            1 << 30,
        );
        assert!(!out.truncated, "spilled output is preserved, not truncated");
        assert_eq!(out.buf.len(), cap, "resident buffer is a bounded preview");
        let spill = out.spill.expect("overflow must produce a spill");
        assert_eq!(spill.len, payload.len() as u64, "spill len is the TRUE len");
        assert!(!spill.truncated);
        assert_eq!(spill.hash, expect_hash, "hash addresses the full content");
        // The on-disk file is exactly the full stream.
        let on_disk = std::fs::read(&spill.path).unwrap();
        assert_eq!(on_disk, payload);
        assert_eq!(blake3::hash(&on_disk).to_hex().to_string(), expect_hash);
    }

    /// Sub-cap output with a spill configured stays fully resident and never
    /// touches disk — zero regression for the common case.
    #[test]
    fn drain_stdout_under_cap_stays_resident_no_spill() {
        let cap = 4096;
        let dir = tempfile::tempdir().unwrap();
        let payload = vec![b'y'; 1000];
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload.clone()));
        let out = drain_stdout(
            r,
            cap,
            Some(SpillConfig {
                dir: dir.path().to_path_buf(),
            }),
            1 << 30,
        );
        assert_eq!(out.buf, payload);
        assert!(out.spill.is_none(), "sub-cap output must not spill");
        assert!(!out.truncated);
        // The spill dir stays empty (no file created).
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// A spill that itself exceeds `spill_cap` is bounded on disk too, and flags
    /// its own truncation — `let x = (yes)` fills neither RAM nor disk.
    #[test]
    fn drain_stdout_spill_is_bounded_by_spill_cap() {
        let cap = 1024;
        let spill_cap = 8192u64;
        let dir = tempfile::tempdir().unwrap();
        let payload = vec![b'z'; 100_000];
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload));
        let out = drain_stdout(
            r,
            cap,
            Some(SpillConfig {
                dir: dir.path().to_path_buf(),
            }),
            spill_cap,
        );
        let spill = out.spill.expect("overflow must produce a spill");
        assert_eq!(spill.len, spill_cap, "disk is bounded by the spill cap");
        assert!(spill.truncated, "hitting the spill cap flags truncation");
        let on_disk = std::fs::read(&spill.path).unwrap();
        assert_eq!(on_disk.len(), spill_cap as usize);
        assert_eq!(spill.hash, blake3::hash(&on_disk).to_hex().to_string());
    }

    /// With no spill configured, stdout draining is byte-identical to the
    /// pre-spill `drain_capped` behavior: bounded + flagged.
    #[test]
    fn drain_stdout_without_spill_matches_capped() {
        let cap = 4096;
        let payload = vec![b'x'; cap + 100];
        let r: Box<dyn Read + Send> = Box::new(io::Cursor::new(payload));
        let out = drain_stdout(r, cap, None, 1 << 30);
        assert_eq!(out.buf.len(), cap);
        assert!(out.truncated);
        assert!(out.spill.is_none());
    }

    /// The configurable cap resolves (default or override) to a positive bound.
    #[test]
    fn capture_hard_cap_is_positive_and_overridable() {
        assert!(crate::capture_hard_cap() > 0);
        crate::set_capture_hard_cap(1234);
        assert_eq!(crate::capture_hard_cap(), 1234);
        crate::set_capture_hard_cap(0);
        assert_eq!(
            crate::capture_hard_cap(),
            1,
            "zero is clamped to a positive cap"
        );
        // Restore the resolved default so other tests in this binary are unaffected.
        crate::set_capture_hard_cap(crate::DEFAULT_CAPTURE_HARD_CAP);
    }
}
