//! Kernel-native pub/sub (AGENT-SURFACE §4/§6): the per-channel ring buffer
//! plus the `events.*` dispatch handlers (`events.read`, `events.publish`,
//! `events.subscribe`, `events.unsubscribe`). Split out of `lib.rs`
//! (docs/ROADMAP.md wave R4): pure mechanical move, zero wire/behavior
//! change.
use super::*;

/// A per-connection socket writer shared between the request/response path
/// and any subscription push threads. Whole frames are serialized then
/// written under this lock so a pushed `event` notification never
/// interleaves with a response on the same fd.
pub(crate) type SharedWriter = Arc<Mutex<UnixStream>>;

/// One ring buffer per channel; `seq` is monotonic per channel. Subscribers
/// get `event` notifications pushed on their own connection via a bounded,
/// per-subscriber queue (AGENT-SURFACE §6) — see `SubQueue` below for why.
#[derive(Default)]
pub(crate) struct EventBus {
    channels: Mutex<HashMap<String, ChannelBuf>>,
    subs: Mutex<Vec<Subscriber>>,
    /// The seq↔journal-entry correspondence for the `journal` channel, the one
    /// channel whose events are backed by durable journal rows and are
    /// therefore replayable past the in-memory ring (AGENT-SURFACE §4). Dense
    /// `Vec` indexed by the journal channel's `seq` (0-based, contiguous),
    /// holding the journal `entry_id` each seq was published for. This is only
    /// the *pointer* — the event payload (`head`/`ok`/`principal`) is
    /// reconstructed from the journal itself, not from here, so an aged-out
    /// `journal` event costs one `i64` of memory, not a buffered event.
    ///
    /// Written (under the `channels` lock, so it can never diverge from the
    /// seqs the ring hands out) only by `publish_journal`; read by the kernel's
    /// `read_journal_channel` fallback. Other channels never touch it — they
    /// stay ring-only (AGENT-SURFACE §4: `user.*`/`task.{id}`/`approval`/
    /// `render`/`session.transcript` are NOT journal-backed).
    journal_index: Mutex<Vec<i64>>,
}

/// Ring-buffered event log for one channel.
#[derive(Default)]
struct ChannelBuf {
    next_seq: u64,
    ring: VecDeque<Event>,
}

struct Subscriber {
    conn: u64,
    channel: String,
    queue: Arc<SubQueue>,
}

/// Ring depth per channel (AGENT-SURFACE §4 requires ≥1024).
pub(crate) const EVENT_RING_CAP: usize = 1024;

/// Bound on a subscriber's own outgoing queue — distinct from the per-channel
/// ring buffer above. This is the backpressure boundary AGENT-SURFACE §6
/// promises: `publish()` (below) only ever appends to this bounded, in-memory
/// queue, never performs the blocking socket write itself, so a stalled
/// subscriber can delay at most its own dedicated writer thread — never the
/// producer, and never any other subscriber. Once a subscriber's queue holds
/// this many not-yet-written events, further events for that subscriber are
/// dropped and coalesced into a running `{dropped, latest_seq}` summary
/// (`SubQueueState`) instead of buffered unboundedly.
const SUB_QUEUE_CAP: usize = 256;

/// The static channels a session may always subscribe to (AGENT-SURFACE §4).
/// `task.{id}` and `user.{name}` are dynamic and not listed here.
///
/// `reef` used to be advertised here with nothing ever publishing to it — a
/// dead channel a client could subscribe to and wait on forever. Tool
/// lock/drift/fetch events originate inside `shoal-eval`'s reef resolution
/// (a different crate, outside this crate's lane), so there is no natural
/// emit point reachable from here yet; rather than leave it advertised but
/// silent, it has been removed until an eval-side event-forwarder hook
/// (analogous to `session.rs`'s `user.*` bridge) makes it real. See
/// `docs/AGENT-SURFACE.md`'s status section.
pub(crate) const STATIC_CHANNELS: &[&str] =
    &["session.transcript", "journal", "approval", "render"];

