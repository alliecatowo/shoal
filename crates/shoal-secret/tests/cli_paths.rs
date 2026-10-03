use std::io::Write as _;
use std::process::{Command, Stdio};

#[test]
fn cli_and_evaluator_override_select_the_same_store() {
    let temp = tempfile::tempdir().unwrap();
    let override_dir = temp.path().join("override-store");
    let fallback_data = temp.path().join("fallback-data");
    let binary = env!("CARGO_BIN_EXE_shoal-secret");

    let mut child = Command::new(binary)
        .args(["set", "shared-name"])
        .env("SHOAL_SECRET_DIR", &override_dir)
        .env("XDG_DATA_HOME", &fallback_data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"shared-value")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "set failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let store = shoal_secret::SecretStore::open(&override_dir).unwrap();
    let value = store.get("shared-name").unwrap().unwrap();
    assert_eq!(value.as_slice(), b"shared-value");
    assert!(!fallback_data.join("shoal/secrets").exists());

    let output = Command::new(binary)
        .arg("list")
        .env("SHOAL_SECRET_DIR", &override_dir)
        .env("XDG_DATA_HOME", &fallback_data)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"shared-name\n");
}

#[test]
fn empty_override_uses_the_xdg_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let fallback_data = temp.path().join("fallback-data");
    let output = Command::new(env!("CARGO_BIN_EXE_shoal-secret"))
        .arg("list")
        .env("SHOAL_SECRET_DIR", "")
        .env("XDG_DATA_HOME", &fallback_data)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fallback_data.join("shoal/secrets/master.key").is_file());
}
