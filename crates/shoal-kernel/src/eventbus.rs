//! Kernel-native pub/sub (site/content/internals/kernel-protocol.md): the per-channel ring buffer
//! plus the `events.*` dispatch handlers (`events.read`, `events.publish`,
//! `events.subscribe`, `events.unsubscribe`). See
//! `site/content/internals/kernel-protocol.md` for the wire contract.
use super::*;

mod channels;
mod durable;
mod subscriptions;

use channels::ChannelRegistry;
use durable::{DURABLE_POINTER_CAP, DurableChannel, DurableIndexes};
pub(crate) use subscriptions::SharedWriter;
use subscriptions::SubscriptionRegistry;
#[cfg(test)]
use subscriptions::{SUB_QUEUE_CAP, SUB_QUEUE_MAX_BYTES, SubQueue};

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

/// Default and absolute row ceilings for one pull page. Byte bounding below
/// is a second wall: a handful of hostile large payloads cannot evade this
/// cap and produce an oversized JSON-RPC response.
pub(crate) const EVENTS_DEFAULT_PAGE: usize = 256;
pub(crate) const EVENTS_MAX_PAGE: usize = 256;
const EVENTS_MAX_CONTENT_BYTES: usize = MAX_FRAME_LEN / 2;

/// Live EventBus admission and retained-memory walls. These are deliberately
/// below the 16 MiB frame ceiling: one accepted user event can be cloned into
/// a ring and several subscriber queues, so frame-size admission alone is not
/// a meaningful memory bound.
pub(crate) const EVENT_CHANNEL_MAX_BYTES: usize = 128;
pub(crate) const USER_CHANNELS_PER_OWNER_MAX: usize = 256;
pub(crate) const USER_EVENT_PAYLOAD_MAX_BYTES: usize = 64 * 1024;
pub(crate) const EVENT_PAYLOAD_MAX_DEPTH: usize = 64;
pub(crate) const EVENT_RING_MAX_BYTES: usize = 2 * 1024 * 1024;

struct JsonSizeWriter {
    bytes: usize,
    max: usize,
}

