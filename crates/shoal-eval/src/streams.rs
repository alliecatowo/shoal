//! System-populated stream sources (`watch`/`tail`/`every`). See
//! `site/content/internals/streams-channels.md`.
//! Each returns a lazy `stream<T>` over a live, channel-backed source fed by a
//! background producer. The producer sends into a **bounded** `sync_channel`
//! (bounded buffers, never unbounded) wrapped by
//! [`StreamVal::from_channel`]; when the consumer drops the stream (a satisfied
//! `.take`, `Ctrl-C`, end of `.each`), the receiver drops, the producer's send
//! fails, and it exits — releasing the OS resource (timer / inotify / kqueue
//! watch). This provides sink-to-source cancellation for free.
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
//! `watch`/`tail` use the `notify` crate (inotify on Linux, FSEvents/kqueue on
//! macOS — cross-platform, mac first-class), never a poll loop in language
//! space.

use crate::{ChildKind, Evaluator};
use notify::{EventKind, RecursiveMode, Watcher};
use shoal_exec::CancelToken;
use shoal_value::{
    ErrorVal, Fs, Pull, Record, StreamGap, StreamGapReason, StreamVal, VResult, Value,
};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, channel, sync_channel};
use std::time::Duration;

/// Consumer-facing buffer cap for `watch` and `tail` (site/content/internals/streams-channels.md —
/// "bounded ring buffer" / "bounded line buffer"). The spec sizes `watch`'s
/// ring "to the kernel's underlying event-queue limit (platform-dependent)"
/// without naming a number; 64 is
/// that documented default here for both. `every` instead uses a 1-slot
/// buffer (site/content/internals/streams-channels.md: ticks coalesce, memory O(1) always).
const SOURCE_BUF: usize = 64;
const PUMP_POLL: Duration = Duration::from_millis(25);

impl Evaluator {
    /// Move `stream` and a fully-owned child evaluator into a producer thread,
    /// decoupling upstream work from the downstream sink through exactly
    /// `capacity` slots. Pulls use short deadlines so dropping the receiver or
    /// cancelling the execution tears down even an idle live source promptly.
    pub(crate) fn spawn_stream_buffer(
        &mut self,
        stream: StreamVal,
        capacity: usize,
    ) -> VResult<StreamVal> {
        debug_assert!(capacity > 0);
        let label = stream.label.clone();
        let bounded = stream.is_bounded();
        let mut upstream = stream.take_upstream()?;
        let (tx, rx) = sync_channel(capacity);
        let cancel = self.cancellation_token();
        let stop = Arc::new(AtomicBool::new(false));
        let producer_stop = stop.clone();
        let child = self.child_context();

        std::thread::spawn(move || {
            let mut evaluator = child.build(ChildKind::StreamPump, cancel.clone());
            loop {
                if cancel.is_cancelled() || producer_stop.load(Ordering::SeqCst) {
                    break;
                }
                let delivery = match upstream.pull(&mut evaluator, Some(PUMP_POLL)) {
                    Ok(Pull::Item(value)) => Ok(value),
                    Ok(Pull::Timeout) => continue,
                    Ok(Pull::End) => break,
                    Err(error) => Err(error),
                };
                let terminal = delivery.is_err();
                if !send_to_buffer(&tx, &cancel, &producer_stop, delivery) || terminal {
                    break;
                }
            }
        });

        Ok(StreamVal::from_buffered_channel(label, bounded, rx, stop))
    }

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
        if interval.is_zero() {
            return Err(ErrorVal::arg_error("every expects a positive interval"));
        }
        let (tx, rx) = sync_channel(1);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(interval);
                let now = jiff::Zoned::now();
                match tx.try_send(Ok(Value::DateTime(Box::new(now)))) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {} // consumer busy → tick coalesced away
                    Err(TrySendError::Disconnected(_)) => break, // consumer gone → stop the timer
                }
            }
        });
        Ok(Value::Stream(StreamVal::from_channel("datetime", rx)))
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
        if !root.exists() {
            return Err(ErrorVal::new(
                "not_found",
                format!("watch target does not exist: {}", root.display()),
            ));
        }
        let (tx, rx) = sync_channel::<VResult<Value>>(SOURCE_BUF);
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        std::thread::spawn(move || {
            // notify pushes raw events onto its own channel; we translate + filter
            // and forward. Keeping `watcher` alive in this scope keeps the OS watch
            // open; both drop together when the loop ends.
            let (raw_tx, raw_rx) = channel();
            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = raw_tx.send(res);
            }) {
                Ok(w) => w,
                Err(e) => {
                    let _ = tx.send(Err(ErrorVal::new("io_error", format!("watch: {e}"))));
                    return;
                }
            };
            if let Err(e) = watcher.watch(&root, mode) {
                let _ = tx.send(Err(ErrorVal::new("io_error", format!("watch: {e}"))));
                return;
            }
            // Coalesce state: `true` when at least one event was dropped on a
            // full buffer and the consumer is owed the documented coalescing summary event.
            let mut coalesce_owed = 0u64;
            for res in raw_rx {
                let event = match res {
                    Ok(ev) => ev,
                    Err(e) => {
                        // Errors are rare and must not be lost: a blocking
                        // send here is bounded by the consumer's next pull
                        // (and fails immediately if the consumer is gone).
                        if tx
                            .send(Err(ErrorVal::new("io_error", format!("watch: {e}"))))
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let Some(kind) = event_kind(&event.kind) else {
                    continue;
                };
                let mut consumer_gone = false;
                for path in &event.paths {
                    if let Some(m) = &matcher
                        && !m.matches_path(path)
                    {
                        continue;
                    }
                    if !send_coalescing(&tx, &mut coalesce_owed, &root, watch_event(path, kind)) {
                        consumer_gone = true;
                        break;
                    }
                }
                if consumer_gone {
                    break; // consumer gone → drop the watch
                }
            }
        });
        Ok(Value::Stream(StreamVal::from_channel("event", rx)))
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
        if !path.exists() {
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
        std::thread::spawn(move || tail_loop(&fs, &path, from_start, &tx));
        Ok(Value::Stream(StreamVal::from_channel("str", rx)))
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

fn send_to_buffer(
    tx: &SyncSender<VResult<Value>>,
    cancel: &CancelToken,
    stop: &AtomicBool,
    mut delivery: VResult<Value>,
) -> bool {
    loop {
        if cancel.is_cancelled() || stop.load(Ordering::SeqCst) {
            return false;
        }
        match tx.try_send(delivery) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                delivery = returned;
                std::thread::park_timeout(Duration::from_millis(2));
            }
        }
    }
}

