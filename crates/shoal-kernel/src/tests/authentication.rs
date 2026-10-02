use super::*;

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
fn attach_rejects_absurd_or_ambiguous_identity_input_without_secret_echo() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("tokens.json");
    let mut tokens = TokenStore::open(&token_path).unwrap();
    let (valid_bearer, _) = tokens
        .create("agent:bounded".into(), "readonly".into(), vec![], None)
        .unwrap();
    drop(tokens);
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let worker = kernel.clone();
    let thread = std::thread::spawn(move || worker.handle_stream(server).unwrap());

    // A malformed authority snapshot proves an absurd bearer is rejected by
    // shape before any token-store read. The response is generic and does not
    // amplify the credential into an error.
    let malformed = b"malformed-authority-snapshot".to_vec();
    std::fs::write(&token_path, &malformed).unwrap();
    let absurd = "s".repeat(1_000_000);
    let response = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"token":absurd,"client":{"kind":"agent","tty":false}}),
    );
    let error = response.error.unwrap();
    assert_eq!(error.code, AUTH_FAILED);
    assert_eq!(error.message, "invalid, expired, or revoked bearer token");
    assert!(error.message.len() < 128);
    assert_eq!(std::fs::read(&token_path).unwrap(), malformed);

    let invalid_cases = [
        json!({"session":"s".repeat(MAX_SESSION_NAME_BYTES + 1),"client":{"kind":"agent","tty":false}}),
        json!({"client":{"kind":"k".repeat(MAX_CLIENT_KIND_BYTES + 1),"tty":false}}),
        json!({"local_auth":"x".repeat(1_000_000),"client":{"kind":"agent","tty":false}}),
        json!({"unexpected":{"deep":[[[1]]]},"client":{"kind":"agent","tty":false}}),
        json!({"client":{"kind":"agent","tty":false,"unexpected":true}}),
    ];
    for (offset, params) in invalid_cases.into_iter().enumerate() {
        let response = call(
            &mut client,
            &mut reader,
            2 + offset as i64,
            "session.attach",
            params,
        );
        let error = response.error.unwrap();
        assert_eq!(error.code, INVALID_PARAMS);
        assert!(error.message.len() < 128);
        assert!(!error.message.contains(&"x".repeat(129)));
    }

    // A correctly shaped credential reaches the store and still fails closed
    // on the deliberately malformed snapshot.
    let response = call(
        &mut client,
        &mut reader,
        20,
        "session.attach",
        json!({"token":valid_bearer,"client":{"kind":"agent","tty":false}}),
    );
    assert_eq!(response.error.unwrap().code, AUTH_FAILED);
    assert_eq!(std::fs::read(&token_path).unwrap(), malformed);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn durable_kernel_rejects_asserted_and_bearer_named_human_authority() {
    let dir = tempfile::tempdir().unwrap();
    let mut tokens = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    let (human_token, _) = tokens
        .create("human:operator".into(), "local-human".into(), vec![], None)
        .unwrap();
    let (supervisor_token, _) = tokens
        .create("agent:supervisor".into(), "supervisor".into(), vec![], None)
        .unwrap();
    drop(tokens);
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let public_kernel = kernel.clone();
    let thread = std::thread::spawn(move || public_kernel.handle_stream(server).unwrap());

    let asserted = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({
            "local_auth":"local-human",
            "client":{"kind":"raw-agent","tty":true}
        }),
    );
    let error = asserted
        .error
        .expect("raw durable-socket callers cannot assert human authority");
    assert_eq!(error.code, AUTH_FAILED);
    assert_eq!(error.data.unwrap()["human_presence_supported"], false);

    let credentialed = call(
        &mut client,
        &mut reader,
        2,
        "session.attach",
        json!({
            "token":human_token,
            "client":{"kind":"native-human","tty":true}
        }),
    )
    .result
    .expect("the bearer remains a valid machine credential");
    assert_eq!(credentialed["principal"], "human:operator");
    assert_eq!(credentialed["auth_mode"], "bearer");
    assert_eq!(credentialed["caps"]["profile"], "local-human");

    let status = call(&mut client, &mut reader, 3, "kernel.status", json!({}))
        .result
        .expect("every attached principal may inspect lifecycle status");
    assert_eq!(status["durable"], true);
    assert_eq!(
        status["security"]["bearer_establishes_human_presence"],
        false
    );

    let denied = call(&mut client, &mut reader, 4, "kernel.shutdown", json!({}))
        .error
        .expect("a bearer cannot gain authority from a human-sounding profile");
    assert_eq!(denied.code, LEASH_DENIED);

    call(
        &mut client,
        &mut reader,
        5,
        "session.attach",
        json!({
            "token":supervisor_token,
            "client":{"kind":"machine-supervisor","tty":false}
        }),
    )
    .result
    .expect("explicit machine administration remains available");
    let shutdown = call(&mut client, &mut reader, 6, "kernel.shutdown", json!({}))
        .result
        .expect("the supervisor machine credential may request managed shutdown");
    assert_eq!(shutdown["stopping"], true);
    assert!(kernel.lifecycle.shutdown_requested.load(Ordering::SeqCst));

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
    let kernel = Kernel::builder()
        .durable(dir.path())
        .limits(Limits {
            frame_read_timeout_ms: 40,
            ..Limits::default()
        })
        .build()
        .unwrap();
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
    assert_eq!(kernel.runtime.events.subscriber_count(), 1);

    let mut revoker = TokenStore::open(dir.path().join("tokens.json")).unwrap();
    assert!(revoker.revoke(&meta.id).unwrap());
    let mut byte = [0u8; 1];
    assert_eq!(client.read(&mut byte).unwrap(), 0);
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(kernel.runtime.events.subscriber_count(), 0);
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
        connection_trust: ConnectionTrust::EmbeddedHuman,
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
        ConnectionTrust::EmbeddedHuman,
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
    let status = call(&mut client, &mut reader, 3, "kernel.status", json!({}));
    assert!(status.error.is_none());
    let shutdown = call(&mut client, &mut reader, 4, "kernel.shutdown", json!({}));
    assert_eq!(shutdown.error.unwrap().code, LEASH_DENIED);
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
