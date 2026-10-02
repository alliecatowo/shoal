//! Event-bus publication, replay, forwarding, and quarantine operations.

use super::*;

impl EventBus {
    pub fn shared() -> Arc<EventBus> {
        Arc::new(EventBus::default())
    }

    /// Install the external forwarder (kernel hosting only; the standalone
    /// REPL/script binary never sets one and behaves exactly as before).
    pub fn set_forwarder(&self, f: EventForwarder) {
        let replacement = Some(Arc::from(f));
        match self.forwarder.lock() {
            Ok(mut forwarder) => *forwarder = replacement,
            Err(poisoned) => {
                *poisoned.into_inner() = replacement;
                self.forwarder.clear_poison();
            }
        }
        self.forwarder_quarantined.store(false, Ordering::Release);
    }

    /// Publish `payload` on `name`; returns the assigned monotonic `seq`. Every
    /// live subscriber receives the event record; dead subscribers (their stream
    /// dropped) are pruned. `user.*` events are additionally mirrored to the
    /// host's external bus when a forwarder is installed — the SAME
    /// client-writable rule the wire's `events.publish` enforces, so language
    /// code can never spoof a kernel-owned semantic channel
    /// (`journal`/`approval`/`session.transcript`/…) to wire subscribers.
    pub fn emit(&self, name: &str, payload: Value) -> VResult<u64> {
        validate_channel_name(name)?;
        let payload_bytes = payload_retained_size(&payload)?;
        // Resolve forwarder health before committing locally: a poisoned
        // bridge must not report failure after an event was already appended.
        let forwarder = if name.starts_with("user.") {
            self.forwarder_snapshot()?
        } else {
            None
        };
        let seq = self.publish_local(name, &payload, payload_bytes)?;
        if let Some(f) = forwarder
            && catch_unwind(AssertUnwindSafe(|| f(name, &payload))).is_err()
        {
            self.forwarder_quarantined.store(true, Ordering::Release);
            return Err(channel_poisoned("event forwarder"));
        }
        Ok(seq)
    }

    /// Publish an event that ORIGINATED on the external bus (the reverse
    /// direction of [`Self::emit`]'s mirror): ring + local subscribers only,
    /// never the forwarder — that would echo the event straight back out.
    pub fn inject(&self, name: &str, payload: Value) -> u64 {
        match self.try_inject(name, payload) {
            Ok(seq) => seq,
            Err(error) => {
                eprintln!("shoal: external event injection rejected: {error}");
                u64::MAX
            }
        }
    }

    /// Fallible host-facing form of [`Self::inject`]. New hosts should use this
    /// so a quarantined language bus is surfaced at their request boundary.
    pub fn try_inject(&self, name: &str, payload: Value) -> VResult<u64> {
        validate_channel_name(name)?;
        let payload_bytes = payload_retained_size(&payload)?;
        self.publish_local(name, &payload, payload_bytes)
    }

    fn publish_local(&self, name: &str, payload: &Value, payload_bytes: usize) -> VResult<u64> {
        let mut map = self.lock_channels()?;
        prune_closed_subscribers(&mut map);
        if map
            .get(name)
            .is_some_and(|channel| channel.next_seq > i64::MAX as u64)
        {
            self.quarantine_known_channels(&map);
            return Err(channel_poisoned("channel sequence"));
        }
        admit_channel_identity(&map, name)?;
        let st = map.entry(name.to_string()).or_default();
        let seq = st.next_seq;
        st.next_seq += 1;
        let ts_ns = now_ns();
        let stored_bytes = payload_bytes.saturating_add(std::mem::size_of::<Stored>());
        st.ring.push_back(Stored {
            seq,
            ts_ns,
            payload: payload.clone(),
            retained_bytes: stored_bytes,
        });
        st.ring_bytes = st.ring_bytes.saturating_add(stored_bytes);
        while st.ring.len() > RING_CAP || st.ring_bytes > RING_BYTE_CAP {
            if let Some(evicted) = st.ring.pop_front() {
                st.ring_bytes = st.ring_bytes.saturating_sub(evicted.retained_bytes);
            }
        }
        let event = event_record(name, seq, ts_ns, payload);
        let event_bytes = event_retained_bytes(name, payload_bytes);
        st.subs.retain(|sub| sub.push(event.clone(), event_bytes));
        Ok(seq)
    }

    /// The last payload published on `name`, or `null` if none (no wait).
    pub fn latest(&self, name: &str) -> VResult<Value> {
        validate_channel_name(name)?;
        let map = self.lock_channels()?;
        Ok(map
            .get(name)
            .and_then(|st| st.ring.back())
            .map(|s| s.payload.clone())
            .unwrap_or(Value::Null))
    }

    /// Subscribe to `name`, returning a receiver of `event` records. Replay
    /// mirrors the kernel EventBus (site/content/internals/kernel-protocol.md): `since: None` replays the
    /// whole ring then goes live; `since: Some(n)` replays only `seq > n` (the
    /// in-language `?since=` cursor, site/content/internals/streams-channels.md), then live.
    pub fn events(&self, name: &str, since: Option<u64>) -> VResult<EventReceiver> {
        validate_channel_name(name)?;
        self.subscribe(name, Replay::from_since(since))
    }

