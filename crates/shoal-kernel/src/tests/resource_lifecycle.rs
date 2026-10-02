use super::*;

#[test]
fn task_refs_are_hidden_from_another_principal_with_the_same_session_name() {
    let kernel = Kernel::new();
    let alpha = kernel.session("shared-tasks", "agent:alpha").unwrap();
    let beta = kernel.session("shared-tasks", "agent:beta").unwrap();
    let task_ref = Ref::new("task", 4242);
    let alpha_owner = alpha.key.owner();
    let alpha_session_id = alpha.id.clone();
    kernel.runtime.tasks.insert(Arc::new(TaskEntry {
        task: task_ref.clone(),
        owner: alpha_owner,
        session_id: alpha_session_id,
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
    let mut attached = Some(Attachment {
        session: beta,
        principal: "agent:beta".into(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
        connection_trust: ConnectionTrust::EmbeddedHuman,
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
        connection_trust: ConnectionTrust::EmbeddedHuman,
    });
    let mut beta_attached = Some(Attachment {
        session: beta,
        principal: "agent:beta".into(),
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
        connection_trust: ConnectionTrust::EmbeddedHuman,
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
fn poisoned_plan_registry_fails_closed_without_reentering_its_map() {
    let plans = PlanRegistry::new();
    plans.poison_for_test();

    for _ in 0..2 {
        let error = plans
            .transaction(|entries| entries.len())
            .expect_err("a poisoned plan registry must remain quarantined");
        assert_eq!(error.code, INTERNAL_ERROR);
        assert_eq!(error.data.unwrap()["subsystem"], "plans");
    }
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
            exit_code: None,
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
        deadline_ms: None,
        deadline_exceeded: AtomicBool::new(false),
    });
    let poisoner = task.clone();
    let thread = std::thread::spawn(move || {
        let _inner = poisoner
            .inner
            .lock()
            .expect("test lock should not be poisoned");
        panic!("inject task-record poison");
    });
    assert!(thread.join().is_err());
    assert!(task.inner.is_poisoned());

    task.fail_worker_panic();
    assert!(!task.inner.is_poisoned());
    let inner = task.inner.lock().expect("test lock should not be poisoned");
    assert_eq!(inner.state, "failed");
    assert!(inner.finished_ns.is_some());
    assert!(inner.result_ref.is_none());
    assert_eq!(inner.error.as_ref().unwrap().code, INTERNAL_ERROR);
    assert!(inner.active_slot.is_none());
    drop(inner);
    assert!(
        task.session_lease
            .lock()
            .expect("test lock should not be poisoned")
            .is_none()
    );
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
            exit_code: None,
            error: None,
            active_slot: None,
        }),
        done: Condvar::new(),
        cancel: shoal_exec::CancelToken::new(),
        cancel_requested: AtomicBool::new(false),
        deadline_ms: None,
        deadline_exceeded: AtomicBool::new(false),
    });
    assert_eq!(Arc::strong_count(&session), baseline + 1);
    task.release_session_lease();
    assert_eq!(Arc::strong_count(&session), baseline);
    kernel.runtime.tasks.insert(task);
    kernel.reap_finished_tasks(&owner);
    assert!(!kernel.runtime.tasks.contains(&task_ref));
}

#[test]
fn active_task_quota_releases_when_a_task_becomes_terminal() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_tasks_per_session: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
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
    let kernel = Kernel::builder()
        .limits(Limits {
            max_ptys_per_session: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
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
        connection_trust: ConnectionTrust::EmbeddedHuman,
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
fn poisoned_pty_session_quarantines_only_that_entry_and_releases_quota() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_ptys_per_session: 2,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let actor = principal();
    let session = kernel.session("poisoned-pty-session", &actor).unwrap();
    let owner = session.key.owner();
    let mut attached = Some(Attachment {
        session,
        principal: actor,
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
        connection_trust: ConnectionTrust::EmbeddedHuman,
    });
    let opened = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut attached)
        .unwrap();
    let pty_ref: Ref = serde_json::from_value(opened["pty_id"].clone()).unwrap();
    let poisoned_pid = opened["pid"].as_u64().unwrap() as libc::pid_t;
    let neighbor = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut attached)
        .expect("open healthy neighbor");
    let neighbor_ref: Ref = serde_json::from_value(neighbor["pty_id"].clone()).unwrap();
    let entry = kernel.runtime.ptys.get(&pty_ref).unwrap();
    let poisoner = entry.clone();
    assert!(
        std::thread::spawn(move || {
            let _session = poisoner
                .session
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject PTY session poison");
        })
        .join()
        .is_err()
    );

    let error = kernel
        .handle_pty_read(json!({"pty_id":pty_ref}), &mut attached)
        .err()
        .unwrap();
    assert_eq!(error.code, INTERNAL_ERROR);
    assert_eq!(error.data.as_ref().unwrap()["component"], "session");
    assert_eq!(error.data.unwrap()["scope"], "entry");
    assert!(kernel.runtime.ptys.get(&pty_ref).is_none());
    drop(entry);

    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    loop {
        // SAFETY: signal 0 only probes the pid and delivers no signal.
        let gone = unsafe { libc::kill(poisoned_pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "quarantined PTY child was not reaped"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    kernel
        .handle_pty_send(
            json!({"pty_id":neighbor_ref, "input":"healthy-neighbor\r"}),
            &mut attached,
        )
        .expect("poisoned entry must not disrupt its healthy neighbor");
    kernel
        .handle_pty_read(json!({"pty_id":neighbor_ref}), &mut attached)
        .expect("healthy neighbor remains readable");

    let replacement = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut attached)
        .expect("quarantined PTY must release exactly its quota slot");
    kernel
        .handle_pty_close(json!({"pty_id":replacement["pty_id"]}), &mut attached)
        .unwrap();
    kernel
        .handle_pty_close(json!({"pty_id":neighbor_ref}), &mut attached)
        .unwrap();
    assert!(!kernel.runtime.ptys.active_for(&owner));
}

#[test]
fn poisoned_pty_lifecycle_fails_typed_and_releases_on_entry_drop() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_ptys_per_session: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let actor = principal();
    let session = kernel.session("poisoned-pty-lifecycle", &actor).unwrap();
    let mut attached = Some(Attachment {
        session,
        principal: actor,
        can_approve: false,
        tty: false,
        cancel_epoch: None,
        bearer: None,
        security_epoch: ATTACH_SECURITY_EPOCH,
        connection_trust: ConnectionTrust::EmbeddedHuman,
    });
    let opened = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut attached)
        .unwrap();
    let pty_ref: Ref = serde_json::from_value(opened["pty_id"].clone()).unwrap();
    let entry = kernel.runtime.ptys.get(&pty_ref).unwrap();
    let poisoner = entry.clone();
    assert!(
        std::thread::spawn(move || {
            let _lifecycle = poisoner
                .lifecycle
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject PTY lifecycle poison");
        })
        .join()
        .is_err()
    );

    let error = kernel
        .handle_pty_close(json!({"pty_id":pty_ref}), &mut attached)
        .err()
        .unwrap();
    assert_eq!(error.code, INTERNAL_ERROR);
    assert_eq!(error.data.as_ref().unwrap()["component"], "lifecycle");
    assert_eq!(error.data.unwrap()["scope"], "entry");
    assert!(kernel.runtime.ptys.get(&pty_ref).is_none());
    drop(entry);

    let replacement = kernel
        .handle_pty_open(json!({"cmd":"cat"}), &mut attached)
        .expect("dropping a quarantined PTY must release its permit");
    kernel
        .handle_pty_close(json!({"pty_id":replacement["pty_id"]}), &mut attached)
        .unwrap();
}

