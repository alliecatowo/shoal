//! Kernel-native pub/sub (site/content/internals/kernel-protocol.md): the per-channel ring buffer
//! plus the `events.*` dispatch handlers (`events.read`, `events.publish`,
//! `events.subscribe`, `events.unsubscribe`). See
//! `site/content/internals/kernel-protocol.md` for the wire contract.
use super::*;

/// A per-connection socket writer shared between the request/response path
/// and any subscription push threads. Whole frames are serialized then
/// written under this lock so a pushed `event` notification never
/// interleaves with a response on the same fd.
pub(crate) type SharedWriter = Arc<Mutex<UnixStream>>;

/// One ring buffer per channel; `seq` is monotonic per channel. Subscribers
/// get `event` notifications pushed on their own connection via a bounded,
/// per-subscriber queue (site/content/internals/kernel-protocol.md) — see `SubQueue` below for why.
#[derive(Default)]
pub(crate) struct EventBus {
    channels: Mutex<HashMap<String, ChannelBuf>>,
    subs: Mutex<Vec<Subscriber>>,
    /// The seq↔journal-entry correspondence for the `journal` channel, the
    /// first channel whose events were made replayable past the in-memory
    /// ring (see `site/content/internals/kernel-protocol.md`). Dense `Vec` indexed by the
    /// journal channel's `seq` (0-based, contiguous), holding the journal
    /// `entry_id` each seq was published for. This is only the *pointer* —
    /// the event payload (`head`/`ok`/`principal`) is reconstructed from the
    /// journal's `entry` table itself, not from here, so an aged-out
    /// `journal` event costs one `i64` of memory, not a buffered event.
    ///
    /// Written (under the `channels` lock, so it can never diverge from the
    /// seqs the ring hands out) only by `publish_journal`; read by the
    /// kernel's `read_journal_channel` fallback. Also rebuilt WHOLESALE, once,
    /// at kernel construction time by [`EventBus::seed_from_journal`] so
    /// event-bus seqs survive a kernel restart when reopening an existing
    /// on-disk store — see that method for how it recovers exactly this same
    /// membership/order from durable state alone.
    journal_index: Mutex<Vec<i64>>,
    /// The same dense-index idea as `journal_index`, for the
    /// `session.transcript` channel: indexed
    /// by that channel's own `seq`, holding the journal `entry_id` the
    /// transcript event was published for. Unlike `journal_index`, the
    /// payload this points at is NOT reconstructed from pre-existing journal
    /// columns — `shoal-journal`'s `transcript_event` table (keyed by that
    /// same `entry_id`) holds it verbatim, written by `record_transcript_event`
    /// at the same call site that publishes the live event. Written only by
    /// `publish_transcript`; read by `read_transcript_channel`. `approval`/
    /// `render`/`user.*` still touch neither index — they stay ring-only.
    /// Also rebuilt at construction time by [`EventBus::seed_from_journal`],
    /// same as `journal_index` above.
    transcript_index: Mutex<Vec<i64>>,
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

/// Ring depth per channel (site/content/internals/kernel-protocol.md requires ≥1024).
pub(crate) const EVENT_RING_CAP: usize = 1024;

/// Bound on a subscriber's own outgoing queue — distinct from the per-channel
/// ring buffer above. This is the backpressure boundary site/content/internals/kernel-protocol.md
/// promises: `publish()` (below) only ever appends to this bounded, in-memory
/// queue, never performs the blocking socket write itself, so a stalled
/// subscriber can delay at most its own dedicated writer thread — never the
/// producer, and never any other subscriber. Once a subscriber's queue holds
/// this many not-yet-written events, further events for that subscriber are
/// dropped and coalesced into a running `{dropped, latest_seq}` summary
/// (`SubQueueState`) instead of buffered unboundedly.
const SUB_QUEUE_CAP: usize = 256;

/// The static channels a session may always subscribe to (site/content/internals/kernel-protocol.md).
/// `task.{id}` and `user.{name}` are dynamic and not listed here.
///
/// `reef` used to be advertised here with nothing ever publishing to it — a
/// dead channel a client could subscribe to and wait on forever. Tool
/// lock/drift/fetch events originate inside `shoal-eval`'s reef resolution
/// (a different crate, outside this crate's lane), so there is no natural
/// emit point reachable from here yet; rather than leave it advertised but
/// silent, it has been removed until an eval-side event-forwarder hook
/// (analogous to `session.rs`'s `user.*` bridge) makes it real. See
/// `site/content/internals/kernel-protocol.md`'s status section.
pub(crate) const STATIC_CHANNELS: &[&str] =
    &["session.transcript", "journal", "approval", "render"];

/// A subscriber's outgoing queue (site/content/internals/kernel-protocol.md): a bounded FIFO of
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

/// Which of `EventBus`'s two durable indices (if any) a `publish_inner` call
/// should record the `entry_id` pointer into — `journal` and
/// `session.transcript` are the only two journal-backed channels
/// (site/content/internals/kernel-protocol.md); every other channel stays ring-only.
enum DurableChannel {
    Journal,
    Transcript,
}

impl EventBus {
    /// Append `payload` to `channel`'s ring and enqueue it for every live
    /// subscriber (thin wrapper over [`EventBus::publish_inner`] with no
    /// durable-id recording — the path every ring-only channel takes).
    pub(crate) fn publish(&self, channel: &str, payload: Json) -> Event {
        self.publish_inner(channel, payload, None)
    }