    /// Subscribe as a language stream. The custom upstream preserves the
    /// bounded queue's explicit overflow records instead of hiding it behind an
    /// unbounded `mpsc` adapter.
    pub fn event_stream(&self, name: &str, since: Option<u64>) -> VResult<StreamVal> {
        let rx = self.events(name, since)?;
        Ok(StreamVal::from_upstream(
            "event",
            false,
            Box::new(EventUpstream {
                channel: name.to_string(),
                rx,
            }),
        ))
    }

    /// Register a subscriber with the given replay policy.
    fn subscribe(&self, name: &str, replay: Replay) -> VResult<EventReceiver> {
        validate_channel_name(name)?;
        let queue = Arc::new(SubscriberQueue::default());
        let sub = Subscriber(queue.clone());
        let mut map = self.lock_channels()?;
        prune_closed_subscribers(&mut map);
        let live_subscribers = map
            .values()
            .map(|channel| channel.subs.len())
            .sum::<usize>();
        if live_subscribers >= LIVE_SUBSCRIBER_CAP {
            return Err(ErrorVal::new(
                "channel_subscriber_limit",
                format!(
                    "channel subscriber limit ({LIVE_SUBSCRIBER_CAP}) reached; drop a channel stream before subscribing again"
                ),
            ));
        }
        admit_channel_identity(&map, name)?;
        let st = map.entry(name.to_string()).or_default();
        if let Replay::Since(since) = replay {
            let expected = since.saturating_add(1);
            let first_retained = st.ring.front().map_or(st.next_seq, |event| event.seq);
            if first_retained > expected {
                let gap =
                    StreamGap::new(StreamGapReason::HistoryEvicted, first_retained - expected)
                        .with_seq_range(expected, first_retained - 1);
                let _ = sub.push_gap(gap);
            }
        }
        for s in &st.ring {
            if replay.wants(s.seq) {
                let _ = sub.push(
                    event_record(name, s.seq, s.ts_ns, &s.payload),
                    event_retained_bytes(name, s.retained_bytes),
                );
            }
        }
        st.subs.push(sub);
        Ok(EventReceiver { queue })
    }

    /// Block for the next payload on `name` (site/content/internals/streams-channels.md). `timeout` bounds the
    /// wait: `timeout`/`channel_closed` errors surface rather than blocking a host
    /// forever. Subscribes with no replay, so only events published *after* this
    /// call are seen.
    pub fn take(&self, name: &str, timeout: Option<Duration>) -> VResult<Value> {
        self.take_cancelled(name, timeout, None)
    }

    pub(super) fn take_cancelled(
        &self,
        name: &str,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
    ) -> VResult<Value> {
        let rx = self.subscribe(name, Replay::None)?;
        let deadline = timeout
            .map(|duration| {
                Instant::now().checked_add(duration).ok_or_else(|| {
                    ErrorVal::arg_error(format!("channel `{name}` timeout is out of range"))
                })
            })
            .transpose()?;
        loop {
            let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
            match rx.recv(remaining, cancel) {
                Received::Event(event) => return Ok(payload_of(&event)),
                // `.take` promises a payload rather than a delivery-status
                // record. Skip the marker and return the oldest retained event.
                Received::Gap(_) => continue,
                Received::Timeout => {
                    return Err(ErrorVal::new(
                        "timeout",
                        format!("channel `{name}`: no event within timeout"),
                    ));
                }
                Received::Closed => {
                    return Err(ErrorVal::new(
                        "channel_closed",
                        format!("channel `{name}` closed"),
                    ));
                }
                Received::Cancelled => {
                    return Err(ErrorVal::new(
                        "cancelled",
                        format!("channel `{name}` wait cancelled"),
                    ));
                }
                Received::Poisoned => return Err(channel_poisoned("subscriber queue")),
            }
        }
    }

    fn forwarder_snapshot(&self) -> VResult<Option<SharedEventForwarder>> {
        if self.forwarder_quarantined.load(Ordering::Acquire) {
            return Err(channel_poisoned("event forwarder"));
        }
        match self.forwarder.lock() {
            Ok(forwarder) => Ok(forwarder.clone()),
            Err(poisoned) => {
                poisoned.into_inner().take();
                self.forwarder.clear_poison();
                self.forwarder_quarantined.store(true, Ordering::Release);
                Err(channel_poisoned("event forwarder"))
            }
        }
    }

    fn lock_channels(&self) -> VResult<MutexGuard<'_, HashMap<String, ChannelState>>> {
        if self.channels_quarantined.load(Ordering::Acquire) {
            return Err(channel_poisoned("channel registry"));
        }
        match self.channels.lock() {
            Ok(channels) => Ok(channels),
            Err(poisoned) => {
                self.quarantine_channels(poisoned);
                Err(channel_poisoned("channel registry"))
            }
        }
    }

    fn quarantine_channels(
        &self,
        poisoned: PoisonError<MutexGuard<'_, HashMap<String, ChannelState>>>,
    ) {
        self.channels_quarantined.store(true, Ordering::Release);
        let channels = poisoned.into_inner();
        self.quarantine_known_channels(&channels);
    }

    fn quarantine_known_channels(&self, channels: &HashMap<String, ChannelState>) {
        self.channels_quarantined.store(true, Ordering::Release);
        for channel in channels.values() {
            for subscriber in &channel.subs {
                subscriber.0.quarantine();
            }
        }
    }
}
