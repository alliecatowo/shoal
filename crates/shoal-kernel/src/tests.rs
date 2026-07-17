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
fn connection_quota_reservation_is_atomic_and_released_on_drop() {
    let kernel = Kernel::new();
    kernel.configure_limits(Limits {
        max_connections: 1,
        ..Limits::default()
    });
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
    assert_eq!(kernel.connections.active(), 1);
    drop(reservations);
    assert_eq!(kernel.connections.active(), 0);
}

#[test]
fn read_deadline_evicts_silent_and_partial_unauthenticated_connections() {
    use std::io::{Read, Write};

    for initial in [b"".as_slice(), b"{".as_slice()] {
        let kernel = Kernel::new();
        kernel.configure_limits(Limits {
            max_connections: 1,
            frame_read_timeout_ms: 40,
            ..Limits::default()
        });
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
            worker_kernel.handle_stream(server)
        });
        let mut byte = [0u8; 1];
        assert_eq!(client.read(&mut byte).unwrap(), 0, "connection was closed");
        assert!(worker.join().unwrap().is_err(), "deadline is observable");
        assert_eq!(kernel.connections.active(), 0);
    }
}

#[test]
fn attached_idle_connection_is_not_subject_to_first_byte_deadline() {
    let kernel = Kernel::new();
    kernel.configure_limits(Limits {
        frame_read_timeout_ms: 40,
        ..Limits::default()
    });
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
    assert_eq!(quota.counts.lock().unwrap().get(&owner), Some(&1));
    drop(reservations);
    assert!(!quota.counts.lock().unwrap().contains_key(&owner));
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
            .sessions
            .snapshot()
            .keys()
            .filter(|key| key.principal == "agent:concurrent")
            .count(),
        1
    );
}

