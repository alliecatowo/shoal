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
    let suspended = loop {
        let response = call(
            &mut client,
            &mut reader,
            request_id,
            "task.suspend",
            json!({"task": task}),
        );
        request_id += 1;
        if let Some(result) = response.result {
            break result;
        }
        let error = response.error.unwrap();
        assert_eq!(error.code, TASK_CONTROL_UNAVAILABLE);
        assert!(
            Instant::now() < deadline,
            "child process group never became active"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
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

    let error = call(
        &mut client,
        &mut reader,
        3,
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
        4,
        "task.cancel",
        json!({"task": task}),
    );
    let cancelled = call(
        &mut client,
        &mut reader,
        5,
        "task.await",
        json!({"task": task}),
    )
    .result
    .unwrap();
    assert_eq!(cancelled["state"], "cancelled");
    drop(client);
    drop(reader);
    thread.join().unwrap();
}
