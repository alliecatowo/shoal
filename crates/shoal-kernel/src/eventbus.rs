//! Kernel-native pub/sub (site/content/internals/kernel-protocol.md): the per-channel ring buffer
//! plus the `events.*` dispatch handlers (`events.read`, `events.publish`,
//! `events.subscribe`, `events.unsubscribe`). See
//! `site/content/internals/kernel-protocol.md` for the wire contract.
use super::*;

mod channels;
mod durable;
mod subscriptions;

use channels::ChannelRegistry;
use durable::{DurableChannel, DurableIndexes};
pub(crate) use subscriptions::SharedWriter;
use subscriptions::SubscriptionRegistry;
#[cfg(test)]
use subscriptions::{SUB_QUEUE_CAP, SubQueue};

/// One ring buffer per channel; `seq` is monotonic per channel. Subscribers
/// get `event` notifications pushed on their own connection via a bounded,
/// per-subscriber queue (site/content/internals/kernel-protocol.md) — see `SubQueue` below for why.
#[derive(Default)]
pub(crate) struct EventBus {
    channels: ChannelRegistry,
    durable: DurableIndexes,
    subscriptions: SubscriptionRegistry,
}

/// Ring depth per channel (site/content/internals/kernel-protocol.md requires ≥1024).
pub(crate) const EVENT_RING_CAP: usize = 1024;

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

impl EventBus {
    /// Channel cursors and durable indexes form one replay invariant. Once
    /// either component is poisoned, requests must fail closed until process
    /// restart can rebuild both from the journal.
    fn ensure_replay_subsystem(&self) -> Result<(), RpcError> {
        if self.channels.is_quarantined() || self.durable.is_quarantined() {
            return Err(RpcError {
                code: INTERNAL_ERROR,
                message: "event replay subsystem is quarantined; restart the kernel".into(),
                data: Some(json!({"subsystem": "events", "quarantined": true})),
            });
        }
        Ok(())
    }

    /// Append `payload` to `channel`'s ring and enqueue it for every live
    /// subscriber (thin wrapper over [`EventBus::publish_inner`] with no
    /// durable-id recording — the path every ring-only channel takes).
    pub(crate) fn publish(&self, owner: &OwnerKey, channel: &str, payload: Json) -> Event {
        self.publish_inner(owner, channel, payload, None)
    }

