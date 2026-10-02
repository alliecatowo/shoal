use super::*;

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
    assert_eq!(r["enforcement"]["available_tier"], expected);
    assert_eq!(r["enforcement"]["activation"], "deferred-to-spawn");
    assert_eq!(r["enforcement"]["filesystem_enforceable"], false);
    assert_eq!(
        r["enforcement"]["spawn_disposition"],
        "no-os-scope-requested"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn attach_and_plan_share_dimension_level_enforcement_truth() {
    let who = principal();
    let policy = Policy::from_toml(&format!(
        "[principal.\"{who}\"]\nhermetic=true\nopaque='allow'\nauto_apply='in-grant'\n\
         net_connect=[\"example.com:443\"]\nproc_spawn=[\"cat\"]\n\
         process_cpu_seconds=7\nprocess_memory_bytes=67108864\n\n\
         [principal.\"{who}\".fs]\nread=[\"/usr/**\"]\n"
    ))
    .unwrap();
    let kernel = Kernel::with_policy(policy);
    let (mut client, mut reader, thread) = spawn(&kernel);
    let attached = attach(&mut client, &mut reader).result.unwrap();
    let planned = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"1 + 2","mode":"plan"}),
    )
    .result
    .unwrap();
    assert_eq!(planned["enforcement"], attached["enforcement"]);
    let enforcement = &planned["enforcement"];
    assert_eq!(enforcement["filesystem_requested"], true);
    assert_eq!(enforcement["network_scope_requested"], true);
    assert_eq!(enforcement["network_enforceable"], false);
    assert_eq!(enforcement["spawn_pin_requested"], true);
    assert_eq!(enforcement["spawn_pin_atomic"], false);
    assert_eq!(enforcement["process_limits_requested"], true);
    assert_eq!(enforcement["process_limits_enforceable"], true);
    assert_eq!(enforcement["hermetic"], true);
    assert_eq!(enforcement["spawn_disposition"], "refuse-unmet-hermetic");
    assert!(
        enforcement["limitations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "network-policy-only")
    );
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
    let dir = tempfile::tempdir().unwrap();
    let doomed = dir.path().join("permanent.txt");
    std::fs::write(&doomed, b"doomed").unwrap();
    let permanent = call(
        &mut client,
        &mut reader,
        20,
        "exec",
        json!({
            "src": format!("rm --permanent {}", doomed.display()),
            "mode": "plan"
        }),
    )
    .result
    .unwrap();
    assert_eq!(permanent["reversibility"], "irreversible", "{permanent}");
    assert_eq!(permanent["effects"][0]["kind"], "fs_delete", "{permanent}");
    assert_eq!(permanent["effects"][0]["permanent"], true, "{permanent}");
    let permanent_ref = permanent["plan_ref"].as_str().unwrap().to_owned();

    let stored = call(
        &mut client,
        &mut reader,
        21,
        "plan.get",
        json!({"plan_ref": permanent_ref}),
    )
    .result
    .unwrap();
    assert_eq!(stored["reversibility"], "irreversible", "{stored}");
    assert_eq!(stored["effects"][0]["permanent"], true, "{stored}");

    let applied = call(
        &mut client,
        &mut reader,
        22,
        "plan.apply",
        json!({"plan_ref": permanent_ref}),
    )
    .result
    .expect("an allowed stored permanent-delete plan applies");
    assert_eq!(applied["value"]["ok"], true, "{applied}");
    assert!(
        !doomed.exists(),
        "plan.apply must execute the stored permanent mode"
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
fn later_kernel_requests_parse_with_live_session_bindings() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    attach(&mut client, &mut reader);

    let define = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"let persisted = 40","position":"value"}),
    );
    assert!(define.error.is_none(), "binding request failed: {define:?}");

    // A context-free parse treats an unadorned word at statement head as an
    // external command. The live evaluator snapshot must instead classify
    // this as an expression in both plan and run modes.
    let plan = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src":"persisted + 2","mode":"plan","position":"value"}),
    );
    assert!(plan.error.is_none(), "bound-value plan failed: {plan:?}");
    assert_eq!(plan.result.unwrap()["effects"], json!([]));

    let run = call(
        &mut client,
        &mut reader,
        4,
        "exec",
        json!({"src":"persisted + 2","position":"value"}),
    );
    assert!(run.error.is_none(), "bound-value run failed: {run:?}");
    assert_eq!(run.result.unwrap()["value"], json!({"$":"int","v":42}));

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

    kernel
        .authority
        .fail_approval_audit
        .store(true, Ordering::SeqCst);
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

    kernel
        .authority
        .fail_approval_audit
        .store(false, Ordering::SeqCst);
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
fn cap_request_unwind_restores_its_grant_reservation() {
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

    kernel
        .authority
        .panic_approval_audit
        .store(true, Ordering::SeqCst);
    let failed = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref}),
    );
    assert_eq!(failed.error.unwrap().code, INTERNAL_ERROR);
    kernel
        .runtime
        .plans
        .transaction(|plans| {
            assert!(matches!(
                plans.get(&plan_ref).map(|plan| &plan.authorization),
                Some(PlanAuthorization::Pending)
            ));
        })
        .unwrap();
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn stale_grant_reservation_recovers_before_the_next_transaction() {
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
    kernel
        .runtime
        .plans
        .transaction(|plans| {
            let stored = plans.get_mut(&plan_ref).unwrap();
            let record = ApprovalRecord {
                requester: stored.principal.clone(),
                approver: stored.principal.clone(),
                plan_ref: stored.plan.plan_ref.clone(),
                plan_hash: stored.plan_hash.clone(),
                source_hash: stored.source_hash.clone(),
                session: stored.session.clone(),
                scope: stored.plan.effects.iter().map(effect_kind).collect(),
                approved_at_ns: now_ns(),
                grant_audit_id: 0,
                consumed_by: None,
            };
            stored.authorization = PlanAuthorization::Granting {
                record,
                restore_policy_allowed: false,
                started_at: Instant::now() - GRANT_RESERVATION_TTL,
                lease: std::sync::Weak::new(),
            };
        })
        .unwrap();
    let granted = call(
        &mut client,
        &mut reader,
        3,
        "cap.request",
        json!({"plan_ref": plan_ref}),
    );
    assert!(
        granted.error.is_none(),
        "stale reservation recovered: {granted:?}"
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
fn only_explicit_machine_approver_role_can_approve_on_durable_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    let (requester_token, _) = tokens
        .create("agent:requester".into(), "agent".into(), vec![], None)
        .unwrap();
    let (unauthorized_token, _) = tokens
        .create("agent:other".into(), "agent".into(), vec![], None)
        .unwrap();
    let (human_token, _) = tokens
        .create("human:operator".into(), "local-human".into(), vec![], None)
        .unwrap();
    let (supervisor_token, _) = tokens
        .create("agent:supervisor".into(), "supervisor".into(), vec![], None)
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
        json!({"token":human_token,"client":{"kind":"human","tty":false}}),
    )
    .result
    .unwrap();
    assert_eq!(human_attach["caps"]["profile"], "local-human");
    let denied = call(
        &mut human,
        &mut human_reader,
        2,
        "cap.request",
        json!({"plan_ref":plan_ref}),
    )
    .error
    .expect("a human-sounding bearer profile is not human-presence evidence");
    assert_eq!(denied.code, LEASH_DENIED);

    let (mut supervisor, mut supervisor_reader, supervisor_thread) = spawn(&kernel);
    let supervisor_attach = call(
        &mut supervisor,
        &mut supervisor_reader,
        1,
        "session.attach",
        json!({"token":supervisor_token,"client":{"kind":"supervisor","tty":false}}),
    )
    .result
    .unwrap();
    let supervisor_principal = supervisor_attach["principal"].as_str().unwrap().to_owned();
    let approved = call(
        &mut supervisor,
        &mut supervisor_reader,
        2,
        "cap.request",
        json!({"plan_ref":plan_ref}),
    )
    .result
    .expect("the explicit supervisor machine role can approve");
    assert_eq!(approved["approver"], supervisor_principal);

    drop(requester);
    drop(requester_reader);
    requester_thread.join().unwrap();
    drop(other);
    drop(other_reader);
    other_thread.join().unwrap();
    drop(human);
    drop(human_reader);
    human_thread.join().unwrap();
    drop(supervisor);
    drop(supervisor_reader);
    supervisor_thread.join().unwrap();
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
        "[principal.\"agent:alpha\"]\nopaque='ask'\nauto_apply='never'\njournal_read=true\n\n\
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
