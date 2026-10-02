#[test]
fn managed_manifest_accepts_only_the_exact_canonical_generation() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(&temporary);
    validate_manifest(&config, &valid_manifest(&config)).unwrap();
}

#[test]
fn forced_uninstall_cannot_trust_an_unrelated_manifest_target() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(&temporary);
    config.action = Action::Uninstall;
    config.force = true;
    let mut manifest = valid_manifest(&config);
    manifest.artifacts[0].relative = "unrelated-user-file".into();
    manifest.generation = manifest_generation(&manifest.artifacts);
    let error = validate_manifest(&config, &manifest).unwrap_err();
    assert!(error.to_string().contains("canonical layout"), "{error}");

    let root = SafeRoot::open(&config.prefix).unwrap();
    root.install_bytes(
        Path::new(MANIFEST_NAME),
        &serde_json::to_vec(&manifest).unwrap(),
        0o600,
        "manifest-fixture",
    )
    .unwrap();
    root.install_bytes(
        Path::new("unrelated-user-file"),
        b"keep",
        0o644,
        "unrelated-fixture",
    )
    .unwrap();
    assert!(uninstall(&config, &root).is_err());
    assert_eq!(
        root.read_file(Path::new("unrelated-user-file")).unwrap(),
        b"keep"
    );
    assert!(!config.prefix.join(TRANSACTION_NAME).exists());
}

#[test]
fn managed_manifest_rejects_duplicate_paths_bad_modes_and_bad_generation() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(&temporary);

    let mut duplicate = valid_manifest(&config);
    duplicate.artifacts[1].relative = duplicate.artifacts[0].relative.clone();
    duplicate.generation = manifest_generation(&duplicate.artifacts);
    assert!(validate_manifest(&config, &duplicate).is_err());

    let mut mode = valid_manifest(&config);
    mode.artifacts[0].mode = 0o777;
    mode.generation = manifest_generation(&mode.artifacts);
    assert!(validate_manifest(&config, &mode).is_err());

    let mut generation = valid_manifest(&config);
    generation.generation = "0".repeat(64);
    let error = validate_manifest(&config, &generation).unwrap_err();
    assert!(error.to_string().contains("generation digest"), "{error}");
}