#[test]
fn session_registry_evicts_idle_lru_but_never_active_leases() {
    let kernel = Kernel::new();
    let principal = "agent:bounded-sessions";
    let first = kernel.session("s0", principal).unwrap();
    let first_owner = first.key.owner();
    let stale_plan_ref = "plan:stale-session-generation".to_string();
    kernel.plans.transaction(|plans| {
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
    });
    let stale_task_ref = Ref::new("task", 9090);
    kernel.tasks.insert(Arc::new(TaskEntry {
        task: stale_task_ref.clone(),
        owner: first_owner.clone(),
        session_id: first.id.clone(),
        session_lease: Mutex::new(None),
        started_ns: now_ns(),
        inner: Mutex::new(TaskInner {
            state: "completed",
            finished_ns: Some(now_ns()),
            result_ref: None,
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
    }));
    drop(first);
    std::thread::sleep(std::time::Duration::from_millis(1));
    for i in 1..MAX_SESSIONS_PER_PRINCIPAL {
        drop(kernel.session(&format!("s{i}"), principal).unwrap());
    }
    kernel
        .events
        .publish_journal(&first_owner, 1, json!({"stale":true}));
    assert_eq!(kernel.events.journal_published_count(&first_owner), 1);

    drop(kernel.session("new", principal).unwrap());
    let sessions = kernel.sessions.snapshot();
    assert_eq!(
        sessions
            .keys()
            .filter(|key| key.principal == principal)
            .count(),
        MAX_SESSIONS_PER_PRINCIPAL
    );
    assert!(!sessions.contains_key(&SessionKey::new(principal, "s0")));
    assert_eq!(
        kernel.events.journal_published_count(&first_owner),
        0,
        "eviction removes the old owner's in-memory event indexes"
    );
    assert!(
        !kernel.tasks.contains(&stale_task_ref),
        "eviction removes terminal task metadata tied to the old transcript"
    );
    assert!(
        !kernel.plans.contains(&stale_plan_ref),
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

#[test]
fn task_refs_are_hidden_from_another_principal_with_the_same_session_name() {
    let kernel = Kernel::new();
    let alpha = kernel.session("shared-tasks", "agent:alpha").unwrap();
    let beta = kernel.session("shared-tasks", "agent:beta").unwrap();
    let task_ref = Ref::new("task", 4242);
    let alpha_owner = alpha.key.owner();
    let alpha_session_id = alpha.id.clone();
    kernel.tasks.insert(Arc::new(TaskEntry {
        task: task_ref.clone(),
        owner: alpha_owner,
        session_id: alpha_session_id,
        session_lease: Mutex::new(None),
        started_ns: now_ns(),
        inner: Mutex::new(TaskInner {
            state: "completed",
            finished_ns: Some(now_ns()),
            result_ref: None,
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
    }));
    let mut attached = Some(Attachment {
        session: beta,
        principal: "agent:beta".into(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
    });

    let listed = kernel.handle_task_list(&mut attached).unwrap();
    assert!(listed.as_array().unwrap().is_empty());
    let error = kernel
        .handle_task_get(json!({"task": task_ref}), &mut attached)
        .unwrap_err();
    assert_eq!(error.code, UNKNOWN_TASK);
}

#[test]
fn pty_refs_are_hidden_from_another_principal_with_the_same_session_name() {
    let kernel = Kernel::new();
    let alpha = kernel.session("shared-pty", "agent:alpha").unwrap();
    let beta = kernel.session("shared-pty", "agent:beta").unwrap();
    let mut alpha_attached = Some(Attachment {
        session: alpha,
        principal: "agent:alpha".into(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
    });
    let mut beta_attached = Some(Attachment {
        session: beta,
        principal: "agent:beta".into(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
    });

    let opened = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut alpha_attached)
        .expect("open alpha PTY");
    let pty_id = opened["pty_id"].clone();
    assert!(
        kernel.handle_pty_list(&mut beta_attached).unwrap()["ptys"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let error = kernel
        .handle_pty_read(json!({"pty_id": pty_id}), &mut beta_attached)
        .unwrap_err();
    assert_eq!(error.code, UNKNOWN_PTY);
    kernel
        .handle_pty_close(json!({"pty_id": pty_id}), &mut alpha_attached)
        .expect("owner closes PTY");
}

#[test]
fn poisoned_task_record_is_rebuilt_as_terminal_failure() {
    let kernel = Kernel::new();
    let actor = principal();
    let session = kernel.session("poisoned-task", &actor).unwrap();
    let task = Arc::new(TaskEntry {
        task: Ref::new("task", 999),
        owner: session.key.owner(),
        session_id: session.id.clone(),
        session_lease: Mutex::new(Some(session)),
        started_ns: now_ns(),
        inner: Mutex::new(TaskInner {
            state: "running",
            finished_ns: None,
            result_ref: Some(Ref::new("out", 1)),
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
    });
    let poisoner = task.clone();
    let thread = std::thread::spawn(move || {
        let _inner = poisoner.inner.lock().unwrap();
        panic!("inject task-record poison");
    });
    assert!(thread.join().is_err());
    assert!(task.inner.is_poisoned());

    task.fail_worker_panic();
    assert!(!task.inner.is_poisoned());
    let inner = task.inner.lock().unwrap();
    assert_eq!(inner.state, "failed");
    assert!(inner.finished_ns.is_some());
    assert!(inner.result_ref.is_none());
    assert_eq!(inner.error.as_ref().unwrap().code, INTERNAL_ERROR);
    assert!(inner.active_slot.is_none());
    drop(inner);
    assert!(task.session_lease.lock().unwrap().is_none());
}

#[test]
fn terminal_tasks_release_sessions_and_stale_records_are_reaped() {
    let kernel = Kernel::new();
    let actor = "agent:task-retention";
    let session = kernel.session("retention", actor).unwrap();
    let owner = session.key.owner();
    let baseline = Arc::strong_count(&session);
    let task_ref = Ref::new("task", 1001);
    let task = Arc::new(TaskEntry {
        task: task_ref.clone(),
        owner: owner.clone(),
        session_id: session.id.clone(),
        session_lease: Mutex::new(Some(session.clone())),
        started_ns: now_ns().saturating_sub(TASK_RETENTION_NS + 1),
        inner: Mutex::new(TaskInner {
            state: "completed",
            finished_ns: Some(now_ns().saturating_sub(TASK_RETENTION_NS + 1)),
            result_ref: None,
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
    });
    assert_eq!(Arc::strong_count(&session), baseline + 1);
    task.release_session_lease();
    assert_eq!(Arc::strong_count(&session), baseline);
    kernel.tasks.insert(task);
    kernel.reap_finished_tasks(&owner);
    assert!(!kernel.tasks.contains(&task_ref));
}

#[test]
fn active_task_quota_releases_when_a_task_becomes_terminal() {
    let kernel = Kernel::new();
    kernel.configure_limits(Limits {
        max_tasks_per_session: 1,
        ..Limits::default()
    });
    let (mut client, mut reader, server) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let first = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { sleep 30 }","async":true}),
    )
    .result
    .unwrap();
    let task = first["task"].clone();
    let rejected = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"1 + 1","async":true}),
    );
    assert_eq!(rejected.error.unwrap().code, QUOTA_EXCEEDED);
    call(
        &mut client,
        &mut reader,
        4,
        "task.cancel",
        json!({"task": task}),
    );
    call(
        &mut client,
        &mut reader,
        5,
        "task.await",
        json!({"task": task}),
    );
    let next = call(
        &mut client,
        &mut reader,
        6,
        "exec",
        json!({"src":"1 + 1","async":true}),
    );
    assert!(
        next.error.is_none(),
        "terminal task released its slot: {next:?}"
    );
    let next_task = next.result.unwrap()["task"].clone();
    call(
        &mut client,
        &mut reader,
        7,
        "task.await",
        json!({"task": next_task}),
    );
    drop(client);
    drop(reader);
    server.join().unwrap();
}

#[test]
fn pty_quota_reserves_before_spawn_and_releases_on_close() {
    let kernel = Kernel::new();
    kernel.configure_limits(Limits {
        max_ptys_per_session: 1,
        ..Limits::default()
    });
    let (mut client, mut reader, server) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let first = call(
        &mut client,
        &mut reader,
        2,
        "pty.open",
        json!({"cmd":"cat"}),
    )
    .result
    .unwrap();
    let pty_id = first["pty_id"].clone();
    let rejected = call(
        &mut client,
        &mut reader,
        3,
        "pty.open",
        json!({"cmd":"cat"}),
    );
    assert_eq!(rejected.error.unwrap().code, QUOTA_EXCEEDED);
    assert!(
        call(
            &mut client,
            &mut reader,
            4,
            "pty.close",
            json!({"pty_id": pty_id}),
        )
        .error
        .is_none()
    );
    let next = call(
        &mut client,
        &mut reader,
        5,
        "pty.open",
        json!({"cmd":"cat"}),
    );
    assert!(
        next.error.is_none(),
        "closed PTY released its slot: {next:?}"
    );
    let next_id = next.result.unwrap()["pty_id"].clone();
    call(
        &mut client,
        &mut reader,
        6,
        "pty.close",
        json!({"pty_id": next_id}),
    );
    drop(client);
    drop(reader);
    server.join().unwrap();
}

#[test]
fn concurrent_pty_close_has_exactly_one_teardown_owner() {
    let kernel = Kernel::new();
    let actor = principal();
    let session = kernel.session("pty-close-race", &actor).unwrap();
    let attachment = Attachment {
        session,
        principal: actor,
        can_approve: true,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
    };
    let mut opener = Some(attachment.clone());
    let opened = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut opener)
        .unwrap();
    let pty_id = opened["pty_id"].clone();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let close = |kernel: Arc<Kernel>,
                 attachment: Attachment,
                 barrier: Arc<std::sync::Barrier>,
                 pty_id: Json| {
        std::thread::spawn(move || {
            let mut attached = Some(attachment);
            barrier.wait();
            kernel.handle_pty_close(json!({"pty_id":pty_id}), &mut attached)
        })
    };
    let first = close(
        kernel.clone(),
        attachment.clone(),
        barrier.clone(),
        pty_id.clone(),
    );
    let second = close(kernel.clone(), attachment, barrier.clone(), pty_id);
    barrier.wait();
    let results = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .filter(|error| error.code == UNKNOWN_PTY)
            .count(),
        1
    );
}

#[test]
fn self_exited_pty_releases_active_and_session_leases_without_a_request() {
    let kernel = Kernel::new();
    kernel.configure_limits(Limits {
        max_ptys_per_session: 1,
        ..Limits::default()
    });
    let session = kernel.session("self-exited-pty", &principal()).unwrap();
    let owner = session.key.owner();
    let mut attached = Some(Attachment {
        session,
        principal: principal(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
    });
    let opened = kernel
        .handle_pty_open(json!({"cmd":"sh", "args":["-c", "exit 0"]}), &mut attached)
        .unwrap();
    let pty_ref: Ref = serde_json::from_value(opened["pty_id"].clone()).unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while kernel.ptys.active_for(&owner) {
        assert!(
            Instant::now() < deadline,
            "PTY watcher did not release quota"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let retained = kernel.ptys.get(&pty_ref).unwrap();
    let lifecycle = retained.lifecycle.lock().unwrap();
    assert!(lifecycle.active_slot.is_none());
    assert!(lifecycle.session_lease.is_none());
    assert!(lifecycle.terminal_since.is_some());
    drop(lifecycle);

    let next = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut attached)
        .expect("self-exit released capacity before another client request");
    kernel
        .handle_pty_close(json!({"pty_id":next["pty_id"]}), &mut attached)
        .unwrap();
}

#[test]
fn transcript_is_bounded_at_insertion_time() {
    let kernel = Kernel::new();
    let session = kernel.session("bounded", &principal()).unwrap();
    for id in 1..=(MAX_TRANSCRIPT_PER_SESSION as u64 + 1) {
        session.insert_transcript(Ref::new("out", id), Value::Int(id as i64));
    }
    let transcript = session.transcript.lock().unwrap();
    assert_eq!(transcript.len(), MAX_TRANSCRIPT_PER_SESSION);
    assert!(!transcript.contains_key(&Ref::new("out", 1)));
    assert!(transcript.contains_key(&Ref::new("out", MAX_TRANSCRIPT_PER_SESSION as u64 + 1)));
}

#[test]
fn expired_plan_is_rejected_before_it_can_execute() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
        principal()
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    kernel.set_allow_self_ack(true);
    let (mut client, mut reader, server) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let plan_ref = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo expired }","mode":"plan"}),
    )
    .result
    .unwrap()["plan_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref}),
    );
    kernel.plans.transaction(|plans| {
        plans.get_mut(&plan_ref).unwrap().created_at =
            Instant::now() - PLAN_TTL - std::time::Duration::from_secs(1);
    });
    let apply = call(
        &mut client,
        &mut reader,
        4,
        "plan.apply",
        json!({"plan_ref": plan_ref}),
    );
    assert_eq!(apply.error.unwrap().code, UNKNOWN_PLAN);
    assert!(!kernel.plans.contains(&plan_ref));
    drop(client);
    drop(reader);
    server.join().unwrap();
}

fn call(
    writer: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    id: i64,
    method: &str,
    params: Json,
) -> Response {
    write_frame(
        writer,
        &Request {
            jsonrpc: JSONRPC.into(),
            id: id.into(),
            method: method.into(),
            params,
        },
    )
    .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(reader, &mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}
#[test]
fn unix_stream_session_roundtrip() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    assert!(
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"local_auth":"local-human","client":{"kind":"test","tty":false}})
        )
        .error
        .is_none()
    );
    assert!(
        call(&mut client, &mut reader, 2, "parse", json!({"src":"1 + 2"}))
            .error
            .is_none()
    );
    let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":"1 + 2"}));
    let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
    let get = call(
        &mut client,
        &mut reader,
        4,
        "value.get",
        json!({"ref":value_ref,"path":null,"slice":null}),
    );
    assert_eq!(get.result.unwrap()["value"]["v"], 3);
    assert!(
        call(&mut client, &mut reader, 5, "task.list", json!({}))
            .error
            .is_none()
    );
    let journal = call(
        &mut client,
        &mut reader,
        6,
        "journal.query",
        json!({"limit":10}),
    );
    let entries = journal.result.unwrap();
    assert_eq!(entries[0]["src"], "1 + 2");
    assert_eq!(entries[0]["ok"], true);
    assert_eq!(
        entries[0]["opaque"], false,
        "pure arithmetic must not be journaled opaque:true"
    );
    assert!(
        entries[0]["outputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["kind"] == "value"
                && o["len"].as_i64().unwrap() > 0
                && o["hash"].as_str().unwrap().len() == 64)
    );
    let value_hash = entries[0]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["kind"] == "value")
        .unwrap()["hash"]
        .as_str()
        .unwrap();
    let blob = kernel
        .journal
        .lock()
        .unwrap()
        .read_blob(value_hash)
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(blob).unwrap().contains("\"v\":3"));
    // Slice applies to tables (list<record> semantically) — it used to
    // silently no-op and return the whole table — and slicing an
    // unordered/scalar value is an explicit error, not a silent identity.
    let texec = call(
        &mut client,
        &mut reader,
        40,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    );
    let table_ref = texec.result.unwrap()["ref"].as_str().unwrap().to_owned();
    let sliced = call(
        &mut client,
        &mut reader,
        41,
        "value.get",
        json!({"ref":table_ref,"slice":[1,3]}),
    );
    let sliced = sliced.result.unwrap()["value"].clone();
    assert_eq!(sliced["$"], "table", "csv.parse yields a table: {sliced}");
    assert_eq!(sliced["n"], 2, "table slice should keep rows 1..3");
    let bad = call(
        &mut client,
        &mut reader,
        42,
        "value.get",
        json!({"ref":value_ref,"slice":[0,1]}),
    );
    assert_eq!(
        bad.error.expect("slicing an int must error").code,
        BAD_PATH_OR_SLICE,
        "slice on a scalar must be an explicit error"
    );
    // `[a..b]` path ranges (site/content/internals/kernel-protocol.md) — used to be "bad index".
    let ranged = call(
        &mut client,
        &mut reader,
        43,
        "value.get",
        json!({"ref":table_ref,"path":"rows[0..2]"}),
    );
    let ranged = ranged.result.unwrap()["value"].clone();
    assert_eq!(ranged["$"], "list", "rows[0..2]: {ranged}");
    assert_eq!(ranged["v"].as_array().unwrap().len(), 2);
    // `format=render` returns the human string; `format=raw` on a non-str
    // value is an explicit error.
    let rendered = call(
        &mut client,
        &mut reader,
        44,
        "value.get",
        json!({"ref":table_ref,"format":"render"}),
    );
    let rendered = rendered.result.unwrap();
    assert!(
        rendered["render"].as_str().unwrap().contains('1'),
        "render output: {rendered}"
    );
    let raw_bad = call(
        &mut client,
        &mut reader,
        45,
        "value.get",
        json!({"ref":value_ref,"format":"raw"}),
    );
    assert_eq!(
        raw_bad.error.expect("raw on int must error").code,
        BAD_PATH_OR_SLICE
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn leash_plan_approval_and_denial_flow() {
    for (opaque, expected, approvable) in
        [("ask", "approval_required", true), ("deny", "deny", false)]
    {
        let policy = Policy::from_toml(&format!(
            "[principal.\"{}\"]\nopaque='{opaque}'\nauto_apply='never'\n",
            principal()
        ))
        .unwrap();
        let kernel = Kernel::with_policy(policy);
        // Single-connection approve→apply flow: opt into self-ack (HR-D3);
        // the cross-principal separation gate is tested on its own.
        kernel.set_allow_self_ack(true);
        let (mut client, server) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let server_kernel = kernel.clone();
        let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
        call(
            &mut client,
            &mut reader,
            1,
            "session.attach",
            json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
        );
        let planned = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
        );
        let result = planned.result.unwrap();
        assert_eq!(result["verdict"], expected);
        assert_eq!(result["effects"], json!([{"kind":"opaque"}]));
        let plan_ref = result["plan_ref"].as_str().unwrap();
        assert!(
            call(
                &mut client,
                &mut reader,
                3,
                "plan.apply",
                json!({"plan_ref":plan_ref})
            )
            .error
            .is_some()
        );
        let grant = call(
            &mut client,
            &mut reader,
            4,
            "cap.request",
            json!({"plan_ref":plan_ref,"effects":[]}),
        );
        if approvable {
            assert!(grant.error.is_none());
            let applied = call(
                &mut client,
                &mut reader,
                5,
                "plan.apply",
                json!({"plan_ref":plan_ref}),
            );
            let value = applied.result.unwrap()["value"].clone();
            assert_eq!(value["$"], "outcome");
            assert_eq!(value["ok"], true);
        } else {
            assert!(grant.error.is_some());
        }
        drop(client);
        drop(reader);
        thread.join().unwrap();
    }
}

/// Regression: `mode:"approved"` used to skip the leash verdict for ANY
/// caller — the magic string alone bypassed policy. It is `plan.apply`'s
/// re-entry and must name a stored plan that is approved for this
/// session/principal with the same source; anything else is rejected
/// even though a plain `run` of the same source would only be
/// approval_required.
#[test]
fn approved_mode_is_not_a_caller_assertable_bypass() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
        principal()
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    // Approves its own plan over one connection to reach the approved
    // re-entry it is really testing: opt into self-ack (HR-D3).
    kernel.set_allow_self_ack(true);
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    // Baseline: the policy gates a plain run of this source.
    let run = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"run","position":"stmt"}),
    );
    assert_eq!(
        run.error.expect("run must be gated").code,
        APPROVAL_REQUIRED
    );
    // The bypass: bare `mode:"approved"` (no plan_ref) must be rejected…
    let bare = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"sh { echo hi }","mode":"approved","position":"stmt"}),
    );
    assert_eq!(
        bare.error.expect("bare approved must fail").code,
        LEASH_DENIED
    );
    // …as must a plan_ref that was never approved…
    let planned = call(
        &mut client,
        &mut reader,
        4,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
    );
    let plan_ref = planned.result.unwrap()["plan_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let unapproved = call(
        &mut client,
        &mut reader,
        5,
        "exec",
        json!({"src":"sh { echo hi }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
    );
    assert_eq!(
        unapproved
            .error
            .expect("unapproved plan_ref must fail")
            .code,
        LEASH_DENIED
    );
    // …and an approved plan_ref may not smuggle DIFFERENT source.
    call(
        &mut client,
        &mut reader,
        6,
        "cap.request",
        json!({"plan_ref":plan_ref,"effects":[]}),
    );
    let smuggled = call(
        &mut client,
        &mut reader,
        7,
        "exec",
        json!({"src":"sh { rm -rf / }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
    );
    assert_eq!(
        smuggled.error.expect("source smuggling must fail").code,
        LEASH_DENIED
    );
    // The sanctioned path still works: same source, approved plan.
    let sanctioned = call(
        &mut client,
        &mut reader,
        8,
        "exec",
        json!({"src":"sh { echo hi }","mode":"approved","position":"stmt","plan_ref":plan_ref}),
    );
    assert!(
        sanctioned.error.is_none(),
        "sanctioned approved exec failed: {:?}",
        sanctioned.error
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// Regression for the plan_ref collision (Plan identity used to hash only
/// effects/reversibility/estimates, so any two opaque `sh { }` plans
/// collided and `apply` silently ran whichever plan was last inserted).
#[test]
fn plan_refs_are_unique_per_source_and_apply_targets_the_right_one() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let plan_a = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo FIRST }","mode":"plan"}),
    )
    .result
    .unwrap();
    let plan_b = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"sh { echo SECOND }","mode":"plan"}),
    )
    .result
    .unwrap();
    let ref_a = plan_a["plan_ref"].as_str().unwrap().to_owned();
    let ref_b = plan_b["plan_ref"].as_str().unwrap().to_owned();
    assert_ne!(ref_a, ref_b, "distinct sources must not share a plan_ref");
    // Both plans are opaque (`sh { }`), so both need cap.request before
    // apply under the default permissive-but-opaque='allow' policy —
    // plan mode always requires explicit approval regardless of opaque
    // mode; grant both, then apply A and confirm it — not B — ran.
    call(
        &mut client,
        &mut reader,
        4,
        "cap.request",
        json!({"plan_ref":ref_a}),
    );
    call(
        &mut client,
        &mut reader,
        5,
        "cap.request",
        json!({"plan_ref":ref_b}),
    );
    let applied = call(
        &mut client,
        &mut reader,
        6,
        "plan.apply",
        json!({"plan_ref":ref_a}),
    );
    let out = applied.result.unwrap()["value"]["out"].clone();
    assert_eq!(out["$"], "str");
    assert_eq!(out["v"], "FIRST");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn storing_identical_plan_twice_creates_distinct_objects() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let src = "sh { echo SAME }";
    let first = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":src,"mode":"plan"}),
    )
    .result
    .unwrap();
    let second = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":src,"mode":"plan"}),
    )
    .result
    .unwrap();
    let first_ref = first["plan_ref"].as_str().unwrap();
    let second_ref = second["plan_ref"].as_str().unwrap();
    assert_ne!(first_ref, second_ref, "stored objects must never replace");
    assert!(first_ref.len() > 64, "plan refs carry the full digest");
    assert!(second_ref.len() > 64, "plan refs carry the full digest");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn real_effects_not_opaque_for_pure_builtins() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    for src in ["1 + 2", "ls"] {
        let planned = call(
            &mut client,
            &mut reader,
            2,
            "exec",
            json!({"src":src,"mode":"plan"}),
        )
        .result
        .unwrap();
        assert_ne!(
            planned["effects"],
            json!([{"kind":"opaque"}]),
            "`{src}` must derive real effects, not the opaque fallback"
        );
        let exec = call(&mut client, &mut reader, 3, "exec", json!({"src":src}));
        let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
        let journal = call(
            &mut client,
            &mut reader,
            4,
            "journal.query",
            json!({"limit":1}),
        )
        .result
        .unwrap();
        assert_eq!(journal[0]["src"], src);
        assert_eq!(
            journal[0]["opaque"], false,
            "`{src}` must not be journaled opaque:true"
        );
        let _ = value_ref;
    }
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn value_get_path_traversal() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hello world }"}),
    );
    let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
    let out = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref":value_ref,"path":"out"}),
    );
    assert_eq!(
        out.result.unwrap()["value"],
        json!({"$":"str","v":"hello world"})
    );
    let ok = call(
        &mut client,
        &mut reader,
        4,
        "value.get",
        json!({"ref":value_ref,"path":"ok"}),
    );
    assert_eq!(ok.result.unwrap()["value"], json!({"$":"bool","v":true}));
    let bad = call(
        &mut client,
        &mut reader,
        5,
        "value.get",
        json!({"ref":value_ref,"path":"nope"}),
    );
    assert_eq!(bad.error.unwrap().code, BAD_PATH_OR_SLICE);

    let ls_exec = call(&mut client, &mut reader, 6, "exec", json!({"src":"ls"}));
    let ls_ref = ls_exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
    let rows0 = call(
        &mut client,
        &mut reader,
        7,
        "value.get",
        json!({"ref":ls_ref,"path":"rows[0].name"}),
    );
    assert!(rows0.error.is_none(), "{:?}", rows0.error);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn exec_position_stmt_raises_value_does_not() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let stmt = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { exit 7 }","position":"stmt"}),
    );
    assert_eq!(stmt.error.unwrap().code, RAISED);
    let value = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"sh { exit 7 }","position":"value"}),
    );
    let result = value.result.unwrap();
    assert_eq!(result["value"]["$"], "outcome");
    assert_eq!(result["value"]["ok"], false);
    assert_eq!(result["value"]["status"], 7);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn async_tasks_survive_disconnect_and_cancel() {
    let kernel = Kernel::new();
    let (mut first, server) = UnixStream::pair().unwrap();
    let mut first_reader = BufReader::new(first.try_clone().unwrap());
    let k = kernel.clone();
    let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
    call(
        &mut first,
        &mut first_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"tasks","client":{"kind":"test","tty":false}}),
    );
    let started = call(
        &mut first,
        &mut first_reader,
        2,
        "exec",
        json!({"src":"sh { sleep 0.2 }","async":true}),
    );
    let survived: Ref = serde_json::from_value(started.result.unwrap()["task"].clone()).unwrap();
    drop(first);
    drop(first_reader);
    thread.join().unwrap();

    let (mut second, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(second.try_clone().unwrap());
    let k = kernel.clone();
    let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
    call(
        &mut second,
        &mut reader,
        3,
        "session.attach",
        json!({"local_auth":"local-human","session":"tasks","client":{"kind":"test","tty":false}}),
    );
    let awaited = call(
        &mut second,
        &mut reader,
        4,
        "task.await",
        json!({"task":survived}),
    );
    let awaited_value = awaited.result.unwrap();
    assert_eq!(awaited_value["state"], "completed", "{awaited_value}");
    let long = call(
        &mut second,
        &mut reader,
        5,
        "exec",
        json!({"src":"sh { sleep 30 }","async":true}),
    );
    let task: Ref = serde_json::from_value(long.result.unwrap()["task"].clone()).unwrap();
    let listed = call(&mut second, &mut reader, 6, "task.list", json!({}));
    assert!(listed.result.unwrap().as_array().unwrap().len() >= 2);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(
        call(
            &mut second,
            &mut reader,
            7,
            "task.cancel",
            json!({"task":task})
        )
        .error
        .is_none()
    );
    let before = Instant::now();
    let cancelled = call(
        &mut second,
        &mut reader,
        8,
        "task.await",
        json!({"task":task}),
    );
    assert!(before.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(cancelled.result.unwrap()["state"], "cancelled");
    drop(second);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn bearer_attach_uses_token_principal_and_rejects_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    let (secret, meta) = tokens
        .create(
            "agent:codex".into(),
            "readonly".into(),
            vec!["fs.read".into()],
            None,
        )
        .unwrap();
    drop(tokens);
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let k = kernel.clone();
    let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"token":secret,"client":{"kind":"agent","tty":false}}),
    );
    assert_eq!(attached.result.unwrap()["principal"], "agent:codex");
    assert!(
        call(&mut client, &mut reader, 2, "parse", json!({"src":"1 + 2"}))
            .error
            .is_none()
    );
    let mut revoker = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    assert!(revoker.revoke(&meta.id).unwrap());
    let revoked = call(&mut client, &mut reader, 3, "parse", json!({"src":"1 + 2"}));
    assert_eq!(revoked.error.unwrap().code, AUTH_FAILED);
    let detached = call(&mut client, &mut reader, 4, "session.env", json!({}));
    assert_eq!(detached.error.unwrap().code, NOT_ATTACHED);
    let reattached = call(
        &mut client,
        &mut reader,
        5,
        "session.attach",
        json!({"client":{"kind":"agent","tty":false}}),
    );
    assert_eq!(reattached.result.unwrap()["auth_mode"], "restricted-agent");
    let denied = call(
        &mut client,
        &mut reader,
        6,
        "session.attach",
        json!({"token":"not-a-token","client":{"kind":"agent","tty":false}}),
    );
    assert_eq!(denied.error.unwrap().code, AUTH_FAILED);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn idle_revoked_bearer_is_disconnected_and_loses_subscriptions() {
    use std::io::Read;

    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    let (secret, meta) = tokens
        .create("agent:idle".into(), "readonly".into(), vec![], None)
        .unwrap();
    let kernel = Kernel::open(dir.path()).unwrap();
    kernel.configure_limits(Limits {
        frame_read_timeout_ms: 40,
        ..Limits::default()
    });
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let worker_kernel = kernel.clone();
    let worker = std::thread::spawn(move || worker_kernel.handle_stream(server));
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"token":secret,"client":{"kind":"agent","tty":false}}),
    );
    assert!(attached.error.is_none());
    let subscribed = call(
        &mut client,
        &mut reader,
        2,
        "events.subscribe",
        json!({"channel":"user.revoked"}),
    );
    assert!(subscribed.error.is_none());
    assert_eq!(kernel.events.subscriber_count(), 1);

    let mut revoker = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    assert!(revoker.revoke(&meta.id).unwrap());
    let mut byte = [0u8; 1];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(kernel.events.subscriber_count(), 0);
}

