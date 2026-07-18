use std::process::Command;

fn history() -> Command {
    Command::new(env!("CARGO_BIN_EXE_shoal-history"))
}

#[test]
fn layered_relative_state_dir_targets_the_project_journal() {
    let dir = tempfile::tempdir().unwrap();
    let config_home = dir.path().join("config-home");
    std::fs::create_dir(&config_home).unwrap();
    std::fs::write(
        dir.path().join(".shoal.toml"),
        "[journal]\nstate_dir = 'project-journal'\n",
    )
    .unwrap();

    let output = history()
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", dir.path().join("fallback-state"))
        .arg("status")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("project-journal/journal.db").is_file());
    assert!(!dir.path().join("fallback-state/shoal/journal.db").exists());
}

#[test]
fn explicit_state_dir_bypasses_malformed_layered_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".shoal.toml"), "[journal]\nstate_dir = 5\n").unwrap();
    let explicit = dir.path().join("explicit-journal");

    let output = history()
        .current_dir(dir.path())
        .args(["--state-dir", explicit.to_str().unwrap(), "status"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(explicit.join("journal.db").is_file());
}

#[test]
fn malformed_config_never_silently_opens_the_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let fallback = dir.path().join("fallback-state");
    std::fs::write(dir.path().join(".shoal.toml"), "[journal]\nstate_dir = 5\n").unwrap();

    let output = history()
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join("config-home"))
        .env("XDG_STATE_HOME", &fallback)
        .arg("status")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("configuration:"));
    assert!(!fallback.join("shoal/journal.db").exists());
}