#[test]
fn self_exited_pty_releases_active_and_session_leases_without_a_request() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_ptys_per_session: 1,
            ..Limits::default()
        })
        .build()
        .unwrap();
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
        connection_trust: ConnectionTrust::EmbeddedHuman,
    });
    let opened = kernel
        .handle_pty_open(json!({"cmd":"sh", "args":["-c", "exit 0"]}), &mut attached)
        .unwrap();
    let pty_ref: Ref = serde_json::from_value(opened["pty_id"].clone()).unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while kernel.runtime.ptys.active_for(&owner) {
        assert!(
            Instant::now() < deadline,
            "PTY watcher did not release quota"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let retained = kernel.runtime.ptys.get(&pty_ref).unwrap();
    let lifecycle = retained
        .lifecycle
        .lock()
        .expect("test lock should not be poisoned");
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
    let transcript = session
        .transcript
        .lock()
        .expect("test lock should not be poisoned");
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
    kernel
        .runtime
        .plans
        .transaction(|plans| {
            plans.get_mut(&plan_ref).unwrap().created_at =
                Instant::now() - PLAN_TTL - std::time::Duration::from_secs(1);
        })
        .unwrap();
    let apply = call(
        &mut client,
        &mut reader,
        4,
        "plan.apply",
        json!({"plan_ref": plan_ref}),
    );
    assert_eq!(apply.error.unwrap().code, UNKNOWN_PLAN);
    assert!(!kernel.runtime.plans.contains(&plan_ref));
    drop(client);
    drop(reader);
    server.join().unwrap();
}