/// A subscriber's outgoing queue (AGENT-SURFACE §6): a bounded FIFO of
/// not-yet-written events, plus a running count of events dropped since the
/// queue last drained past capacity. Protected by its OWN lock — never
/// `EventBus::subs` — so `publish()` appending to one subscriber's queue
/// never contends with, let alone blocks on, another subscriber's slow
/// writer thread doing a blocking socket write.
struct SubQueue {
    channel: String,
    state: Mutex<SubQueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct SubQueueState {
    events: VecDeque<Event>,
    dropped: u64,
    latest_dropped_seq: u64,
    closed: bool,
}

impl SubQueue {
    fn new(channel: String) -> Arc<Self> {
        Arc::new(Self {
            channel,
            state: Mutex::new(SubQueueState::default()),
            ready: Condvar::new(),
        })
    }

    /// Enqueue `event` for this subscriber. Never blocks: at capacity the
    /// event is dropped and folded into the running `{dropped, latest_seq}`
    /// summary rather than buffered — this is the only thing `publish()`
    /// calls per subscriber, and it is a plain in-memory push, never a
    /// socket write, so a stalled subscriber can never stall `publish()`.
    fn push(&self, event: Event) {
        let mut state = self.state.lock().unwrap();
        if state.events.len() < SUB_QUEUE_CAP {
            state.events.push_back(event);
        } else {
            state.dropped += 1;
            state.latest_dropped_seq = event.seq;
        }
        drop(state);
        self.ready.notify_one();
    }

    /// Mark this queue closed (connection gone / explicitly unsubscribed) so
    /// its writer thread stops waiting and exits instead of blocking forever.
    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_one();
    }

    /// Block until there is something to write: a buffered event, a pending
    /// dropped-summary (synthesized into an `Event` here so the writer thread
    /// has one uniform thing to serialize), or closure (`None`). A pending
    /// summary is always delivered before any event that arrives after it —
    /// the subscriber learns about the gap before it sees anything past it.
    fn next(&self) -> Option<Event> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(event) = state.events.pop_front() {
                return Some(event);
            }
            if state.dropped > 0 {
                let dropped = state.dropped;
                let latest_seq = state.latest_dropped_seq;
                state.dropped = 0;
                return Some(Event {
                    channel: self.channel.clone(),
                    seq: latest_seq,
                    ts: now_ns(),
                    payload: json!({"dropped": dropped, "latest_seq": latest_seq}),
                });
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }
}

/// One dedicated thread per subscription, draining `queue` and performing
/// the (potentially blocking) socket write — the ONLY place that write
/// happens. Isolating it here, off any `EventBus`-wide lock and off the
/// `publish()` call path entirely, is what makes a slow/stalled client's
/// blocking `write_all` a problem for this one thread alone.
fn spawn_subscriber_writer(queue: Arc<SubQueue>, writer: SharedWriter) {
    std::thread::spawn(move || {
        while let Some(event) = queue.next() {
            let note = json!({
                "jsonrpc": JSONRPC,
                "method": "event",
                "params": &event,
            });
            let mut w = writer.lock().unwrap();
            let ok = write_json_notification(&mut w, &note).is_ok();
            drop(w);
            if !ok {
                // Dead connection: stop trying. The subscriber entry itself
                // is pruned from `EventBus::subs` by `remove_conn` when the
                // read side of this same connection notices the disconnect
                // (or by an explicit `unsubscribe`) — until then, `publish()`
                // simply keeps pushing into a queue nothing drains, which is
                // harmless: it is bounded, so it costs at most `SUB_QUEUE_CAP`
                // events of memory, never unbounded growth or a blocked
                // producer.
                queue.close();
                return;
            }
        }
    });
}

impl EventBus {
    /// Append `payload` to `channel`'s ring and enqueue it for every live
    /// subscriber (thin wrapper over [`EventBus::publish_inner`] with no
    /// durable-id recording — the path every non-`journal` channel takes).
    pub(crate) fn publish(&self, channel: &str, payload: Json) -> Event {
        self.publish_inner(channel, payload, None)
    }

