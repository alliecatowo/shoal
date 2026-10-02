use super::*;

#[test]
fn self_ack_env_is_an_explicit_boolean() {
    use std::ffi::OsStr;

    for enabled in ["1", "true", "TRUE", "yes", "on"] {
        assert!(parse_env_bool(Some(OsStr::new(enabled))), "{enabled}");
    }
    for disabled in ["", "0", "false", "FALSE", "no", "off", "garbage"] {
        assert!(!parse_env_bool(Some(OsStr::new(disabled))), "{disabled}");
    }
    assert!(!parse_env_bool(None));
}

#[test]
fn attached_peer_disconnect_errors_are_clean_but_pre_auth_errors_remain_visible() {
    for kind in [
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::UnexpectedEof,
    ] {
        assert!(
            normalize_attached_disconnect(Err(io::Error::from(kind)), true).is_ok(),
            "{kind:?}"
        );
        assert_eq!(
            normalize_attached_disconnect(Err(io::Error::from(kind)), false)
                .unwrap_err()
                .kind(),
            kind
        );
    }
    assert_eq!(
        normalize_attached_disconnect(
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "revoked")),
            true,
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn connection_quota_reservation_is_atomic_and_released_on_drop() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_connections: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let reserve = |kernel: Arc<Kernel>, barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            kernel.reserve_connection_slot()
        })
    };
    let first = reserve(kernel.clone(), barrier.clone());
    let second = reserve(kernel.clone(), barrier.clone());
    barrier.wait();
    let reservations = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(
        reservations.iter().filter(|result| result.is_ok()).count(),
        1
    );
    assert_eq!(kernel.admission.connections.active(), 1);
    drop(reservations);
    assert_eq!(kernel.admission.connections.active(), 0);
}

#[test]
fn overcomplex_request_closes_only_that_connection_and_kernel_stays_live() {
    use std::io::Write as _;

    let kernel = Kernel::new();
    let (mut hostile, server) = UnixStream::pair().unwrap();
    let worker_kernel = kernel.clone();
    let worker = std::thread::spawn(move || worker_kernel.handle_stream(server));
    let wide = format!(
        "[{}]\n",
        std::iter::repeat_n("null", MAX_JSON_CONTAINER_ITEMS + 1)
            .collect::<Vec<_>>()
            .join(",")
    );
    hostile.write_all(wide.as_bytes()).unwrap();
    drop(hostile);
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("complexity limit"));

    let (mut client, mut reader, healthy) = spawn(&kernel);
    let response = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"client":{"kind":"test","tty":false}}),
    );
    assert!(response.error.is_none());
    drop(client);
    drop(reader);
    healthy.join().unwrap();
}

#[test]
fn read_deadline_evicts_silent_and_partial_unauthenticated_connections() {
    use std::io::{Read, Write};

    for initial in [b"".as_slice(), b"{".as_slice()] {
        let kernel = Kernel::builder()
            .limits(Limits {
                max_connections: 1,
                frame_read_timeout_ms: 40,
                ..Limits::default()
            })
            .build()
            .unwrap();
        let slot = kernel.reserve_connection_slot().unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        if !initial.is_empty() {
            client.write_all(initial).unwrap();
        }
        let worker_kernel = kernel.clone();
        let worker = std::thread::spawn(move || {
            let _slot = slot;
            serve_embedded_test_stream(worker_kernel, server)
        });
        let mut byte = [0u8; 1];
        assert_eq!(client.read(&mut byte).unwrap(), 0, "connection was closed");
        assert!(worker.join().unwrap().is_err(), "deadline is observable");
        assert_eq!(kernel.admission.connections.active(), 0);
    }
}

