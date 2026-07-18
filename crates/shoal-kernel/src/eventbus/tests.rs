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
        let _state = poisoner
            .state
            .lock()
            .expect("test lock should not be poisoned");
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
        let _guard = hold.lock().expect("test lock should not be poisoned");
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

/// Once a stalled subscriber's queue exceeds either retained-state wall,
/// further events for it must coalesce into a
/// `{dropped, dropped_bytes, latest_seq}`
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
        let _guard = hold.lock().expect("test lock should not be poisoned");
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
        .expect("expected a coalesced {dropped, dropped_bytes, latest_seq} summary after overflow");
    assert!(
        summary["dropped"].as_u64().unwrap() > 0,
        "summary must report a nonzero drop count: {summary}"
    );
    assert!(
        summary["dropped_bytes"].as_u64().unwrap() > 0,
        "summary must report dropped encoded bytes: {summary}"
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
// Lazy exact-owner hydration: seqs surviving a kernel restart.
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
                kind: shoal_journal::EntryKind::Statement,
                parent_id: None,
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
            kind: shoal_journal::EntryKind::Exec,
            parent_id: None,
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
fn owner_hydration_recovers_coarse_entries_and_seq_continues() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();

    // Three simulated execs, each with an interleaved fine per-statement
    // row; only the first two get a transcript row (the third stands in
    // for a failed exec: journaled, but no session.transcript event).
    let coarse_a = append_simulated_exec(&journal, true, true);
    let coarse_b = append_simulated_exec(&journal, true, true);
    let coarse_c = append_simulated_exec(&journal, true, false);

    let bus = EventBus::default();
    let owner = SessionKey::new("human", "s").owner();
    bus.seed_owner_from_journal(&journal, &owner).unwrap();

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
        let _ = seed_bus.seed_index(
            DurableChannel::Journal,
            &seed_owner,
            "journal",
            2,
            &[10, 11],
        );
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
    if bus.channels.is_quarantined() {
        assert_eq!(
            bus.publish_journal(&owner, 100, json!({})).seq,
            u64::MAX,
            "a publish that beat owner hydration makes the ambiguous cursor fail closed"
        );
    } else {
        assert_eq!(bus.journal_published_count(&owner), 3);
        assert_eq!(bus.publish_journal(&owner, 100, json!({})).seq, 3);
    }
}

#[test]
fn durable_pointer_tail_stays_bounded_across_long_history() {
    let bus = EventBus::default();
    let owner = owner("bounded-tail");
    let total = DURABLE_POINTER_CAP + 37;
    for id in 0..total {
        assert_eq!(
            bus.publish_journal(&owner, id as i64, json!({})).seq,
            id as u64
        );
    }
    assert_eq!(bus.journal_published_count(&owner), total as u64);
    assert_eq!(
        bus.durable.retained_len(DurableChannel::Journal, &owner),
        DURABLE_POINTER_CAP
    );
    let retained = bus.journal_index_range(&owner, None, total as u64);
    assert_eq!(retained.len(), DURABLE_POINTER_CAP);
    assert_eq!(retained[0].0, (total - DURABLE_POINTER_CAP) as u64);
}

#[test]
fn sequence_exhaustion_quarantines_instead_of_reusing_max() {
    let bus = EventBus::default();
    let owner = owner("seq-exhaustion");
    bus.seed_index(DurableChannel::Journal, &owner, "journal", u64::MAX, &[])
        .unwrap();
    let marker = bus.publish_journal(&owner, 1, json!({}));
    assert_eq!(marker.seq, u64::MAX);
    assert!(bus.channels.is_quarantined());
}

#[test]
fn oversized_hydration_fails_closed_instead_of_saturating_base_seq() {
    let bus = EventBus::default();
    let owner = owner("invalid-hydration");
    assert!(
        bus.seed_index(DurableChannel::Journal, &owner, "journal", 0, &[7])
            .is_err()
    );
    assert!(bus.channels.is_quarantined());
    assert!(bus.durable.is_quarantined());
}

