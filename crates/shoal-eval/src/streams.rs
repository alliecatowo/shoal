//! System-populated stream sources (`watch`/`tail`/`every`). See
//! `site/content/internals/streams-channels.md`.
//! Each returns a lazy `stream<T>` over a live, channel-backed source fed by a
//! background producer. The producer sends into a **bounded** `sync_channel`
//! (bounded buffers, never unbounded) wrapped by
//! [`StreamVal::from_buffered_channel`]; when the consumer drops the stream (a
//! satisfied `.take`, `Ctrl-C`, end of `.each`), an explicit stop flag wakes
//! even idle producers, releasing the OS resource (timer / inotify / kqueue
//! watch) and its process-safe worker lease.
//!
//! Backpressure discipline: a producer never blocks on a slow
//! consumer and never buffers without bound — it `try_send`s, and on a full
//! buffer applies each source's coalesce/drop contract: `every` drops the tick
//! outright (ticks coalesce, no marker), `watch` owes a single
//! `{coalesced: true}` summary event, and `tail` counts dropped lines and
//! owes a `{dropped: n}` marker element. The only blocking sends are
//! for stream-level *errors*, which are rare and must not be lost.
//!
//! These sources are timing/IO-dependent, so they are unit-tested here and in
//! `tests/streams.rs` rather than in the host-safe conformance corpus.
//! The default injected watch port uses `notify` (inotify on Linux,
//! FSEvents/kqueue on macOS), never a poll loop in language space.

mod buffer;

use crate::{Evaluator, WatchKind, WatchPoll, WatchSubscription};
use shoal_value::{
    ErrorVal, Fs, FsFileSnapshot, Record, StreamGap, StreamGapReason, StreamVal, VResult, Value,
};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

/// Consumer-facing buffer cap for `watch` and `tail` (site/content/internals/streams-channels.md —
/// "bounded ring buffer" / "bounded line buffer"). The spec sizes `watch`'s
/// ring "to the kernel's underlying event-queue limit (platform-dependent)"
/// without naming a number; 64 is
/// that documented default here for both. `every` instead uses a 1-slot
/// buffer (site/content/internals/streams-channels.md: ticks coalesce, memory O(1) always).
const SOURCE_BUF: usize = 64;
/// A live tail line has the same per-line wall as a lazy CAS line stream.
/// Framing bytes are not included; admission happens before retaining bytes.
const MAX_TAIL_LINE_BYTES: usize = 1024 * 1024;
const PUMP_POLL: Duration = Duration::from_millis(25);
const MAX_STREAM_PUMPS: usize = 64;
static ACTIVE_STREAM_PUMPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct StreamPumpLease {
    counter: &'static AtomicUsize,
}

impl Drop for StreamPumpLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

fn try_acquire_stream_pump(
    counter: &'static AtomicUsize,
    limit: usize,
) -> VResult<StreamPumpLease> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
            (active < limit).then_some(active + 1)
        })
        .map(|_| StreamPumpLease { counter })
        .map_err(|_| {
            ErrorVal::new(
                "stream_pump_limit",
                format!("stream worker limit reached ({limit})"),
            )
            .with_hint(
                "consume or drop an existing live/buffered/feed stream before creating another",
            )
        })
}

pub(crate) fn acquire_stream_pump() -> VResult<StreamPumpLease> {
    try_acquire_stream_pump(&ACTIVE_STREAM_PUMPS, MAX_STREAM_PUMPS)
}

impl Evaluator {
    /// `every(interval) -> stream<datetime>` (site/content/internals/streams-channels.md): one timer thread,
    /// ticking the wall-clock `datetime` of each firing into a **1-slot
    /// bounded buffer** (`sync_channel(1)` + `try_send`). Ticks are never
    /// queued past that slot: a tick that fires while the slot is still
    /// occupied (a slow consumer) is dropped — coalesced away — so
    /// memory is O(1) and a stalled consumer resumes with at most one buffered
    /// tick, then live ones. One implementation detail differs from the prose
    /// contract: the buffered tick is
    /// the *earliest* undelivered one (at most one interval stale), not "the
    /// latest missed" — a 1-slot `try_send` buffer cannot replace its
    /// occupant. Ticks are indistinguishable apart from their timestamp, so no
    /// marker is emitted, as required by `site/content/internals/streams-channels.md`.
    pub(crate) fn source_every(&self, interval: Duration) -> VResult<Value> {
        self.source_every_with_budget(interval, &ACTIVE_STREAM_PUMPS, MAX_STREAM_PUMPS)
    }