#[test]
fn stale_attachment_security_epoch_fails_closed_and_detaches() {
    let kernel = Kernel::new();
    let session = kernel.session("stale-epoch", "agent:mcp").unwrap();
    let mut attached = Some(Attachment {
        session,
        principal: "agent:mcp".into(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH.saturating_sub(1),
    });
    let response = kernel.dispatch(
        Request {
            jsonrpc: JSONRPC.into(),
            id: json!(1),
            method: "parse".into(),
            params: json!({"src":"1 + 2"}),
        },
        77,
        &mut attached,
        None,
    );
    assert_eq!(response.error.unwrap().code, AUTH_FAILED);
    assert!(attached.is_none());
}

#[test]
fn zero_token_attach_defaults_restricted_and_reports_security_metadata() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"client":{"kind":"untrusted","tty":false}}),
    )
    .result
    .unwrap();
    assert_eq!(attached["principal"], "agent:mcp");
    assert_eq!(attached["auth_mode"], "restricted-agent");
    assert_eq!(attached["session_isolation"], PRINCIPAL_SESSION_ISOLATION);
    assert_eq!(attached["security_epoch"], ATTACH_SECURITY_EPOCH);

    let denied = call(&mut client, &mut reader, 2, "exec", json!({"src":"1 + 2"}));
    assert_eq!(denied.error.unwrap().code, LEASH_DENIED);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn token_and_local_auth_cannot_be_combined() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    let response = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({
            "token":"irrelevant",
            "local_auth":"local-human",
            "client":{"kind":"test","tty":false}
        }),
    );
    assert_eq!(response.error.unwrap().code, INVALID_PARAMS);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// End-to-end proof that a real (on-disk) kernel session's own