/// Map a `notify` event kind onto shoal's closed `kind` enum, dropping
/// access/other events that carry no create/modify/remove meaning.
fn event_kind(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Create(_) => Some("created"),
        EventKind::Modify(_) => Some("modified"),
        EventKind::Remove(_) => Some("removed"),
        _ => None,
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
fn tail_loop(fs: &Arc<dyn Fs>, path: &Path, from_start: bool, tx: &SyncSender<VResult<Value>>) {
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
    // Lines dropped on a full consumer buffer, owed to the consumer as a
    // single `{dropped: n}` marker element once room appears (site/content/internals/streams-channels.md).
    let mut dropped: u64 = 0;

    let (raw_tx, raw_rx) = channel();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            let _ = tx.send(Err(ErrorVal::new("io_error", format!("tail: {e}"))));
            return;
        }
    };
    if let Err(e) = watcher.watch(path, RecursiveMode::NonRecursive) {
        let _ = tx.send(Err(ErrorVal::new("io_error", format!("tail: {e}"))));
        return;
    }

    // Read whatever is already available past `pos` (the from_start backlog, or
    // any bytes written between open and the first event).
    if !read_new_lines(fs, path, &mut pos, tx, &mut dropped) {
        return;
    }
    for res in raw_rx {
        match res {
            Ok(ev) if matches!(ev.kind, EventKind::Modify(_) | EventKind::Create(_)) => {
                // Truncation/rotation: file shrank → restart from its new EOF.
                if let Ok(meta) = fs.metadata(path)
                    && meta.len() < pos
                {
                    pos = 0;
                }
                if !read_new_lines(fs, path, &mut pos, tx, &mut dropped) {
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                if tx
                    .send(Err(ErrorVal::new("io_error", format!("tail: {e}"))))
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    drop(watcher);
}

/// Read complete lines from `path` starting at `*pos`, advancing `*pos` to just
/// past the last full line. A trailing partial line (no `\n`) is left unread.
/// Returns `false` if the consumer has gone (send failed) so the caller stops.
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
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => return true,
        };
        if n == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            // Partial trailing line — don't advance past it; wait for its newline.
            break;
        }
        *pos += n as u64;
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        if !send_line_bounded(tx, dropped, line) {
            return false;
        }
    }
    true
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
    use shoal_value::{CallCtx, Upstream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct CountingSource {
        pulls: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
    }

    struct IdleSource {
        dropped: Arc<AtomicBool>,
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
}