    fn source_every_with_budget(
        &self,
        interval: Duration,
        counter: &'static AtomicUsize,
        limit: usize,
    ) -> VResult<Value> {
        if interval.is_zero() {
            return Err(ErrorVal::arg_error("every expects a positive interval"));
        }
        let lease = try_acquire_stream_pump(counter, limit)?;
        let (tx, rx) = sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("shoal-stream-every".into())
            .spawn(move || {
                let _lease = lease;
                while wait_interval_or_stop(interval, &worker_stop) {
                    let now = jiff::Zoned::now();
                    match tx.try_send(Ok(Value::DateTime(Box::new(now)))) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {} // consumer busy → tick coalesced away
                        Err(TrySendError::Disconnected(_)) => break, // consumer gone → stop the timer
                    }
                }
            })
            .map_err(|error| {
                ErrorVal::new("io_error", format!("spawn every stream worker: {error}"))
            })?;
        Ok(Value::Stream(StreamVal::from_buffered_channel(
            "datetime",
            false,
            rx,
            stop,
            Box::new(|| {}),
        )))
    }

    /// `watch(target, recursive: bool = true) -> stream<event>` (site/content/internals/streams-channels.md).
    /// `target` may be a path or a glob; a glob watches its directory prefix and
    /// filters events by the compiled pattern. Elements are
    /// `{path, kind: "created"|"modified"|"removed", ts}`. The consumer-facing
    /// buffer is bounded ([`SOURCE_BUF`]); a burst faster than the consumer
    /// drains coalesces to a single `{path: root, kind: "modified", ts,
    /// coalesced: true}` summary event (site/content/internals/streams-channels.md) via [`send_coalescing`].
    pub(crate) fn source_watch(&self, target: &Value, recursive: bool) -> VResult<Value> {
        let (root, matcher) = self.watch_root_and_matcher(target)?;
        if !self.host.fs.exists(&root) {
            return Err(ErrorVal::new(
                "not_found",
                format!("watch target does not exist: {}", root.display()),
            ));
        }
        let (tx, rx) = sync_channel::<VResult<Value>>(SOURCE_BUF);
        let lease = acquire_stream_pump()?;
        let mut subscription = self
            .host
            .watch
            .subscribe(&root, recursive)
            .map_err(|error| ErrorVal::new("io_error", format!("watch: {error}")))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("shoal-stream-watch".into())
            .spawn(move || {
                let _lease = lease;
                // Coalesce state: `true` when at least one event was dropped on a
                // full buffer and the consumer is owed the documented coalescing summary event.
                let mut coalesce_owed = 0u64;
                while !worker_stop.load(Ordering::SeqCst) {
                    let event = match subscription.poll(PUMP_POLL) {
                        WatchPoll::Event(event) => event,
                        WatchPoll::Overflow(dropped) => {
                            coalesce_owed = coalesce_owed.saturating_add(dropped);
                            if !flush_coalesced(&tx, &mut coalesce_owed, &root) {
                                break;
                            }
                            continue;
                        }
                        WatchPoll::Error(error) => {
                            // Errors are rare and must not be lost: a blocking
                            // send here is bounded by the consumer's next pull
                            // (and fails immediately if the consumer is gone).
                            if tx
                                .send(Err(ErrorVal::new("io_error", format!("watch: {error}"))))
                                .is_err()
                            {
                                break;
                            }
                            continue;
                        }
                        WatchPoll::Timeout => {
                            // A full consumer buffer can defer the gap marker. Retry on
                            // idle polls so the marker is eventually visible even when no
                            // later filesystem event arrives to drive another flush.
                            if !flush_coalesced(&tx, &mut coalesce_owed, &root) {
                                break;
                            }
                            continue;
                        }
                        WatchPoll::Closed => break,
                    };
                    let Some(kind) = event_kind(event.kind) else {
                        continue;
                    };
                    let mut consumer_gone = false;
                    for path in &event.paths {
                        if let Some(m) = &matcher
                            && !m.matches_path(path)
                        {
                            continue;
                        }
                        if !send_coalescing(&tx, &mut coalesce_owed, &root, watch_event(path, kind))
                        {
                            consumer_gone = true;
                            break;
                        }
                    }
                    if consumer_gone {
                        break; // consumer gone → drop the watch
                    }
                }
            })
            .map_err(|error| {
                ErrorVal::new("io_error", format!("spawn watch stream worker: {error}"))
            })?;
        Ok(Value::Stream(StreamVal::from_buffered_channel(
            "event",
            false,
            rx,
            stop,
            Box::new(|| {}),
        )))
    }

    /// `tail(file, from_start: bool = false) -> stream<str>` (site/content/internals/streams-channels.md):
    /// follows appends to `file` line-by-line, event-driven via `notify`. Seeks to
    /// EOF by default (matching `tail -f`), or byte 0 with `from_start: true`.
    /// Detects truncation/rotation (size shrank → re-read from the new start).
    /// The consumer-facing line buffer is bounded ([`SOURCE_BUF`]); an unread
    /// backlog beyond the cap is dropped and coalesced to a `{dropped: n}`
    /// line-count marker element (site/content/internals/streams-channels.md) via [`send_line_bounded`].
    pub(crate) fn source_tail(&self, file: &Value, from_start: bool) -> VResult<Value> {
        let path = self.stream_path_arg(file)?;
        if !self.host.fs.exists(&path) {
            return Err(ErrorVal::new(
                "not_found",
                format!("tail target does not exist: {}", path.display()),
            ));
        }
        let (tx, rx) = sync_channel::<VResult<Value>>(SOURCE_BUF);
        // The tail producer reads through the `Fs` port (not `std::fs` directly)
        // so the streaming source is interposable/fakeable like every other
        // filesystem effect; the `Arc` clone rides into the producer thread.
        let fs = self.host.fs.clone();
        let lease = acquire_stream_pump()?;
        let subscription = self
            .host
            .watch
            .subscribe(&path, false)
            .map_err(|error| ErrorVal::new("io_error", format!("tail: {error}")))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("shoal-stream-tail".into())
            .spawn(move || {
                let _lease = lease;
                tail_loop(&fs, subscription, &path, from_start, &tx, &worker_stop);
            })
            .map_err(|error| {
                ErrorVal::new("io_error", format!("spawn tail stream worker: {error}"))
            })?;
        Ok(Value::Stream(StreamVal::from_buffered_channel(
            "str",
            false,
            rx,
            stop,
            Box::new(|| {}),
        )))
    }

    /// Split a `watch` target into the directory to watch and an optional glob
    /// matcher. A plain path watches itself; a glob watches its literal directory
    /// prefix and filters events by the whole (absolute) pattern.
    fn watch_root_and_matcher(&self, target: &Value) -> VResult<(PathBuf, Option<glob::Pattern>)> {
        let pattern = match target {
            Value::Glob(g) => g.pattern.clone(),
            Value::Path(p) => return Ok((self.abs(p.clone()), None)),
            Value::Str(s) if !s.contains(['*', '?', '[']) => {
                return Ok((self.abs(PathBuf::from(s)), None));
            }
            Value::Str(s) => s.clone(),
            v => {
                return Err(ErrorVal::type_error(format!(
                    "watch expects a path or glob, found {}",
                    v.type_name()
                )));
            }
        };
        let root = self.abs(glob_prefix(&pattern));
        let abs_pattern = self.abs(PathBuf::from(&pattern));
        let compiled = glob::Pattern::new(&abs_pattern.to_string_lossy())
            .map_err(|e| ErrorVal::new("arg_error", format!("watch: invalid glob: {e}")))?;
        Ok((root, Some(compiled)))
    }

    fn stream_path_arg(&self, v: &Value) -> VResult<PathBuf> {
        match v {
            Value::Path(p) => Ok(self.abs(p.clone())),
            Value::Str(s) => Ok(self.abs(PathBuf::from(s))),
            v => Err(ErrorVal::type_error(format!(
                "expected a path, found {}",
                v.type_name()
            ))),
        }
    }

    fn abs(&self, p: PathBuf) -> PathBuf {
        if p.is_absolute() {
            p
        } else {
            self.exec.shell.cwd.join(p)
        }
    }
}