impl std::io::Write for JsonSizeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.checked_add(buf.len()) else {
            return Err(io::Error::other("JSON size overflow"));
        };
        if next > self.max {
            return Err(io::Error::other("JSON exceeds retained-size limit"));
        }
        self.bytes = next;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn json_depth(value: &Json, depth: usize) -> Result<(), RpcError> {
    if depth > EVENT_PAYLOAD_MAX_DEPTH {
        return Err(RpcError {
            code: INVALID_PARAMS,
            message: format!(
                "event payload nesting exceeds the {EVENT_PAYLOAD_MAX_DEPTH}-level limit"
            ),
            data: Some(json!({
                "limit":"event_payload_depth",
                "max":EVENT_PAYLOAD_MAX_DEPTH,
            })),
        });
    }
    match value {
        Json::Array(items) => {
            for item in items {
                json_depth(item, depth + 1)?;
            }
        }
        Json::Object(fields) => {
            for item in fields.values() {
                json_depth(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_encoded_len(value: &Json, max: usize) -> Result<usize, RpcError> {
    let mut writer = JsonSizeWriter { bytes: 0, max };
    serde_json::to_writer(&mut writer, value).map_err(|_| RpcError {
        code: INVALID_PARAMS,
        message: format!("event payload exceeds the {max}-byte encoded limit"),
        data: Some(json!({"limit":"event_payload_bytes","max":max})),
    })?;
    Ok(writer.bytes)
}

pub(super) fn event_retained_bytes(event: &Event) -> usize {
    let mut writer = JsonSizeWriter {
        bytes: 0,
        max: usize::MAX,
    };
    if serde_json::to_writer(&mut writer, event).is_err() {
        return usize::MAX;
    }
    writer.bytes
}

fn validate_channel_name(channel: &str) -> Result<(), RpcError> {
    if channel.is_empty()
        || channel.len() > EVENT_CHANNEL_MAX_BYTES
        || !channel
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(RpcError {
            code: INVALID_PARAMS,
            message: format!(
                "channel must be 1..={EVENT_CHANNEL_MAX_BYTES} ASCII bytes using [A-Za-z0-9._-]"
            ),
            data: Some(json!({
                "limit":"event_channel_name",
                "max_bytes":EVENT_CHANNEL_MAX_BYTES,
                "actual_bytes":channel.len(),
            })),
        });
    }
    Ok(())
}

fn validate_user_event(channel: &str, payload: &Json) -> Result<(), RpcError> {
    validate_channel_name(channel)?;
    if !channel.starts_with("user.") || channel.len() == "user.".len() {
        return Err(RpcError {
            code: INVALID_PARAMS,
            message: "only non-empty user.* channels may be published to".into(),
            data: Some(json!({"channel":channel})),
        });
    }
    json_depth(payload, 0)?;
    json_encoded_len(payload, USER_EVENT_PAYLOAD_MAX_BYTES)?;
    Ok(())
}

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

    /// Fallible client/language publication boundary. Validation happens
    /// before the payload is cloned into a ring, subscriber queues, or the
    /// reverse evaluator bridge.
    pub(crate) fn publish_user(
        &self,
        owner: &OwnerKey,
        channel: &str,
        payload: Json,
    ) -> Result<Event, RpcError> {
        validate_user_event(channel, &payload)?;
        self.ensure_replay_subsystem()?;
        let event = self
            .channels
            .publish_user(&self.durable, owner, channel, payload)?;
        self.ensure_replay_subsystem()?;
        self.subscriptions.deliver(owner, channel, &event);
        Ok(event)
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

    fn published_count(&self, owner: &OwnerKey, channel: &str) -> u64 {
        self.channels.next_seq(owner, channel)
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

    /// Lazily hydrate one exact owner. Kernel construction remains O(1): no
    /// historical row is loaded until that owner first attaches/reads/execs,
    /// and even then only a count plus the newest bounded pointer window is
    /// retained. Older seq pages are resolved directly from the journal.
    pub(crate) fn seed_owner_from_journal(
        &self,
        journal: &Journal,
        owner: &OwnerKey,
    ) -> Result<(), String> {
        if self
            .channels
            .durable_is_initialized(&self.durable, DurableChannel::Journal, owner)
            && self.channels.durable_is_initialized(
                &self.durable,
                DurableChannel::Transcript,
                owner,
            )
        {
            return Ok(());
        }
        let journal_seed = journal
            .journal_event_seed(&owner.0.principal, &owner.0.name, DURABLE_POINTER_CAP)
            .map_err(|error| error.to_string())?;
        let transcript_seed = journal
            .transcript_event_seed(&owner.0.principal, &owner.0.name, DURABLE_POINTER_CAP)
            .map_err(|error| error.to_string())?;
        self.seed_index(
            DurableChannel::Journal,
            owner,
            "journal",
            journal_seed.published,
            &journal_seed.tail_entry_ids,
        )?;
        self.seed_index(
            DurableChannel::Transcript,
            owner,
            "session.transcript",
            transcript_seed.published,
            &transcript_seed.tail_entry_ids,
        )?;
        Ok(())
    }

    /// Shared hydration step for one durable index: install its full count
    /// and bounded ascending tail, then set the channel's `next_seq` so the
    /// first post-restart publish continues from N.
    fn seed_index(
        &self,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
        channel: &str,
        published: u64,
        entry_ids: &[i64],
    ) -> Result<(), String> {
        self.channels
            .seed_durable(
                &self.durable,
                durable_channel,
                owner,
                channel,
                published,
                entry_ids,
            )
            .then_some(())
            .ok_or_else(|| "durable event cursor hydration was quarantined".to_string())
    }
}

/// Apply the byte wall after the row wall but before encoding the RPC result.
/// A single oversized payload is replaced by an explicit marker that keeps
/// its channel/seq/ts cursor advance honest; otherwise the page stops before
/// the event that would cross the wall and `next_since` resumes from the last
/// returned sequence.
fn bound_event_page(events: Vec<Event>) -> Result<(Vec<Event>, usize, bool), RpcError> {
    let mut bounded = Vec::with_capacity(events.len());
    let mut content_bytes = 0usize;
    let mut payloads_truncated = events.iter().any(event_has_truncated_payload);
    for event in events {
        let (accepted, truncated) = push_bounded_event(&mut bounded, &mut content_bytes, event)?;
        payloads_truncated |= truncated;
        if !accepted {
            break;
        }
    }
    Ok((bounded, content_bytes, payloads_truncated))
}

fn event_has_truncated_payload(event: &Event) -> bool {
    event.payload["v"]["payload_truncated"]["v"] == Json::Bool(true)
}

/// Append one event without allowing the accumulated encoded events to cross
/// the page byte wall. A single oversized event becomes a small explicit
/// marker so its sequence remains consumable; an aggregate overflow stops
/// before the event and lets continuation resume from the prior cursor.
fn push_bounded_event(
    events: &mut Vec<Event>,
    content_bytes: &mut usize,
    mut event: Event,
) -> Result<(bool, bool), RpcError> {
    let mut encoded = serde_json::to_vec(&event).map_err(internal)?;
    let mut payload_truncated = false;
    if encoded.len() > EVENTS_MAX_CONTENT_BYTES {
        event.payload = json!({
            "$": "record",
            "v": {
                "payload_truncated": {"$":"bool","v":true},
                "reason": {"$":"str","v":"event payload exceeds events.read byte wall"}
            }
        });
        encoded = serde_json::to_vec(&event).map_err(internal)?;
        payload_truncated = true;
    }
    let Some(next_bytes) = content_bytes.checked_add(encoded.len()) else {
        return Ok((false, payload_truncated));
    };
    if next_bytes > EVENTS_MAX_CONTENT_BYTES {
        return Ok((false, payload_truncated));
    }
    *content_bytes = next_bytes;
    events.push(event);
    Ok((true, payload_truncated))
}

impl Kernel {
    pub(crate) fn ensure_event_owner(&self, owner: &OwnerKey) -> Result<(), RpcError> {
        let journal = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?;
        self.events
            .seed_owner_from_journal(&journal, owner)
            .map_err(internal)
    }

    pub(crate) fn handle_events_read(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        self.events.ensure_replay_subsystem()?;
        let owner = attachment.session.key.owner();
        self.ensure_event_owner(&owner)?;
        let p: EventsReadParams = decode(params)?;
        validate_channel_name(&p.channel)?;
        let effective_limit = p.limit.unwrap_or(EVENTS_DEFAULT_PAGE).min(EVENTS_MAX_PAGE);
        // The `journal` and `session.transcript` channels are journal-backed:
        // a `since` older than the ring's oldest retained seq is served from
        // the durable journal rather than lost (site/content/internals/kernel-protocol.md). Every
        // other channel is ring-only.
        let durable = p.channel == "journal" || p.channel == "session.transcript";
        let published = if p.channel == "journal" {
            self.events.journal_published_count(&owner)
        } else if p.channel == "session.transcript" {
            self.events.transcript_published_count(&owner)
        } else {
            self.events.published_count(&owner, &p.channel)
        };
        let oldest_available = if durable {
            0
        } else {
            self.events
                .ring_oldest_seq(&owner, &p.channel)
                .unwrap_or(published)
        };
        let events = if p.channel == "journal" {
            self.read_journal_channel(&owner, p.since, effective_limit)?
        } else if p.channel == "session.transcript" {
            self.read_transcript_channel(&owner, p.since, effective_limit)?
        } else {
            self.events
                .read(&owner, &p.channel, p.since, Some(effective_limit))
        };
        let (events, content_bytes, payloads_truncated) = bound_event_page(events)?;
        let returned = events.len();
        let requested_start = p.since.map_or(0, |seq| seq.saturating_add(1));
        let cursor = events.last().map(|event| event.seq).or(p.since);
        let consumed = cursor.map_or(0, |seq| seq.saturating_add(1));
        let truncated = consumed < published;
        encode(json!({
            "channel": p.channel,
            "events": events,
            "page": {
                "returned": returned,
                "content_bytes": content_bytes,
                "max_events": EVENTS_MAX_PAGE,
                "max_content_bytes": EVENTS_MAX_CONTENT_BYTES,
                "next_since": truncated.then_some(cursor).flatten(),
                "truncated": truncated,
                "request_clamped": p.limit.is_some_and(|limit| limit > EVENTS_MAX_PAGE),
                "payloads_truncated": payloads_truncated,
                "oldest_available": oldest_available,
                "history_lost": !durable && requested_start < oldest_available,
            }
        }))
    }

    /// Read the `journal` channel with journal-backed replay, as specified by
    /// `site/content/internals/kernel-protocol.md`. Events still in the in-memory ring are served from it
    /// exactly as before (the fast path is untouched); events that have aged
    /// out of the ring — a `since` below the ring's oldest retained seq — are
    /// reconstructed from the durable journal so an agent can replay the
    /// channel from ANY seq, not just the last `EVENT_RING_CAP`.
    ///
    /// The seq↔journal correspondence: every `journal` event's `seq` was
    /// recorded against the coarse exec-level journal `entry_id` it
    /// announced. Reconstruction resolves each aged-out seq from the bounded
    /// pointer tail or an exact owner-scoped journal page, then
    /// rebuilds the `{entry_id, head, ok, principal}` payload from the journal
    /// row itself. Only the newest pointer window lives in memory; payloads
    /// and older pointer pages are journal-backed. Using this membership as the
    /// membership set is also what keeps reconstruction faithful in on-disk
    /// sessions, where the session evaluator ALSO writes its own finer
    /// per-statement entries into the same store (`session.rs`): those rows
    /// were never published on this channel, so they are excluded because their
    /// ids are not in the index.
    fn read_journal_channel(
        self: &Arc<Self>,
        owner: &OwnerKey,
        since: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Event>, RpcError> {
        // Nothing published yet, or `since` at/after the newest seq: the ring
        // already answers correctly (empty), and there is nothing older to
        // reconstruct. This also covers the not-found/beyond-newest case.
        let published = self.events.journal_published_count(owner);
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self.events.read(owner, "journal", since, Some(limit)));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let start = since.map_or(0, |seq| seq.saturating_add(1));
        let end = start.saturating_add(limit as u64).min(published);
        let mut out: Vec<Event> = Vec::new();
        // Reconstruct the gap below the ring, if `since` reaches into it. The
        // ring can be genuinely EMPTY here even though `published > 0`: right
        // after a kernel restart, lazy owner hydration seeds the durable
        // cursor from a pre-existing store but deliberately leaves the
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
        let cold_end = oldest.min(end);
        let want = self.journal_pointer_page(owner, start, cold_end)?;
        if !want.is_empty() {
            out = self.reconstruct_journal_events(&want)?;
            if out.len() < want.len() {
                return Ok(out);
            }
        }
        let ring_start = start.max(oldest);
        if ring_start < end {
            let ring_since = ring_start.checked_sub(1);
            out.extend(self.events.read(
                owner,
                "journal",
                ring_since,
                Some(usize::try_from(end - ring_start).unwrap_or(EVENTS_MAX_PAGE)),
            ));
        }
        Ok(out)
    }

    fn journal_pointer_page(
        &self,
        owner: &OwnerKey,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, i64)>, RpcError> {
        if start >= end {
            return Ok(Vec::new());
        }
        let since = start.checked_sub(1);
        let cached = self.events.journal_index_range(owner, since, end);
        let wanted = usize::try_from(end - start).map_err(internal)?;
        if cached.len() == wanted && cached.first().is_some_and(|(seq, _)| *seq == start) {
            return Ok(cached);
        }
        let ids = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?
            .journal_event_entry_ids(&owner.0.principal, &owner.0.name, start, wanted)
            .map_err(internal)?;
        Ok((start..).zip(ids).collect())
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
        let journal = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?;
        let mut events = Vec::with_capacity(want.len());
        let mut content_bytes = 0usize;
        for &(seq, id) in want {
            // Fetch one row at a time so 256 near-frame-sized historical
            // sources cannot be materialized before the response byte wall
            // gets a chance to stop the page.
            let mut rows = journal.entries_by_id(&[id]).map_err(internal)?;
            let row = rows.pop().ok_or_else(|| RpcError {
                code: INTERNAL_ERROR,
                message: format!("durable journal event {id} is missing"),
                data: Some(json!({"subsystem":"events","entry_id":id,"quarantined":true})),
            })?;
            serde_json::from_str::<Program>(&row.ast_json).map_err(|error| RpcError {
                code: INTERNAL_ERROR,
                message: format!(
                    "durable journal event {} has invalid whole-program AST: {error}",
                    row.id
                ),
                data: Some(json!({"subsystem":"events","entry_id":row.id,"quarantined":true})),
            })?;
            let ok = row.ok.unwrap_or(false);
            let event = Event {
                channel: "journal".to_string(),
                seq,
                // The journal records the entry's start (`ts_ns`) and, once
                // finished, its duration; the live event fired at finish, so
                // start + duration is the faithful reconstruction of that
                // instant (falls back to start for the degenerate no-dur
                // case). Consumers dedup by seq, never by ts.
                ts: row.ts_ns.saturating_add(row.dur_ns.unwrap_or(0)),
                payload: journal_event(row.id, &row.src, ok, &row.principal),
            };
            if !push_bounded_event(&mut events, &mut content_bytes, event)?.0 {
                break;
            }
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
        limit: usize,
    ) -> Result<Vec<Event>, RpcError> {
        let published = self.events.transcript_published_count(owner);
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self
                .events
                .read(owner, "session.transcript", since, Some(limit)));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let start = since.map_or(0, |seq| seq.saturating_add(1));
        let end = start.saturating_add(limit as u64).min(published);
        let mut out: Vec<Event> = Vec::new();
        // Same ring-can-be-empty-but-published>0 fallback as
        // `read_journal_channel` above (post-restart, pre-first-publish).
        let oldest = self
            .events
            .ring_oldest_seq(owner, "session.transcript")
            .unwrap_or(published);
        let cold_end = oldest.min(end);
        let want = self.transcript_pointer_page(owner, start, cold_end)?;
        if !want.is_empty() {
            out = self.reconstruct_transcript_events(&want)?;
            if out.len() < want.len() {
                return Ok(out);
            }
        }
        let ring_start = start.max(oldest);
        if ring_start < end {
            let ring_since = ring_start.checked_sub(1);
            out.extend(self.events.read(
                owner,
                "session.transcript",
                ring_since,
                Some(usize::try_from(end - ring_start).unwrap_or(EVENTS_MAX_PAGE)),
            ));
        }
        Ok(out)
    }

    fn transcript_pointer_page(
        &self,
        owner: &OwnerKey,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, i64)>, RpcError> {
        if start >= end {
            return Ok(Vec::new());
        }
        let since = start.checked_sub(1);
        let cached = self.events.transcript_index_range(owner, since, end);
        let wanted = usize::try_from(end - start).map_err(internal)?;
        if cached.len() == wanted && cached.first().is_some_and(|(seq, _)| *seq == start) {
            return Ok(cached);
        }
        let ids = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?
            .transcript_event_entry_ids(&owner.0.principal, &owner.0.name, start, wanted)
            .map_err(internal)?;
        Ok((start..).zip(ids).collect())
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
        let journal = self
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?;
        let mut events = Vec::with_capacity(want.len());
        let mut content_bytes = 0usize;
        for &(seq, id) in want {
            // Payloads are intentionally read incrementally: a page of many
            // individually legal but near-frame-sized transcript rows must
            // not allocate in proportion to the requested row limit.
            let mut rows = journal
                .transcript_events_by_entry(&[id])
                .map_err(internal)?;
            let row = rows.pop().ok_or_else(|| RpcError {
                code: INTERNAL_ERROR,
                message: format!("durable transcript event {id} is missing"),
                data: Some(json!({"subsystem":"events","entry_id":id,"quarantined":true})),
            })?;
            let payload: Json = serde_json::from_str(&row.payload_json).map_err(internal)?;
            let event = Event {
                channel: "session.transcript".to_string(),
                seq,
                ts: row.ts_ns,
                payload,
            };
            if !push_bounded_event(&mut events, &mut content_bytes, event)?.0 {
                break;
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
        // Validate the borrowed decoded value before making the first clone
        // for ring/subscriber ownership.
        validate_user_event(&p.channel, &p.payload)?;
        let event = self.events.publish_user(
            &attachment.session.key.owner(),
            &p.channel,
            p.payload.clone(),
        )?;
        // Reverse direction: a wire publish is normally also visible to the
        // session's in-language channels (`channel("user.x").latest()` /
        // `.events()`). The wire event is already authoritative at this point,
        // so a quarantined/full language bus is reported as mirror degradation
        // in the successful result rather than turning a committed publish into
        // a retryable RPC error. `try_inject` never re-forwards, so no echo loop
        // is possible. The cached bus avoids waiting on the evaluator lock.
        let payload = shoal_value::json_to_value(&p.payload);
        let language_mirror = match attachment.session.lang_bus.try_inject(&p.channel, payload) {
            Ok(seq) => json!({"ok":true,"seq":seq}),
            Err(error) => json!({
                "ok":false,
                "error":{"code":error.code,"message":error.msg},
            }),
        };
        encode(json!({
            "channel": event.channel,
            "seq": event.seq,
            "ts": event.ts,
            "language_mirror": language_mirror,
        }))
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
        validate_channel_name(&p.channel)?;
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
        validate_channel_name(&p.channel)?;
        self.events
            .unsubscribe(client, &attachment.session.key.owner(), &p.channel);
        encode(json!({"channel": p.channel, "subscribed": false}))
    }
}

#[cfg(test)]
#[path = "eventbus/tests.rs"]
mod tests;
