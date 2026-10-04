use super::*;

/// Regression (audit C1): `pty.open` used to skip the plan verdict entirely, so
/// a principal `exec` denied could still get an unconfined shell on a pty.
#[test]
fn pty_open_is_denied_for_principals_exec_denies() {
    let marker = std::env::temp_dir().join(format!("shoal-pty-c1-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let policy =
        Policy::from_toml("[principal.\"agent:denied\"]\ntime=true\nopaque='deny'\n").unwrap();
    let kernel = Kernel::with_policy(policy);
    for principal in ["agent:denied", "agent:unknown"] {
        let session = kernel.session("pty-c1", principal).unwrap();
        let mut attached = Some(Attachment {
            session,
            principal: principal.into(),
            can_approve: false,
            tty: false,
            cancel_epoch: None,
            bearer: None,
            security_epoch: ATTACH_SECURITY_EPOCH,
            connection_trust: ConnectionTrust::EmbeddedHuman,
        });
        let error = kernel
            .handle_pty_open(
                json!({"cmd":"sh","args":["-c", format!("echo x > {}", marker.display())]}),
                &mut attached,
            )
            .unwrap_err();
        assert_eq!(error.code, LEASH_DENIED, "{principal}");
        assert!(
            kernel.handle_pty_list(&mut attached).unwrap()["ptys"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!marker.exists(), "denied pty.open must not run the program");
}