    /// Publish on the `journal` channel, recording the durable `entry_id` this
    /// event corresponds to so it can be reconstructed from the journal after
    /// it ages out of the ring (site/content/internals/kernel-protocol.md journal-backed replay). The
    /// only difference from `publish` is that the seq↔`entry_id` pair is
    /// appended to `journal_index` atomically with the seq assignment.
    pub(crate) fn publish_journal(&self, entry_id: i64, payload: Json) -> Event {
        self.publish_inner(
            "journal",
            payload,
            Some((DurableChannel::Journal, entry_id)),
        )
    }

    /// Publish on the `session.transcript` channel, recording the durable
    /// `entry_id` this event corresponds to (mirrors
    /// `publish_journal` exactly, but for `transcript_index`). The caller
    /// (`handlers_exec.rs`) has already durably persisted the payload itself
    /// via `Journal::record_transcript_event` — this only records the
    /// seq↔`entry_id` pointer, same division of labor as the `journal`
    /// channel.
    pub(crate) fn publish_transcript(&self, entry_id: i64, payload: Json) -> Event {
        self.publish_inner(
            "session.transcript",
            payload,
            Some((DurableChannel::Transcript, entry_id)),
        )
    }

    /// Append `payload` to `channel`'s ring and enqueue it for every live
    /// subscriber of that channel. Never blocks on a subscriber's socket:
    /// `Subscriber::queue.push` (site/content/internals/kernel-protocol.md) is a bounded, in-memory
    /// operation, and the lock held here (`subs`) guards only that push, not
    /// any write — the actual blocking I/O happens later, on each
    /// subscription's own dedicated writer thread (`spawn_subscriber_writer`).
    /// A stalled/slow subscriber can therefore delay at most its own
    /// delivery, never another subscriber's, and never this call.
    ///
    /// `durable` is `Some` only for the `journal`/`session.transcript`
    /// channels: the seq↔entry pointer is pushed onto the matching index
    /// inside the same `channels`-lock critical section that assigns `seq`,
    /// so concurrent publishes can never interleave an index out of order
    /// relative to the seqs the ring hands out (index position == seq,
    /// always, per index).
    fn publish_inner(
        &self,
        channel: &str,
        payload: Json,
        durable: Option<(DurableChannel, i64)>,
    ) -> Event {
        let event = {
            let mut channels = self.channels.lock().unwrap();
            let buf = channels.entry(channel.to_string()).or_default();
            let seq = buf.next_seq;
            buf.next_seq += 1;
            if let Some((which, entry_id)) = durable {
                let index = match which {
                    DurableChannel::Journal => &self.journal_index,
                    DurableChannel::Transcript => &self.transcript_index,
                };
                let mut index = index.lock().unwrap();
                debug_assert_eq!(
                    index.len() as u64,
                    seq,
                    "a durable index must stay dense and aligned with its channel's seqs"
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
        Self::index_len(&self.journal_index)
    }

    /// As [`EventBus::journal_published_count`], for `session.transcript`.
    pub(crate) fn transcript_published_count(&self) -> u64 {
        Self::index_len(&self.transcript_index)
    }

    fn index_len(index: &Mutex<Vec<i64>>) -> u64 {
        index.lock().unwrap().len() as u64
    }

    /// The `(seq, entry_id)` pairs for journal-channel events whose `seq` is
    /// strictly greater than `since` and strictly less than `upto` — i.e. the
    /// events that a caller asked for (via `since`) but that have already aged
    /// out of the ring (below `upto`, the ring's oldest retained seq). Returned
    /// ascending by seq. This is the seq→entry pointer set the kernel then
    /// resolves against the journal to rebuild the actual events.
    pub(crate) fn journal_index_range(&self, since: Option<u64>, upto: u64) -> Vec<(u64, i64)> {
        Self::index_range(&self.journal_index, since, upto)
    }

    /// As [`EventBus::journal_index_range`], for `session.transcript`.
    pub(crate) fn transcript_index_range(&self, since: Option<u64>, upto: u64) -> Vec<(u64, i64)> {
        Self::index_range(&self.transcript_index, since, upto)
    }

    fn index_range(index: &Mutex<Vec<i64>>, since: Option<u64>, upto: u64) -> Vec<(u64, i64)> {
        let index = index.lock().unwrap();
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

    /// Rebuild `journal`/`session.transcript` seq state from an EXISTING
    /// on-disk journal's rows (`site/content/internals/kernel-protocol.md` requires replay from
    /// ANY seq" promise): called once, by `Kernel::open`/`open_with_policy`,
    /// BEFORE the kernel starts serving any connection — so a freshly-built
    /// `EventBus::default()` re-mounting a store a PRIOR kernel process
    /// already wrote to starts both channels' seq counters past whatever
    /// that prior lifetime handed out, instead of colliding a reconnecting
    /// agent's persisted `since=N` cursor with a brand-new seq climbing from
    /// 0 again.
    ///
    /// Precisely reconstructs the SAME membership `publish_journal`/
    /// `publish_transcript` would have built incrementally in memory during
    /// the prior lifetime, from durable state alone — no schema change, no
    /// touching `shoal-journal`:
    /// - `journal`: an entry is a coarse, whole-submission entry (the kind
    ///   `handle_exec` appends exactly once per `exec` call — the ONLY kind
    ///   that ever fires a `journal` event) iff its `ast` column
    ///   deserializes as a [`shoal_ast::Program`], the shape `handle_exec`
    ///   always records there (`serde_json::to_string(&ast)` where `ast:
    ///   Program`). A session's own evaluator instead journals one entry per
    ///   top-level STATEMENT (`shoal-eval`'s `journal_begin_stmt`), each
    ///   recording a bare [`shoal_ast::Stmt`] (internally tagged `kind`, no
    ///   `stmts` field) — that shape fails to deserialize as a `Program`, so
    ///   those rows are correctly excluded, exactly as the in-memory index
    ///   already excluded them within one process lifetime (pinned by
    ///   `journal_channel_replay_excludes_evaluator_per_statement_entries`).
    /// - `session.transcript`: exactly the coarse entries (from the filter
    ///   above) that also have a persisted `transcript_event` row —
    ///   `Journal::transcript_events_by_entry` already silently skips any id
    ///   without one (the error-exec path never records one), so this is
    ///   precisely the successful-exec subset, in the same ascending order
    ///   the live channel always assigned.
    ///
    /// Both indexes are seeded oldest-first (seq 0 == the earliest surviving
    /// entry), in ascending entry-id order — the same order concurrent
    /// `handle_exec` calls assign seqs in live, since a publish always
    /// follows its own entry's append.
    ///
    /// Best-effort: a query failure (corrupt store, I/O error) leaves the
    /// affected channel(s) seeded at zero — no worse than before this fix,
    /// never a hard failure of kernel construction. An empty store is a
    /// no-op: both channels correctly start at 0, same as an ephemeral
    /// in-memory kernel.
    pub(crate) fn seed_from_journal(&self, journal: &Journal) {
        let Ok(mut entries) = journal.query(&JournalQuery {
            // The whole store, oldest-to-newest once reversed below — the
            // only way to enumerate every existing row through the existing
            // filtered-query API (there is no dedicated count/list-ids call,
            // and adding one is a `shoal-journal` schema/API change out of
            // this fix's lane).
            limit: i64::MAX as usize,
            ..Default::default()
        }) else {
            return;
        };
        if entries.is_empty() {
            return;
        }
        // `query` is newest-first (`ORDER BY id DESC`); every index here is
        // oldest-first.
        entries.reverse();
        let coarse_ids: Vec<i64> = entries
            .iter()
            .filter(|e| serde_json::from_str::<Program>(&e.ast_json).is_ok())
            .map(|e| e.id)
            .collect();
        self.seed_index(&self.journal_index, "journal", &coarse_ids);
        if let Ok(rows) = journal.transcript_events_by_entry(&coarse_ids) {
            let transcript_ids: Vec<i64> = rows.iter().map(|r| r.entry_id).collect();
            self.seed_index(
                &self.transcript_index,
                "session.transcript",
                &transcript_ids,
            );
        }
    }

    /// Shared seeding step for one durable index (see
    /// [`EventBus::seed_from_journal`]): append `entry_ids` (already
    /// ascending, already the exact membership for `channel`) and set that
    /// channel's `next_seq` to match, so the first post-restart publish
    /// continues from N rather than colliding with seq 0..N-1. Only ever
    /// called before this bus is shared with a serving kernel, so there is
    /// no concurrent publish to race with.
    fn seed_index(&self, index: &Mutex<Vec<i64>>, channel: &str, entry_ids: &[i64]) {
        if entry_ids.is_empty() {
            return;
        }
        let mut idx = index.lock().unwrap();
        idx.extend_from_slice(entry_ids);
        let mut channels = self.channels.lock().unwrap();
        let buf = channels.entry(channel.to_string()).or_default();
        buf.next_seq = idx.len() as u64;
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
        // The `journal` and `session.transcript` channels are journal-backed:
        // a `since` older than the ring's oldest retained seq is served from
        // the durable journal rather than lost (site/content/internals/kernel-protocol.md). Every
        // other channel is ring-only.
        let events = if p.channel == "journal" {
            self.read_journal_channel(p.since, p.limit)?
        } else if p.channel == "session.transcript" {
            self.read_transcript_channel(p.since, p.limit)?
        } else {
            self.events.read(&p.channel, p.since, p.limit)
        };
        encode(json!({"channel": p.channel, "events": events}))
    }

    /// Read the `journal` channel with journal-backed replay, as specified by
    /// `site/content/internals/kernel-protocol.md`. Events still in the in-memory ring are served from it
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
        // Reconstruct the gap below the ring, if `since` reaches into it. The
        // ring can be genuinely EMPTY here even though `published > 0`: right
        // after a kernel restart, `EventBus::seed_from_journal` seeds the
        // durable index from a pre-existing store but deliberately leaves the
        // ring untouched, and nothing has been published yet in this fresh
        // process — `ring_oldest_seq` returns `None` in exactly that case
        // (impossible pre-seeding, since every publish always pushed into
        // both the ring and the index together). Treat "no ring yet" as
        // "everything published so far is aged out", not "nothing to
        // reconstruct" — `published` itself is the right upper bound.
        let oldest = self.events.ring_oldest_seq("journal").unwrap_or(published);
        let want = self.events.journal_index_range(since, oldest);
        if !want.is_empty() {
            out = self.reconstruct_journal_events(&want)?;
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
    /// pairs via [`shoal_journal::Journal::entries_by_id`] — a targeted
    /// fetch of exactly the rows this channel needs (the coarse exec-level
    /// entries it published), rather than a wide `query()` scan filtered in
    /// memory now that `shoal-journal` exposes a targeted lookup. The
    /// evaluator's finer per-statement rows present in on-disk stores are
    /// never fetched at all, because their ids are simply absent from
    /// `want`.
    ///
    /// This is the cold fallback path (a subscriber that fell behind by more
    /// than `EVENT_RING_CAP`), not the hot path.
    fn reconstruct_journal_events(
        self: &Arc<Self>,
        want: &[(u64, i64)],
    ) -> Result<Vec<Event>, RpcError> {
        let ids: Vec<i64> = want.iter().map(|&(_, id)| id).collect();
        let rows = self
            .journal
            .lock()
            .unwrap()
            .entries_by_id(&ids)
            .map_err(internal)?;
        // `entries_by_id` returns rows in the SAME relative order as `ids`
        // (i.e. `want`'s order), with any id it couldn't find (an entry GC'd
        // out from under the index) simply absent — so a single forward scan
        // through `want`, skipping whichever entries a row doesn't match,
        // zips the two back together without a HashMap membership dance.
        let mut events = Vec::with_capacity(rows.len());
        let mut want = want.iter();
        for row in &rows {
            let seq = loop {
                let &(seq, id) = want
                    .next()
                    .expect("entries_by_id returned a row for an id it wasn't asked for");
                if id == row.id {
                    break seq;
                }
            };
            let ok = row.ok.unwrap_or(false);
            events.push(Event {
                channel: "journal".to_string(),
                seq,
                // The journal records the entry's start (`ts_ns`) and, once
                // finished, its duration; the live event fired at finish, so
                // start + duration is the faithful reconstruction of that
                // instant (falls back to start for the degenerate no-dur
                // case). Consumers dedup by seq, never by ts.
                ts: row.ts_ns.saturating_add(row.dur_ns.unwrap_or(0)),
                payload: journal_event(row.id, &row.src, ok, &row.principal),
            });
        }
        Ok(events)
    }

    /// Read the `session.transcript` channel with journal-backed replay
    /// (`site/content/internals/kernel-protocol.md`). Mirrors
    /// `read_journal_channel` exactly: the ring tail is served unchanged
    /// (fast path untouched), and a `since` reaching below the ring's oldest
    /// retained seq is reconstructed from the durable
    /// `shoal_journal::TranscriptEventRow`s `handlers_exec.rs` persists
    /// alongside every live transcript event.
    fn read_transcript_channel(
        self: &Arc<Self>,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<Event>, RpcError> {
        let published = self.events.transcript_published_count();
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self.events.read("session.transcript", since, None));
        }
        let mut out: Vec<Event> = Vec::new();
        // Same ring-can-be-empty-but-published>0 fallback as
        // `read_journal_channel` above (post-restart, pre-first-publish).
        let oldest = self
            .events
            .ring_oldest_seq("session.transcript")
            .unwrap_or(published);
        let want = self.events.transcript_index_range(since, oldest);
        if !want.is_empty() {
            out = self.reconstruct_transcript_events(&want)?;
        }
        out.extend(self.events.read("session.transcript", since, None));
        if let Some(limit) = limit
            && out.len() > limit
        {
            out = out.split_off(out.len() - limit);
        }
        Ok(out)
    }

    /// Rebuild `session.transcript` events for the given ascending `(seq,
    /// entry_id)` pairs via [`shoal_journal::Journal::transcript_events_by_entry`].
    /// Unlike `reconstruct_journal_events`, the payload here is not
    /// re-derived from other columns — it is the exact `$`-tagged JSON the
    /// live event carried, stored verbatim by `Journal::record_transcript_event`
    /// at the same call site that publishes it, so reconstruction only
    /// re-wraps it into an `Event`.
    fn reconstruct_transcript_events(
        self: &Arc<Self>,
        want: &[(u64, i64)],
    ) -> Result<Vec<Event>, RpcError> {
        let ids: Vec<i64> = want.iter().map(|&(_, id)| id).collect();
        let rows = self
            .journal
            .lock()
            .unwrap()
            .transcript_events_by_entry(&ids)
            .map_err(internal)?;
        // Same order-preserving zip as `reconstruct_journal_events`: rows
        // come back in `ids`' order (== `want`'s order), any entry with no
        // transcript row (the exec failed, so no transcript event was ever
        // published for it) simply absent.
        let mut events = Vec::with_capacity(rows.len());
        let mut want = want.iter();
        for row in &rows {
            let seq = loop {
                let &(seq, id) = want.next().expect(
                    "transcript_events_by_entry returned a row for an id it wasn't asked for",
                );
                if id == row.entry_id {
                    break seq;
                }
            };
            let payload: Json = serde_json::from_str(&row.payload_json).map_err(internal)?;
            events.push(Event {
                channel: "session.transcript".to_string(),
                seq,
                ts: row.ts_ns,
                payload,
            });
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
        // site/content/internals/kernel-protocol.md: only `user.*` channels are client-writable;
        // the kernel owns the semantic channels.
        if !p.channel.starts_with("user.") {
            return Err(RpcError {
                code: INVALID_PARAMS,
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
                code: INTERNAL_ERROR,
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

    /// Regression: `publish()` must return promptly even when a
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

    /// A genuinely stalled subscriber (its writer thread blocked mid
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

    /// Once a stalled subscriber's queue overflows `SUB_QUEUE_CAP`,
    /// further events for it must coalesce into a `{dropped, latest_seq}`
    /// summary (site/content/internals/kernel-protocol.md) rather than buffering unboundedly — and
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

    // -----------------------------------------------------------------------
    // `EventBus::seed_from_journal`: seqs surviving a kernel restart.
    // -----------------------------------------------------------------------

    /// Appends one "coarse" entry (`ast` = a whole [`Program`]) to `journal`,
    /// optionally preceded by a "fine" per-statement entry (`ast` = a bare
    /// [`Stmt`]) — mirroring exactly what a real on-disk kernel session
    /// leaves behind: `handle_exec`'s own coarse entry plus the session
    /// evaluator's per-statement ones, sharing the same store. Returns the
    /// coarse entry's id. `with_transcript` mirrors a successful exec also
    /// recording a `session.transcript` row for that same entry.
    fn append_simulated_exec(journal: &Journal, with_fine_row: bool, with_transcript: bool) -> i64 {
        let stmt = Stmt::Return {
            value: None,
            span: shoal_ast::Span::default(),
        };
        if with_fine_row {
            let fine_id = journal
                .append(&EntryRecord {
                    session: "s".into(),
                    principal: "human".into(),
                    ts_ns: 0,
                    cwd: vec![],
                    src: "return".into(),
                    ast_json: serde_json::to_string(&stmt).unwrap(),
                    effects_json: "[]".into(),
                    opaque: false,
                })
                .unwrap();
            journal.finish(fine_id, Some(0), true, 0).unwrap();
        }
        let program = Program { stmts: vec![stmt] };
        let coarse_id = journal
            .append(&EntryRecord {
                session: "s".into(),
                principal: "human".into(),
                ts_ns: 0,
                cwd: vec![],
                src: "return".into(),
                ast_json: serde_json::to_string(&program).unwrap(),
                effects_json: "[]".into(),
                opaque: false,
            })
            .unwrap();
        journal.finish(coarse_id, Some(0), true, 0).unwrap();
        if with_transcript {
            journal.record_transcript_event(coarse_id, 0, "{}").unwrap();
        }
        coarse_id
    }

    /// Core restart regression: seeding from an on-disk store that already
    /// holds prior "exec" entries must (1) recover ONLY the coarse
    /// whole-submission entries into `journal_index` — the interleaved fine
    /// per-statement rows a real session evaluator also writes must be
    /// excluded, exactly as the in-memory index already excludes them within
    /// one process lifetime — (2) recover only the subset with a persisted
    /// transcript row into `transcript_index`, and (3) leave both channels'
    /// `next_seq` past the seeded count, so the very next publish continues
    /// rather than colliding with seq 0.
    #[test]
    fn seed_from_journal_recovers_coarse_entries_and_seq_continues() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();

        // Three simulated execs, each with an interleaved fine per-statement
        // row; only the first two get a transcript row (the third stands in
        // for a failed exec: journaled, but no session.transcript event).
        let coarse_a = append_simulated_exec(&journal, true, true);
        let coarse_b = append_simulated_exec(&journal, true, true);
        let coarse_c = append_simulated_exec(&journal, true, false);

        let bus = EventBus::default();
        bus.seed_from_journal(&journal);

        assert_eq!(
            bus.journal_published_count(),
            3,
            "only the 3 coarse entries seed the journal index, not the 3 interleaved fine rows"
        );
        assert_eq!(
            bus.journal_index_range(None, 3),
            vec![(0, coarse_a), (1, coarse_b), (2, coarse_c)],
            "seeded oldest-first, in ascending entry-id order"
        );
        assert_eq!(
            bus.transcript_published_count(),
            2,
            "only the 2 coarse entries with a persisted transcript_event row seed the \
             transcript index"
        );
        assert_eq!(
            bus.transcript_index_range(None, 2),
            vec![(0, coarse_a), (1, coarse_b)],
        );

        // The next publish on each channel continues from the seeded count,
        // not 0 — no collision with a cursor a pre-restart agent might hold.
        let journal_event = bus.publish_journal(999, json!({"probe": true}));
        assert_eq!(
            journal_event.seq, 3,
            "journal seq must continue past the seeded count, not reset to 0"
        );
        let transcript_event = bus.publish_transcript(999, json!({"probe": true}));
        assert_eq!(
            transcript_event.seq, 2,
            "transcript seq must continue past the seeded count, not reset to 0"
        );
    }

    /// Zero-regression companion: seeding from a brand-new, empty store (the
    /// common case — most kernel opens are not a restart of a previously
    /// used store) must be a no-op, leaving both channels starting at seq 0
    /// exactly as an ephemeral in-memory kernel does.
    #[test]
    fn seed_from_journal_is_a_no_op_on_a_fresh_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path()).unwrap();
        let bus = EventBus::default();
        bus.seed_from_journal(&journal);
        assert_eq!(bus.journal_published_count(), 0);
        assert_eq!(bus.transcript_published_count(), 0);
        let event = bus.publish_journal(1, json!({}));
        assert_eq!(
            event.seq, 0,
            "a fresh empty store must still start seqs at 0"
        );
    }
}