    /// Publish on the `journal` channel, recording the durable `entry_id` this
    /// event corresponds to so it can be reconstructed from the journal after
    /// it ages out of the ring (site/content/internals/kernel-protocol.md journal-backed replay). The
    /// only difference from `publish` is that the seq↔`entry_id` pair is
    /// appended to `journal_index` atomically with the seq assignment.
    pub(crate) fn publish_journal(&self, owner: &OwnerKey, entry_id: i64, payload: Json) -> Event {
        self.publish_inner(
            owner,
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
    pub(crate) fn publish_transcript(
        &self,
        owner: &OwnerKey,
        entry_id: i64,
        payload: Json,
    ) -> Event {
        self.publish_inner(
            owner,
            "session.transcript",
            payload,
            Some((DurableChannel::Transcript, entry_id)),
        )
    }

    /// Append `payload` to `channel`'s ring and enqueue it for every live
    /// subscriber of that channel. Never blocks on a subscriber's socket:
    /// Each per-subscription queue push is a bounded, in-memory operation; no
    /// socket write happens on this path. Blocking I/O happens later in the
    /// owning connection's dispatcher. A stalled/slow connection can
    /// therefore delay at most its own delivery, never another connection's,
    /// and never this call.
    ///
    /// `durable` is `Some` only for the `journal`/`session.transcript`
    /// channels: the seq↔entry pointer is pushed onto the matching index
    /// inside the same `channels`-lock critical section that assigns `seq`,
    /// so concurrent publishes can never interleave an index out of order
    /// relative to the seqs the ring hands out (index position == seq,
    /// always, per index).
    fn publish_inner(
        &self,
        owner: &OwnerKey,
        channel: &str,
        payload: Json,
        durable: Option<(DurableChannel, i64)>,
    ) -> Event {
        // ChannelRegistry releases both its channel lock and (for durable
        // events) the selected index lock before subscription delivery starts.
        let event = self
            .channels
            .publish(&self.durable, owner, channel, payload, durable);
        // A poison observed inside publish can only be discovered after the
        // infallible internal call has begun. Never deliver its private fault
        // marker to subscribers.
        if self.ensure_replay_subsystem().is_err() {
            return event;
        }
        self.subscriptions.deliver(owner, channel, &event);
        event
    }

    /// Oldest `seq` still retained in `channel`'s ring, or `None` if the
    /// channel has never published. Used by the journal-backed read path to
    /// find the boundary below which events have aged out of the ring.
    pub(crate) fn ring_oldest_seq(&self, owner: &OwnerKey, channel: &str) -> Option<u64> {
        self.channels.oldest_seq(owner, channel)
    }

    /// Total number of events ever published on the `journal` channel this
    /// process (== the channel's `next_seq`). `journal_index` has exactly one
    /// entry per published journal event, so its length is that count.
    pub(crate) fn journal_published_count(&self, owner: &OwnerKey) -> u64 {
        self.channels
            .durable_len(&self.durable, DurableChannel::Journal, owner)
    }

    /// As [`EventBus::journal_published_count`], for `session.transcript`.
    pub(crate) fn transcript_published_count(&self, owner: &OwnerKey) -> u64 {
        self.channels
            .durable_len(&self.durable, DurableChannel::Transcript, owner)
    }

    /// The `(seq, entry_id)` pairs for journal-channel events whose `seq` is
    /// strictly greater than `since` and strictly less than `upto` — i.e. the
    /// events that a caller asked for (via `since`) but that have already aged
    /// out of the ring (below `upto`, the ring's oldest retained seq). Returned
    /// ascending by seq. This is the seq→entry pointer set the kernel then
    /// resolves against the journal to rebuild the actual events.
    pub(crate) fn journal_index_range(
        &self,
        owner: &OwnerKey,
        since: Option<u64>,
        upto: u64,
    ) -> Vec<(u64, i64)> {
        self.channels
            .durable_range(&self.durable, DurableChannel::Journal, owner, since, upto)
    }

    /// As [`EventBus::journal_index_range`], for `session.transcript`.
    pub(crate) fn transcript_index_range(
        &self,
        owner: &OwnerKey,
        since: Option<u64>,
        upto: u64,
    ) -> Vec<(u64, i64)> {
        self.channels.durable_range(
            &self.durable,
            DurableChannel::Transcript,
            owner,
            since,
            upto,
        )
    }

    /// Buffered tail of `channel` from `since` (exclusive), capped at `limit`.
    fn read(
        &self,
        owner: &OwnerKey,
        channel: &str,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Vec<Event> {
        self.channels.read(owner, channel, since, limit)
    }

    /// Register `writer` as a subscriber to `channel` (idempotent per
    /// `(conn, channel)` — re-subscribing finds the existing queue). Any
    /// already-buffered events after `since` are merged into the same bounded
    /// queue that receives live events and drained by the connection's shared
    /// dispatcher, so replay and live delivery are never a separate, ad hoc
    /// blocking write on the calling (dispatch) thread.
    fn subscribe(
        &self,
        conn: u64,
        owner: &OwnerKey,
        channel: &str,
        since: Option<u64>,
        writer: &SharedWriter,
        max_per_session: usize,
    ) -> Result<(), RpcError> {
        self.ensure_replay_subsystem()?;
        let handle = self
            .subscriptions
            .subscribe(conn, owner, channel, writer, max_per_session)?;
        let replay = if handle.is_new() {
            self.read(owner, channel, since, None)
        } else {
            Vec::new()
        };
        // Live delivery was registered first, but remains staged inside the
        // per-channel queue. finish_replay merges by seq, deduplicates, and only
        // then makes the monotonic stream visible to the connection dispatcher.
        handle.finish_replay(replay);
        Ok(())
    }

    fn unsubscribe(&self, conn: u64, owner: &OwnerKey, channel: &str) {
        self.subscriptions.unsubscribe(conn, owner, channel);
    }

    pub(crate) fn remove_conn(&self, conn: u64) {
        self.subscriptions.remove_conn(conn);
    }

    /// Remove every in-memory channel cursor/index/subscription for an evicted
    /// idle owner. Durable command history remains in the journal; event cursors
    /// intentionally restart if that owner later recreates the session.
    pub(crate) fn remove_owner(&self, owner: &OwnerKey) {
        self.channels.remove_owner(&self.durable, owner);
        // Never overlap subscription state with channel or durable locks.
        self.subscriptions.remove_owner(owner);
    }

    #[cfg(test)]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscriptions.len()
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
        let coarse: Vec<(i64, OwnerKey)> = entries
            .iter()
            .filter(|e| serde_json::from_str::<Program>(&e.ast_json).is_ok())
            .map(|e| (e.id, SessionKey::new(&e.principal, &e.session).owner()))
            .collect();
        let mut journal_groups: HashMap<OwnerKey, Vec<i64>> = HashMap::new();
        for (entry_id, owner) in &coarse {
            journal_groups
                .entry(owner.clone())
                .or_default()
                .push(*entry_id);
        }
        for (owner, entry_ids) in &journal_groups {
            self.seed_index(DurableChannel::Journal, owner, "journal", entry_ids);
        }
        let coarse_ids: Vec<i64> = coarse.iter().map(|(entry_id, _)| *entry_id).collect();
        if let Ok(rows) = journal.transcript_events_by_entry(&coarse_ids) {
            let owners: HashMap<i64, OwnerKey> = coarse.into_iter().collect();
            let mut transcript_groups: HashMap<OwnerKey, Vec<i64>> = HashMap::new();
            for row in &rows {
                if let Some(owner) = owners.get(&row.entry_id) {
                    transcript_groups
                        .entry(owner.clone())
                        .or_default()
                        .push(row.entry_id);
                }
            }
            for (owner, entry_ids) in &transcript_groups {
                self.seed_index(
                    DurableChannel::Transcript,
                    owner,
                    "session.transcript",
                    entry_ids,
                );
            }
        }
    }

    /// Shared seeding step for one durable index (see
    /// [`EventBus::seed_from_journal`]): append `entry_ids` (already
    /// ascending, already the exact membership for `channel`) and set that
    /// channel's `next_seq` to match, so the first post-restart publish
    /// continues from N rather than colliding with seq 0..N-1. Only ever
    /// called before this bus is shared with a serving kernel, so there is
    /// no concurrent publish to race with.
    fn seed_index(
        &self,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
        channel: &str,
        entry_ids: &[i64],
    ) {
        self.channels
            .seed_durable(&self.durable, durable_channel, owner, channel, entry_ids);
    }
}

impl Kernel {
    pub(crate) fn handle_events_read(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        self.events.ensure_replay_subsystem()?;
        let owner = attachment.session.key.owner();
        let p: EventsReadParams = decode(params)?;
        // The `journal` and `session.transcript` channels are journal-backed:
        // a `since` older than the ring's oldest retained seq is served from
        // the durable journal rather than lost (site/content/internals/kernel-protocol.md). Every
        // other channel is ring-only.
        let events = if p.channel == "journal" {
            self.read_journal_channel(&owner, p.since, p.limit)?
        } else if p.channel == "session.transcript" {
            self.read_transcript_channel(&owner, p.since, p.limit)?
        } else {
            self.events.read(&owner, &p.channel, p.since, p.limit)
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
        owner: &OwnerKey,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<Event>, RpcError> {
        // Nothing published yet, or `since` at/after the newest seq: the ring
        // already answers correctly (empty), and there is nothing older to
        // reconstruct. This also covers the not-found/beyond-newest case.
        let published = self.events.journal_published_count(owner);
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self.events.read(owner, "journal", since, None));
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
        let oldest = self
            .events
            .ring_oldest_seq(owner, "journal")
            .unwrap_or(published);
        let want = self.events.journal_index_range(owner, since, oldest);
        if !want.is_empty() {
            out = self.reconstruct_journal_events(&want)?;
        }
        // Then the ring tail (fast path, byte-for-byte as before).
        out.extend(self.events.read(owner, "journal", since, None));
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
        owner: &OwnerKey,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Vec<Event>, RpcError> {
        let published = self.events.transcript_published_count(owner);
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self.events.read(owner, "session.transcript", since, None));
        }
        let mut out: Vec<Event> = Vec::new();
        // Same ring-can-be-empty-but-published>0 fallback as
        // `read_journal_channel` above (post-restart, pre-first-publish).
        let oldest = self
            .events
            .ring_oldest_seq(owner, "session.transcript")
            .unwrap_or(published);
        let want = self.events.transcript_index_range(owner, since, oldest);
        if !want.is_empty() {
            out = self.reconstruct_transcript_events(&want)?;
        }
        out.extend(self.events.read(owner, "session.transcript", since, None));
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
        self.events.ensure_replay_subsystem()?;
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
        let event = self.events.publish(
            &attachment.session.key.owner(),
            &p.channel,
            p.payload.clone(),
        );
        self.events.ensure_replay_subsystem()?;
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
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let p: EventsSubParams = decode(params)?;
        let Some(writer) = conn else {
            return Err(RpcError {
                code: INTERNAL_ERROR,
                message: "subscription requires a live connection".into(),
                data: None,
            });
        };
        self.events.subscribe(
            client,
            &attachment.session.key.owner(),
            &p.channel,
            p.since,
            writer,
            self.max_subscriptions_per_session.load(Ordering::Relaxed),
        )?;
        encode(json!({"channel": p.channel, "subscribed": true}))
    }

    pub(crate) fn handle_events_unsubscribe(
        self: &Arc<Self>,
        params: Json,
        client: u64,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        let p: EventsSubParams = decode(params)?;
        self.events
            .unsubscribe(client, &attachment.session.key.owner(), &p.channel);
        encode(json!({"channel": p.channel, "subscribed": false}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn owner(name: &str) -> OwnerKey {
        SessionKey::new("principal:test", name).owner()
    }

    fn attachment(kernel: &Arc<Kernel>, name: &str) -> Option<Attachment> {
        Some(Attachment {
            session: kernel.session(name, "principal:test").unwrap(),
            principal: "principal:test".into(),
            can_approve: false,
            tty: false,
            cancel_epoch: None,
            bearer: None,
            security_epoch: ATTACH_SECURITY_EPOCH,
            connection_trust: ConnectionTrust::EmbeddedHuman,
        })
    }

    #[test]
    fn poisoned_subscriber_queue_is_discarded_and_closed() {
        let queue = SubQueue::new("user.poison".into());
        let poisoner = queue.clone();
        let thread = std::thread::spawn(move || {
            let _state = poisoner.state.lock().unwrap();
            panic!("inject subscriber queue poison");
        });
        assert!(thread.join().is_err());
        assert!(queue.state.is_poisoned());

        queue.push_live(Event {
            channel: "user.poison".into(),
            seq: 1,
            ts: now_ns(),
            payload: json!({"never":"delivered"}),
        });
        queue.finish_replay(Vec::new());
        assert!(
            queue.pop().is_none(),
            "a poisoned subscriber must be dropped, not resumed"
        );
        assert!(
            !queue.state.is_poisoned(),
            "the closed/empty invariant was explicitly restored"
        );
    }

    #[test]
    fn poisoned_channel_registry_makes_repeated_requests_fail_closed() {
        let kernel = Kernel::new();
        let mut attached = attachment(&kernel, "poisoned-channels");
        kernel.events.channels.poison_buffers_for_test();

        for _ in 0..2 {
            let error = kernel
                .handle_events_read(
                    json!({"channel": "user.poison", "since": null}),
                    &mut attached,
                )
                .expect_err("a poisoned replay registry must reject requests");
            assert_eq!(error.code, INTERNAL_ERROR);
            assert_eq!(error.data.unwrap()["quarantined"], true);
        }

        // Internal semantic publishers are infallible by design. They must
        // stop at the quarantine boundary rather than panic or notify.
        let marker = kernel.events.publish(
            &attached.as_ref().unwrap().session.key.owner(),
            "user.poison",
            json!({"ignored": true}),
        );
        assert_eq!(marker.seq, u64::MAX);
    }

    #[test]
    fn poisoned_durable_index_makes_repeated_requests_fail_closed() {
        let kernel = Kernel::new();
        let mut attached = attachment(&kernel, "poisoned-durable");
        kernel.events.durable.poison_journal_for_test();

        for _ in 0..2 {
            let error = kernel
                .handle_events_read(json!({"channel": "journal"}), &mut attached)
                .expect_err("a poisoned durable index must reject requests");
            assert_eq!(error.code, INTERNAL_ERROR);
            assert_eq!(error.data.unwrap()["subsystem"], "events");
        }
    }

    #[test]
    fn poisoned_subscription_registry_is_quarantined_without_request_panics() {
        let kernel = Kernel::new();
        let mut attached = attachment(&kernel, "poisoned-subscriptions");
        let (_peer, server) = UnixStream::pair().unwrap();
        let writer = Arc::new(Mutex::new(server));
        kernel.events.subscriptions.poison_connections_for_test();

        for _ in 0..2 {
            let error = kernel
                .handle_events_subscribe(
                    json!({"channel": "user.poison"}),
                    41,
                    &mut attached,
                    Some(&writer),
                )
                .expect_err("a poisoned subscription registry must reject requests");
            assert_eq!(error.code, INTERNAL_ERROR);
            assert_eq!(error.data.unwrap()["subsystem"], "event_subscriptions");
        }
    }

    #[test]
    fn poisoned_dispatcher_closes_only_its_connection() {
        let bus = EventBus::default();
        let owner = owner("dispatcher-isolation");
        let (mut bad_peer, bad_server) = UnixStream::pair().unwrap();
        let (good_peer, good_server) = UnixStream::pair().unwrap();
        let bad_writer = Arc::new(Mutex::new(bad_server));
        let good_writer = Arc::new(Mutex::new(good_server));
        bus.subscribe(1, &owner, "user.isolated", None, &bad_writer, 8)
            .unwrap();
        bus.subscribe(2, &owner, "user.isolated", None, &good_writer, 8)
            .unwrap();
        bus.subscriptions.poison_dispatcher_for_test(1);

        bus.publish(&owner, "user.isolated", json!({"ok": true}));
        bad_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            std::io::Read::read(&mut bad_peer, &mut byte).unwrap(),
            0,
            "poisoned dispatcher closes its RPC connection"
        );
        let mut good_reader = io::BufReader::new(good_peer);
        let event = recv_line(&mut good_reader);
        assert_eq!(event["params"]["payload"]["ok"], true);
        bus.remove_conn(2);
    }

    #[test]
    fn rings_and_durable_indexes_are_private_to_exact_owner() {
        let bus = EventBus::default();
        let alpha = SessionKey::new("agent:alpha", "shared").owner();
        let beta = SessionKey::new("agent:beta", "shared").owner();

        let alpha_event = bus.publish(&alpha, "user.private", json!({"owner":"alpha"}));
        let beta_event = bus.publish(&beta, "user.private", json!({"owner":"beta"}));
        assert_eq!(alpha_event.seq, 0);
        assert_eq!(beta_event.seq, 0, "each owner has an independent cursor");
        assert_eq!(
            bus.read(&alpha, "user.private", None, None)[0].payload["owner"],
            "alpha"
        );
        assert_eq!(
            bus.read(&beta, "user.private", None, None)[0].payload["owner"],
            "beta"
        );

        bus.publish_journal(&alpha, 11, json!({}));
        bus.publish_journal(&beta, 22, json!({}));
        assert_eq!(bus.journal_index_range(&alpha, None, 1), vec![(0, 11)]);
        assert_eq!(bus.journal_index_range(&beta, None, 1), vec![(0, 22)]);
    }

    #[test]
    fn subscribe_merges_a_racing_live_event_with_replay_exactly_once_in_order() {
        let bus = EventBus::default();
        let owner = owner("replay-race");
        bus.publish(&owner, "user.race", json!({"i": 0}));
        let (client, server) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server));

        // Deterministically stop at the exact former race window: registered
        // for live delivery, but initial replay has not been read/installed.
        let handle = bus
            .subscriptions
            .subscribe(1, &owner, "user.race", &writer, usize::MAX)
            .unwrap();
        assert!(handle.is_new());
        bus.publish(&owner, "user.race", json!({"i": 1}));
        let replay = bus.read(&owner, "user.race", None, None);
        handle.finish_replay(replay);

        let mut reader = io::BufReader::new(client);
        let first = recv_line(&mut reader);
        let second = recv_line(&mut reader);
        assert_eq!(first["params"]["seq"], 0);
        assert_eq!(second["params"]["seq"], 1);
        assert_eq!(first["params"]["payload"]["i"], 0);
        assert_eq!(second["params"]["payload"]["i"], 1);

        reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut duplicate = String::new();
        let error = std::io::BufRead::read_line(&mut reader, &mut duplicate)
            .expect_err("the racing seq must not be replayed a second time");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        bus.remove_conn(1);
    }

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
        let owner = owner("s");
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server_end));
        bus.subscribe(1, &owner, "user.stress", None, &writer, usize::MAX)
            .unwrap();