/// `Evaluator` gets a journal installed automatically, with no test-side
/// probe injection (unlike `handlers_exec::tests::
/// exec_calls_set_source_so_stmt_journal_entries_carry_src`, which
/// installs a journal by hand because it drives an ephemeral
/// `Kernel::new()` — deliberately the ONE case that must stay journal-
/// less, since it has no on-disk state dir at all). `Kernel::open` gives
/// this kernel a real `state_dir`, so `session()` should now open a
/// second journal handle on it and hand it to the session's evaluator
/// (`crates/shoal-kernel/src/session.rs`). Attach, run a marker
/// statement, then run the in-language `history` builtin — all over the
/// same real Unix-socket wire `session.attach`/`exec` use in production —
/// and confirm the marker's exact source text comes back in `history`'s
/// `src` column. Before the fix, `session()` never called
/// `Evaluator::set_journal` at all, so `history` inside a kernel session
/// always came back empty regardless of what the kernel's own separate,
/// coarser exec-level journal (`self.journal`, `journal.query`) recorded.
#[test]
fn kernel_open_installs_a_session_journal_so_history_builtin_sees_real_data() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let marker_src = "let kernel_journal_probe_4471 = 4471";
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src": marker_src}),
    );
    assert!(
        exec.error.is_none(),
        "marker exec must succeed: {:?}",
        exec.error
    );

    let hist = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src": "history"}),
    );
    let hist_result = hist.result.expect("exec of `history` must succeed");
    let cols = hist_result["value"]["cols"]["src"]
        .as_array()
        .unwrap_or_else(|| panic!("history's table has no src column: {hist_result:?}"));
    assert!(
        cols.iter().any(|v| v["v"] == marker_src),
        "no journal entry with src={marker_src:?} found among {cols:?} — the session \
             evaluator has no journal installed, so the in-language `history` builtin is inert \
             even though a real on-disk Kernel::open must install one automatically"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// site/content/internals/kernel-protocol.md (`pty.list` / `shoal://pty`): open PTY sessions are
/// first-class and session-scoped. A pty opened by session A is enumerated
/// by A's `pty.list`, is invisible to a DIFFERENT session B (both the list
/// and a direct `pty.read` of A's ref — an opaque `UNKNOWN_PTY`), and
/// leaves A's list once closed. Drives a real `cat` on a PTY like the live
/// MCP test, over the same Unix-socket wire production uses.
#[test]
fn pty_list_is_session_scoped() {
    let kernel = Kernel::new();
    // Session A on connection one.
    let (mut a, server) = UnixStream::pair().unwrap();
    let mut a_reader = BufReader::new(a.try_clone().unwrap());
    let ka = kernel.clone();
    let ta = std::thread::spawn(move || ka.handle_stream(server).unwrap());
    call(
        &mut a,
        &mut a_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"A","client":{"kind":"agent","tty":false}}),
    );
    let opened = call(&mut a, &mut a_reader, 2, "pty.open", json!({"cmd":"cat"}));
    let pty_id = opened.result.unwrap()["pty_id"]
        .as_str()
        .expect("pty.open returns a pty_id")
        .to_owned();

    // A sees exactly its one pty, with the documented shape.
    let list_a = call(&mut a, &mut a_reader, 3, "pty.list", json!({}));
    let ptys_a = list_a.result.unwrap()["ptys"].as_array().unwrap().clone();
    assert_eq!(ptys_a.len(), 1, "session A sees its one pty: {ptys_a:?}");
    assert_eq!(ptys_a[0]["pty_id"], json!(pty_id));
    assert_eq!(ptys_a[0]["cmd"], "cat");
    assert_eq!(ptys_a[0]["alive"], true);
    assert!(ptys_a[0]["pid"].as_u64().unwrap() > 0);
    assert!(ptys_a[0]["cols"].as_u64().unwrap() > 0);
    assert!(ptys_a[0]["rows"].as_u64().unwrap() > 0);

    // Session B on a second connection: a different session must NOT see
    // A's ptys, and cannot read A's pty by ref (opaque not-found).
    let (mut b, server) = UnixStream::pair().unwrap();
    let mut b_reader = BufReader::new(b.try_clone().unwrap());
    let kb = kernel.clone();
    let tb = std::thread::spawn(move || kb.handle_stream(server).unwrap());
    call(
        &mut b,
        &mut b_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"B","client":{"kind":"agent","tty":false}}),
    );
    let list_b = call(&mut b, &mut b_reader, 2, "pty.list", json!({}));
    assert!(
        list_b.result.unwrap()["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "session B must not see session A's ptys"
    );
    let read_b = call(
        &mut b,
        &mut b_reader,
        3,
        "pty.read",
        json!({"pty_id": pty_id}),
    );
    assert_eq!(
        read_b.error.expect("B cannot read A's pty").code,
        UNKNOWN_PTY,
        "another session's pty ref is an opaque UNKNOWN_PTY"
    );

    // Closing from A drops it out of A's list.
    call(
        &mut a,
        &mut a_reader,
        4,
        "pty.close",
        json!({"pty_id": pty_id}),
    );
    let list_a2 = call(&mut a, &mut a_reader, 5, "pty.list", json!({}));
    assert!(
        list_a2.result.unwrap()["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a closed pty must leave pty.list"
    );

    drop(a);
    drop(a_reader);
    ta.join().unwrap();
    drop(b);
    drop(b_reader);
    tb.join().unwrap();
}

// -----------------------------------------------------------------------
// The elision rule (site/content/internals/kernel-protocol.md).
// -----------------------------------------------------------------------

/// A >100-row table (real `ls` over a directory with 150 files, not a
/// synthetic stand-in) must come back elided: shape + schema + a 5-row
/// preview, never the 150-row payload. Then drill into a single row by
/// field-path and confirm that small result is NOT elided.
#[test]
fn big_table_exec_elides_then_drills_by_path() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..150 {
        std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src": format!("ls {}", dir.path().display())}),
    );
    let result = exec.result.expect("ls must succeed");
    let value_ref = result["ref"].as_str().unwrap().to_owned();
    // `ls` is a command: its wire shape is `outcome` with a structured
    // `.out`. Elision unwraps to `.out` for the decision (mirroring
    // render_block's outcome-unification) — the 150-row *table* elides,
    // the outer outcome envelope (status/ok/cmd/…) still travels.
    let value = &result["value"];
    assert_eq!(value["$"], "outcome");
    let out = &value["out"];
    assert_eq!(out["$"], "ref", "a 150-row table must elide, got {out}");
    assert_eq!(out["of"], "table");
    assert_eq!(out["n"], 150);
    assert_eq!(
        out["cols"]["name"], "str",
        "shape (schema) travels even when the payload does not"
    );
    assert_eq!(out["preview"]["$"], "table");
    assert_eq!(
        out["preview"]["n"], 5,
        "preview is a small head, not the full 150 rows"
    );
    assert!(out["render_head"].as_str().unwrap().contains("name"));
    let wire_len = serde_json::to_string(value).unwrap().len();
    assert!(
        wire_len < 4 * 1024,
        "the elided form itself must stay tiny, was {wire_len} bytes"
    );

    // Drill in: value.get with a field-path returns one small row —
    // NOT elided, because it never hits any threshold.
    let get = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref, "path": "out[3]"}),
    );
    let drilled = get.result.unwrap()["value"].clone();
    assert_ne!(
        drilled["$"], "ref",
        "a single drilled row must not be elided: {drilled}"
    );
    assert_eq!(drilled["$"], "record");
    assert!(
        drilled["v"]["name"].is_object(),
        "drilled row keeps its fields: {drilled}"
    );

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn small_value_is_not_elided() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"[1,2,3]"}),
    );
    let value = exec.result.unwrap()["value"].clone();
    assert_eq!(
        value["$"], "list",
        "a 3-item list is nowhere near any threshold"
    );
    assert_eq!(
        value["v"],
        json!([{"$":"int","v":1},{"$":"int","v":2},{"$":"int","v":3}])
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// A caller may loosen the byte budget, but never past the 64 KiB hard
/// cap — a misbehaving agent cannot flood its own context by asking
/// nicely.
#[test]
fn elision_hard_cap_cannot_be_disabled() {
    let huge = Value::Str("x".repeat(100_000));
    let loosened = ElideSpec {
        max_bytes: Some(5_000_000),
        max_rows: None,
        max_items: None,
    };
    let budget = ElideBudget::from_spec(Some(&loosened));
    assert_eq!(
        budget.max_bytes, ELIDE_HARD_CAP,
        "a requested budget above the hard cap must clamp down to it"
    );
    match elide_wire_value(&huge, "shoal://out/1", &budget) {
        WireValue::Ref { of, n, .. } => {
            assert_eq!(of, "str");
            assert_eq!(n, 100_000);
        }
        other => panic!(
            "a 100 KB string must still elide despite a 5 MB requested budget, got {other:?}"
        ),
    }
}

/// The flip side: loosening below the hard cap is honored, so a caller
/// that wants a bit more headroom than the 8 KiB default legitimately
/// gets it.
#[test]
fn elision_budget_can_be_loosened_up_to_the_hard_cap() {
    let modest = Value::Str("y".repeat(20_000)); // > 8 KiB default, < 64 KiB cap
    let loosened = ElideSpec {
        max_bytes: Some(5_000_000),
        max_rows: None,
        max_items: None,
    };
    let budget = ElideBudget::from_spec(Some(&loosened));
    match elide_wire_value(&modest, "shoal://out/1", &budget) {
        WireValue::Str { .. } => {}
        other => panic!("a 20 KiB string fits under a loosened 64 KiB cap, got {other:?}"),
    }
}

#[test]
fn value_get_elide_param_tightens_default_row_threshold() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    // 10 items: under every default threshold, so a plain exec would not elide.
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"[0,1,2,3,4,5,6,7,8,9]"}),
    );
    assert_ne!(exec.result.as_ref().unwrap()["value"]["$"], "ref");
    let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
    // A caller may tighten the budget per call — max_items:5 must elide
    // this same 10-item list on a follow-up `value.get`.
    let get = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref, "path": null, "slice": null, "elide": {"max_items": 5}}),
    );
    let value = get.result.unwrap()["value"].clone();
    assert_eq!(
        value["$"], "ref",
        "a tightened per-call budget must elide: {value}"
    );
    assert_eq!(value["n"], 10);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// site/content/internals/language-conformance-contract.md wire follow-up: `value.get` RESOLVES a CAS-backed bytes ref. A
