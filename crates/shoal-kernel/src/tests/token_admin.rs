use super::*;

fn create_token(state: &Path, principal: &str, profile: &str, caps: &[&str]) -> String {
    let mut store = TokenStore::open(state.join("tokens.json")).unwrap();
    store
        .create(
            principal.into(),
            profile.into(),
            caps.iter().map(|cap| (*cap).into()).collect(),
            None,
        )
        .unwrap()
        .0
}

fn attach_bearer(client: &mut UnixStream, reader: &mut BufReader<UnixStream>, token: &str) {
    let response = call(
        client,
        reader,
        1,
        "session.attach",
        json!({"token": token, "client":{"kind":"test", "tty":false}}),
    );
    assert!(
        response.error.is_none(),
        "attach failed: {:?}",
        response.error
    );
}

fn spawn_public(
    kernel: &Arc<Kernel>,
) -> (
    UnixStream,
    BufReader<UnixStream>,
    std::thread::JoinHandle<()>,
) {
    let (client, server) = UnixStream::pair().unwrap();
    let reader = BufReader::new(client.try_clone().unwrap());
    let kernel = kernel.clone();
    let thread = std::thread::spawn(move || kernel.handle_stream(server).unwrap());
    (client, reader, thread)
}

#[test]
fn explicit_admin_can_create_list_and_revoke_with_live_revalidation() {
    let dir = tempfile::tempdir().unwrap();
    let kernel =
        Kernel::open_with_policy(dir.path(), Policy::permissive("agent:token-administrator"))
            .unwrap();
    let admin = create_token(
        dir.path(),
        "agent:token-administrator",
        "operator",
        &["token.admin"],
    );
    let (mut admin_client, mut admin_reader, admin_thread) = spawn_public(&kernel);
    attach_bearer(&mut admin_client, &mut admin_reader, &admin);

    let created = call(
        &mut admin_client,
        &mut admin_reader,
        2,
        "auth.token.create",
        json!({
            "principal": "agent:worker",
            "profile": "worker",
            "caps": ["jobs.read"],
            "ttl_seconds": 60
        }),
    )
    .result
    .expect("explicit token administrator can create a bearer");
    assert_eq!(created["secret_shown_once"], true);
    assert_eq!(created["meta"]["principal"], "agent:worker");
    assert!(created.get("digest").is_none());
    let worker = created["token"].as_str().unwrap().to_string();
    let worker_id = created["meta"]["id"].as_str().unwrap().to_string();

    let listed = call(
        &mut admin_client,
        &mut admin_reader,
        3,
        "auth.token.list",
        json!({}),
    )
    .result
    .unwrap();
    let listed = listed.as_array().unwrap();
    assert!(listed.iter().any(|token| token["id"] == worker_id));
    assert!(listed.iter().all(|token| token.get("digest").is_none()));

    let (mut worker_client, mut worker_reader, worker_thread) = spawn_public(&kernel);
    attach_bearer(&mut worker_client, &mut worker_reader, &worker);
    let revoked = call(
        &mut admin_client,
        &mut admin_reader,
        4,
        "auth.token.revoke",
        json!({"id": worker_id}),
    )
    .result
    .unwrap();
    assert_eq!(revoked["revoked"], true);

    let invalidated = call(
        &mut worker_client,
        &mut worker_reader,
        2,
        "kernel.status",
        json!({}),
    )
    .error
    .expect("revocation must invalidate an already attached bearer on its next request");
    assert_eq!(invalidated.code, AUTH_FAILED);

    let absent = call(
        &mut admin_client,
        &mut admin_reader,
        5,
        "auth.token.revoke",
        json!({"id": "0000000000000000"}),
    )
    .result
    .unwrap();
    assert_eq!(absent["revoked"], false);

    drop(worker_client);
    drop(worker_reader);
    worker_thread.join().unwrap();
    drop(admin_client);
    drop(admin_reader);
    admin_thread.join().unwrap();
}

#[test]
fn supervisor_and_plan_approval_do_not_imply_token_administration() {
    let dir = tempfile::tempdir().unwrap();
    let kernel =
        Kernel::open_with_policy(dir.path(), Policy::permissive("agent:supervisor")).unwrap();
    let supervisor = create_token(
        dir.path(),
        "agent:supervisor",
        "supervisor",
        &["plan.approve"],
    );
    let (mut client, mut reader, thread) = spawn_public(&kernel);
    attach_bearer(&mut client, &mut reader, &supervisor);

    let denied = call(&mut client, &mut reader, 2, "auth.token.list", json!({}))
        .error
        .expect("plan approval authority must not mint credentials");
    assert_eq!(denied.code, LEASH_DENIED);
    assert_eq!(denied.data.unwrap()["required_capability"], "token.admin");

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn ephemeral_kernel_refuses_token_admin_even_for_embedded_human() {
    let kernel = Kernel::new();
    let (left, right) = UnixStream::pair().unwrap();
    let serve = kernel.clone();
    let thread = std::thread::spawn(move || serve_embedded_test_stream(serve, right).unwrap());
    let mut client = left;
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({
            "local_auth": "local-human",
            "client":{"kind":"shoal-repl", "tty":true}
        }),
    );
    assert!(attached.error.is_none());

    let unavailable = call(&mut client, &mut reader, 2, "auth.token.list", json!({}))
        .error
        .expect("an ephemeral kernel has no credential store to administer");
    assert_eq!(unavailable.code, AUTH_FAILED);
    assert_eq!(unavailable.data.unwrap()["durable_kernel_required"], true);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}
