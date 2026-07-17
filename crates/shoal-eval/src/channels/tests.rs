use super::*;

#[test]
fn channel_names_and_identities_are_typed_and_bounded() {
    let bus = EventBus::default();
    let oversized = "é".repeat(CHANNEL_NAME_BYTES / 2 + 1);
    assert_eq!(
        bus.emit(&oversized, Value::Null).unwrap_err().code,
        "channel_name_limit"
    );
    assert_eq!(
        channel_handle(&oversized).unwrap_err().code,
        "channel_name_limit"
    );

    for index in 0..CHANNEL_CAP {
        assert_eq!(
            bus.emit(&format!("identity-{index}"), Value::Int(index as i64))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        bus.emit("one-too-many", Value::Null).unwrap_err().code,
        "channel_registry_limit"
    );
    assert_eq!(
        bus.latest("identity-0").unwrap(),
        Value::Int(0),
        "admission failure must not evict or retarget an existing identity"
    );
    assert_eq!(bus.emit("identity-0", Value::Int(9)).unwrap(), 1);
    assert_eq!(bus.channels.lock().unwrap().len(), CHANNEL_CAP);
}

#[test]
fn huge_deep_wide_and_opaque_payloads_fail_before_sequence_or_bridge_commit() {
    let bus = EventBus::default();
    let forwarded = Arc::new(AtomicBool::new(false));
    let observed = forwarded.clone();
    bus.set_forwarder(Box::new(move |_, _| {
        observed.store(true, Ordering::Release);
    }));

    let huge = Value::Str("x".repeat(PAYLOAD_BYTE_CAP + 1));
    assert_eq!(
        bus.emit("user.hostile", huge).unwrap_err().code,
        "channel_payload_limit"
    );
    assert!(!forwarded.load(Ordering::Acquire));

    let mut deep = Value::Null;
    for _ in 0..=PAYLOAD_DEPTH_CAP {
        deep = Value::List(vec![deep]);
    }
    assert_eq!(
        bus.emit("deep", deep).unwrap_err().code,
        "channel_payload_limit"
    );
    assert_eq!(
        bus.emit("wide", Value::List(vec![Value::Null; PAYLOAD_NODE_CAP + 1]))
            .unwrap_err()
            .code,
        "channel_payload_limit"
    );
    assert_eq!(
        bus.emit("opaque", Value::Task(TaskVal::new("retained")))
            .unwrap_err()
            .code,
        "channel_payload_type"
    );

    assert_eq!(bus.emit("user.hostile", Value::Int(1)).unwrap(), 0);
    assert!(forwarded.load(Ordering::Acquire));
}

#[test]
fn ring_byte_eviction_preserves_exact_cursor_gap_and_sequence() {
    let bus = EventBus::default();
    let payload = Value::Str("r".repeat(60_000));
    let published = 10usize;
    for _ in 0..published {
        bus.emit("byte-ring", payload.clone()).unwrap();
    }
    {
        let map = bus.channels.lock().unwrap();
        let state = map.get("byte-ring").unwrap();
        assert!(state.ring_bytes <= RING_BYTE_CAP);
        assert!(state.ring.len() < RING_CAP, "byte cap must trigger first");
        assert_eq!(state.next_seq, published as u64);
    }

    let rx = bus.events("byte-ring", Some(0)).unwrap();
    let mut retained = 0usize;
    let mut dropped = 0usize;
    loop {
        match rx.recv(Some(Duration::ZERO), None) {
            Received::Event(_) => retained += 1,
            Received::Gap(gap) => dropped += gap.dropped as usize,
            Received::Timeout => break,
            Received::Closed | Received::Cancelled | Received::Poisoned => {
                panic!("subscription ended early")
            }
        }
    }
    assert_eq!(retained + dropped, published - 1);
    assert!(dropped > 0);
}

#[test]
fn subscriber_byte_overflow_is_bounded_and_exactly_accounted() {
    let bus = EventBus::default();
    let rx = bus.events("byte-queue", None).unwrap();
    let payload = Value::Str("q".repeat(60_000));
    let published = 10usize;
    for _ in 0..published {
        bus.emit("byte-queue", payload.clone()).unwrap();
    }
    {
        let state = rx.queue.state.lock().unwrap();
        assert!(state.retained_bytes <= SUBSCRIBER_BYTE_CAP);
        assert!(
            state.items.len() < SUBSCRIBER_CAP,
            "byte cap must trigger first"
        );
    }

    let mut retained = 0usize;
    let mut dropped = 0usize;
    loop {
        match rx.recv(Some(Duration::ZERO), None) {
            Received::Event(_) => retained += 1,
            Received::Gap(gap) => dropped += gap.dropped as usize,
            Received::Timeout => break,
            Received::Closed | Received::Cancelled | Received::Poisoned => {
                panic!("subscription ended early")
            }
        }
    }
    assert_eq!(retained + dropped, published);
    assert!(dropped > 0);
}

#[test]
fn global_subscriber_admission_prunes_closed_churn() {
    let bus = EventBus::default();
    let mut receivers = (0..LIVE_SUBSCRIBER_CAP)
        .map(|index| bus.events(&format!("live-{index}"), None).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        bus.events("rejected", None).err().unwrap().code,
        "channel_subscriber_limit"
    );

    drop(receivers.pop());
    receivers.push(bus.events("replacement", None).unwrap());
    drop(receivers);

    for index in 0..(CHANNEL_CAP * 4) {
        let receiver = bus.events(&format!("churn-{index}"), None).unwrap();
        drop(receiver);
    }
    let mut map = bus.channels.lock().unwrap();
    prune_closed_subscribers(&mut map);
    assert!(
        map.is_empty(),
        "closed empty subscription shells must be pruned"
    );
}

#[test]
fn slow_subscriber_is_bounded_and_every_gap_is_accounted_for() {
    let bus = EventBus::default();
    let rx = bus.events("burst", None).unwrap();
    let published = SUBSCRIBER_CAP + 80;
    for i in 0..published {
        bus.emit("burst", Value::Int(i as i64)).unwrap();
    }

    let mut retained = 0usize;
    let mut dropped = 0usize;
    loop {
        match rx.recv(Some(Duration::ZERO), None) {
            Received::Event(_) => retained += 1,
            Received::Gap(gap) => dropped += gap.dropped as usize,
            Received::Timeout => break,
            Received::Closed | Received::Cancelled | Received::Poisoned => {
                panic!("subscription ended early")
            }
        }
    }
    assert!(retained <= SUBSCRIBER_CAP);
    assert!(dropped > 0, "overflow must be explicit, never silent");
    assert_eq!(retained + dropped, published);
}

#[test]
fn cancellation_wakes_an_idle_subscription_promptly() {
    let bus = EventBus::default();
    let rx = bus.events("idle", None).unwrap();
    let cancel = CancelToken::new();
    let trip = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        trip.cancel();
    });

    let start = Instant::now();
    assert!(matches!(rx.recv(None, Some(&cancel)), Received::Cancelled));
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "cancelled receive stayed blocked"
    );
}