/// top-level `CasBytes` (a value-position capture spilled to the CAS) elides
/// to a `ref` on the default `format=json` path — a huge blob never ships
/// whole — but an explicit `slice` or `format=raw` fetches the real content
/// from the CAS through the value's own loader (the same `BytesLoad`/`Cas`
/// seam), honoring the elision wall on what actually travels back.
#[test]
fn value_get_resolves_cas_backed_bytes_ref() {
    struct FixedLoader(Vec<u8>);
    impl shoal_value::BytesLoad for FixedLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }
    struct FailLoader;
    impl shoal_value::BytesLoad for FailLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "blob gone",
            ))
        }
    }
    let decode =
        |s: &str| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap();

    let kernel = Kernel::new();
    // Pre-populate a session transcript with a CAS-backed bytes value (5000
    // bytes — well over the raw budget, so the default fetch must elide) and
    // a second one whose loader fails (an unresolvable ref).
    let content: Vec<u8> = (0u32..5000).map(|i| (i % 251) as u8).collect();
    let session = kernel.session("casb", &principal()).unwrap();
    {
        let ok = std::sync::Arc::new(shoal_value::CasBytesVal {
            hash: "a".repeat(64),
            len: content.len() as u64,
            preview: std::sync::Arc::new(content[..64].to_vec()),
            truncated: false,
            loader: std::sync::Arc::new(FixedLoader(content.clone())),
        });
        let broken = std::sync::Arc::new(shoal_value::CasBytesVal {
            hash: "b".repeat(64),
            len: 123,
            preview: std::sync::Arc::new(Vec::new()),
            truncated: false,
            loader: std::sync::Arc::new(FailLoader),
        });
        let mut t = session.transcript.lock().unwrap();
        t.insert(Ref::new("out", 1u64), Value::CasBytes(ok));
        t.insert(Ref::new("out", 2u64), Value::CasBytes(broken));
    }

    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"casb","client":{"kind":"agent","tty":false}}),
    );

    // Default json: a CAS-backed value elides to an honest ref (no content),
    // carrying the TRUE length — a huge blob never ships whole.
    let def = call(
        &mut client,
        &mut reader,
        2,
        "value.get",
        json!({"ref":"out:1"}),
    );
    let v = def.result.unwrap()["value"].clone();
    assert_eq!(v["$"], "ref", "a CAS-backed value elides by default: {v}");
    assert_eq!(v["of"], "bytes");
    assert_eq!(
        v["n"],
        content.len(),
        "the elided ref carries the true length"
    );

    // A small slice RESOLVES to the exact CAS bytes, inline.
    let sl = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref":"out:1","slice":[0,10]}),
    );
    let v = sl.result.unwrap()["value"].clone();
    assert_eq!(v["$"], "bytes", "a small slice resolves inline: {v}");
    assert_eq!(decode(v["v"].as_str().unwrap()), content[0..10]);

    // format=raw resolves the FULL content (base64).
    let raw = call(
        &mut client,
        &mut reader,
        4,
        "value.get",
        json!({"ref":"out:1","format":"raw"}),
    );
    assert_eq!(
        decode(raw.result.unwrap()["raw_base64"].as_str().unwrap()),
        content
    );

    // slice + format=raw resolves exactly the requested sub-range.
    let rawslice = call(
        &mut client,
        &mut reader,
        5,
        "value.get",
        json!({"ref":"out:1","slice":[5,15],"format":"raw"}),
    );
    assert_eq!(
        decode(rawslice.result.unwrap()["raw_base64"].as_str().unwrap()),
        content[5..15]
    );

    // A slice that is itself still oversized re-elides at the wall.
    let big = call(
        &mut client,
        &mut reader,
        6,
        "value.get",
        json!({"ref":"out:1","slice":[0,5000]}),
    );
    assert_eq!(
        big.result.unwrap()["value"]["$"],
        "ref",
        "an oversized slice re-elides rather than shipping whole"
    );

    // An unresolvable ref (its CAS blob is gone) is a clear error, no panic.
    let bad = call(
        &mut client,
        &mut reader,
        7,
        "value.get",
        json!({"ref":"out:2","slice":[0,1]}),
    );
    assert!(
        bad.error.is_some(),
        "a failed CAS resolution surfaces an error, not a panic"
    );

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// Read one already-written frame off the socket (no request sent) — for
/// asserting on pushed `event` notifications interleaved with responses.
fn recv_line(reader: &mut BufReader<UnixStream>) -> Json {
    let mut line = String::new();
    std::io::BufRead::read_line(reader, &mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn attach(client: &mut UnixStream, reader: &mut BufReader<UnixStream>) -> Response {
    call(
        client,
        reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    )
}

fn spawn(
    kernel: &Arc<Kernel>,
) -> (
    UnixStream,
    BufReader<UnixStream>,
    std::thread::JoinHandle<()>,
) {
    let (client, server) = UnixStream::pair().unwrap();
    let reader = BufReader::new(client.try_clone().unwrap());
    let k = kernel.clone();
    let thread = std::thread::spawn(move || k.handle_stream(server).unwrap());
    (client, reader, thread)
}

// -----------------------------------------------------------------------
// Events — channels, cursors, push (site/content/internals/kernel-protocol.md).
// -----------------------------------------------------------------------

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
        assert_eq!(published.result.unwrap()["seq"], i as i64);
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
    assert_eq!(kernel.events.subscriber_count(), 1);

    call(
        &mut client,
        &mut reader,
        5,
        "session.attach",
        json!({"local_auth":"local-human","session":"owner-b","client":{"kind":"test","tty":false}}),
    );
    assert_eq!(
        kernel.events.subscriber_count(),
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
        json!({"hash":hash}),
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
    let read = call(
        &mut client,
        &mut reader,
        9001,
        "events.read",
        json!({"channel":"journal","since":0}),
    );
    let events = read.result.unwrap()["events"].as_array().unwrap().clone();
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

    // A `limit` still bounds the result to the newest N even when the
    // fallback contributed older events.
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
    assert_eq!(limited.len(), 5, "limit keeps the newest 5");
    assert_eq!(limited[4]["seq"].as_u64().unwrap(), (total - 1) as u64);

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
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);

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
    let jq = call(
        &mut client,
        &mut reader,
        9000,
        "journal.query",
        json!({"limit": 1_000_000}),
    );
    let rows = jq.result.unwrap().as_array().unwrap().len();
    assert!(
        rows >= execs * 3,
        "on-disk store should also hold the finer per-statement entries: {rows} rows for \
             {execs} execs"
    );

    // Replay from seq 0 (aged out): EXACTLY one event per exec, contiguous
    // seqs, each the coarse whole-submission entry (head "let") — no
    // phantom per-statement events.
    let read = call(
        &mut client,
        &mut reader,
        9001,
        "events.read",
        json!({"channel":"journal","since":0}),
    );
    let events = read.result.unwrap()["events"].as_array().unwrap().clone();
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
    let read = call(
        &mut client,
        &mut reader,
        9001,
        "events.read",
        json!({"channel":"session.transcript","since":0}),
    );
    let events = read.result.unwrap()["events"].as_array().unwrap().clone();
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

    // A `limit` still bounds the result to the newest N even when the
    // fallback contributed older events.
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
    assert_eq!(limited.len(), 5, "limit keeps the newest 5");
    assert_eq!(limited[4]["seq"].as_u64().unwrap(), (total - 1) as u64);

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

    let read = call(
        &mut client,
        &mut reader,
        9001,
        "events.read",
        json!({"channel":"session.transcript","since":0}),
    );
    let events = read.result.unwrap()["events"].as_array().unwrap().clone();
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
    let pre_restart_execs = 5usize;

    // "Prior lifetime": open, run a few execs, then drop the kernel
    // entirely (closing its journal handle) before reopening.
    {
        let kernel = Kernel::open(dir.path()).unwrap();
        let (mut client, mut reader, thread) = spawn(&kernel);
        attach(&mut client, &mut reader);
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
    attach(&mut client2, &mut reader2);

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
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
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

/// `approval` used to be advertised in
/// `STATIC_CHANNELS` with nothing ever publishing to it — a dead
/// channel. `exec {mode:"plan"}` now fires it the moment a plan lands at
/// `Verdict::ApprovalRequired`, so a SEPARATE principal (a human's
/// session, a supervising agent — a different connection here, on
/// purpose) learns about a pending approval by subscribing, never by
/// polling `journal.query` or re-deriving the same plan.
#[test]
fn approval_channel_fires_when_a_plan_needs_approval() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
        principal()
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);

    // A separate observer connection, subscribed to `approval`, never
    // itself issuing the plan below.
    let (mut observer, mut observer_reader, observer_thread) = spawn(&kernel);
    attach(&mut observer, &mut observer_reader);
    call(
        &mut observer,
        &mut observer_reader,
        2,
        "events.subscribe",
        json!({"channel":"approval"}),
    );

    // A different connection: the agent whose plan lands at
    // approval_required.
    let (mut agent, mut agent_reader, agent_thread) = spawn(&kernel);
    attach(&mut agent, &mut agent_reader);
    let planned = call(
        &mut agent,
        &mut agent_reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
    )
    .result
    .unwrap();
    assert_eq!(planned["verdict"], "approval_required");
    assert_eq!(planned["approval_pending"], true);
    let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();

    // The observer receives the approval event on its own connection —
    // it never touched this plan itself.
    let note = recv_line(&mut observer_reader);
    assert_eq!(note["method"], "event", "expected a pushed event: {note}");
    assert_eq!(note["params"]["channel"], "approval");
    let payload = &note["params"]["payload"]["v"];
    assert_eq!(payload["plan_ref"]["v"], plan_ref);
    assert_eq!(payload["principal"]["v"], principal());
    assert_eq!(payload["effects"]["v"], json!([{"kind":"opaque"}]));
    assert_eq!(
        payload["expires"]["$"], "null",
        "no plan-expiry mechanism exists yet — honestly null, not fabricated: {payload}"
    );

    drop(observer);
    drop(observer_reader);
    observer_thread.join().unwrap();
    drop(agent);
    drop(agent_reader);
    agent_thread.join().unwrap();
}

/// A plan that is immediately `Verdict::Allow` never needed approval, so
/// it must NOT fire `approval` — only a plan actually stuck pending
/// should ever announce on this channel.
#[test]
fn approval_channel_stays_silent_for_an_immediately_allowed_plan() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    call(
        &mut client,
        &mut reader,
        2,
        "events.subscribe",
        json!({"channel":"approval"}),
    );
    // The default-permissive policy allows pure arithmetic outright.
    let planned = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"1 + 2","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(planned["verdict"], "allow");
    let read_back = call(
        &mut client,
        &mut reader,
        4,
        "events.read",
        json!({"channel":"approval"}),
    )
    .result
    .unwrap();
    assert_eq!(
        read_back["events"].as_array().unwrap().len(),
        0,
        "an allowed plan must never announce on `approval`: {read_back}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn attach_advertises_channels_elide_defaults_and_enforcement() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    let r = attach(&mut client, &mut reader).result.unwrap();
    assert_eq!(r["caps_enforced"], false);
    assert_eq!(r["elide_defaults"]["max_rows"], 100);
    assert_eq!(r["elide_defaults"]["hard_cap"], 64 * 1024);
    assert!(
        r["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "session.transcript")
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn attach_reports_the_honest_detected_tier() {
    // site/content/internals/language-conformance-contract.md tier honesty: the tier at attach is the strongest OS backend
    // this host actually has (detected), NOT a hardcoded "D". Under the
    // default-permissive human policy nothing is confined, so `enforced`
    // stays false even where a backend exists.
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    let r = attach(&mut client, &mut reader).result.unwrap();
    let expected = tier_letter(EnforcementStatus::detect().available_tier);
    assert_eq!(r["caps"]["tier"], expected);
    assert_eq!(r["caps_enforced"], false);
    assert_eq!(r["caps"]["enforced"], false);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn attach_enforces_only_for_a_scoped_principal_with_a_real_backend() {
    // A genuinely-scoped principal reports `enforced: true` — but only when
    // a real OS backend (Landlock/Seatbelt) exists; on a host without one
    // the answer honestly degrades to false rather than claiming a wall
    // that isn't there.
    let who = principal();
    let policy = Policy::from_toml(&format!(
        "[principal.\"{who}\"]\nopaque='allow'\nauto_apply='in-grant'\n\n\
             [principal.\"{who}\".fs]\nread=[\"/usr/**\"]\n"
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    let (mut client, mut reader, thread) = spawn(&kernel);
    let r = attach(&mut client, &mut reader).result.unwrap();
    let status = EnforcementStatus::detect();
    let backend_present = matches!(
        status.available_tier,
        EnforcementTier::A | EnforcementTier::C
    );
    assert_eq!(r["caps_enforced"], backend_present);
    assert_eq!(r["caps"]["enforced"], backend_present);
    assert_eq!(r["caps"]["tier"], tier_letter(status.available_tier));
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

// -----------------------------------------------------------------------
// Plan reversibility (site/content/internals/kernel-protocol.md) — derived, not hardcoded.
// -----------------------------------------------------------------------

#[test]
fn plan_reversibility_is_derived_from_effects() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    // shoal's `rm` is trash-based (journaled, `apply` fully recovers it)
    // — NOT an opaque, unrecoverable delete, so a plan for it must not
    // be flatly reported "irreversible" (bug: a cold agent driving the
    // MCP surface found this misleading).
    let del = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"rm doomed.txt","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(
        del["reversibility"], "reversible",
        "shoal's rm trashes (journaled undo) rather than deleting outright: {del}"
    );
    let pure = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"1 + 2","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(pure["reversibility"], "reversible");
    // An opaque external command is a DIFFERENT effect (`Effect::Opaque`,
    // never `Effect::FsDelete`) and must stay irreversible even when its
    // source text also happens to say "rm -rf" — the kernel cannot see
    // inside a `sh{}` block's effects at all, so it can never mistake
    // this for shoal's own trash-based delete.
    let opaque = call(
        &mut client,
        &mut reader,
        4,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(opaque["reversibility"], "irreversible");
    let opaque_rm = call(
        &mut client,
        &mut reader,
        5,
        "exec",
        json!({"src":"sh { rm -rf doomed.txt }","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(
        opaque_rm["reversibility"], "irreversible",
        "an opaque external rm -rf must never be reported reversible: {opaque_rm}"
    );
    // `mv`'s source-clearing "delete" is also journaled/undoable
    // (MoveBack/RestoreBytes), so it gets the same treatment as `rm`.
    let moved = call(
        &mut client,
        &mut reader,
        6,
        "exec",
        json!({"src":"mv a.txt b.txt","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(moved["reversibility"], "reversible", "{moved}");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

// -----------------------------------------------------------------------
// Value-position multi-statement + error-still-yields-a-ref behavior; see
// `site/content/internals/language-conformance-contract.md`.
// -----------------------------------------------------------------------

#[test]
fn value_position_captures_final_expr_of_multi_statement_src() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    // Two statements; the final bare command must be *captured* (ok:false),
    // not raised — the previous single-statement-only special case raised.
    let r = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"let a = 1\nsh { exit 4 }","position":"value"}),
    );
    let value = r
        .result
        .expect("value position must not raise")
        .get("value")
        .cloned()
        .unwrap();
    assert_eq!(value["$"], "outcome");
    assert_eq!(value["ok"], false);
    assert_eq!(value["status"], 4);
    // And a binding from the first statement is visible to the last.
    let r2 = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"let x = 10\nx + 5","position":"value"}),
    );
    assert_eq!(r2.result.unwrap()["value"], json!({"$":"int","v":15}));
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn raised_error_still_yields_an_inspectable_transcript_ref() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    // A genuine raise (statement position, failed command).
    let raised = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { exit 5 }","position":"stmt"}),
    );
    let err = raised.error.expect("must raise");
    assert_eq!(err.code, RAISED);
    let data = err.data.unwrap();
    let value_ref = data["ref"]
        .as_str()
        .expect("error carries a transcript ref");
    // The agent can shoal_get that ref and read the structured error.
    let got = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref}),
    );
    assert_eq!(got.result.unwrap()["value"]["$"], "error");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

// -----------------------------------------------------------------------
// cap.request effect scoping (site/content/internals/kernel-protocol.md), complete/explain (site/content/internals/kernel-protocol.md).
// -----------------------------------------------------------------------

#[test]
fn cap_request_scopes_the_grant_to_requested_effects() {
    let kernel = Kernel::new();
    // This test drives the requester and approver over ONE connection, so
    // it opts into self-acknowledgement (HR-D3); it exercises scope
    // narrowing, not the separation-of-duties gate (covered separately).
    kernel.set_allow_self_ack(true);
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let plan = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan"}),
    )
    .result
    .unwrap();
    let plan_ref = plan["plan_ref"].as_str().unwrap().to_owned();
    // Scoped to fs.write only — the plan's opaque effect isn't covered, so
    // the grant stays pending (never silently widens).
    let scoped = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref, "effects":["fs.write"]}),
    )
    .result
    .unwrap();
    assert_eq!(scoped["grant"], "approval_pending", "{scoped}");
    // Scoped to the actual effect kind — now it grants.
    let ok = call(
        &mut client,
        &mut reader,
        4,
        "cap.request",
        json!({"plan_ref": plan_ref, "effects":["opaque"]}),
    )
    .result
    .unwrap();
    assert_eq!(ok["grant"], "approved");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// HR-D3: with self-acknowledgement OFF (the default), a plan's requester
