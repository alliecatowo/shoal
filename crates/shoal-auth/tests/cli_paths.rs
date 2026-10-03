use std::process::Command;

#[test]
fn cli_uses_the_shared_nonempty_token_store_override() {
    let temp = tempfile::tempdir().unwrap();
    let override_path = temp.path().join("authority/tokens.json");
    let fallback_state = temp.path().join("fallback-state");
    let output = Command::new(env!("CARGO_BIN_EXE_shoal-token"))
        .args(["create", "agent:path-test", "default", "--cap", "read"])
        .env("SHOAL_TOKEN_STORE", &override_path)
        .env("XDG_STATE_HOME", &fallback_state)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
    let store = shoal_auth::TokenStore::open(&override_path).unwrap();
    let tokens = store.try_list().unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].principal, "agent:path-test");
    assert!(!fallback_state.join("shoal/tokens.json").exists());
}

#[test]
fn empty_override_uses_the_xdg_state_store() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_shoal-token"))
        .arg("list")
        .env("SHOAL_TOKEN_STORE", "")
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(state.join("shoal/tokens.json").is_file());
}
