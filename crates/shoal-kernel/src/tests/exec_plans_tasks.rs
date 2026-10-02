use super::*;

#[test]
fn unix_stream_session_roundtrip() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
        .persistence
        .journal
        .lock()
        .expect("test lock should not be poisoned")
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
fn session_snapshot_tracks_exec_state_and_exit_codes() {
    let kernel = Kernel::new();
    let (mut client, mut reader, worker) = spawn(&kernel);
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"shoal-repl","tty":true}}),
    );
    assert!(attached.error.is_none(), "attach failed: {attached:?}");
    let before = call(&mut client, &mut reader, 2, "session.snapshot", json!({}))
        .result
        .expect("initial snapshot");
    assert!(before["bindings"].as_array().is_some());
    assert!(before["jobs"].is_object());
    assert!(before["reef"]["bindings"].as_array().is_some());

    let changed = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"let snapshot_local = 41\ncd /"}),
    );
    assert!(changed.error.is_none(), "state-changing exec: {changed:?}");
    let after = call(&mut client, &mut reader, 4, "session.snapshot", json!({}))
        .result
        .expect("updated snapshot");
    assert_ne!(before["cwd"], after["cwd"]);
    let binding = after["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|binding| binding["name"] == "snapshot_local")
        .expect("new lexical binding appears in snapshot");
    assert_eq!(binding["callable"], false);
    assert_eq!(binding["type"], "int");

    let first_value = call(&mut client, &mut reader, 5, "exec", json!({"src":"40 + 1"}))
        .result
        .expect("first transcript value");
    assert_eq!(first_value["value"]["v"], 41);
    let it_response = call(&mut client, &mut reader, 6, "exec", json!({"src":"it + 1"}));
    assert!(
        it_response.error.is_none(),
        "it exec failed: {it_response:?}"
    );
    let it_value = it_response.result.unwrap();
    assert_eq!(it_value["value"]["v"], 42);
    let out_value = call(
        &mut client,
        &mut reader,
        7,
        "exec",
        json!({"src":"out[-1]"}),
    )
    .result
    .expect("out transcript advances after successful kernel exec");
    assert_eq!(out_value["value"]["v"], 42);

    let exited = call(&mut client, &mut reader, 8, "exec", json!({"src":"exit 7"}))
        .result
        .expect("exit is a host signal, not process termination");
    assert_eq!(exited["exit_code"], 7);

    let started = call(
        &mut client,
        &mut reader,
        9,
        "exec",
        json!({"src":"exit 9","async":true}),
    )
    .result
    .expect("background exit task starts");
    let task = started["task"].clone();
    let completed = call(
        &mut client,
        &mut reader,
        10,
        "task.await",
        json!({"task":task}),
    )
    .result
    .expect("background exit task completes");
    assert_eq!(completed["state"], "completed");
    assert_eq!(completed["exit_code"], 9);
    drop(client);
    drop(reader);
    worker.join().unwrap();
}

#[test]
fn kernel_undo_out_index_targets_the_evaluator_statement_journal() {
    let temp = tempfile::tempdir().unwrap();
    let victim = temp.path().join("victim.txt");
    std::fs::write(&victim, b"payload").unwrap();
    let state = temp.path().join("state");
    let kernel = Kernel::open(&state).unwrap();
    let (mut client, mut reader, worker) = spawn(&kernel);
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({
            "local_auth":"local-human",
            "session":"undo-index",
            "client":{"kind":"shoal-repl","tty":true}
        }),
    );
    assert!(attached.error.is_none(), "attach failed: {attached:?}");
    let quoted_dir = serde_json::to_string(temp.path().to_str().unwrap()).unwrap();
    let changed_dir = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":format!("cd {quoted_dir}")}),
    );
    assert!(changed_dir.error.is_none(), "cd failed: {changed_dir:?}");
    let removed = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"rm victim.txt"}),
    );
    assert!(removed.error.is_none(), "rm failed: {removed:?}");
    assert!(!victim.exists(), "rm must move the victim out of place");

    let undone = call(
        &mut client,
        &mut reader,
        4,
        "exec",
        json!({"src":"undo out[1]"}),
    );
    assert!(undone.error.is_none(), "undo out[1] failed: {undone:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"payload");
    drop(client);
    drop(reader);
    worker.join().unwrap();
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
        let thread =
            std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
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
    let thread = std::thread::spawn(move || serve_embedded_test_stream(k, server).unwrap());
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
    let thread = std::thread::spawn(move || serve_embedded_test_stream(k, server).unwrap());
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