        let start = Instant::now();
        for i in 0..500 {
            // A few KB per event: comfortably past any default socket
            // buffer many times over across 500 publishes, so this is a
            // faithful stand-in for "a subscriber that never reads".
            bus.publish(
                &owner,
                "user.stress",
                json!({"i": i, "pad": "x".repeat(2048)}),
            );
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "publish() blocked on an unread subscriber: {elapsed:?}"
        );
        drop(client_end);
    }

    /// A genuinely stalled connection (its dispatcher blocked mid-write)
    /// must not stall a second, healthy connection on the same
    /// channel — proving `publish()`'s per-subscriber queue push is
    /// independent across subscribers, not just fast in isolation. The stall
    /// is simulated deterministically (holding the stalled subscriber's own
    /// `SharedWriter` mutex from another thread) rather than relying on OS
    /// socket-buffer sizes, which vary by host and would make this flaky.
    #[test]
    fn a_stalled_subscriber_never_stalls_a_healthy_one() {
        let bus = Arc::new(EventBus::default());
        let owner = owner("s");
        let (stalled_client, stalled_server) = UnixStream::pair().unwrap();
        let stalled_writer: SharedWriter = Arc::new(Mutex::new(stalled_server));
        let (healthy_client, healthy_server) = UnixStream::pair().unwrap();
        let healthy_writer: SharedWriter = Arc::new(Mutex::new(healthy_server));

        bus.subscribe(1, &owner, "user.race", None, &stalled_writer, usize::MAX)
            .unwrap();
        bus.subscribe(2, &owner, "user.race", None, &healthy_writer, usize::MAX)
            .unwrap();

        // Simulate the stalled connection dispatcher being stuck mid-write by
        // holding its writer's mutex from here.
        let hold = stalled_writer.clone();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let stall_thread = std::thread::spawn(move || {
            let _guard = hold.lock().unwrap();
            let _ = release_rx.recv();
        });
        // Give the stall thread a moment to actually acquire the lock before
        // the connection dispatcher (spawned by `subscribe` above) has a
        // chance to race for it.
        std::thread::sleep(Duration::from_millis(50));

        let start = Instant::now();
        for i in 0..10 {
            bus.publish(&owner, "user.race", json!({"i": i}));
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
        let owner = owner("s");
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server_end));
        bus.subscribe(1, &owner, "user.overflow", None, &writer, usize::MAX)
            .unwrap();

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
            bus.publish(&owner, "user.overflow", json!({"i": i}));
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
    fn unsubscribe_stops_the_connection_dispatcher_instead_of_leaking_it() {
        let bus = EventBus::default();
        let owner = owner("s");
        let (client_end, server_end) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server_end));
        bus.subscribe(1, &owner, "user.bye", None, &writer, usize::MAX)
            .unwrap();
        assert_eq!(bus.subscriptions.len(), 1);
        bus.unsubscribe(1, &owner, "user.bye");
        assert_eq!(bus.subscriptions.len(), 0);
        drop(client_end);
    }

    #[test]
    fn one_connection_uses_one_bounded_dispatcher_and_stops_with_its_last_subscription() {
        let bus = EventBus::default();
        let owner = owner("one-dispatcher");
        let (client, server) = UnixStream::pair().unwrap();
        let writer: SharedWriter = Arc::new(Mutex::new(server));
        let channels = 128;
        for id in 0..channels {
            bus.subscribe(
                77,
                &owner,
                &format!("user.channel-{id}"),
                None,
                &writer,
                usize::MAX,
            )
            .unwrap();
        }
        assert_eq!(bus.subscriptions.len(), channels);
        assert_eq!(bus.subscriptions.dispatcher_count(), 1);
        let probe = bus.subscriptions.dispatcher_probe(77).unwrap();

        for id in 0..channels {
            bus.unsubscribe(77, &owner, &format!("user.channel-{id}"));
        }
        assert_eq!(bus.subscriptions.len(), 0);
        assert_eq!(bus.subscriptions.dispatcher_count(), 0);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !probe.stopped() {
            assert!(
                Instant::now() < deadline,
                "connection dispatcher did not stop after its last subscription"
            );
            std::thread::yield_now();
        }
        drop(client);
    }

    #[test]
    fn subscribe_reserves_the_per_session_quota_atomically() {
        let bus = Arc::new(EventBus::default());
        let owner = owner("same-session");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let subscribe =
            |conn: u64, bus: Arc<EventBus>, owner: OwnerKey, barrier: Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    let (_client, server) = UnixStream::pair().unwrap();
                    let writer: SharedWriter = Arc::new(Mutex::new(server));
                    barrier.wait();
                    bus.subscribe(conn, &owner, &format!("user.{conn}"), None, &writer, 1)
                })
            };
        let first = subscribe(1, bus.clone(), owner.clone(), barrier.clone());
        let second = subscribe(2, bus.clone(), owner, barrier.clone());
        barrier.wait();
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .filter(|error| error.code == QUOTA_EXCEEDED)
                .count(),
            1
        );
        assert_eq!(bus.subscriptions.len(), 1);
        bus.remove_conn(1);
        bus.remove_conn(2);
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
        let owner = SessionKey::new("human", "s").owner();

        assert_eq!(
            bus.journal_published_count(&owner),
            3,
            "only the 3 coarse entries seed the journal index, not the 3 interleaved fine rows"
        );
        assert_eq!(
            bus.journal_index_range(&owner, None, 3),
            vec![(0, coarse_a), (1, coarse_b), (2, coarse_c)],
            "seeded oldest-first, in ascending entry-id order"
        );
        assert_eq!(
            bus.transcript_published_count(&owner),
            2,
            "only the 2 coarse entries with a persisted transcript_event row seed the \
             transcript index"
        );
        assert_eq!(
            bus.transcript_index_range(&owner, None, 2),
            vec![(0, coarse_a), (1, coarse_b)],
        );

        // The next publish on each channel continues from the seeded count,
        // not 0 — no collision with a cursor a pre-restart agent might hold.
        let journal_event = bus.publish_journal(&owner, 999, json!({"probe": true}));
        assert_eq!(
            journal_event.seq, 3,
            "journal seq must continue past the seeded count, not reset to 0"
        );
        let transcript_event = bus.publish_transcript(&owner, 999, json!({"probe": true}));
        assert_eq!(
            transcript_event.seq, 2,
            "transcript seq must continue past the seeded count, not reset to 0"
        );
    }

    #[test]
    fn durable_seed_and_publish_share_one_lock_order() {
        let bus = Arc::new(EventBus::default());
        let owner = owner("lock-order");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let (done_tx, done_rx) = mpsc::channel();

        let seed_bus = bus.clone();
        let seed_owner = owner.clone();
        let seed_barrier = barrier.clone();
        let seed_done = done_tx.clone();
        let seed = std::thread::spawn(move || {
            seed_barrier.wait();
            seed_bus.seed_index(DurableChannel::Journal, &seed_owner, "journal", &[10, 11]);
            seed_done.send(()).unwrap();
        });
        let publish_bus = bus.clone();
        let publish_owner = owner.clone();
        let publish_barrier = barrier.clone();
        let publish = std::thread::spawn(move || {
            publish_barrier.wait();
            publish_bus.publish_journal(&publish_owner, 99, json!({}));
            done_tx.send(()).unwrap();
        });
        barrier.wait();
        for _ in 0..2 {
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("seed/publish lock order must not deadlock");
        }
        seed.join().unwrap();
        publish.join().unwrap();
        assert_eq!(bus.journal_published_count(&owner), 3);
        assert_eq!(bus.publish_journal(&owner, 100, json!({})).seq, 3);
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
        let owner = owner("empty");
        assert_eq!(bus.journal_published_count(&owner), 0);
        assert_eq!(bus.transcript_published_count(&owner), 0);
        let event = bus.publish_journal(&owner, 1, json!({}));
        assert_eq!(
            event.seq, 0,
            "a fresh empty store must still start seqs at 0"
        );
    }
}
