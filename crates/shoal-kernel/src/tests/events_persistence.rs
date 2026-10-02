use super::*;

#[test]
fn events_publish_read_roundtrips_on_a_user_channel() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    // Only user.* channels are client-writable.
    let denied = call(
        &mut client,
        &mut reader,
        2,
        "events.publish",
        json!({"channel":"session.transcript","payload":{"$":"int","v":1}}),
    );
    assert_eq!(denied.error.unwrap().code, INVALID_PARAMS);
    // Publish two values, then read them back with monotonic per-channel seq.
    for (i, v) in ["go", "stop"].iter().enumerate() {
        let published = call(
            &mut client,
            &mut reader,
            3 + i as i64,
            "events.publish",
            json!({"channel":"user.deploy","payload":{"$":"str","v":v}}),
        );
        let published = published.result.unwrap();
        assert_eq!(published["seq"], i as i64);
        assert_eq!(published["language_mirror"]["ok"], true);
        assert_eq!(published["language_mirror"]["seq"], i as i64);
    }
    let read = call(
        &mut client,
        &mut reader,
        9,
        "events.read",
        json!({"channel":"user.deploy"}),
    );
    let events = read.result.unwrap()["events"].clone();
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["payload"], json!({"$":"str","v":"go"}));
    assert_eq!(events[1]["seq"], 1);
    // Cursor read: since=0 returns only events after seq 0.
    let tail = call(
        &mut client,
        &mut reader,
        10,
        "events.read",
        json!({"channel":"user.deploy","since":0}),
    );
    let tail = tail.result.unwrap()["events"].clone();
    assert_eq!(tail.as_array().unwrap().len(), 1);
    assert_eq!(tail[0]["payload"], json!({"$":"str","v":"stop"}));
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn reattach_scopes_journal_blobs_and_subscriptions_to_the_new_owner() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"owner-a","client":{"kind":"test","tty":false}}),
    );
    call(&mut client, &mut reader, 2, "exec", json!({"src":"1 + 2"}));
    let history = call(
        &mut client,
        &mut reader,
        3,
        "journal.query",
        json!({"limit":100}),
    )
    .result
    .unwrap();
    let row = history
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["src"] == "1 + 2")
        .expect("owner A journal row");
    let hash = row["outputs"]
        .as_array()
        .unwrap()
        .first()
        .expect("recorded value blob")["hash"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        &mut client,
        &mut reader,
        4,
        "events.subscribe",
        json!({"channel":"user.private"}),
    );
    assert_eq!(kernel.runtime.events.subscriber_count(), 1);

    call(
        &mut client,
        &mut reader,
        5,
        "session.attach",
        json!({"local_auth":"local-human","session":"owner-b","client":{"kind":"test","tty":false}}),
    );
    assert_eq!(
        kernel.runtime.events.subscriber_count(),
        0,
        "reattach closes subscriptions owned by the old attachment"
    );
    let history_b = call(
        &mut client,
        &mut reader,
        6,
        "journal.query",
        json!({"limit":100}),
    )
    .result
    .unwrap();
    assert!(
        history_b.as_array().unwrap().is_empty(),
        "owner B cannot read owner A's session journal"
    );
    let denied = call(
        &mut client,
        &mut reader,
        7,
        "blob.get",
        json!({"hash":hash,"offset":u64::MAX,"length":u64::MAX}),
    );
    assert_eq!(
        denied.error.expect("foreign blob is opaque").code,
        UNKNOWN_REF
    );

    call(
        &mut client,
        &mut reader,
        8,
        "session.attach",
        json!({"local_auth":"local-human","session":"owner-a","client":{"kind":"test","tty":false}}),
    );
    let owned = call(
        &mut client,
        &mut reader,
        9,
        "blob.get",
        json!({"hash":hash}),
    );
    assert!(
        owned.error.is_none(),
        "owner must retain blob access: {owned:?}"
    );

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// The `journal` channel is journal-backed, as required by
/// `site/content/internals/kernel-protocol.md`:
/// and replayable from ANY seq, not just the last `EVENT_RING_CAP` events.
/// Generate more than a full ring of journal-backed events (one finished
/// journal entry — hence one `journal` event — per exec), then read from a
/// `since` that has aged out of the in-memory ring and assert every aged-out
/// event comes back with the correct seq, correct scoping, and contiguous
/// with the events the ring still holds. Also pins the not-found (since
/// beyond newest) case and that the in-ring fast path is untouched.
#[test]
fn journal_channel_replays_aged_out_events_from_the_journal() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);

    // One `journal` event per exec; overflow the ring by a clear margin.
    let total = EVENT_RING_CAP + 40;
    for i in 0..total {
        let r = call(
            &mut client,
            &mut reader,
            100 + i as i64,
            "exec",
            json!({"src":"1 + 1"}),
        );
        assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
    }

    // seq 0 has aged out of the ring (only the last EVENT_RING_CAP seqs are
    // still retained). Reading since=0 must still return every event after
    // seq 0 — the aged-out ones rebuilt from the journal, then the ring
    // tail — contiguous across the ring boundary.
    let events = read_all_events(&mut client, &mut reader, 9001, "journal", Some(0));
    assert_eq!(
        events.len(),
        total - 1,
        "since=0 (exclusive) must return every journal event after seq 0 — ring + journal \
             fallback, not just the ring's {EVENT_RING_CAP}"
    );
    for (idx, ev) in events.iter().enumerate() {
        let expected_seq = (idx + 1) as u64;
        assert_eq!(
            ev["seq"].as_u64().unwrap(),
            expected_seq,
            "seqs must be contiguous and ascending across the ring boundary: {ev}"
        );
        assert_eq!(ev["channel"], "journal");
        let payload = &ev["payload"]["v"];
        assert_eq!(payload["ok"]["v"], true, "each `1 + 1` finished ok: {ev}");
        assert_eq!(
            payload["head"]["v"], "1",
            "head is the leading command word of the entry: {ev}"
        );
        assert_eq!(
            payload["principal"]["v"],
            principal(),
            "reconstructed events keep the live channel's principal scoping: {ev}"
        );
        // In-memory kernel: no evaluator double-journaling, so the coarse
        // exec entry's rowid is exactly seq+1 (first append == rowid 1 ==
        // seq 0). Pins the seq↔journal-entry correspondence.
        assert_eq!(
            payload["entry_id"]["v"].as_u64().unwrap(),
            expected_seq + 1,
            "seq↔entry_id correspondence: {ev}"
        );
    }

    // A `limit` is a forward page from the exclusive cursor, so callers can
    // continue without the old newest-tail behavior skipping history.
    let limited = call(
        &mut client,
        &mut reader,
        9002,
        "events.read",
        json!({"channel":"journal","since":0,"limit":5}),
    );
    let limited = limited.result.unwrap()["events"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(limited.len(), 5, "limit returns the next 5");
    assert_eq!(limited[0]["seq"].as_u64().unwrap(), 1);
    assert_eq!(limited[4]["seq"].as_u64().unwrap(), 5);

    // Not-found / beyond-newest: since at or past the newest seq is empty,
    // never an error.
    let last_seq = (total - 1) as u64;
    let at_newest = call(
        &mut client,
        &mut reader,
        9003,
        "events.read",
        json!({"channel":"journal","since": last_seq}),
    );
    assert!(
        at_newest.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "since == newest seq: nothing after it"
    );
    let beyond = call(
        &mut client,
        &mut reader,
        9004,
        "events.read",
        json!({"channel":"journal","since": last_seq + 500}),
    );
    assert!(
        beyond.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "since beyond newest seq: empty, not an error"
    );

    // Fast path untouched: a since WITHIN the ring is served from the ring
    // alone and returns exactly the tail after it.
    let within = last_seq - 3;
    let tail = call(
        &mut client,
        &mut reader,
        9005,
        "events.read",
        json!({"channel":"journal","since": within}),
    );
    let tail = tail.result.unwrap()["events"].as_array().unwrap().clone();
    assert_eq!(tail.len(), 3, "since 3 below newest: exactly the last 3");
    assert_eq!(tail[0]["seq"].as_u64().unwrap(), within + 1);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// The journal-backed replay must reflect the `journal` CHANNEL, not every
/// row in the journal store. In an on-disk session the session evaluator
/// ALSO writes its own finer per-statement entries into the same store
/// (`session.rs`), but only the coarse exec-level entry fires a `journal`
/// event. Reconstruction keys off the seq↔entry index, so those
/// per-statement rows are excluded — the replay has exactly one event per
/// exec, not one per statement, with no phantom events and no leakage.
#[test]
fn journal_channel_replay_excludes_evaluator_per_statement_entries() {
    let dir = tempfile::tempdir().unwrap();
    let human_token = create_local_human_token(dir.path());
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach_bearer(&mut client, &mut reader, &human_token);

    // Three top-level statements per exec: on-disk, the store gets one
    // coarse entry + three per-statement entries per exec, but the channel
    // fires once per exec. Overflow the ring so replay must hit the journal.
    let execs = EVENT_RING_CAP + 5;
    for i in 0..execs {
        let r = call(
            &mut client,
            &mut reader,
            100 + i as i64,
            "exec",
            json!({"src":"let a = 1\nlet b = 2\na + b"}),
        );
        assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
    }

    // Sanity: the store itself holds far more rows than the channel
    // published (the evaluator's per-statement entries), so a naive
    // "reconstruct every journal row" would over-produce.
    // Count through the store API. Asking the wire for every detailed row is
    // intentionally rejected by the frame's aggregate JSON-node bound; this
    // setup assertion is about durable row multiplicity, not pagination.
    let rows = Journal::open(dir.path())
        .unwrap()
        .query(&JournalQuery {
            limit: usize::MAX,
            ..Default::default()
        })
        .unwrap();
    let row_count = rows.len();
    assert!(
        row_count >= execs * 3,
        "on-disk store should also hold the finer per-statement entries: {row_count} rows for \
             {execs} execs"
    );
    let kinds_by_id = rows
        .iter()
        .map(|row| (row.id, row.kind))
        .collect::<std::collections::HashMap<_, _>>();
    for row in &rows {
        match row.kind {
            shoal_journal::EntryKind::Exec => assert_eq!(row.parent_id, None),
            shoal_journal::EntryKind::Statement => {
                let parent = row
                    .parent_id
                    .expect("each kernel evaluator row names its exact coarse execution");
                assert_eq!(
                    kinds_by_id.get(&parent),
                    Some(&shoal_journal::EntryKind::Exec)
                );
            }
            shoal_journal::EntryKind::Approval => {
                panic!("this test did not issue an approval")
            }
        }
    }

    // Replay from seq 0 (aged out): EXACTLY one event per exec, contiguous
    // seqs, each the coarse whole-submission entry (head "let") — no
    // phantom per-statement events.
    let events = read_all_events(&mut client, &mut reader, 9001, "journal", Some(0));
    assert_eq!(
        events.len(),
        execs - 1,
        "one journal event per exec, not per statement — evaluator rows are filtered out"
    );
    for (idx, ev) in events.iter().enumerate() {
        assert_eq!(
            ev["seq"].as_u64().unwrap(),
            (idx + 1) as u64,
            "contiguous seqs, no per-statement phantoms: {ev}"
        );
        assert_eq!(
            ev["payload"]["v"]["head"]["v"], "let",
            "each replayed event is the coarse exec-level entry: {ev}"
        );
    }

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// `site/content/internals/kernel-protocol.md` also requires `session.transcript`
/// is now ALSO journal-backed and replayable from ANY seq, mirroring the
/// `journal` channel's replay (`read_transcript_channel`/
/// `reconstruct_transcript_events` in `eventbus.rs`, backed by
/// `shoal_journal::Journal::record_transcript_event`/
/// `transcript_events_by_entry`). Generate more than a full ring of
/// transcript events (one per successful exec), read from a `since` that
/// has aged out of the ring, and assert every aged-out event comes back
/// with the correct seq and the exact persisted payload, contiguous with
/// whatever the ring still holds. Also pins the `limit`/beyond-newest/
/// within-ring cases, same as the `journal` channel's test.
#[test]
fn session_transcript_channel_replays_aged_out_events_from_the_journal() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);

    // One `session.transcript` event per exec (every exec here succeeds);
    // overflow the ring by a clear margin.
    let total = EVENT_RING_CAP + 40;
    for i in 0..total {
        let r = call(
            &mut client,
            &mut reader,
            100 + i as i64,
            "exec",
            json!({"src":"1 + 1"}),
        );
        assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
    }

    // seq 0 has aged out of the ring. Reading since=0 must still return
    // every event after seq 0 — the aged-out ones rebuilt from the
    // journal's `transcript_event` table, then the ring tail —
    // contiguous across the ring boundary.
    let events = read_all_events(
        &mut client,
        &mut reader,
        9001,
        "session.transcript",
        Some(0),
    );
    assert_eq!(
        events.len(),
        total - 1,
        "since=0 (exclusive) must return every transcript event after seq 0 — ring + \
             journal fallback, not just the ring's {EVENT_RING_CAP}"
    );
    for (idx, ev) in events.iter().enumerate() {
        let expected_seq = (idx + 1) as u64;
        assert_eq!(
            ev["seq"].as_u64().unwrap(),
            expected_seq,
            "seqs must be contiguous and ascending across the ring boundary: {ev}"
        );
        assert_eq!(ev["channel"], "session.transcript");
        let payload = &ev["payload"]["v"];
        // Every exec here produces exactly one out[n], so seq N's ref is
        // out:(N+2): seq 0 is out:1 (excluded by since=0), so the first
        // returned event (seq 1) is out:2.
        assert_eq!(
            payload["ref"]["v"],
            format!("out:{}", expected_seq + 1),
            "reconstructed ref must match the live numbering: {ev}"
        );
        assert_eq!(
            payload["summary"]["v"]["type"]["v"], "int",
            "reconstructed summary must reflect the value's real shape, not a placeholder: \
                 {ev}"
        );
    }

    // A `limit` is a forward page from the exclusive cursor.
    let limited = call(
        &mut client,
        &mut reader,
        9002,
        "events.read",
        json!({"channel":"session.transcript","since":0,"limit":5}),
    );
    let limited = limited.result.unwrap()["events"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(limited.len(), 5, "limit returns the next 5");
    assert_eq!(limited[0]["seq"].as_u64().unwrap(), 1);
    assert_eq!(limited[4]["seq"].as_u64().unwrap(), 5);

    // Not-found / beyond-newest: since at or past the newest seq is
    // empty, never an error.
    let last_seq = (total - 1) as u64;
    let at_newest = call(
        &mut client,
        &mut reader,
        9003,
        "events.read",
        json!({"channel":"session.transcript","since": last_seq}),
    );
    assert!(
        at_newest.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "since == newest seq: nothing after it"
    );
    let beyond = call(
        &mut client,
        &mut reader,
        9004,
        "events.read",
        json!({"channel":"session.transcript","since": last_seq + 500}),
    );
    assert!(
        beyond.result.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty(),
        "since beyond newest seq: empty, not an error"
    );

    // Fast path untouched: a since WITHIN the ring is served from the
    // ring alone and returns exactly the tail after it.
    let within = last_seq - 3;
    let tail = call(
        &mut client,
        &mut reader,
        9005,
        "events.read",
        json!({"channel":"session.transcript","since": within}),
    );
    let tail = tail.result.unwrap()["events"].as_array().unwrap().clone();
    assert_eq!(tail.len(), 3, "since 3 below newest: exactly the last 3");
    assert_eq!(tail[0]["seq"].as_u64().unwrap(), within + 1);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// The `session.transcript` channel fires only on a SUCCESSFUL exec
/// (`handlers_exec.rs`'s error path never reaches `publish_transcript`);
/// a failed exec still consumes an `out[n]` slot and gets its own coarse
/// journal entry, but no `transcript_event` row. Journal-backed replay
/// must reflect exactly that — entries with no transcript row are simply
/// absent from the replayed channel, never phantom events — while
/// staying contiguous and in order for the entries that DO have one, the
/// same "reconstruction reflects the channel, not the whole store"
/// property `journal_channel_replay_excludes_evaluator_per_statement_
/// entries` pins for the `journal` channel.
#[test]
fn session_transcript_channel_replay_skips_entries_with_no_transcript_row() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);

    // Overflow the ring by a clear margin even after excluding the
    // ~third of execs that fail (and thus never fire a transcript
    // event): every exec still consumes an out[n] slot and a journal
    // entry, so the store ends up with MORE entries than transcript
    // events, mirroring the journal channel's over-production hazard.
    let total = EVENT_RING_CAP * 2;
    let mut expected_refs = Vec::new();
    for i in 0..total {
        let fails = i % 3 == 0;
        let src = if fails { "1 / 0" } else { "1 + 1" };
        let r = call(
            &mut client,
            &mut reader,
            100 + i as i64,
            "exec",
            json!({"src": src}),
        );
        let n = i + 1; // out[n] numbering: consumed by every exec, pass or fail
        if fails {
            assert!(r.error.is_some(), "exec {i} ({src}) should have failed");
        } else {
            assert!(r.error.is_none(), "exec {i} failed: {:?}", r.error);
            expected_refs.push(format!("out:{n}"));
        }
    }
    assert!(
        expected_refs.len() > EVENT_RING_CAP,
        "the mix must still overflow the transcript ring: {} successes",
        expected_refs.len()
    );

    let events = read_all_events(
        &mut client,
        &mut reader,
        9001,
        "session.transcript",
        Some(0),
    );
    // since=0 excludes seq 0 itself (the first successful exec's own
    // transcript event) — every OTHER successful exec's event must
    // appear, in order, contiguous, with none for a failed exec.
    let expected = &expected_refs[1..];
    assert_eq!(
        events.len(),
        expected.len(),
        "exactly one event per SUCCESSFUL exec after seq 0 — no phantoms for the failed ones"
    );
    for (idx, (ev, want_ref)) in events.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            ev["seq"].as_u64().unwrap(),
            (idx + 1) as u64,
            "contiguous seqs across the ring boundary: {ev}"
        );
        assert_eq!(&ev["payload"]["v"]["ref"]["v"], want_ref);
    }

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// Event-bus seq state for BOTH
/// journal-backed channels must survive a real kernel RESTART, not just
/// live within one process's lifetime. Before this fix, `Kernel::open`
/// always built a brand-new, unseeded `EventBus::default()` — so
/// reopening an EXISTING on-disk store reset both channels' `next_seq`
/// to 0 and their durable indexes to empty, even though the store itself
/// still held every entry from the prior process. A reconnecting agent's
/// persisted `since=N` cursor would then get an empty read, and the
/// freshly-restarted kernel's own next publish would collide with
/// whatever seq 0 meant in the PRIOR lifetime.
///
/// Simulates a restart with two SEPARATE `Kernel::open` calls against the
/// same on-disk state dir, one after the other (never both alive at
/// once — a real process restart, not two concurrent kernels sharing a
/// store). The "prior lifetime" execs are 3-statement programs, exactly
/// like `journal_channel_replay_excludes_evaluator_per_statement_entries`,
/// so the on-disk store also holds the session evaluator's own
/// per-statement rows — proving the seeded index still reflects the
/// CHANNEL after a restart, not every row in the store.
#[test]
fn event_bus_seq_state_survives_a_kernel_restart() {
    let dir = tempfile::tempdir().unwrap();
    let human_token = create_local_human_token(dir.path());
    let pre_restart_execs = 5usize;

    // "Prior lifetime": open, run a few execs, then drop the kernel
    // entirely (closing its journal handle) before reopening.
    {
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach_bearer(&mut client, &mut reader, &human_token);
        for i in 0..pre_restart_execs {
            let r = call(
                &mut client,
                &mut reader,
                100 + i as i64,
                "exec",
                json!({"src":"let a = 1\nlet b = 2\na + b"}),
            );
            assert!(r.error.is_none(), "prelude exec {i} failed: {:?}", r.error);
        }
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }

    // Sanity: the on-disk store holds more rows than execs (the
    // evaluator's own per-statement entries are in there too).
    let total_rows = Journal::open(dir.path())
        .unwrap()
        .query(&JournalQuery {
            limit: 1_000_000,
            ..Default::default()
        })
        .unwrap()
        .len();
    assert!(
        total_rows > pre_restart_execs,
        "the on-disk store should also hold per-statement rows: {total_rows} rows for \
             {pre_restart_execs} execs"
    );

    // "Restart": a brand-new `Kernel::open` (fresh `EventBus::default()`)
    // against the exact same on-disk state dir.
    let kernel2 = Kernel::open(dir.path()).unwrap();
    let (mut client2, mut reader2, thread2) = spawn(&kernel2);
    attach_bearer(&mut client2, &mut reader2, &human_token);

    // A newly published `journal` event must continue past the
    // pre-existing (coarse) journal-channel entry count — never reset
    // to 0 and never collide with a pre-restart seq.
    let exec = call(
        &mut client2,
        &mut reader2,
        200,
        "exec",
        json!({"src":"1 + 1"}),
    );
    assert!(
        exec.error.is_none(),
        "post-restart exec failed: {:?}",
        exec.error
    );

    let read_after = call(
        &mut client2,
        &mut reader2,
        201,
        "events.read",
        json!({"channel":"journal","since":0}),
    );
    let events_after = read_after.result.unwrap()["events"]
        .as_array()
        .unwrap()
        .clone();
    let newest_seq = events_after
        .last()
        .expect("at least the just-published post-restart event")["seq"]
        .as_u64()
        .unwrap();
    assert!(
        newest_seq >= pre_restart_execs as u64,
        "(a) a newly-published journal seq ({newest_seq}) must continue past the \
             {pre_restart_execs} pre-restart journal entries, not reset to 0: {events_after:?}"
    );

    // (b) The pre-restart journal events are still replayable: reading
    // since=0 (aged out of the brand-new, empty ring) reconstructs them
    // from the durable journal — exactly `pre_restart_execs - 1` of them
    // (since=0 excludes seq 0 itself), each the coarse whole-submission
    // entry, not a per-statement phantom.
    let pre_restart_events: Vec<&Json> = events_after
        .iter()
        .filter(|e| e["seq"].as_u64().unwrap() < pre_restart_execs as u64)
        .collect();
    assert_eq!(
        pre_restart_events.len(),
        pre_restart_execs - 1,
        "replay after restart must recover exactly the pre-restart journal events, no more \
             (per-statement rows) and no fewer: {events_after:?}"
    );
    for ev in &pre_restart_events {
        assert_eq!(
            ev["payload"]["v"]["head"]["v"], "let",
            "a reconstructed pre-restart event must be the coarse entry, not a \
                 per-statement row: {ev}"
        );
    }

    // (c) Same replay-survives-restart property for `session.transcript`
    // — every pre-restart exec here succeeded, so each has a persisted
    // transcript row too.
    let transcript_read = call(
        &mut client2,
        &mut reader2,
        202,
        "events.read",
        json!({"channel":"session.transcript","since":0}),
    );
    let transcript_events = transcript_read.result.unwrap()["events"]
        .as_array()
        .unwrap()
        .clone();
    let pre_restart_transcript: Vec<&Json> = transcript_events
        .iter()
        .filter(|e| e["seq"].as_u64().unwrap() < pre_restart_execs as u64)
        .collect();
    assert_eq!(
        pre_restart_transcript.len(),
        pre_restart_execs - 1,
        "replay after restart must recover the pre-restart transcript events too: \
             {transcript_events:?}"
    );

    drop(client2);
    drop(reader2);
    thread2.join().unwrap();
}