fn wait_interval_or_stop(interval: Duration, stop: &AtomicBool) -> bool {
    let deadline = Instant::now().checked_add(interval);
    let Some(deadline) = deadline else {
        return false;
    };
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        std::thread::park_timeout(remaining.min(PUMP_POLL));
    }
}

/// Map a watch-port event kind onto shoal's closed `kind` enum, dropping
/// access/other events that carry no create/modify/remove meaning.
fn event_kind(kind: WatchKind) -> Option<&'static str> {
    match kind {
        WatchKind::Created => Some("created"),
        WatchKind::Modified => Some("modified"),
        WatchKind::Removed => Some("removed"),
        WatchKind::Other => None,
    }
}

fn watch_event(path: &Path, kind: &str) -> Value {
    let mut r = Record::new();
    r.insert("path".into(), Value::Path(path.to_path_buf()));
    r.insert("kind".into(), Value::Str(kind.to_string()));
    r.insert("ts".into(), now_datetime());
    Value::Record(r)
}

fn now_datetime() -> Value {
    Value::DateTime(Box::new(jiff::Zoned::now()))
}

/// The longest literal directory prefix of a glob pattern (the directory to root
/// a `watch` at). `src/**/*.rs` → `src`; `*.log` → `.` (cwd, resolved by caller).
fn glob_prefix(pattern: &str) -> PathBuf {
    let mut dir = PathBuf::new();
    for comp in Path::new(pattern).components() {
        let s = comp.as_os_str().to_string_lossy();
        if s.contains(['*', '?', '[']) {
            break;
        }
        dir.push(comp);
    }
    if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    }
}

