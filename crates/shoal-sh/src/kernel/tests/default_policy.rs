use super::*;

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

    // Regression (audit H4): the stock agent policy lets pure evaluation run
    // (the plugin's "verify" step) but still gates opaque commands.
    let sum = call(&mut client, &mut reader, 2, "exec", json!({"src":"1 + 2"}));
    assert!(sum.error.is_none(), "{:?}", sum.error);
    let opaque = call(
        &mut client,
        &mut reader,
        5,
        "exec",
        json!({"src":"sh { id }"}),
    );
    assert_eq!(opaque.error.unwrap().code, APPROVAL_REQUIRED);
    let status = call(&mut client, &mut reader, 3, "kernel.status", json!({}));
    assert!(status.error.is_none());
    let shutdown = call(&mut client, &mut reader, 4, "kernel.shutdown", json!({}));
    assert_eq!(shutdown.error.unwrap().code, LEASH_DENIED);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}