/// cannot approve its own plan. `cap.request` from the same principal that
/// derived the plan is rejected with `LEASH_DENIED`, and the plan stays
/// unapproved (`plan.apply` still fails) — approval is a genuine
/// second-party boundary, not a rubber stamp the requester applies itself.
#[test]
fn cap_request_default_denies_self_approval() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
        principal()
    ))
    .unwrap();
    // No `set_allow_self_ack` — self-ack defaults OFF.
    let kernel = Kernel::with_policy(policy);
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let plan = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan"}),
    )
    .result
    .unwrap();
    let plan_ref = plan["plan_ref"].as_str().unwrap().to_owned();
    let denied = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref, "effects": []}),
    );
    let err = denied
        .error
        .expect("a requester approving its own plan must be denied by default");
    assert_eq!(
        err.code, LEASH_DENIED,
        "self-approval must be LEASH_DENIED: {err:?}"
    );
    assert!(
        err.message.contains("self-approval"),
        "the denial names the reason: {}",
        err.message
    );
    // The plan is still unapproved: plan.apply refuses it.
    let apply = call(
        &mut client,
        &mut reader,
        4,
        "plan.apply",
        json!({"plan_ref": plan_ref}),
    );
    assert!(
        apply.error.is_some(),
        "a plan that was never validly approved must not apply"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn cap_request_fails_closed_when_the_grant_audit_cannot_be_written() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
        principal()
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    kernel.set_allow_self_ack(true);
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let planned = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
    )
    .result
    .unwrap();
    let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();

    kernel.fail_approval_audit.store(true, Ordering::SeqCst);
    let failed = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref}),
    );
    assert_eq!(
        failed.error.expect("audit failure must reject grant").code,
        INTERNAL_ERROR
    );
    let apply = call(
        &mut client,
        &mut reader,
        4,
        "plan.apply",
        json!({"plan_ref": plan_ref}),
    );
    assert_eq!(
        apply
            .error
            .expect("failed audit must leave plan pending")
            .code,
        APPROVAL_REQUIRED
    );

    kernel.fail_approval_audit.store(false, Ordering::SeqCst);
    let granted = call(
        &mut client,
        &mut reader,
        5,
        "cap.request",
        json!({"plan_ref": plan_ref}),
    );
    assert!(
        granted.error.is_none(),
        "a later durable grant succeeds: {granted:?}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn concurrent_plan_apply_consumes_an_explicit_approval_exactly_once() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='ask'\nauto_apply='never'\n",
        principal()
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    kernel.set_allow_self_ack(true);

    let (mut owner, mut owner_reader, owner_server) = spawn(&kernel);
    attach(&mut owner, &mut owner_reader);
    let plan_ref = call(
        &mut owner,
        &mut owner_reader,
        2,
        "exec",
        json!({"src":"sh { echo one-shot }","mode":"plan","position":"stmt"}),
    )
    .result
    .unwrap()["plan_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        call(
            &mut owner,
            &mut owner_reader,
            3,
            "cap.request",
            json!({"plan_ref": plan_ref}),
        )
        .error
        .is_none()
    );

    let (mut a, mut ar, a_server) = spawn(&kernel);
    attach(&mut a, &mut ar);
    let (mut b, mut br, b_server) = spawn(&kernel);
    attach(&mut b, &mut br);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let apply = |mut stream: UnixStream,
                 mut reader: BufReader<UnixStream>,
                 barrier: Arc<std::sync::Barrier>,
                 plan_ref: String| {
        std::thread::spawn(move || {
            barrier.wait();
            call(
                &mut stream,
                &mut reader,
                4,
                "plan.apply",
                json!({"plan_ref": plan_ref}),
            )
        })
    };
    let first = apply(a, ar, barrier.clone(), plan_ref.clone());
    let second = apply(b, br, barrier.clone(), plan_ref);
    barrier.wait();
    let responses = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(
        responses
            .iter()
            .filter(|response| response.error.is_none())
            .count(),
        1,
        "exactly one concurrent apply may execute: {responses:?}"
    );
    assert_eq!(
        responses
            .iter()
            .filter_map(|response| response.error.as_ref())
            .filter(|error| error.code == LEASH_DENIED)
            .count(),
        1,
        "the loser is rejected as an already-consumed approval: {responses:?}"
    );

    drop(owner);
    drop(owner_reader);
    owner_server.join().unwrap();
    a_server.join().unwrap();
    b_server.join().unwrap();
}

#[test]
fn distinct_agent_without_approver_role_is_denied_but_local_human_can_approve() {
    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    let (requester_token, _) = tokens
        .create("agent:requester".into(), "agent".into(), vec![], None)
        .unwrap();
    let (unauthorized_token, _) = tokens
        .create("agent:other".into(), "agent".into(), vec![], None)
        .unwrap();
    drop(tokens);
    let policy =
        Policy::from_toml("[principal.\"agent:requester\"]\nopaque='ask'\nauto_apply='never'\n")
            .unwrap();
    let kernel = Kernel::open_with_policy(dir.path(), policy).unwrap();

    let (mut requester, mut requester_reader, requester_thread) = spawn(&kernel);
    call(
        &mut requester,
        &mut requester_reader,
        1,
        "session.attach",
        json!({"token":requester_token,"client":{"kind":"agent","tty":false}}),
    );
    let plan_ref = call(
        &mut requester,
        &mut requester_reader,
        2,
        "exec",
        json!({"src":"sh { echo guarded }","mode":"plan"}),
    )
    .result
    .unwrap()["plan_ref"]
        .as_str()
        .unwrap()
        .to_owned();

    let (mut other, mut other_reader, other_thread) = spawn(&kernel);
    call(
        &mut other,
        &mut other_reader,
        1,
        "session.attach",
        json!({"token":unauthorized_token,"client":{"kind":"agent","tty":false}}),
    );
    let denied = call(
        &mut other,
        &mut other_reader,
        2,
        "cap.request",
        json!({"plan_ref":plan_ref}),
    )
    .error
    .expect("a distinct ordinary agent is not automatically an approver");
    assert_eq!(denied.code, LEASH_DENIED);
    assert!(denied.message.contains("not authorized"), "{denied:?}");

    let (mut human, mut human_reader, human_thread) = spawn(&kernel);
    let human_attach = call(
        &mut human,
        &mut human_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"human","tty":false}}),
    )
    .result
    .unwrap();
    let human_principal = human_attach["principal"].as_str().unwrap().to_owned();
    let approved = call(
        &mut human,
        &mut human_reader,
        2,
        "cap.request",
        json!({"plan_ref":plan_ref}),
    )
    .result
    .expect("the trusted local-human path remains usable");
    assert_eq!(approved["approver"], human_principal);

    drop(requester);
    drop(requester_reader);
    requester_thread.join().unwrap();
    drop(other);
    drop(other_reader);
    other_thread.join().unwrap();
    drop(human);
    drop(human_reader);
    human_thread.join().unwrap();
}