/// Companion to the restart test above: a brand-new on-disk store (the
/// common case — most `Kernel::open` calls are not reopening a
/// previously used store) must still start both journal-backed
/// channels' seqs at 0, exactly as before this fix.
#[test]
fn kernel_open_on_a_fresh_store_still_starts_journal_channel_seqs_at_zero() {
    let dir = tempfile::tempdir().unwrap();
    let human_token = create_local_human_token(dir.path());
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach_bearer(&mut client, &mut reader, &human_token);
    let exec = call(&mut client, &mut reader, 1, "exec", json!({"src":"1 + 1"}));
    assert!(exec.error.is_none());
    let read = call(
        &mut client,
        &mut reader,
        2,
        "events.read",
        json!({"channel":"journal","since":null}),
    );
    let events = read.result.unwrap()["events"].as_array().unwrap().clone();
    assert_eq!(
        events[0]["seq"], 0,
        "a fresh on-disk store's first journal event must still start at seq 0: {events:?}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn subscribe_pushes_session_transcript_event_before_the_exec_response() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    call(
        &mut client,
        &mut reader,
        2,
        "events.subscribe",
        json!({"channel":"session.transcript"}),
    );
    write_frame(
        &mut client,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: 3.into(),
            method: "exec".into(),
            params: json!({"src":"1 + 2"}),
        },
    )
    .unwrap();
    // Both the pushed `session.transcript` notification and the exec
    // response land on this connection, but which arrives FIRST is no
    // longer guaranteed (see `site/content/internals/kernel-protocol.md`): the notification is
    // now delivered by a dedicated per-subscriber writer thread, off the
    // dispatch call path entirely, so `publish()` never blocks on a
    // slow/stalled subscriber's socket. That decoupling is exactly what
    // makes the ordering this test used to pin (event strictly before
    // response, because the old code wrote the notification inline,
    // synchronously, from within dispatch) impossible to promise anymore
    // — read both frames and check each on its own merits, regardless of
    // which arrives first.
    let first = recv_line(&mut reader);
    let second = recv_line(&mut reader);
    let (note, resp) = if first["method"] == "event" {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(note["method"], "event", "expected a pushed event: {note}");
    assert_eq!(note["params"]["channel"], "session.transcript");
    assert_eq!(note["params"]["payload"]["v"]["ref"]["v"], "out:1");
    assert_eq!(resp["id"], 3);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}