#[test]
fn events_read_clamps_rows_and_bytes_with_continuation_metadata() {
    let kernel = Kernel::new();
    let mut attached = attachment(&kernel, "bounded-read");
    let owner = attached.as_ref().unwrap().session.key.owner();
    for n in 0..(EVENTS_MAX_PAGE + 20) {
        kernel.events.publish(&owner, "user.page", json!({"n":n}));
    }
    let page = kernel
        .handle_events_read(
            json!({"channel":"user.page","limit":usize::MAX}),
            &mut attached,
        )
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), EVENTS_MAX_PAGE);
    assert_eq!(page["page"]["truncated"], true);
    assert_eq!(page["page"]["request_clamped"], true);
    assert_eq!(page["page"]["next_since"], (EVENTS_MAX_PAGE - 1) as u64);

    let omitted = kernel
        .handle_events_read(json!({"channel":"user.page"}), &mut attached)
        .unwrap();
    assert_eq!(
        omitted["events"].as_array().unwrap().len(),
        EVENTS_DEFAULT_PAGE
    );
    assert_eq!(omitted["page"]["truncated"], true);

    let empty = kernel
        .handle_events_read(json!({"channel":"user.page","limit":0}), &mut attached)
        .unwrap();
    assert!(empty["events"].as_array().unwrap().is_empty());

    let (large, _, truncated) = bound_event_page(vec![Event {
        channel: "user.large".into(),
        seq: 0,
        ts: 0,
        payload: json!({"body":"x".repeat(EVENTS_MAX_CONTENT_BYTES + 1024)}),
    }])
    .unwrap();
    assert!(truncated);
    assert!(serde_json::to_vec(&large).unwrap().len() < MAX_FRAME_LEN);
}

#[test]
fn user_publish_rejects_huge_deep_and_invalid_channels_before_retention() {
    let kernel = Kernel::new();
    let mut attached = attachment(&kernel, "user-admission");
    let owner = attached.as_ref().unwrap().session.key.owner();

    let huge = kernel
        .handle_events_publish(
            json!({
                "channel":"user.huge",
                "payload":{"body":"x".repeat(USER_EVENT_PAYLOAD_MAX_BYTES + 1)},
            }),
            &mut attached,
        )
        .expect_err("oversized user payload must be rejected before cloning");
    assert_eq!(huge.code, INVALID_PARAMS);
    assert_eq!(huge.data.unwrap()["limit"], "event_payload_bytes");

    let mut deep = Json::Null;
    for _ in 0..=EVENT_PAYLOAD_MAX_DEPTH {
        deep = Json::Array(vec![deep]);
    }
    let deep = kernel
        .handle_events_publish(json!({"channel":"user.deep","payload":deep}), &mut attached)
        .expect_err("deep payload must be rejected before serialization");
    assert_eq!(deep.data.unwrap()["limit"], "event_payload_depth");

    let invalid = kernel
        .handle_events_publish(
            json!({
                "channel":format!("user.{}", "x".repeat(1024 * 1024)),
                "payload":null,
            }),
            &mut attached,
        )
        .expect_err("long channel identity must be rejected");
    let invalid_data = invalid.data.unwrap();
    assert_eq!(invalid_data["limit"], "event_channel_name");
    assert_eq!(invalid_data["actual_bytes"], "user.".len() + 1024 * 1024);
    assert_eq!(
        invalid_data.get("channel"),
        None,
        "invalid names must not be cloned into error responses"
    );
    assert!(
        serde_json::to_vec(&invalid_data).unwrap().len() < 1024,
        "hostile name rejection must stay far below the frame wall"
    );
    assert_eq!(kernel.events.channels.user_identity_count(&owner), 0);
}

