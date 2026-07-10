#[test]
fn child_landlock_allows_subtree_and_denies_sibling() {
    if shoal_leash::landlock_abi().is_none() {
        eprintln!("Landlock unavailable; skipping enforcement assertion");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let allowed = d.path().join("allowed");
    let denied = d.path().join("denied");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(&denied).unwrap();
    let a = allowed.join("a");
    let b = denied.join("b");
    std::fs::write(&a, b"ok").unwrap();
    std::fs::write(&b, b"no").unwrap();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_shoal-landlock-helper"))
        .args([&a, &b])
        .status()
        .unwrap();
    if status.code() == Some(77) {
        eprintln!("Landlock reported but could not be activated in this container; skipping");
        return;
    }
    assert!(status.success(), "helper status {status}")
}