#[test]
fn cancellation_preempts_a_subscription_backlog() {
    let bus = EventBus::default();
    let rx = bus.events("backlog", None).unwrap();
    for i in 0..SUBSCRIBER_CAP {
        bus.emit("backlog", Value::Int(i as i64)).unwrap();
    }
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(matches!(rx.recv(None, Some(&cancel)), Received::Cancelled));
}

#[test]
fn stale_cursor_reports_exact_history_and_queue_gaps() {
    let bus = EventBus::default();
    let published = RING_CAP + 10;
    for i in 0..published {
        bus.emit("history", Value::Int(i as i64)).unwrap();
    }
    let rx = bus.events("history", Some(0)).unwrap();
    let mut retained = 0usize;
    let mut dropped = 0usize;
    let mut typed = false;
    loop {
        match rx.recv(Some(Duration::ZERO), None) {
            Received::Event(_) => retained += 1,
            Received::Gap(gap) => {
                dropped += gap.dropped as usize;
                let marker = overflow_record("history", gap);
                let Value::Record(record) = marker else {
                    unreachable!()
                };
                typed |= record.get("marker") == Some(&Value::Str("stream_gap".into()))
                    && record.get("reason").is_some()
                    && record.get("from_seq").is_some()
                    && record.get("to_seq").is_some();
            }
            Received::Timeout => break,
            Received::Closed | Received::Cancelled | Received::Poisoned => {
                panic!("subscription ended early")
            }
        }
    }
    assert!(typed, "every gap uses the stable discriminated shape");
    assert_eq!(
        retained + dropped,
        published - 1,
        "every event newer than the cursor is retained or accounted for"
    );
}

fn poison<T: Send>(mutex: &Mutex<T>) {
    std::thread::scope(|scope| {
        let poisoner = scope.spawn(|| {
            let _guard = mutex.lock().expect("test mutex should start healthy");
            panic!("inject evaluator channel poison");
        });
        assert!(poisoner.join().is_err());
    });
    assert!(mutex.is_poisoned());
}

#[test]
fn poisoned_subscriber_queue_is_terminal_bounded_and_repeatable() {
    let bus = EventBus::default();
    let rx = bus.events("queue-poison", None).unwrap();
    for i in 0..SUBSCRIBER_CAP {
        bus.emit("queue-poison", Value::Int(i as i64)).unwrap();
    }
    poison(&rx.queue.state);

    assert!(matches!(
        rx.recv(Some(Duration::ZERO), None),
        Received::Poisoned
    ));
    assert!(matches!(
        rx.recv(Some(Duration::ZERO), None),
        Received::Poisoned
    ));
    let state = rx.queue.state.lock().expect("poison must be repaired");
    assert!(state.closed && state.poisoned);
    assert!(state.items.is_empty(), "unknown backlog must be discarded");
    assert_eq!(state.retained_bytes, 0);
    assert!(state.items.capacity() <= SUBSCRIBER_CAP.next_power_of_two());
}

