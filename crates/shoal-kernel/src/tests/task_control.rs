use super::*;

#[test]
fn suspend_resume_controls_process_backed_work_and_rejects_unknown_tasks() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let bg = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { sleep 30 }","background":true}),
    )
    .result
    .unwrap();
    let task = bg["task"].clone();

    let resume_before_suspend = call(
        &mut client,
        &mut reader,
        3,
        "task.resume",
        json!({"task": task}),
    );
    let error = resume_before_suspend
        .error
        .expect("a running task cannot be resumed");
    assert_eq!(error.code, TASK_CONTROL_UNAVAILABLE);
    assert!(error.message.contains("not suspended"));

    let deadline = Instant::now() + std::time::Duration::from_secs(2);
    let mut request_id = 4;
    let ready = loop {
        let record = call(
            &mut client,
            &mut reader,
            request_id,
            "task.get",
            json!({"task": task}),
        )
        .result
        .unwrap();
        request_id += 1;
        if record["controls"]["suspend"] == true {
            break record;
        }
        assert!(
            Instant::now() < deadline,
            "task record never advertised its child process group"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(ready["controls"]["cancel"], true);
    assert_eq!(ready["controls"]["resume"], false);
    assert_eq!(ready["controls"]["active_process_groups"], 1);

    let suspended = call(
        &mut client,
        &mut reader,
        request_id,
        "task.suspend",
        json!({"task": task}),
    )
    .result
    .expect("advertised suspension should succeed while the group stays active");
    request_id += 1;
    assert_eq!(suspended["suspended"], true);
    assert_eq!(suspended["process_groups"], 1);

    let snapshot = call(
        &mut client,
        &mut reader,
        request_id,
        "task.get",
        json!({"task": task}),
    )
    .result
    .unwrap();
    request_id += 1;
    assert_eq!(snapshot["state"], "suspended");
    assert_eq!(snapshot["controls"]["cancel"], true);
    assert_eq!(snapshot["controls"]["suspend"], false);
    assert_eq!(snapshot["controls"]["resume"], true);
    assert_eq!(snapshot["controls"]["active_process_groups"], 1);

    let resumed = call(
        &mut client,
        &mut reader,
        request_id,
        "task.resume",
        json!({"task": task}),
    )
    .result
    .unwrap();
    request_id += 1;
    assert_eq!(resumed["suspended"], false);
    assert_eq!(resumed["process_groups"], 1);

    // Ownership/identity checks still happen before process-control lookup.
    let unknown = json!({"task": "task:999999"});
    assert_eq!(
        call(
            &mut client,
            &mut reader,
            request_id,
            "task.resume",
            unknown.clone(),
        )
        .error
        .unwrap()
        .code,
        UNKNOWN_TASK
    );
    request_id += 1;
    assert_eq!(
        call(
            &mut client,
            &mut reader,
            request_id,
            "task.suspend",
            unknown,
        )
        .error
        .unwrap()
        .code,
        UNKNOWN_TASK
    );
    request_id += 1;

    let suspended_again = call(
        &mut client,
        &mut reader,
        request_id,
        "task.suspend",
        json!({"task": task}),
    )
    .result
    .unwrap();
    request_id += 1;
    assert_eq!(suspended_again["suspended"], true);

    call(
        &mut client,
        &mut reader,
        request_id,
        "task.cancel",
        json!({"task": task}),
    );
    request_id += 1;
    let cancel_started = Instant::now();
    let cancelled = call(
        &mut client,
        &mut reader,
        request_id,
        "task.await",
        json!({"task": task}),
    )
    .result
    .unwrap();
    assert!(cancel_started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(cancelled["state"], "cancelled");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn suspend_remains_honest_for_evaluator_only_work() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let started = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sleep 30s","background":true}),
    )
    .result
    .unwrap();
    let task = started["task"].clone();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let record = call(
        &mut client,
        &mut reader,
        3,
        "task.get",
        json!({"task": task}),
    )
    .result
    .unwrap();
    assert_eq!(record["controls"]["cancel"], true);
    assert_eq!(record["controls"]["suspend"], false);
    assert_eq!(record["controls"]["resume"], false);
    assert_eq!(record["controls"]["active_process_groups"], 0);

    let error = call(
        &mut client,
        &mut reader,
        4,
        "task.suspend",
        json!({"task": task}),
    )
    .error
    .expect("evaluator-only work has no process group to stop");
    assert_eq!(error.code, TASK_CONTROL_UNAVAILABLE);
    assert_eq!(
        error.data.unwrap()["reason"],
        "task has no active child process group"
    );

    call(
        &mut client,
        &mut reader,
        5,
        "task.cancel",
        json!({"task": task}),
    );
    let cancelled = call(
        &mut client,
        &mut reader,
        6,
        "task.await",
        json!({"task": task}),
    )
    .result
    .unwrap();
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["controls"]["cancel"], false);
    assert_eq!(cancelled["controls"]["suspend"], false);
    assert_eq!(cancelled["controls"]["resume"], false);
    assert_eq!(cancelled["controls"]["active_process_groups"], 0);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn task_await_bounds_connection_wait_without_cancelling_work() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let task = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sleep 30s","background":true}),
    )
    .result
    .unwrap()["task"]
        .as_str()
        .unwrap()
        .to_owned();

    let started = Instant::now();
    let snapshot = call(
        &mut client,
        &mut reader,
        3,
        "task.await",
        json!({"task":task,"timeout_ms":5}),
    )
    .result
    .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(snapshot["timed_out"], true);
    assert_eq!(snapshot["wait_ms"], 5);
    assert_eq!(snapshot["request_clamped"], false);
    assert!(matches!(
        snapshot["state"].as_str(),
        Some("running" | "suspended" | "cancelling")
    ));

    call(
        &mut client,
        &mut reader,
        4,
        "task.cancel",
        json!({"task":task}),
    );
    let terminal = call(
        &mut client,
        &mut reader,
        5,
        "task.await",
        json!({"task":task,"timeout_ms":5000}),
    )
    .result
    .unwrap();
    assert_eq!(terminal["timed_out"], false);
    assert_eq!(terminal["state"], "cancelled");

    let clamped = call(
        &mut client,
        &mut reader,
        6,
        "task.await",
        json!({"task":task,"timeout_ms":u64::MAX}),
    )
    .result
    .unwrap();
    assert_eq!(clamped["timed_out"], false);
    assert_eq!(clamped["wait_ms"], 60_000);
    assert_eq!(clamped["request_clamped"], true);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}