#[test]
fn wire_publish_reports_language_mirror_degradation_without_duplicate_retry_signal() {
    let kernel = Kernel::new();
    let mut attached = attachment(&kernel, "language-mirror-cap");
    let session = attached.as_ref().unwrap().session.clone();
    for n in 0..64 {
        session
            .lang_bus
            .try_inject(&format!("user.language-{n}"), shoal_value::Value::Null)
            .unwrap();
    }

    let published = kernel
        .handle_events_publish(
            json!({"channel":"user.wire-only","payload":{"n":65}}),
            &mut attached,
        )
        .expect("the authoritative wire publish remains successful");
    assert_eq!(published["seq"], 0);
    assert_eq!(published["language_mirror"]["ok"], false);
    assert_eq!(
        published["language_mirror"]["error"]["code"],
        "channel_registry_limit"
    );
    assert_eq!(
        session.lang_bus.latest("user.wire-only").unwrap(),
        shoal_value::Value::Null
    );

    let read = kernel
        .handle_events_read(json!({"channel":"user.wire-only"}), &mut attached)
        .unwrap();
    assert_eq!(read["events"].as_array().unwrap().len(), 1);
    assert_eq!(read["events"][0]["payload"], json!({"n":65}));
}

#[test]
fn user_channel_identity_churn_is_bounded_per_exact_owner() {
    let bus = EventBus::default();
    let alpha = owner("channel-churn-alpha");
    let beta = owner("channel-churn-beta");
    for n in 0..USER_CHANNELS_PER_OWNER_MAX {
        bus.publish_user(&alpha, &format!("user.churn-{n}"), json!({"n":n}))
            .unwrap();
    }
    assert_eq!(
        bus.channels.user_identity_count(&alpha),
        USER_CHANNELS_PER_OWNER_MAX
    );
    let error = bus
        .publish_user(&alpha, "user.one-too-many", json!(null))
        .unwrap_err();
    assert_eq!(error.code, QUOTA_EXCEEDED);
    assert_eq!(
        error.data.unwrap()["limit"],
        "user_event_channels_per_session"
    );

    let continued = bus
        .publish_user(&alpha, "user.churn-0", json!({"again":true}))
        .unwrap();
    assert_eq!(continued.seq, 1, "existing identities remain usable");
    assert_eq!(
        bus.publish_user(&beta, "user.independent", json!(null))
            .unwrap()
            .seq,
        0,
        "an exact neighboring owner has its own identity budget"
    );
}

#[test]
fn channel_ring_enforces_count_and_byte_budgets() {
    let bus = EventBus::default();
    let owner = owner("ring-byte-budget");
    let payload = json!({"body":"x".repeat(8 * 1024)});
    for seq in 0..400u64 {
        assert_eq!(
            bus.publish_user(&owner, "user.bytes", payload.clone())
                .unwrap()
                .seq,
            seq
        );
    }
    let (retained, bytes) = bus.channels.ring_stats(&owner, "user.bytes");
    assert!(
        retained < EVENT_RING_CAP,
        "byte wall, not count wall, evicts"
    );
    assert!(bytes <= EVENT_RING_MAX_BYTES);
    assert_eq!(bus.published_count(&owner, "user.bytes"), 400);
    assert!(bus.ring_oldest_seq(&owner, "user.bytes").unwrap() > 0);
}

#[test]
fn subscription_queue_byte_overflow_is_exact_and_coalesced() {
    let queue = SubQueue::new("user.queue-bytes".into());
    queue.finish_replay(Vec::new());
    let mut total_bytes = 0u64;
    let total = 80u64;
    for seq in 0..total {
        let event = Event {
            channel: "user.queue-bytes".into(),
            seq,
            ts: 0,
            payload: json!({"body":"x".repeat(32 * 1024)}),
        };
        total_bytes += u64::try_from(event_retained_bytes(&event)).unwrap();
        queue.push_live(event);
    }
    assert!(queue.retained_bytes() <= SUB_QUEUE_MAX_BYTES);

    let mut delivered = 0u64;
    let mut delivered_bytes = 0u64;
    let mut summary = None;
    while let Some(event) = queue.pop() {
        if event.payload.get("dropped").is_some() {
            summary = Some(event);
            break;
        }
        delivered += 1;
        delivered_bytes += u64::try_from(event_retained_bytes(&event)).unwrap();
    }
    let summary = summary.expect("overflow must produce one coalesced summary");
    assert_eq!(summary.payload["dropped"], total - delivered);
    assert_eq!(
        summary.payload["dropped_bytes"],
        total_bytes - delivered_bytes
    );
    assert_eq!(summary.payload["latest_seq"], total - 1);
    assert!(queue.pop().is_none());

    let replay_only = SubQueue::new("user.replay-too-large".into());
    let oversized = Event {
        channel: "user.replay-too-large".into(),
        seq: 7,
        ts: 0,
        payload: json!({"body":"x".repeat(SUB_QUEUE_MAX_BYTES + 1)}),
    };
    let oversized_bytes = event_retained_bytes(&oversized);
    replay_only.finish_replay(vec![oversized]);
    let summary = replay_only
        .pop()
        .expect("an all-overflow replay still emits its gap summary");
    assert_eq!(summary.payload["dropped"], 1);
    assert_eq!(summary.payload["dropped_bytes"], oversized_bytes);
    assert_eq!(summary.payload["latest_seq"], 7);
}