#[test]
fn attached_idle_connection_is_not_subject_to_first_byte_deadline() {
    let kernel = Kernel::builder()
        .limits(Limits {
            frame_read_timeout_ms: 40,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let (mut client, mut reader, server) = spawn(&kernel);
    attach(&mut client, &mut reader);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let response = call(&mut client, &mut reader, 2, "parse", json!({"src":"1 + 2"}));
    assert!(response.error.is_none(), "attached idle client stayed live");
    drop(client);
    drop(reader);
    server.join().unwrap();
}

#[test]
fn hostile_parse_nesting_is_typed_and_connection_remains_healthy() {
    let kernel = Kernel::new();
    let (mut client, mut reader, server) = spawn(&kernel);
    attach(&mut client, &mut reader);

    let levels = shoal_syntax::MAX_PARSE_NESTING.saturating_mul(2);
    let source = format!("{}0{}", "[".repeat(levels), "]".repeat(levels));
    let rejected = call(&mut client, &mut reader, 2, "exec", json!({"src": source}));
    let error = rejected
        .error
        .expect("over-nested source must fail as an RPC error");
    assert_eq!(error.code, PARSE_ERROR);
    assert!(error.message.contains("nesting limit"), "{error:?}");

    let healthy = call(&mut client, &mut reader, 3, "exec", json!({"src":"1 + 2"}));
    assert!(
        healthy.error.is_none(),
        "same connection must survive typed parse rejection: {healthy:?}"
    );

    drop(client);
    drop(reader);
    server.join().unwrap();
}

#[test]
fn evaluator_panic_quarantines_only_that_session_and_keeps_connection_alive() {
    let kernel = Kernel::new();
    let (mut client, mut reader, server) = spawn(&kernel);
    attach(&mut client, &mut reader);

    let panic_response = call(
        &mut client,
        &mut reader,
        2,
        "test.panic_evaluator",
        json!({}),
    );
    let panic_error = panic_response.error.expect("panic becomes RPC error");
    assert_eq!(panic_error.code, INTERNAL_ERROR);
    assert_eq!(panic_error.data.unwrap()["session_quarantined"], true);

    let rejected = call(&mut client, &mut reader, 3, "session.env", json!({}));
    let rejected_error = rejected.error.expect("poisoned session stays closed");
    assert_eq!(rejected_error.code, INTERNAL_ERROR);
    assert_eq!(rejected_error.data.unwrap()["session_quarantined"], true);

    // Pure parsing needs no session state, so the request loop itself is
    // demonstrably still alive after the panic.
    assert!(
        call(&mut client, &mut reader, 4, "parse", json!({"src":"1 + 2"}))
            .error
            .is_none()
    );

    // Reattach this same connection to an independent session and resume
    // normal evaluation; the poisoned evaluator guard is never opened.
    let attached = call(
        &mut client,
        &mut reader,
        5,
        "session.attach",
        json!({"local_auth":"local-human","session":"after-panic","client":{"kind":"test","tty":false}}),
    );
    assert!(attached.error.is_none(), "reattach failed: {attached:?}");
    let exec = call(&mut client, &mut reader, 6, "exec", json!({"src":"1 + 2"}));
    assert!(
        exec.error.is_none(),
        "healthy session exec failed: {exec:?}"
    );

    drop(client);
    drop(reader);
    server.join().unwrap();
}

#[test]
fn per_session_resource_reservation_is_atomic_under_race() {
    let quota = Arc::new(SessionQuota::default());
    let owner = SessionKey::new("principal:test", "s").owner();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let reserve = |quota: Arc<SessionQuota>, owner: OwnerKey, barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            quota.reserve(&owner, 1, "test", "resource")
        })
    };
    let first = reserve(quota.clone(), owner.clone(), barrier.clone());
    let second = reserve(quota.clone(), owner.clone(), barrier.clone());
    barrier.wait();
    let reservations = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(
        reservations.iter().filter(|result| result.is_ok()).count(),
        1
    );
    assert_eq!(
        quota
            .counts
            .lock()
            .expect("test lock should not be poisoned")
            .get(&owner),
        Some(&1)
    );
    drop(reservations);
    assert!(
        !quota
            .counts
            .lock()
            .expect("test lock should not be poisoned")
            .contains_key(&owner)
    );
}