    /// Publish on the `journal` channel, recording the durable `entry_id` this
    /// event corresponds to so it can be reconstructed from the journal after
    /// it ages out of the ring (AGENT-SURFACE §4 journal-backed replay). The
    /// only difference from `publish` is that the seq↔`entry_id` pair is
    /// appended to `journal_index` atomically with the seq assignment.
    pub(crate) fn publish_journal(&self, entry_id: i64, payload: Json) -> Event {
        self.publish_inner("journal", payload, Some(entry_id))
    }

    /// Append `payload` to `channel`'s ring and enqueue it for every live
    /// subscriber of that channel. Never blocks on a subscriber's socket:
    /// `Subscriber::queue.push` (AGENT-SURFACE §6) is a bounded, in-memory
    /// operation, and the lock held here (`subs`) guards only that push, not
    /// any write — the actual blocking I/O happens later, on each
    /// subscription's own dedicated writer thread (`spawn_subscriber_writer`).
    /// A stalled/slow subscriber can therefore delay at most its own
    /// delivery, never another subscriber's, and never this call.
    ///
    /// `durable_id` is `Some` only for the `journal` channel: the seq↔entry
    /// pointer is pushed onto `journal_index` inside the same `channels`-lock
    /// critical section that assigns `seq`, so concurrent publishes can never
    /// interleave the index out of order relative to the seqs the ring hands
    /// out (index position == seq, always).
    fn publish_inner(&self, channel: &str, payload: Json, durable_id: Option<i64>) -> Event {
        let event = {
            let mut channels = self.channels.lock().unwrap();
            let buf = channels.entry(channel.to_string()).or_default();
            let seq = buf.next_seq;
            buf.next_seq += 1;
            if let Some(entry_id) = durable_id {
                let mut index = self.journal_index.lock().unwrap();
                debug_assert_eq!(
                    index.len() as u64,
                    seq,
                    "journal_index must stay dense and aligned with journal-channel seqs"
                );
                index.push(entry_id);
            }
            let event = Event {
                channel: channel.to_string(),
                seq,
                ts: now_ns(),
                payload,
            };
            buf.ring.push_back(event.clone());
            while buf.ring.len() > EVENT_RING_CAP {
                buf.ring.pop_front();
            }
            event
        };
        let subs = self.subs.lock().unwrap();
        for sub in subs.iter().filter(|s| s.channel == channel) {
            sub.queue.push(event.clone());
        }
        event
    }

    /// Oldest `seq` still retained in `channel`'s ring, or `None` if the
    /// channel has never published. Used by the journal-backed read path to
    /// find the boundary below which events have aged out of the ring.
    pub(crate) fn ring_oldest_seq(&self, channel: &str) -> Option<u64> {
        self.channels
            .lock()
            .unwrap()
            .get(channel)
            .and_then(|buf| buf.ring.front().map(|e| e.seq))
    }

    /// Total number of events ever published on the `journal` channel this
    /// process (== the channel's `next_seq`). `journal_index` has exactly one
    /// entry per published journal event, so its length is that count.
    pub(crate) fn journal_published_count(&self) -> u64 {
        self.journal_index.lock().unwrap().len() as u64
    }

    /// The `(seq, entry_id)` pairs for journal-channel events whose `seq` is
    /// strictly greater than `since` and strictly less than `upto` — i.e. the
    /// events that a caller asked for (via `since`) but that have already aged
    /// out of the ring (below `upto`, the ring's oldest retained seq). Returned
    /// ascending by seq. This is the seq→entry pointer set the kernel then
    /// resolves against the journal to rebuild the actual events.
    pub(crate) fn journal_index_range(&self, since: Option<u64>, upto: u64) -> Vec<(u64, i64)> {
        let index = self.journal_index.lock().unwrap();
        let start = since.map(|s| s.saturating_add(1)).unwrap_or(0);
        (start..upto)
            .filter_map(|seq| index.get(seq as usize).map(|&entry_id| (seq, entry_id)))
            .collect()
    }