#[test]
fn corrupt_program_shaped_ast_is_never_replayed_as_an_event() {
    let kernel = Kernel::new();
    let mut attached = attachment(&kernel, "corrupt-ast");
    let owner = attached.as_ref().unwrap().session.key.owner();
    let id = {
        let journal = kernel.journal.lock().unwrap();
        let id = journal
            .append(&EntryRecord {
                kind: shoal_journal::EntryKind::Exec,
                parent_id: None,
                session: owner.0.name.clone(),
                principal: owner.0.principal.clone(),
                ts_ns: 0,
                cwd: vec![],
                src: "corrupt".into(),
                ast_json: r#"{"stmts":[{"bad":true}]}"#.into(),
                effects_json: "[]".into(),
                opaque: false,
            })
            .unwrap();
        journal.finish(id, Some(0), true, 0).unwrap();
        id
    };
    let error = kernel
        .handle_events_read(json!({"channel":"journal"}), &mut attached)
        .expect_err("a merely program-shaped corrupt AST must fail closed");
    assert_eq!(error.code, INTERNAL_ERROR);
    assert_eq!(error.data.unwrap()["entry_id"], id);
}

#[test]
fn historical_large_transcripts_stop_before_materializing_a_whole_row_page() {
    let kernel = Kernel::new();
    let mut attached = attachment(&kernel, "large-transcript-page");
    let owner = attached.as_ref().unwrap().session.key.owner();
    {
        let journal = kernel.journal.lock().unwrap();
        for n in 0..3 {
            let id = journal
                .append(&EntryRecord {
                    kind: shoal_journal::EntryKind::Exec,
                    parent_id: None,
                    session: owner.0.name.clone(),
                    principal: owner.0.principal.clone(),
                    ts_ns: n,
                    cwd: vec![],
                    src: "return".into(),
                    ast_json: r#"{"stmts":[]}"#.into(),
                    effects_json: "[]".into(),
                    opaque: false,
                })
                .unwrap();
            journal.finish(id, Some(0), true, 0).unwrap();
            let payload = json!({"body":"x".repeat(EVENTS_MAX_CONTENT_BYTES / 3)});
            journal
                .record_transcript_event(id, n, &serde_json::to_string(&payload).unwrap())
                .unwrap();
        }
    }
    let page = kernel
        .handle_events_read(json!({"channel":"session.transcript"}), &mut attached)
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["page"]["truncated"], true);
    assert_eq!(page["page"]["next_since"], 1);
    assert!(serde_json::to_vec(&page).unwrap().len() < MAX_FRAME_LEN);
}

/// Zero-regression companion: seeding from a brand-new, empty store (the
/// common case — most kernel opens are not a restart of a previously
/// used store) must be a no-op, leaving both channels starting at seq 0
/// exactly as an ephemeral in-memory kernel does.
#[test]
fn owner_hydration_is_a_no_op_on_a_fresh_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let bus = EventBus::default();
    let owner = owner("empty");
    bus.seed_owner_from_journal(&journal, &owner).unwrap();
    assert_eq!(bus.journal_published_count(&owner), 0);
    assert_eq!(bus.transcript_published_count(&owner), 0);
    let event = bus.publish_journal(&owner, 1, json!({}));
    assert_eq!(
        event.seq, 0,
        "a fresh empty store must still start seqs at 0"
    );
}