#[test]
fn same_visible_session_name_is_private_to_each_principal() {
    let kernel = Kernel::new();
    let alpha = kernel.session("shared", "agent:alpha").unwrap();
    let alpha_again = kernel.session("shared", "agent:alpha").unwrap();
    let beta = kernel.session("shared", "agent:beta").unwrap();

    assert!(Arc::ptr_eq(&alpha, &alpha_again));
    assert!(!Arc::ptr_eq(&alpha, &beta));
    assert_eq!(alpha.id, beta.id, "the wire-visible name stays stable");
    assert_ne!(
        alpha.key, beta.key,
        "the registry identity includes principal"
    );

    let quota = Arc::new(SessionQuota::default());
    let alpha_slot = quota
        .reserve(&alpha.key.owner(), 1, "test", "resource")
        .unwrap();
    let beta_slot = quota
        .reserve(&beta.key.owner(), 1, "test", "resource")
        .expect("same session name under another principal has its own quota");
    assert!(
        quota
            .reserve(&alpha.key.owner(), 1, "test", "resource")
            .is_err(),
        "the limit still applies within one exact owner"
    );
    drop((alpha_slot, beta_slot));
}

#[test]
fn concurrent_session_creation_returns_one_registry_object() {
    let kernel = Kernel::new();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let open = |kernel: Arc<Kernel>, barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            kernel.session("same", "agent:concurrent").unwrap()
        })
    };
    let first = open(kernel.clone(), barrier.clone());
    let second = open(kernel.clone(), barrier.clone());
    barrier.wait();
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(
        kernel
            .runtime
            .sessions
            .snapshot()
            .keys()
            .filter(|key| key.principal == "agent:concurrent")
            .count(),
        1
    );
}

#[test]
fn global_session_limit_is_atomic_across_principals() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_sessions: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let open = |kernel: Arc<Kernel>, principal: &'static str, barrier: Arc<std::sync::Barrier>| {
        std::thread::spawn(move || {
            barrier.wait();
            kernel.session("raced", principal)
        })
    };
    let alpha = open(kernel.clone(), "agent:alpha", barrier.clone());
    let beta = open(kernel.clone(), "agent:beta", barrier.clone());
    barrier.wait();
    let sessions = [alpha.join().unwrap(), beta.join().unwrap()];

    assert_eq!(
        sessions.iter().filter(|result| result.is_ok()).count(),
        1,
        "serialized admission must reserve exactly one global slot"
    );
    let error = sessions
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one racing admission must be rejected");
    assert_eq!(error.code, QUOTA_EXCEEDED);
    assert_eq!(error.data.as_ref().unwrap()["limit"], "sessions_global");
    assert_eq!(kernel.runtime.sessions.snapshot().len(), 1);
}

#[test]
fn global_session_limit_keeps_existing_keys_and_rejects_when_all_are_active() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_sessions: 2,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let alpha = kernel.session("one", "agent:alpha").unwrap();
    let beta = kernel.session("two", "agent:beta").unwrap();

    let alpha_again = kernel.session("one", "agent:alpha").unwrap();
    assert!(
        Arc::ptr_eq(&alpha, &alpha_again),
        "an existing principal/name key remains attachable at the cap"
    );
    let error = match kernel.session("three", "agent:gamma") {
        Ok(_) => panic!("a new key cannot displace an active session"),
        Err(error) => error,
    };
    assert_eq!(error.code, QUOTA_EXCEEDED);
    assert_eq!(error.data.as_ref().unwrap()["limit"], "sessions_global");
    assert_eq!(kernel.runtime.sessions.snapshot().len(), 2);
    drop((alpha, alpha_again, beta));
}