/// HR-D2/HR-D3 happy path: a DISTINCT approver (a second bearer principal)
/// may approve a requester's plan, the grant reports both identities, the
/// approval record binds requester→approver→scope on the plan, and once the
/// requester applies the approved plan the record names the consuming
/// execution's journal entry. Two real bearer principals over two
/// connections — the separation-of-duties boundary working as intended.
#[test]
fn cap_request_cross_principal_approval_binds_the_full_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    let (alpha_tok, _) = tokens
        .create("agent:alpha".into(), "agent".into(), vec![], None)
        .unwrap();
    let (beta_tok, _) = tokens
        .create("agent:beta".into(), "supervisor".into(), vec![], None)
        .unwrap();
    drop(tokens);
    // Both principals must ask for opaque effects (so the plan lands
    // approval_required), and alpha's effects must not be a hard Deny (ask,
    // not deny) so approval can lift it.
    let policy = Policy::from_toml(
        "[principal.\"agent:alpha\"]\nopaque='ask'\nauto_apply='never'\n\n\
             [principal.\"agent:beta\"]\nopaque='ask'\nauto_apply='never'\n",
    )
    .unwrap();
    let kernel = Kernel::open_with_policy(dir.path(), policy).unwrap();
    // self-ack stays OFF: this is a genuine two-principal approval.

    // Requester alpha derives an approval-required plan.
    let (mut a, mut a_reader, a_thread) = spawn(&kernel);
    call(
        &mut a,
        &mut a_reader,
        1,
        "session.attach",
        json!({"token":alpha_tok,"session":"pair","client":{"kind":"agent","tty":false}}),
    );
    let planned = call(
        &mut a,
        &mut a_reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
    )
    .result
    .unwrap();
    assert_eq!(planned["verdict"], "approval_required", "{planned}");
    let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();

    // Approver beta (a distinct principal) approves it.
    let (mut b, mut b_reader, b_thread) = spawn(&kernel);
    call(
        &mut b,
        &mut b_reader,
        1,
        "session.attach",
        json!({"token":beta_tok,"client":{"kind":"agent","tty":false}}),
    );
    let grant = call(
        &mut b,
        &mut b_reader,
        2,
        "cap.request",
        json!({"plan_ref": plan_ref, "effects": []}),
    )
    .result
    .expect("a distinct approver may approve");
    assert_eq!(grant["grant"], "approved", "{grant}");
    assert_eq!(grant["requester"], "agent:alpha", "{grant}");
    assert_eq!(grant["approver"], "agent:beta", "{grant}");

    // Storing an identical plan after approval creates another object; it
    // cannot replace or steal the first object's approval.
    let replacement = call(
        &mut a,
        &mut a_reader,
        3,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan","position":"stmt"}),
    )
    .result
    .unwrap();
    let replacement_ref = replacement["plan_ref"].as_str().unwrap();
    assert_ne!(replacement_ref, plan_ref);
    let replacement_apply = call(
        &mut a,
        &mut a_reader,
        4,
        "plan.apply",
        json!({"plan_ref": replacement_ref}),
    );
    assert_eq!(
        replacement_apply.error.unwrap().code,
        APPROVAL_REQUIRED,
        "the later identical object must not inherit the first approval"
    );

    // The requester applies the now-approved plan; it runs.
    let applied = call(
        &mut a,
        &mut a_reader,
        5,
        "plan.apply",
        json!({"plan_ref": plan_ref}),
    )
    .result
    .expect("the requester applies its approved plan");
    assert_eq!(applied["value"]["ok"], true, "{applied}");

    // plan.get surfaces the full binding, including the consuming execution.
    let got = call(
        &mut a,
        &mut a_reader,
        6,
        "plan.get",
        json!({"plan_ref": plan_ref}),
    )
    .result
    .unwrap();
    let approval = &got["approval"];
    assert_eq!(approval["requester"], "agent:alpha", "{got}");
    assert_eq!(approval["approver"], "agent:beta", "{got}");
    assert_eq!(approval["plan_ref"], plan_ref, "{got}");
    assert_eq!(approval["session"], "pair", "{got}");
    assert_eq!(approval["plan_hash"].as_str().unwrap().len(), 64, "{got}");
    assert_eq!(approval["source_hash"].as_str().unwrap().len(), 64, "{got}");
    assert!(approval["grant_audit_id"].is_i64(), "{got}");
    assert!(
        approval["consumed_by"].is_i64(),
        "the approval names the journal entry that consumed it: {got}"
    );

    // An explicit approval is single-use. A sequential replay through the
    // public plan.apply path must be rejected before any effect runs.
    let replay = call(
        &mut a,
        &mut a_reader,
        7,
        "plan.apply",
        json!({"plan_ref": plan_ref}),
    );
    assert_eq!(
        replay.error.expect("approval replay must fail").code,
        LEASH_DENIED
    );

    // The durable execution row itself links back to the completed grant
    // audit row. This survives loss of the in-memory plan map.
    let history = call(
        &mut a,
        &mut a_reader,
        8,
        "journal.query",
        json!({"limit": 100}),
    )
    .result
    .unwrap();
    let entries = history.as_array().unwrap();
    let consumed_by = approval["consumed_by"].as_i64().unwrap();
    let execution = entries
        .iter()
        .find(|entry| entry["id"] == consumed_by)
        .expect("consuming execution is durable");
    let consumption = execution["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|effect| effect["kind"] == "approval.consume")
        .expect("execution embeds its approval linkage");
    assert_eq!(consumption["plan_ref"], plan_ref);
    assert_eq!(consumption["grant_audit_id"], approval["grant_audit_id"]);
    let grant_id = approval["grant_audit_id"].as_i64().unwrap();
    let grant_row = entries
        .iter()
        .find(|entry| entry["id"] == grant_id)
        .expect("grant audit row is durable");
    assert_eq!(grant_row["ok"], true);
    assert!(
        grant_row["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|effect| effect["kind"] == "approval")
    );

    drop(a);
    drop(a_reader);
    a_thread.join().unwrap();
    drop(b);
    drop(b_reader);
    b_thread.join().unwrap();
}

#[test]
fn complete_and_explain_methods() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let c = call(
        &mut client,
        &mut reader,
        2,
        "complete",
        json!({"src":"le","cursor":2}),
    )
    .result
    .unwrap();
    let candidates = c["candidates"].as_array().unwrap();
    assert!(candidates.iter().any(|v| v == "let"));
    assert!(
        candidates
            .iter()
            .all(|v| v.as_str().unwrap().starts_with("le")),
        "candidates must be filtered by the partial word"
    );
    let ex = call(
        &mut client,
        &mut reader,
        3,
        "explain",
        json!({"src":"rm gone.txt"}),
    )
    .result
    .unwrap();
    // shoal's `rm` trashes rather than deleting outright (see
    // `plan_reversibility_is_derived_from_effects`), so `explain` must
    // agree with `shoal_plan`'s answer here.
    assert_eq!(ex["reversibility"], "reversible");
    assert!(ex["ast"].is_object() || ex["ast"].is_array() || ex["ast"]["stmts"].is_array());
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn journal_until_and_effects_filters() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    call(&mut client, &mut reader, 2, "exec", json!({"src":"1 + 2"}));
    call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"rm gone.txt","position":"value"}),
    );
    // effects filter: only entries whose effect set mentions fs.delete.
    let deletes = call(
        &mut client,
        &mut reader,
        4,
        "journal.query",
        json!({"effects":["fs.delete"],"limit":50}),
    )
    .result
    .unwrap();
    let deletes = deletes.as_array().unwrap();
    assert!(!deletes.is_empty());
    assert!(
        deletes
            .iter()
            .all(|e| e["src"].as_str().unwrap().starts_with("rm")),
        "effects filter must keep only fs.delete entries: {deletes:?}"
    );
    // until in the far past matches nothing.
    let none = call(
        &mut client,
        &mut reader,
        5,
        "journal.query",
        json!({"until": 1, "limit":50}),
    )
    .result
    .unwrap();
    assert_eq!(none.as_array().unwrap().len(), 0);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn background_exec_returns_task_and_events_channel() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let bg = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { sleep 0.05 }","background":true}),
    )
    .result
    .unwrap();
    assert!(
        bg["task"].is_string(),
        "background exec returns a task ref: {bg}"
    );
    // site/content/internals/kernel-protocol.md: the events channel is `task.{bare id}` (e.g.
    // `task.7`), NOT `task.{full ref}` — the task ref itself is already
    // `task:7`, so naively prefixing it with `task.` doubles up into
    // `task.task:7`, which no `events.read`/`resources/subscribe` caller
    // can ever match against the real `task.{id}` channel.
    let task_ref = bg["task"].as_str().unwrap();
    let bare_id = task_ref.strip_prefix("task:").unwrap();
    assert_eq!(bg["events"], format!("task.{bare_id}"));
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// site/content/internals/roadmap-and-priorities.md: `task.resume` exists alongside `task.suspend`,
/// wired the same honest way — never a silent no-op, always a clear
/// error until a task's process handle is actually reachable here.
#[test]
fn task_resume_wire_method_is_honest_and_symmetric_with_suspend() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let bg = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { sleep 0.05 }","background":true}),
    )
    .result
    .unwrap();
    let task = bg["task"].clone();

    let resume = call(
        &mut client,
        &mut reader,
        3,
        "task.resume",
        json!({"task": task}),
    );
    let error = resume.error.expect("task.resume is not yet implemented");
    assert_eq!(error.code, TASK_CONTROL_UNAVAILABLE);
    assert!(
        error.message.contains("resume"),
        "message: {}",
        error.message
    );

    // Same shape as `task.suspend` for the same task.
    let suspend = call(
        &mut client,
        &mut reader,
        4,
        "task.suspend",
        json!({"task": task}),
    );
    assert_eq!(suspend.error.unwrap().code, TASK_CONTROL_UNAVAILABLE);

    // An unknown task ref is rejected before the honest-stub error, for
    // both methods.
    let unknown = json!({"task": "task:999999"});
    assert_eq!(
        call(&mut client, &mut reader, 5, "task.resume", unknown.clone())
            .error
            .unwrap()
            .code,
        UNKNOWN_TASK
    );
    assert_eq!(
        call(&mut client, &mut reader, 6, "task.suspend", unknown)
            .error
            .unwrap()
            .code,
        UNKNOWN_TASK
    );

    call(
        &mut client,
        &mut reader,
        7,
        "task.cancel",
        json!({"task": task}),
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn timeout_converts_a_slow_run_to_a_task() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    // A 30s command with a 50ms budget must come back as a task ref, never
    // block the caller's context.
    let r = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { sleep 30 }","timeout_ms":50}),
    )
    .result
    .unwrap();
    assert!(r["task"].is_string(), "a timed-out run yields a task: {r}");
    assert_eq!(r["timed_out"], true);
    // Cancel the still-running task so its `sleep 30` child doesn't linger
    // holding the test's output pipe open.
    call(
        &mut client,
        &mut reader,
        10,
        "task.cancel",
        json!({"task": r["task"]}),
    );
    // A fast command under budget returns inline.
    let fast = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"1 + 2","timeout_ms":5000}),
    )
    .result
    .unwrap();
    assert_eq!(fast["value"], json!({"$":"int","v":3}));
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

