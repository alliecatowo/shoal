use super::*;

#[test]
fn connection_provenance_controls_local_human_attachment() {
    let public_kernel = Kernel::new();
    let (mut public_client, public_server) = UnixStream::pair().unwrap();
    let mut public_reader = BufReader::new(public_client.try_clone().unwrap());
    let public_worker_kernel = public_kernel.clone();
    let public_worker =
        std::thread::spawn(move || public_worker_kernel.handle_stream(public_server).unwrap());
    let denied = call(
        &mut public_client,
        &mut public_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"raw","tty":true}}),
    );
    assert_eq!(denied.error.unwrap().code, AUTH_FAILED);
    drop(public_client);
    drop(public_reader);
    public_worker.join().unwrap();

    let embedded_kernel = Kernel::new();
    let (mut embedded_client, embedded_server) = UnixStream::pair().unwrap();
    let mut embedded_reader = BufReader::new(embedded_client.try_clone().unwrap());
    let worker_kernel = embedded_kernel.clone();
    let embedded_worker = std::thread::spawn(move || {
        serve_embedded_test_stream(worker_kernel, embedded_server).unwrap()
    });
    let attached = call(
        &mut embedded_client,
        &mut embedded_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"shoal-repl","tty":true}}),
    )
    .result
    .expect("inherited endpoint proves embedded human presence");
    assert_eq!(attached["connection_trust"], "embedded-human");
    drop(embedded_client);
    drop(embedded_reader);
    embedded_worker.join().unwrap();
}

#[test]
fn mandatory_public_token_mode_rejects_tokenless_attachment() {
    let kernel = Kernel::new();
    kernel.configure_listener_security(true, false);
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let worker_kernel = kernel.clone();
    let worker = std::thread::spawn(move || worker_kernel.handle_stream(server).unwrap());

    let denied = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"client":{"kind":"raw","tty":false}}),
    )
    .error
    .expect("public tokenless attach must be rejected in mandatory mode");
    assert_eq!(denied.code, AUTH_FAILED);
    assert_eq!(denied.data.unwrap()["auth_mode"], "bearer-required");

    drop(client);
    drop(reader);
    worker.join().unwrap();
}

#[test]
fn named_listener_peer_uid_mode_accepts_the_matching_os_peer() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("peer-bound.sock");
    let kernel = Kernel::new();
    kernel.configure_listener_security(false, true);
    let stop = Arc::new(AtomicBool::new(false));
    let server_kernel = kernel.clone();
    let server_stop = stop.clone();
    let server_socket = socket.clone();
    let worker = std::thread::spawn(move || {
        server_kernel
            .serve_until(server_socket, server_stop)
            .unwrap()
    });
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(socket.exists(), "named listener did not become ready");

    let mut client = UnixStream::connect(&socket).unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let attached = call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"client":{"kind":"peer-test","tty":false}}),
    )
    .result
    .expect("same-effective-UID peer passes the OS credential gate");
    assert_eq!(attached["connection_trust"], "public");
    let status = call(&mut client, &mut reader, 2, "kernel.status", json!({}))
        .result
        .unwrap();
    assert_eq!(status["security"]["public_peer_uid_required"], true);
    assert_eq!(status["security"]["public_token_required"], false);

    drop(client);
    drop(reader);
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();
}
