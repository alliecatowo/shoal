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
/// get `event` notifications pushed on their own connection.
#[derive(Default)]
pub(crate) struct EventBus {
    channels: Mutex<HashMap<String, ChannelBuf>>,
    subs: Mutex<Vec<Subscriber>>,
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
    writer: SharedWriter,
}

/// Ring depth per channel (AGENT-SURFACE §4 requires ≥1024).
const EVENT_RING_CAP: usize = 1024;

/// The static channels a session may always subscribe to (AGENT-SURFACE §4).
/// `task.{id}` and `user.{name}` are dynamic and not listed here.
pub(crate) const STATIC_CHANNELS: &[&str] = &[
    "session.transcript",
    "journal",
    "approval",
    "render",
    "reef",
];

impl EventBus {
    /// Append `payload` to `channel`'s ring and push it to every live
    /// subscriber of that channel. Returns the assigned event.
    pub(crate) fn publish(&self, channel: &str, payload: Json) -> Event {
        let event = {
            let mut channels = self.channels.lock().unwrap();
            let buf = channels.entry(channel.to_string()).or_default();
            let seq = buf.next_seq;
            buf.next_seq += 1;
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
        // Push to subscribers. A dead connection (write error) is dropped from
        // the subscriber list — the accept loop also cleans up on disconnect.
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| {
            if s.channel != channel {
                return true;
            }
            let note = json!({
                "jsonrpc": JSONRPC,
                "method": "event",
                "params": &event,
            });
            let mut w = s.writer.lock().unwrap();
            write_json_notification(&mut w, &note).is_ok()
        });
        event
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

    /// Register `writer` as a subscriber to `channel`. Any already-buffered
    /// events after `since` are pushed immediately (replay, then live).
    fn subscribe(&self, conn: u64, channel: &str, since: Option<u64>, writer: &SharedWriter) {
        {
            let mut subs = self.subs.lock().unwrap();
            if !subs.iter().any(|s| s.conn == conn && s.channel == channel) {
                subs.push(Subscriber {
                    conn,
                    channel: channel.to_string(),
                    writer: writer.clone(),
                });
            }
        }
        for event in self.read(channel, since, None) {
            let note = json!({"jsonrpc": JSONRPC, "method": "event", "params": &event});
            let mut w = writer.lock().unwrap();
            let _ = write_json_notification(&mut w, &note);
        }
    }

    fn unsubscribe(&self, conn: u64, channel: &str) {
        self.subs
            .lock()
            .unwrap()
            .retain(|s| !(s.conn == conn && s.channel == channel));
    }

    pub(crate) fn remove_conn(&self, conn: u64) {
        self.subs.lock().unwrap().retain(|s| s.conn != conn);
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
        let events = self.events.read(&p.channel, p.since, p.limit);
        encode(json!({"channel": p.channel, "events": events}))
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