#[test]
fn global_session_limit_evicts_the_idle_lru_across_principals() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_sessions: 2,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let alpha_key = SessionKey::new("agent:alpha", "oldest");
    drop(
        kernel
            .session(&alpha_key.name, &alpha_key.principal)
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(1));
    let beta = kernel.session("newer", "agent:beta").unwrap();

    let gamma = kernel.session("newest", "agent:gamma").unwrap();
    let sessions = kernel.runtime.sessions.snapshot();
    assert_eq!(sessions.len(), 2);
    assert!(!sessions.contains_key(&alpha_key));
    assert!(sessions.contains_key(&beta.key));
    assert!(sessions.contains_key(&gamma.key));
}

#[test]
fn session_registry_evicts_idle_lru_but_never_active_leases() {
    let kernel = Kernel::new();
    let principal = "agent:bounded-sessions";
    let first = kernel.session("s0", principal).unwrap();
    let first_owner = first.key.owner();
    let stale_plan_ref = "plan:stale-session-generation".to_string();
    kernel
        .runtime
        .plans
        .transaction(|plans| {
            plans.insert(
                stale_plan_ref.clone(),
                StoredPlan {
                    src: "1 + 1".into(),
                    session: first.id.clone(),
                    principal: principal.into(),
                    plan_hash: "stale-plan-hash".into(),
                    source_hash: "stale-source-hash".into(),
                    plan: Plan::new(vec![], Reversibility::Reversible, Estimates::default()),
                    authorization: PlanAuthorization::Pending,
                    created_at: Instant::now(),
                },
            );
        })
        .unwrap();
    let stale_task_ref = Ref::new("task", 9090);
    kernel.runtime.tasks.insert(Arc::new(TaskEntry {
        task: stale_task_ref.clone(),
        owner: first_owner.clone(),
        session_id: first.id.clone(),
        session_lease: Mutex::new(None),
        started_ns: now_ns(),
        inner: Mutex::new(TaskInner {
            state: "completed",
            finished_ns: Some(now_ns()),
            result_ref: None,
            exit_code: None,
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
        deadline_ms: None,
        deadline_exceeded: AtomicBool::new(false),
    }));
    drop(first);
    std::thread::sleep(std::time::Duration::from_millis(1));
    for i in 1..MAX_SESSIONS_PER_PRINCIPAL {
        drop(kernel.session(&format!("s{i}"), principal).unwrap());
    }
    kernel
        .runtime
        .events
        .publish_journal(&first_owner, 1, json!({"stale":true}));
    assert_eq!(
        kernel.runtime.events.journal_published_count(&first_owner),
        1
    );

    drop(kernel.session("new", principal).unwrap());
    let sessions = kernel.runtime.sessions.snapshot();
    assert_eq!(
        sessions
            .keys()
            .filter(|key| key.principal == principal)
            .count(),
        MAX_SESSIONS_PER_PRINCIPAL
    );
    assert!(!sessions.contains_key(&SessionKey::new(principal, "s0")));
    assert_eq!(
        kernel.runtime.events.journal_published_count(&first_owner),
        0,
        "eviction removes the old owner's in-memory event indexes"
    );
    assert!(
        !kernel.runtime.tasks.contains(&stale_task_ref),
        "eviction removes terminal task metadata tied to the old transcript"
    );
    assert!(
        !kernel.runtime.plans.contains(&stale_plan_ref),
        "eviction removes plans bound to the old session generation"
    );

    let active_kernel = Kernel::new();
    let leases = (0..MAX_SESSIONS_PER_PRINCIPAL)
        .map(|i| {
            active_kernel
                .session(&format!("active-{i}"), principal)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let error = match active_kernel.session("over-limit", principal) {
        Ok(_) => panic!("all active session leases must prevent eviction"),
        Err(error) => error,
    };
    assert_eq!(error.code, QUOTA_EXCEEDED);
    assert_eq!(leases.len(), MAX_SESSIONS_PER_PRINCIPAL);
}
