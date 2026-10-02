use super::*;

#[test]
fn complete_and_explain_methods() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);
    let invalid_cursor = call(
        &mut client,
        &mut reader,
        2,
        "complete",
        json!({"src":"é","cursor":1}),
    )
    .error
    .expect("mid-codepoint completion cursor must be rejected");
    assert_eq!(invalid_cursor.code, INVALID_PARAMS);
    let c = call(
        &mut client,
        &mut reader,
        3,
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
        4,
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
fn journal_query_requires_policy_before_decoding_filters() {
    let policy = Policy::from_toml(&format!(
        "[principal.\"{}\"]\nopaque='allow'\njournal_read=false\n",
        principal()
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    let (mut client, mut reader, thread) = spawn(&kernel);
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"test","tty":false}}),
    );
    assert!(attached.error.is_none(), "{attached:?}");

    // Authorization precedes parameter decoding: a denied caller learns
    // neither the journal schema nor whether any rows exist.
    let denied = call(
        &mut client,
        &mut reader,
        2,
        "journal.query",
        Json::String("deliberately-invalid-params".into()),
    )
    .error
    .expect("JournalRead=false must fail closed");
    assert_eq!(denied.code, LEASH_DENIED);
    assert_eq!(denied.data.unwrap()["effect"], "journal.read");

    let blob_denied = call(
        &mut client,
        &mut reader,
        3,
        "blob.get",
        Json::String("also-invalid-before-decode".into()),
    )
    .error
    .expect("journal-backed blob reads require the same grant");
    assert_eq!(blob_denied.code, LEASH_DENIED);
    assert_eq!(blob_denied.data.unwrap()["effect"], "journal.read");

    for (id, method) in [(4, "events.read"), (5, "events.subscribe")] {
        let event_denied = call(
            &mut client,
            &mut reader,
            id,
            method,
            json!({"channel":"journal", "since":"invalid-before-decode"}),
        )
        .error
        .unwrap_or_else(|| panic!("{method} journal access requires JournalRead"));
        assert_eq!(event_denied.code, LEASH_DENIED);
        assert_eq!(event_denied.data.unwrap()["effect"], "journal.read");
    }

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

fn streamed_outcome(stdout: &[u8]) -> Value {
    let Value::Outcome(outcome) = bare_outcome(true, stdout) else {
        unreachable!()
    };
    let mut outcome = Arc::try_unwrap(outcome).unwrap();
    outcome.streamed = true;
    Value::Outcome(Arc::new(outcome))
}

#[test]
fn outcome_streamed_metadata_is_honest_in_full_and_elided_wire_values() {
    let captured = serde_json::to_value(wire_value(&bare_outcome(true, b"builtin\n"))).unwrap();
    assert_eq!(captured["streamed"], false);

    let streamed = streamed_outcome(b"live pty\n");
    let full = serde_json::to_value(wire_value(&streamed)).unwrap();
    assert_eq!(full["streamed"], true);
    let elided = serde_json::to_value(elide_wire_value(
        &streamed,
        "shoal://out/1",
        &ElideBudget::default(),
    ))
    .unwrap();
    assert_eq!(elided["streamed"], true);
}

#[test]
fn value_get_render_reports_live_pty_streaming_without_guessing_from_type() {
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
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"sh { printf streamed-marker }"}),
    )
    .result
    .expect("interactive command exec");
    assert_eq!(exec["value"]["streamed"], true);
    let rendered = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref":exec["ref"],"format":"render"}),
    )
    .result
    .expect("render fetch");
    assert_eq!(rendered["streamed"], true);
    drop(client);
    drop(reader);
    worker.join().unwrap();
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