#[test]
fn condvar_poison_wakes_and_terminalizes_a_blocked_waiter() {
    let bus = EventBus::default();
    let rx = bus.events("blocked-poison", None).unwrap();
    let queue = rx.queue.clone();
    let waiting = Arc::new(std::sync::Barrier::new(2));
    let waiter_barrier = waiting.clone();
    let waiter_queue = queue.clone();
    let waiter = std::thread::spawn(move || {
        let state = waiter_queue
            .state
            .lock()
            .expect("test queue should start healthy");
        waiter_barrier.wait();
        waiter_queue.wait(state).poisoned
    });
    waiting.wait();

    poison(&queue.state);
    queue.ready.notify_all();
    assert!(waiter.join().unwrap());
    assert!(matches!(rx.recv(None, None), Received::Poisoned));
}

#[test]
fn poisoned_channel_registry_quarantines_repeated_calls_and_wakes_waiters() {
    let bus = Arc::new(EventBus::default());
    let rx = bus.events("registry-poison", None).unwrap();
    poison(&bus.channels);

    for error in [
        bus.emit("registry-poison", Value::Int(1)).unwrap_err(),
        bus.latest("registry-poison").unwrap_err(),
        bus.events("registry-poison", None).err().unwrap(),
    ] {
        assert_eq!(error.code, "channel_poisoned");
    }
    assert!(matches!(rx.recv(None, None), Received::Poisoned));
    assert!(matches!(rx.recv(None, None), Received::Poisoned));
    assert!(bus.channels_quarantined.load(Ordering::Acquire));
}

#[test]
fn poisoned_forwarder_requires_explicit_replacement_before_publish() {
    let bus = EventBus::default();
    bus.set_forwarder(Box::new(|_, _| {}));
    poison(&bus.forwarder);

    for _ in 0..2 {
        let error = bus.emit("user.poison", Value::Int(1)).unwrap_err();
        assert_eq!(error.code, "channel_poisoned");
    }
    assert_eq!(bus.latest("user.poison").unwrap(), Value::Null);

    let forwarded = Arc::new(AtomicBool::new(false));
    let observed = forwarded.clone();
    bus.set_forwarder(Box::new(move |_, _| {
        observed.store(true, Ordering::Release);
    }));
    assert_eq!(bus.emit("user.poison", Value::Int(2)).unwrap(), 0);
    assert!(forwarded.load(Ordering::Acquire));
    assert_eq!(bus.latest("user.poison").unwrap(), Value::Int(2));
}

#[test]
fn panicking_forwarder_is_contained_and_then_quarantined() {
    let bus = EventBus::default();
    bus.set_forwarder(Box::new(|_, _| panic!("inject forwarder panic")));
    let error = bus.emit("user.forwarder-panic", Value::Int(1)).unwrap_err();
    assert_eq!(error.code, "channel_poisoned");
    assert_eq!(
        bus.emit("user.forwarder-panic", Value::Int(2))
            .unwrap_err()
            .code,
        "channel_poisoned"
    );
    assert_eq!(
        bus.latest("user.forwarder-panic").unwrap(),
        Value::Int(1),
        "the committed local event remains authoritative"
    );
}

#[test]
fn unrepresentable_sequence_quarantines_instead_of_wrapping() {
    let bus = EventBus::default();
    let rx = bus.events("seq-exhausted", None).unwrap();
    bus.channels
        .lock()
        .expect("test registry should be healthy")
        .get_mut("seq-exhausted")
        .expect("subscription creates channel")
        .next_seq = i64::MAX as u64 + 1;

    for _ in 0..2 {
        assert_eq!(
            bus.emit("seq-exhausted", Value::Null).unwrap_err().code,
            "channel_poisoned"
        );
    }
    assert!(matches!(rx.recv(None, None), Received::Poisoned));
}

#[test]
fn production_channel_locks_have_no_raw_panicking_access() {
    let source = include_str!("../channels.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("production source prefix");
    for forbidden in [
        ".lock().unwrap(",
        ".lock().expect(",
        ".wait(state).unwrap(",
        ".wait_timeout(state, wait).unwrap(",
    ] {
        assert!(
            !production.contains(forbidden),
            "production channel synchronization contains `{forbidden}`"
        );
    }
    let evaluator_surface = include_str!("eval.rs");
    let registered = evaluator_surface
        .find("self.exec.jobs.register(task.clone())")
        .expect("handler task registration");
    let launched = evaluator_surface[registered..]
        .find(".spawn(move ||")
        .expect("fallible handler launch")
        + registered;
    assert!(
        registered < launched,
        "handler task must be registered before its worker can run"
    );
}
