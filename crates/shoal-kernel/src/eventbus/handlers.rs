use super::*;

impl Kernel {
    /// Protect the kernel-owned `journal` event channel with the same policy
    /// as direct journal rows and their CAS outputs. Inspect only the declared
    /// channel before full decoding so a denied caller cannot use malformed
    /// cursor fields to probe replay behavior.
    fn require_journal_event_read(
        &self,
        params: &Json,
        attachment: &Attachment,
    ) -> Result<(), RpcError> {
        if params.get("channel").and_then(Json::as_str) == Some("journal") {
            self.require_journal_read(attachment)?;
        }
        Ok(())
    }

    pub(crate) fn ensure_event_owner(&self, owner: &OwnerKey) -> Result<(), RpcError> {
        let journal = self
            .persistence
            .journal
            .lock()
            .map_err(|_| poisoned_subsystem("journal"))?;
        self.runtime
            .events
            .seed_owner_from_journal(&journal, owner)
            .map_err(internal)
    }

    pub(crate) fn handle_events_read(
        self: &Arc<Self>,
        params: Json,
        attached: &mut Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        self.require_journal_event_read(&params, attachment)?;
        self.runtime.events.ensure_replay_subsystem()?;
        let p: EventsReadParams = decode(params)?;
        validate_channel_name(&p.channel)?;
        let owner = attachment.session.key.owner();
        self.ensure_event_owner(&owner)?;
        let effective_limit = p.limit.unwrap_or(EVENTS_DEFAULT_PAGE).min(EVENTS_MAX_PAGE);
        // The `journal` and `session.transcript` channels are journal-backed:
        // a `since` older than the ring's oldest retained seq is served from
        // the durable journal rather than lost (site/content/internals/kernel-protocol.md). Every
        // other channel is ring-only.
        let durable = p.channel == "journal" || p.channel == "session.transcript";
        let published = if p.channel == "journal" {
            self.runtime.events.journal_published_count(&owner)
        } else if p.channel == "session.transcript" {
            self.runtime.events.transcript_published_count(&owner)
        } else {
            self.runtime.events.published_count(&owner, &p.channel)
        };
        let oldest_available = if durable {
            0
        } else {
            self.runtime
                .events
                .ring_oldest_seq(&owner, &p.channel)
                .unwrap_or(published)
        };
        let events = if p.channel == "journal" {
            self.read_journal_channel(&owner, p.since, effective_limit)?
        } else if p.channel == "session.transcript" {
            self.read_transcript_channel(&owner, p.since, effective_limit)?
        } else {
            self.runtime
                .events
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
        let published = self.runtime.events.journal_published_count(owner);
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self
                .runtime
                .events
                .read(owner, "journal", since, Some(limit)));
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
            .runtime
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
            out.extend(self.runtime.events.read(
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
        let cached = self.runtime.events.journal_index_range(owner, since, end);
        let wanted = usize::try_from(end - start).map_err(internal)?;
        if cached.len() == wanted && cached.first().is_some_and(|(seq, _)| *seq == start) {
            return Ok(cached);
        }
        let ids = self
            .persistence
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
            .persistence
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
            if row.kind != shoal_journal::EntryKind::Exec {
                return Err(RpcError {
                    code: INTERNAL_ERROR,
                    message: format!(
                        "durable journal event {} references a {} row instead of an exec row",
                        row.id, row.kind
                    ),
                    data: Some(json!({"subsystem":"events","entry_id":row.id,"quarantined":true})),
                });
            }
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
        let published = self.runtime.events.transcript_published_count(owner);
        if published == 0 || since.is_some_and(|s| s.saturating_add(1) >= published) {
            return Ok(self
                .runtime
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
            .runtime
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
            out.extend(self.runtime.events.read(
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
        let cached = self
            .runtime
            .events
            .transcript_index_range(owner, since, end);
        let wanted = usize::try_from(end - start).map_err(internal)?;
        if cached.len() == wanted && cached.first().is_some_and(|(seq, _)| *seq == start) {
            return Ok(cached);
        }
        let ids = self
            .persistence
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
            .persistence
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
        let event = self.runtime.events.publish_user(
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
        let language_mirror = match shoal_value::json_to_value(&p.payload) {
            Ok(payload) => match attachment.session.lang_bus.try_inject(&p.channel, payload) {
                Ok(seq) => json!({"ok":true,"seq":seq}),
                Err(error) => json!({
                    "ok":false,
                    "error":{"code":error.code,"message":error.msg},
                }),
            },
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
        self.require_journal_event_read(&params, attachment)?;
        let p: EventsSubParams = decode(params)?;
        validate_channel_name(&p.channel)?;
        let Some(writer) = conn else {
            return Err(RpcError {
                code: INTERNAL_ERROR,
                message: "subscription requires a live connection".into(),
                data: None,
            });
        };
        self.runtime.events.subscribe(
            client,
            &attachment.session.key.owner(),
            &p.channel,
            p.since,
            writer,
            self.admission
                .max_subscriptions_per_session
                .load(Ordering::Relaxed),
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
        self.runtime
            .events
            .unsubscribe(client, &attachment.session.key.owner(), &p.channel);
        encode(json!({"channel": p.channel, "subscribed": false}))
    }
}