    /// Buffered tail of `channel` from `since` (exclusive), capped at `limit`.
    fn read(&self, channel: &str, since: Option<u64>, limit: Option<usize>) -> Vec<Event> {
        let channels = self.channels.lock().unwrap();
        let Some(buf) = channels.get(channel) else {
            return Vec::new();
        };
        let mut out: Vec<Event> = buf
            .ring
            .iter()
            .filter(|e| since.is_none_or(|s| e.seq > s))
            .cloned()
            .collect();
        if let Some(limit) = limit
            && out.len() > limit
        {
            out = out.split_off(out.len() - limit);
        }
        out
    }

    /// Register `writer` as a subscriber to `channel` (idempotent per
    /// `(conn, channel)` — re-subscribing finds the existing queue rather
    /// than spawning a second writer thread). Any already-buffered events
    /// after `since` are enqueued for replay through the same bounded queue
    /// and dedicated writer thread live events use, so replay and live
    /// delivery are never a separate, ad hoc blocking write on the calling
    /// (dispatch) thread.
    fn subscribe(&self, conn: u64, channel: &str, since: Option<u64>, writer: &SharedWriter) {
        let queue = {
            let mut subs = self.subs.lock().unwrap();
            if let Some(existing) = subs.iter().find(|s| s.conn == conn && s.channel == channel) {
                existing.queue.clone()
            } else {
                let queue = SubQueue::new(channel.to_string());
                subs.push(Subscriber {
                    conn,
                    channel: channel.to_string(),
                    queue: queue.clone(),
                });
                spawn_subscriber_writer(queue.clone(), writer.clone());
                queue
            }
        };
        for event in self.read(channel, since, None) {
            queue.push(event);
        }
    }

    fn unsubscribe(&self, conn: u64, channel: &str) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| {
            let keep = !(s.conn == conn && s.channel == channel);
            if !keep {
                s.queue.close();
            }
            keep
        });
    }

    pub(crate) fn remove_conn(&self, conn: u64) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| {
            let keep = s.conn != conn;
            if !keep {
                s.queue.close();
            }
            keep
        });
    }
}

fn write_json_notification(writer: &mut UnixStream, value: &Json) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    use std::io::Write as _;
    writer.write_all(&buf)?;
    writer.flush()
}

impl Kernel {
    pub(crate) fn handle_events_read(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        attached.as_ref().ok_or_else(not_attached)?;
        let p: EventsReadParams = decode(params)?;
        // The `journal` channel is journal-backed: a `since` older than the
        // ring's oldest retained seq is served from the durable journal rather
        // than lost (AGENT-SURFACE §4). Every other channel is ring-only.
        let events = if p.channel == "journal" {
            self.read_journal_channel(p.since, p.limit)?
        } else {
            self.events.read(&p.channel, p.since, p.limit)
        };
        encode(json!({"channel": p.channel, "events": events}))
    }

    /// Read the `journal` channel with journal-backed replay (AGENT-SURFACE §4,
    /// audit gap G2). Events still in the in-memory ring are served from it
    /// exactly as before (the fast path is untouched); events that have aged
    /// out of the ring — a `since` below the ring's oldest retained seq — are
    /// reconstructed from the durable journal so an agent can replay the
    /// channel from ANY seq, not just the last `EVENT_RING_CAP`.
    ///
    /// The seq↔journal correspondence: every `journal` event's `seq` was
    /// recorded, at publish time, against the coarse exec-level journal
    /// `entry_id` it announced (`EventBus::journal_index`). Reconstruction
    /// resolves each aged-out seq to its `entry_id` through that index, then
    /// rebuilds the `{entry_id, head, ok, principal}` payload from the journal
    /// row itself — so the *pointer* lives in memory (one `i64` per event) but
    /// the *payload* is journal-backed and durable. Using the index as the
    /// membership set is also what keeps reconstruction faithful in on-disk
    /// sessions, where the session evaluator ALSO writes its own finer
    /// per-statement entries into the same store (`session.rs`): those rows
    /// were never published on this channel, so they are excluded because their
    /// ids are not in the index.
    fn read_journal_channel(
        self: &Arc<Self>,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<Event>, RpcError> {
        // Nothing published yet, or `since` at/after the newest seq: the ring
        // already answers correctly (empty), and there is nothing older to
        // reconstruct. This also covers the not-found/beyond-newest case.
        let published = self.events.journal_published_count();
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self.events.read("journal", since, None));
        }
        let mut out: Vec<Event> = Vec::new();
        // Reconstruct the gap below the ring, if `since` reaches into it.
        if let Some(oldest) = self.events.ring_oldest_seq("journal") {
            let want = self.events.journal_index_range(since, oldest);
            if !want.is_empty() {
                out = self.reconstruct_journal_events(&want)?;
            }
        }
        // Then the ring tail (fast path, byte-for-byte as before).
        out.extend(self.events.read("journal", since, None));
        // Mirror `EventBus::read`'s limit semantics: keep the newest `limit`.
        if let Some(limit) = limit
            && out.len() > limit
        {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }

    /// Rebuild `journal` events for the given ascending `(seq, entry_id)`
    /// pairs by reading their rows from the durable journal. One query pulls
    /// the journal history; rows are indexed by id and only the requested ids
    /// (the coarse exec-level entries this channel published) are kept — the
    /// evaluator's per-statement rows present in on-disk stores are filtered
    /// out because they are absent from `want`.
    ///
    /// This is the cold fallback path (a subscriber that fell behind by more
    /// than `EVENT_RING_CAP`), not the hot path, so a single wide query +
    /// in-memory filter is deliberately favored over machinery the journal
    /// crate does not expose today (a targeted `entries_by_id` fetch would let
    /// this pull only the needed rows — a clean follow-up once shoal-journal
    /// grows one).
    fn reconstruct_journal_events(
        self: &Arc<Self>,
        want: &[(u64, i64)],
    ) -> Result<Vec<Event>, RpcError> {
        let by_id: HashMap<i64, u64> = want.iter().map(|&(seq, id)| (id, seq)).collect();
        let rows = {
            let journal = self.journal.lock().unwrap();
            journal
                .query(&JournalQuery {
                    // Fetch the whole history (SQLite `LIMIT -1`); we keep only
                    // the ids in `want`. Journals are GC-bounded and this runs
                    // only on the cold replay path.
                    limit: usize::MAX,
                    ..Default::default()
                })
                .map_err(internal)?
        };
        // seq -> event, so we can emit strictly ascending by seq afterwards.
        let mut found: HashMap<u64, Event> = HashMap::new();
        for row in rows {
            let Some(&seq) = by_id.get(&row.id) else {
                continue;
            };
            let ok = row.ok.unwrap_or(false);
            found.insert(
                seq,
                Event {
                    channel: "journal".to_string(),
                    seq,
                    // The journal records the entry's start (`ts_ns`) and, once
                    // finished, its duration; the live event fired at finish, so
                    // start + duration is the faithful reconstruction of that
                    // instant (falls back to start for the degenerate no-dur
                    // case). Consumers dedup by seq, never by ts.
                    ts: row.ts_ns.saturating_add(row.dur_ns.unwrap_or(0)),
                    payload: journal_event(row.id, &row.src, ok, &row.principal),
                },
            );
        }
        // Emit in ascending seq order, contiguous with the ring tail. A seq in
        // `want` with no journal row (its entry GC'd out from under the index)
        // is simply skipped — the gap is honest, not fabricated.
        let mut events: Vec<Event> = Vec::with_capacity(want.len());
        for &(seq, _) in want {
            if let Some(event) = found.remove(&seq) {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub(crate) fn handle_events_publish(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let p: EventsPublishParams = decode(params)?;
        // AGENT-SURFACE §4: only `user.*` channels are client-writable;
        // the kernel owns the semantic channels.
        if !p.channel.starts_with("user.") {
            return Err(RpcError {
                code: -32602,
                message: "only user.* channels may be published to".into(),
                data: Some(json!({"channel": p.channel})),
            });
        }
        let event = self.events.publish(&p.channel, p.payload.clone());
        // One substrate, reverse direction: a wire publish is also visible to
        // the session's in-language channels (`channel("user.x").latest()` /
        // `.events()`), via `inject` — which never re-forwards, so the event
        // cannot echo back onto the wire bus. Uses the cached bus handle, NOT
        // the evaluator lock — a long-running exec must not stall publishes.
        let payload = shoal_value::json_to_value(&p.payload);
        attachment.session.lang_bus.inject(&p.channel, payload);
        encode(json!({"channel": event.channel, "seq": event.seq, "ts": event.ts}))
    }

    pub(crate) fn handle_events_subscribe(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
        conn: Option<&SharedWriter>,
    ) -> Result<Json, RpcError> {
        attached.as_ref().ok_or_else(not_attached)?;
        let p: EventsSubParams = decode(params)?;
        let Some(writer) = conn else {
            return Err(RpcError {
                code: -32603,
                message: "subscription requires a live connection".into(),
                data: None,
            });
        };
        self.events.subscribe(client, &p.channel, p.since, writer);
        encode(json!({"channel": p.channel, "subscribed": true}))
    }

    pub(crate) fn handle_events_unsubscribe(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        attached.as_ref().ok_or_else(not_attached)?;
        let p: EventsSubParams = decode(params)?;
        self.events.unsubscribe(client, &p.channel);
        encode(json!({"channel": p.channel, "subscribed": false}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Read one already-written frame off `reader` (blocking, with a bounded
    /// timeout so a bug that hangs the write path fails the test loudly
    /// instead of hanging the suite). Takes a caller-owned, persistent
    /// `BufReader` — NOT a fresh one per call — because a fresh `BufReader`
    /// wrapping a freshly `try_clone`d fd each call discards whatever extra
    /// bytes its one internal read happened to buffer past the first line
    /// (several frames can arrive in a single burst from a writer thread
    /// draining a backlog); a one-shot `BufReader` silently drops those,
    /// which starves a later call and manifests as a spurious read timeout.
    fn recv_line(reader: &mut io::BufReader<UnixStream>) -> Json {
        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(reader, &mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    /// FIX 1 regression: `publish()` must return promptly even when a
    /// subscriber never reads its socket at all — the original bug had
    /// `publish()` call a blocking `write_all` per subscriber while holding
    /// `EventBus::subs`, so one inert subscriber froze every future publish
    /// to every channel. Here nothing ever reads `client_end`; if `publish`
    /// still blocked on the write, this loop would hang well past the
    /// assertion's bound (or forever, since nothing will ever drain it).
    #[test]
    fn publish_does_not_block_when_a_subscriber_never_reads() {
        let bus = EventBus::default();
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server_end));
        bus.subscribe(1, "user.stress", None, &writer);

        let start = Instant::now();
        for i in 0..500 {
            // A few KB per event: comfortably past any default socket
            // buffer many times over across 500 publishes, so this is a
            // faithful stand-in for "a subscriber that never reads".
            bus.publish("user.stress", json!({"i": i, "pad": "x".repeat(2048)}));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "publish() blocked on an unread subscriber: {elapsed:?}"
        );
        drop(client_end);
    }

    /// FIX 1: a genuinely stalled subscriber (its writer thread blocked mid
    /// write) must not stall a second, healthy subscriber on the same
    /// channel — proving `publish()`'s per-subscriber queue push is
    /// independent across subscribers, not just fast in isolation. The stall
    /// is simulated deterministically (holding the stalled subscriber's own
    /// `SharedWriter` mutex from another thread) rather than relying on OS
    /// socket-buffer sizes, which vary by host and would make this flaky.
    #[test]
    fn a_stalled_subscriber_never_stalls_a_healthy_one() {
        let bus = Arc::new(EventBus::default());
        let (stalled_client, stalled_server) = UnixStream::pair().unwrap();
        let stalled_writer: SharedWriter = Arc::new(Mutex::new(stalled_server));
        let (healthy_client, healthy_server) = UnixStream::pair().unwrap();
        let healthy_writer: SharedWriter = Arc::new(Mutex::new(healthy_server));

        bus.subscribe(1, "user.race", None, &stalled_writer);
        bus.subscribe(2, "user.race", None, &healthy_writer);

        // Simulate the stalled subscriber's writer thread being stuck mid
        // blocking-write by holding its writer's mutex from here.
        let hold = stalled_writer.clone();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let stall_thread = std::thread::spawn(move || {
            let _guard = hold.lock().unwrap();
            let _ = release_rx.recv();
        });
        // Give the stall thread a moment to actually acquire the lock before
        // the subscriber's own writer thread (spawned by `subscribe` above)
        // has a chance to race for it.
        std::thread::sleep(Duration::from_millis(50));

        let start = Instant::now();
        for i in 0..10 {
            bus.publish("user.race", json!({"i": i}));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "publish() blocked while one subscriber's writer was stalled: {elapsed:?}"
        );

        // The healthy subscriber must still receive events promptly, live,
        // while the other subscriber's writer is stuck.
        let mut healthy_reader = io::BufReader::new(healthy_client.try_clone().unwrap());
        for expected in 0..10 {
            let got = recv_line(&mut healthy_reader);
            assert_eq!(got["method"], "event");
            assert_eq!(got["params"]["payload"]["i"], expected);
        }

        release_tx.send(()).unwrap();
        stall_thread.join().unwrap();
        drop(stalled_client);
        drop(healthy_client);
    }

    /// FIX 1: once a stalled subscriber's queue overflows `SUB_QUEUE_CAP`,
    /// further events for it must coalesce into a `{dropped, latest_seq}`
    /// summary (AGENT-SURFACE §6) rather than buffering unboundedly — and
    /// once the stall clears, that summary (not a flood of the individually
    /// dropped events) is what the subscriber actually receives.
    #[test]
    fn a_stalled_subscriber_gets_a_coalesced_dropped_summary() {
        let bus = Arc::new(EventBus::default());
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server_end));
        bus.subscribe(1, "user.overflow", None, &writer);

        let hold = writer.clone();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let stall_thread = std::thread::spawn(move || {
            let _guard = hold.lock().unwrap();
            let _ = release_rx.recv();
        });
        std::thread::sleep(Duration::from_millis(50));

        // Publish well past `SUB_QUEUE_CAP` while the writer is stalled —
        // some of these must be dropped-and-coalesced, not buffered forever.
        let total = SUB_QUEUE_CAP * 3;
        for i in 0..total {
            bus.publish("user.overflow", json!({"i": i}));
        }

        release_tx.send(()).unwrap();
        stall_thread.join().unwrap();

        // Drain frames until we find the coalesced summary. The first
        // SUB_QUEUE_CAP events (whichever the queue happened to still hold)
        // are delivered first, in order, followed by exactly one summary
        // event for everything dropped in between.
        let mut client_reader = io::BufReader::new(client_end);
        let mut found_summary = None;
        for _ in 0..(SUB_QUEUE_CAP + 5) {
            let note = recv_line(&mut client_reader);
            assert_eq!(note["method"], "event");
            let payload = &note["params"]["payload"];
            if payload.get("dropped").is_some() {
                found_summary = Some(payload.clone());
                break;
            }
        }
        let summary = found_summary
            .expect("expected a coalesced {dropped, latest_seq} summary after a queue overflow");
        assert!(
            summary["dropped"].as_u64().unwrap() > 0,
            "summary must report a nonzero drop count: {summary}"
        );
        assert!(
            summary["latest_seq"].as_u64().unwrap() < total as u64,
            "latest_seq must be a real event seq, not the overflowed total: {summary}"
        );
    }

    #[test]
    fn unsubscribe_stops_the_writer_thread_instead_of_leaking_it() {
        let bus = EventBus::default();
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server_end));
        bus.subscribe(1, "user.bye", None, &writer);
        assert_eq!(bus.subs.lock().unwrap().len(), 1);
        bus.unsubscribe(1, "user.bye");
        assert_eq!(bus.subs.lock().unwrap().len(), 0);
        drop(client_end);
    }
}