/// The tail producer: seek, then wait on `notify` for appends, reading whole
/// lines as they land. Exits when the consumer drops `tx`.
fn tail_loop(
    fs: &Arc<dyn Fs>,
    mut subscription: Box<dyn WatchSubscription>,
    path: &Path,
    from_start: bool,
    tx: &SyncSender<VResult<Value>>,
    stop: &AtomicBool,
) {
    let mut file = match fs.open_read(path) {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.send(Err(ErrorVal::new("io_error", format!("tail: {e}"))));
            return;
        }
    };
    let mut pos = if from_start {
        0
    } else {
        file.seek(SeekFrom::End(0)).unwrap_or(0)
    };
    let mut snapshot: Option<FsFileSnapshot> = fs.file_snapshot(path).ok();
    // Lines dropped on a full consumer buffer, owed to the consumer as a
    // single `{dropped: n}` marker element once room appears (site/content/internals/streams-channels.md).
    let mut dropped: u64 = 0;

    // Read whatever is already available past `pos` (the from_start backlog, or
    // any bytes written between open and the first event).
    if !read_new_lines(fs, path, &mut pos, tx, &mut dropped) {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match subscription.poll(PUMP_POLL) {
            WatchPoll::Event(event)
                if matches!(event.kind, WatchKind::Modified | WatchKind::Created) =>
            {
                // Truncation or replacement: restart even when the replacement
                // inode is longer than the old offset (size alone would skip
                // its prefix).
                if let Ok(current) = fs.file_snapshot(path) {
                    if snapshot
                        .as_ref()
                        .is_some_and(|prior| !prior.same_file(&current))
                        || current.len() < pos
                    {
                        pos = 0;
                    }
                    snapshot = Some(current);
                }
                if !read_new_lines(fs, path, &mut pos, tx, &mut dropped) {
                    break;
                }
            }
            WatchPoll::Overflow(_) => {
                if !read_new_lines(fs, path, &mut pos, tx, &mut dropped) {
                    break;
                }
            }
            WatchPoll::Event(_) | WatchPoll::Timeout => {}
            WatchPoll::Closed => break,
            WatchPoll::Error(error) => {
                if tx
                    .send(Err(ErrorVal::new("io_error", format!("tail: {error}"))))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Read complete lines from `path` starting at `*pos`, advancing `*pos` to just
/// past the last full line. A trailing partial line (no `\n`) is left unread.
/// Each logical line is admitted incrementally against
/// [`MAX_TAIL_LINE_BYTES`], so an unterminated hostile line cannot make the
/// worker allocate in proportion to the file. An oversized line emits the
/// stable `stream_line_limit` error and terminates this tail worker.
fn read_new_lines(
    fs: &Arc<dyn Fs>,
    path: &Path,
    pos: &mut u64,
    tx: &SyncSender<VResult<Value>>,
    dropped: &mut u64,
) -> bool {
    let mut file = match fs.open_read(path) {
        Ok(f) => f,
        Err(_) => return true,
    };
    if file.seek(SeekFrom::Start(*pos)).is_err() {
        return true;
    }
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut consumed = 0u64;
        loop {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(_) => return true,
            };
            if available.is_empty() {
                // Partial trailing line: leave `pos` at the beginning so the
                // next append retries it with its eventual delimiter.
                return true;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let content_len = newline.unwrap_or(available.len());
            if line.len().saturating_add(content_len) > MAX_TAIL_LINE_BYTES {
                let error = ErrorVal::new(
                    "stream_line_limit",
                    format!("tail line exceeds its {MAX_TAIL_LINE_BYTES}-byte limit"),
                )
                .with_hint("write line-framed records smaller than 1 MiB");
                let _ = tx.send(Err(error));
                return false;
            }
            line.extend_from_slice(&available[..content_len]);
            let take = content_len + usize::from(newline.is_some());
            reader.consume(take);
            consumed = consumed.saturating_add(take as u64);
            if newline.is_none() {
                continue;
            }

            *pos = pos.saturating_add(consumed);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let value = String::from_utf8_lossy(&line).into_owned();
            if !send_line_bounded(tx, dropped, value) {
                return false;
            }
            break;
        }
    }
}

/// Send one tail line into the bounded consumer buffer (site/content/internals/streams-channels.md). A line that
/// finds the buffer full is DROPPED (not blocked-on) and counted; the debt is
/// flushed as a single `{dropped: n}` marker element as soon as there is room.
/// Returns `false` only when the consumer is gone (so the caller stops).
fn send_line_bounded(tx: &SyncSender<VResult<Value>>, dropped: &mut u64, line: String) -> bool {
    if *dropped > 0 {
        match tx.try_send(Ok(dropped_marker(*dropped))) {
            Ok(()) => *dropped = 0,
            Err(TrySendError::Full(_)) => {
                // Still no room — this line joins the dropped debt.
                *dropped += 1;
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
    match tx.try_send(Ok(Value::Str(line))) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            *dropped += 1;
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// The `{dropped: n}` overflow marker element (site/content/internals/streams-channels.md) — a `tail` stream widens
/// to carry it structurally (a consumer distinguishes it by shape, e.g.
/// `.where(x => x.dropped == null)` to keep only real lines), mirroring how
/// `watch` surfaces overflow as a `coalesced: true` event.
fn dropped_marker(n: u64) -> Value {
    StreamGap::new(StreamGapReason::TailOverflow, n).into_value()
}

/// Forward a `watch` event into the bounded consumer buffer (site/content/internals/streams-channels.md). On a full
/// buffer the event is DROPPED and the consumer is owed a single
/// `{path: root, kind: "modified", ts, coalesced: true}` summary, flushed once
/// room appears. Returns `false` only when the consumer is gone.
fn send_coalescing(
    tx: &SyncSender<VResult<Value>>,
    owed: &mut u64,
    root: &Path,
    event: Value,
) -> bool {
    if *owed > 0 {
        match tx.try_send(Ok(coalesced_event(root, *owed))) {
            Ok(()) => *owed = 0,
            Err(TrySendError::Full(_)) => {
                *owed = (*owed).saturating_add(1);
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
    match tx.try_send(Ok(event)) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            *owed = (*owed).saturating_add(1);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn flush_coalesced(tx: &SyncSender<VResult<Value>>, owed: &mut u64, root: &Path) -> bool {
    if *owed == 0 {
        return true;
    }
    match tx.try_send(Ok(coalesced_event(root, *owed))) {
        Ok(()) => {
            *owed = 0;
            true
        }
        Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// The `coalesced: true` summary event (site/content/internals/streams-channels.md) owed after a `watch` overflow.
fn coalesced_event(root: &Path, dropped: u64) -> Value {
    let mut r = StreamGap::new(StreamGapReason::WatchOverflow, dropped).into_record();
    r.insert("path".into(), Value::Path(root.to_path_buf()));
    r.insert("kind".into(), Value::Str("modified".into()));
    r.insert("ts".into(), now_datetime());
    r.insert("coalesced".into(), Value::Bool(true));
    Value::Record(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WatchPort;
    use shoal_value::{CallCtx, Pull, StdFs, Upstream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingSource {
        pulls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    struct IdleSource {
        dropped: Arc<AtomicBool>,
    }

    struct DenyWatch {
        calls: AtomicUsize,
    }

    struct ReplaceOnceSubscription {
        replacement: PathBuf,
        target: PathBuf,
        replaced: bool,
    }

    impl WatchSubscription for ReplaceOnceSubscription {
        fn poll(&mut self, _timeout: Duration) -> WatchPoll {
            if self.replaced {
                return WatchPoll::Closed;
            }
            std::fs::rename(&self.replacement, &self.target).expect("rotate tail fixture");
            self.replaced = true;
            WatchPoll::Event(crate::WatchEvent {
                kind: WatchKind::Modified,
                paths: vec![self.target.clone()],
            })
        }
    }

    impl WatchPort for DenyWatch {
        fn subscribe(
            &self,
            _path: &Path,
            _recursive: bool,
        ) -> Result<Box<dyn WatchSubscription>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err("watch capability denied".into())
        }
    }

    impl Upstream for IdleSource {
        fn pull(&mut self, _ctx: &mut dyn CallCtx, _timeout: Option<Duration>) -> VResult<Pull> {
            std::thread::sleep(Duration::from_millis(2));
            Ok(Pull::Timeout)
        }
    }

    impl Drop for IdleSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    impl Upstream for CountingSource {
        fn pull(&mut self, _ctx: &mut dyn CallCtx, _timeout: Option<Duration>) -> VResult<Pull> {
            let n = self.pulls.fetch_add(1, Ordering::SeqCst);
            Ok(Pull::Item(Value::Int(n as i64)))
        }
    }

    impl Drop for CountingSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while !ready() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(ready(), "background stream pump did not settle");
    }

    #[test]
    fn pump_leases_fail_closed_at_the_limit_and_recover_on_drop() {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);

        let first = try_acquire_stream_pump(&ACTIVE, 2).unwrap();
        let second = try_acquire_stream_pump(&ACTIVE, 2).unwrap();
        let err = try_acquire_stream_pump(&ACTIVE, 2).unwrap_err();
        assert_eq!(err.code, "stream_pump_limit");

        drop(first);
        let replacement = try_acquire_stream_pump(&ACTIVE, 2).unwrap();
        drop(second);
        drop(replacement);
        assert_eq!(ACTIVE.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn retained_idle_every_sources_hit_quota_and_dropping_reclaims_it() {
        static ACTIVE: AtomicUsize = AtomicUsize::new(0);
        let evaluator = Evaluator::new(std::env::temp_dir());
        let interval = Duration::from_secs(60 * 60);

        let first = evaluator
            .source_every_with_budget(interval, &ACTIVE, 2)
            .unwrap();
        let second = evaluator
            .source_every_with_budget(interval, &ACTIVE, 2)
            .unwrap();
        let error = evaluator
            .source_every_with_budget(interval, &ACTIVE, 2)
            .unwrap_err();
        assert_eq!(error.code, "stream_pump_limit");
        assert!(error.hint.as_deref().unwrap_or_default().contains("drop"));

        drop(first);
        wait_until(|| ACTIVE.load(Ordering::Relaxed) == 1);
        let replacement = evaluator
            .source_every_with_budget(interval, &ACTIVE, 2)
            .unwrap();
        drop(second);
        drop(replacement);
        wait_until(|| ACTIVE.load(Ordering::Relaxed) == 0);
    }

    #[test]
    fn owned_buffer_applies_exact_capacity_backpressure_and_stops_on_drop() {
        let pulls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let source = StreamVal::from_upstream(
            "int",
            false,
            Box::new(CountingSource {
                pulls: pulls.clone(),
                dropped: dropped.clone(),
            }),
        );
        let mut evaluator = Evaluator::new(std::env::temp_dir());
        let buffered = evaluator.spawn_stream_buffer(source, 2).unwrap();

        wait_until(|| pulls.load(Ordering::SeqCst) >= 3);
        assert_eq!(
            pulls.load(Ordering::SeqCst),
            3,
            "two queued items plus the producer's pending send is the hard bound"
        );
        drop(buffered);
        wait_until(|| dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn owned_buffer_rejects_capacity_before_consuming_and_bounds_value_bytes() {
        let source = StreamVal::from_iter("int", [Ok(Value::Int(1))].into_iter());
        let mut evaluator = Evaluator::new(std::env::temp_dir());
        assert_eq!(
            evaluator
                .spawn_stream_buffer_with_limits(source.clone(), 3, 2, 1024)
                .unwrap_err()
                .code,
            "arg_error"
        );
        let mut original = source.take_upstream().unwrap();
        assert!(matches!(
            original.pull(&mut evaluator, None).unwrap(),
            Pull::Item(Value::Int(1))
        ));

        let source = StreamVal::from_iter("str", [Ok(Value::Str("x".repeat(128)))].into_iter());
        let buffered = evaluator
            .spawn_stream_buffer_with_limits(source, 1, 2, 64)
            .unwrap();
        let mut upstream = buffered.take_upstream().unwrap();
        let error = match upstream.pull(&mut evaluator, None) {
            Err(error) => error,
            Ok(_) => panic!("oversized buffered value must fail"),
        };
        assert_eq!(error.code, "stream_buffer_limit");
    }

    #[test]
    fn owned_buffer_stops_while_backpressured_when_execution_is_cancelled() {
        let pulls = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let source = StreamVal::from_upstream(
            "int",
            false,
            Box::new(CountingSource {
                pulls: pulls.clone(),
                dropped: dropped.clone(),
            }),
        );
        let mut evaluator = Evaluator::new(std::env::temp_dir());
        let _buffered = evaluator.spawn_stream_buffer(source, 1).unwrap();
        wait_until(|| pulls.load(Ordering::SeqCst) >= 2);

        evaluator.cancel_current();
        wait_until(|| dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_idle_buffer_signals_its_owned_upstream_without_an_item() {
        let dropped = Arc::new(AtomicBool::new(false));
        let source = StreamVal::from_upstream(
            "idle",
            false,
            Box::new(IdleSource {
                dropped: dropped.clone(),
            }),
        );
        let mut evaluator = Evaluator::new(std::env::temp_dir());
        let buffered = evaluator.spawn_stream_buffer(source, 2).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        drop(buffered);
        wait_until(|| dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn watch_and_tail_registration_use_the_injected_capability() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("app.log");
        std::fs::write(&file, b"").unwrap();
        let watch = Arc::new(DenyWatch {
            calls: AtomicUsize::new(0),
        });
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_watch_port(watch.clone());

        let watch_error = evaluator
            .source_watch(&Value::Path(dir.path().to_path_buf()), true)
            .unwrap_err();
        assert_eq!(watch_error.code, "io_error");
        assert!(watch_error.msg.contains("watch capability denied"));

        let tail_error = evaluator
            .source_tail(&Value::Path(file), false)
            .unwrap_err();
        assert_eq!(tail_error.code, "io_error");
        assert!(tail_error.msg.contains("watch capability denied"));
        assert_eq!(watch.calls.load(Ordering::SeqCst), 2);

        let source = include_str!("streams.rs");
        assert!(!source.contains(&["notify::", "recommended_watcher"].concat()));
        assert!(!source.contains(&["watcher", ".watch("].concat()));
    }

    #[test]
    fn tail_line_is_bounded_before_an_unterminated_file_can_be_retained() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hostile.log");
        std::fs::write(&path, vec![b'x'; MAX_TAIL_LINE_BYTES + 1]).unwrap();
        let fs: Arc<dyn Fs> = Arc::new(StdFs);
        let (tx, rx) = sync_channel(1);
        let mut pos = 0;
        let mut dropped = 0;

        assert!(!read_new_lines(&fs, &path, &mut pos, &tx, &mut dropped));
        let error = rx.recv().unwrap().unwrap_err();
        assert_eq!(error.code, "stream_line_limit");
        assert_eq!(pos, 0, "an oversized partial line is never admitted");
    }

    #[test]
    fn tail_accepts_the_exact_line_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boundary.log");
        let mut bytes = vec![b'x'; MAX_TAIL_LINE_BYTES];
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();
        let fs: Arc<dyn Fs> = Arc::new(StdFs);
        let (tx, rx) = sync_channel(1);
        let mut pos = 0;
        let mut dropped = 0;

        assert!(read_new_lines(&fs, &path, &mut pos, &tx, &mut dropped));
        let Value::Str(line) = rx.recv().unwrap().unwrap() else {
            panic!("expected exact-boundary line");
        };
        assert_eq!(line.len(), MAX_TAIL_LINE_BYTES);
        assert_eq!(pos, (MAX_TAIL_LINE_BYTES + 1) as u64);
    }

    #[cfg(unix)]
    #[test]
    fn tail_restarts_when_a_longer_inode_replaces_the_open_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("app.log");
        let replacement = dir.path().join("replacement.log");
        std::fs::write(&target, b"old\n").unwrap();
        std::fs::write(&replacement, b"new-prefix-is-longer\n").unwrap();
        let subscription = Box::new(ReplaceOnceSubscription {
            replacement,
            target: target.clone(),
            replaced: false,
        });
        let fs: Arc<dyn Fs> = Arc::new(StdFs);
        let (tx, rx) = sync_channel(4);
        let stop = AtomicBool::new(false);

        tail_loop(&fs, subscription, &target, true, &tx, &stop);
        drop(tx);
        let lines = rx
            .into_iter()
            .map(|result| result.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                Value::Str("old".into()),
                Value::Str("new-prefix-is-longer".into())
            ]
        );
    }

    #[test]
    fn deferred_watch_gap_survives_a_full_consumer_buffer() {
        let (tx, rx) = sync_channel(1);
        tx.try_send(Ok(Value::Int(7))).unwrap();
        let root = Path::new("/watched");
        let mut owed = 3;

        assert!(flush_coalesced(&tx, &mut owed, root));
        assert_eq!(owed, 3, "a full buffer must retain the overflow debt");
        assert_eq!(rx.recv().unwrap().unwrap(), Value::Int(7));

        assert!(flush_coalesced(&tx, &mut owed, root));
        assert_eq!(owed, 0);
        let Value::Record(gap) = rx.recv().unwrap().unwrap() else {
            panic!("expected a watch gap record");
        };
        assert_eq!(gap.get("marker"), Some(&Value::Str("stream_gap".into())));
        assert_eq!(gap.get("dropped"), Some(&Value::Int(3)));
    }
}