// -----------------------------------------------------------------------
// Three agent-wire honesty fixes: outcome span (site/content/internals/roadmap-and-priorities.md #4), cap.request
// enforced (site/content/internals/roadmap-and-priorities.md #5), ANSI stripped from render on the headless/MCP
// path (cold-agent field test finding).
// -----------------------------------------------------------------------

fn bare_outcome(ok: bool, stdout: &[u8]) -> Value {
    Value::Outcome(Arc::new(shoal_value::OutcomeVal {
        status: Some(if ok { 0 } else { 1 }),
        signal: None,
        ok,
        stdout: Arc::new(stdout.to_vec()),
        stdout_ref: None,
        stderr: Arc::new(Vec::new()),
        dur_ns: 1_000,
        pid: 42,
        cmd: "echo hi".into(),
        parsed: None,
        streamed: false,
        // Genuinely spanless — mirrors a builtin-wrapped or
        // journal-reconstructed outcome, exercising the honest-omission arm.
        span: None,
    }))
}

/// An outcome that DOES carry a source span (as the command spawn path now
/// stamps): `bare_outcome` plus `OutcomeVal::with_span`.
fn spanned_outcome(ok: bool, stdout: &[u8], span: shoal_ast::Span) -> Value {
    let Value::Outcome(o) = bare_outcome(ok, stdout) else {
        unreachable!()
    };
    let mut inner = Arc::try_unwrap(o).unwrap();
    inner.span = Some(span);
    Value::Outcome(Arc::new(inner))
}

/// Fix 1 (site/content/internals/roadmap-and-priorities.md #4): `OutcomeVal` now carries `Option<Span>`, stamped on
/// the command spawn path with the same span the sibling error path uses
/// (`shoal-eval/src/command.rs`). When a span is present it must reach the
/// wire under the `span` key with the same `{start,end}` shape `ErrorVal`'s
/// span already uses. This pins the populated direction.
#[test]
fn outcome_span_reaches_the_wire_when_stamped() {
    let wire = wire_value(&spanned_outcome(true, b"hi\n", shoal_ast::Span::new(3, 9)));
    let json = serde_json::to_value(&wire).unwrap();
    assert_eq!(
        json.get("span"),
        Some(&serde_json::json!({"start": 3, "end": 9})),
        "a stamped span must travel on the wire: {json}"
    );
}

/// The populated span survives the elision path too (the outer `Outcome`
/// wrapper carries its fields through even when `.out` is elided).
#[test]
fn outcome_span_reaches_the_wire_through_elision_too() {
    let budget = ElideBudget::default();
    let wire = elide_wire_value(
        &spanned_outcome(true, b"hi\n", shoal_ast::Span::new(3, 9)),
        "shoal://out/1",
        &budget,
    );
    let json = serde_json::to_value(&wire).unwrap();
    assert_eq!(
        json.get("span"),
        Some(&serde_json::json!({"start": 3, "end": 9})),
        "{json}"
    );
}

/// The honest-omission contract still holds when the span is *genuinely*
/// absent (builtin-wrapped / journal-reconstructed outcomes): the wire
/// OMITS the field entirely (`skip_serializing_if`), never null-but-present
/// and never a fabricated span.
#[test]
fn outcome_span_is_honestly_omitted_when_absent() {
    let wire = wire_value(&bare_outcome(true, b"hi\n"));
    let json = serde_json::to_value(&wire).unwrap();
    assert!(
        json.get("span").is_none(),
        "span must be honestly omitted when absent, not null-but-present: {json}"
    );
}

/// Same honest-omission contract holds through the elision path (the
/// outer `Outcome` wrapper survives elision of a big `.out` unchanged).
#[test]
fn outcome_span_is_honestly_omitted_through_elision_too() {
    let budget = ElideBudget::default();
    let wire = elide_wire_value(&bare_outcome(true, b"hi\n"), "shoal://out/1", &budget);
    let json = serde_json::to_value(&wire).unwrap();
    assert!(json.get("span").is_none(), "{json}");
}

/// End-to-end: an outcome produced by a REAL command exec through the
/// kernel carries a non-null `{start,end}` span on the wire — proof the
/// eval-side stamp (`command.rs`) reaches the wire boundary, not just a
/// hand-built `OutcomeVal`. `sh { echo hi }` spawns an external command, so
/// it flows through `run_argv`'s spawn path (which stamps the span), not
/// the builtin path (which leaves it `None`).
#[test]
fn real_command_exec_outcome_carries_a_span_on_the_wire() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }"}),
    );
    let value = exec.result.unwrap()["value"].clone();
    let span = &value["span"];
    assert!(
        span.is_object(),
        "real command exec must carry a span object on the wire: {value}"
    );
    let start = span["start"].as_u64().expect("span.start");
    let end = span["end"].as_u64().expect("span.end");
    assert!(
        end > start,
        "span must cover the non-empty invocation source: {span}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// Fix 2 (site/content/internals/roadmap-and-priorities.md #5): `cap.request`'s grant response must report the
/// SAME enforcement truth `session.attach`'s `caps_enforced` already
/// does for this principal — never a hardcoded `false`. Mirrors
/// `attach_enforces_only_for_a_scoped_principal_with_a_real_backend`'s
/// scoped-principal setup so both endpoints are asked about the same
/// principal and must agree.
#[test]
fn cap_request_reports_the_same_enforcement_truth_attach_does() {
    let who = principal();
    let policy = Policy::from_toml(&format!(
        "[principal.\"{who}\"]\nopaque='allow'\nauto_apply='in-grant'\n\n\
             [principal.\"{who}\".fs]\nread=[\"/usr/**\"]\n"
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    // One-connection request→approve: opt into self-ack (HR-D3). This test
    // is about the enforcement-truth field, not the separation gate.
    kernel.set_allow_self_ack(true);
    let (mut client, mut reader, thread) = spawn(&kernel);
    let attach_result = attach(&mut client, &mut reader).result.unwrap();
    let status = EnforcementStatus::detect();
    let backend_present = matches!(
        status.available_tier,
        EnforcementTier::A | EnforcementTier::C
    );
    assert_eq!(attach_result["caps_enforced"], backend_present);
    let planned = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan"}),
    )
    .result
    .unwrap();
    let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();
    let grant = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref, "effects": []}),
    )
    .result
    .unwrap();
    assert_eq!(grant["grant"], "approved", "{grant}");
    assert_eq!(
        grant["enforced"], backend_present,
        "cap.request must report the SAME enforcement truth attach did: {grant}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// Baseline: the default-permissive human principal gets `enforced:false`
/// from BOTH endpoints — a mismatch in this direction would be just as
/// dishonest as under-reporting a real backend.
#[test]
fn cap_request_reports_false_for_the_default_permissive_principal() {
    let kernel = Kernel::new();
    // Single-connection self-approval to reach the grant response under
    // test: opt into self-ack (HR-D3).
    kernel.set_allow_self_ack(true);
    let (mut client, mut reader, thread) = spawn(&kernel);
    let attach_result = attach(&mut client, &mut reader).result.unwrap();
    assert_eq!(attach_result["caps_enforced"], false);
    let planned = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { echo hi }","mode":"plan"}),
    )
    .result
    .unwrap();
    let plan_ref = planned["plan_ref"].as_str().unwrap().to_owned();
    let grant = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref, "effects": []}),
    )
    .result
    .unwrap();
    assert_eq!(grant["grant"], "approved", "{grant}");
    assert_eq!(grant["enforced"], false, "{grant}");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

// -- Fix 3: ANSI stripped from render on the headless/MCP wire ---------

#[test]
fn strip_ansi_removes_sgr_color_codes() {
    assert_eq!(strip_ansi("\x1b[32mhi\x1b[0m"), "hi");
}

#[test]
fn strip_ansi_removes_non_sgr_csi_sequences_too() {
    // Cursor-movement/erase CSI (final bytes `H`/`K`), not just SGR
    // color (`m`) — the stripper covers the general CSI grammar.
    assert_eq!(strip_ansi("\x1b[2K\x1b[1;1Hhello"), "hello");
}

#[test]
fn strip_ansi_is_a_no_op_on_plain_text() {
    assert_eq!(
        strip_ansi("plain text, no escapes here"),
        "plain text, no escapes here"
    );
}

/// Fix 3: on the headless/MCP path (the attaching client did not declare
/// a real tty — what `shoal-mcp` and every shipped client attach with
/// today), the kernel strips ANSI from the human-facing `render` string
/// before it reaches the wire; a genuine interactive (`tty:true`) client
/// keeps the color. The structured `value` field is untouched either
/// way (not asserted on here — it never carried render-layer ANSI to
/// begin with).
#[test]
fn headless_client_gets_ansi_stripped_render_but_a_tty_client_keeps_color() {
    // Sanity first: render_block genuinely emits ANSI for a table (the
    // bold header / dim separator are unconditional, regardless of cell
    // content) — otherwise this test would vacuously pass no matter
    // what the fix does.
    let mut row = shoal_value::Record::new();
    row.insert("n".to_string(), Value::Int(1));
    let raw = shoal_value::render::render_block(&Value::Table(vec![row]), 80);
    assert!(
        raw.contains('\u{1b}'),
        "sanity: render_block must emit ANSI for a table: {raw:?}"
    );

    // Headless (tty:false): the kernel strips it.
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    )
    .result
    .unwrap();
    let render = exec["render"].as_str().unwrap();
    assert!(
        !render.contains('\u{1b}'),
        "headless render must be ANSI-free: {render:?}"
    );
    assert!(
        render.contains('1'),
        "content must still be present: {render:?}"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();

    // A genuine interactive client (tty:true) keeps its color — the fix
    // must not blanket-strip regardless of what the client declared.
    let kernel2 = Kernel::new();
    let (mut client2, mut reader2, thread2) = spawn(&kernel2);
    call(
        &mut client2,
        &mut reader2,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"human","tty":true}}),
    );
    let exec2 = call(
        &mut client2,
        &mut reader2,
        2,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    )
    .result
    .unwrap();
    let render2 = exec2["render"].as_str().unwrap();
    assert!(
        render2.contains('\u{1b}'),
        "a real tty client must keep its color: {render2:?}"
    );
    drop(client2);
    drop(reader2);
    thread2.join().unwrap();
}

/// The same headless stripping applies to `value.get`'s `format=render`
/// path (`handlers_value.rs`), not just `exec`'s inline render.
#[test]
fn headless_value_get_format_render_is_also_ansi_stripped() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"csv.parse(\"n\\n1\\n2\\n3\")"}),
    )
    .result
    .unwrap();
    let value_ref = exec["ref"].as_str().unwrap().to_owned();
    let rendered = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref, "format": "render"}),
    )
    .result
    .unwrap();
    let render = rendered["render"].as_str().unwrap();
    assert!(
        !render.contains('\u{1b}'),
        "headless format=render must be ANSI-free: {render:?}"
    );
    assert!(render.contains('1'), "content preserved: {render:?}");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}
