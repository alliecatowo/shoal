#[test]
fn recovery_journal_rejects_unrelated_duplicate_reordered_and_bad_mode_targets() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(&temporary);

    let mut unrelated = valid_journal(&config, "install");
    unrelated.artifacts[0].relative = "unrelated-user-file".into();
    assert!(validate_journal(&config, &unrelated).is_err());

    let mut duplicate = valid_journal(&config, "install");
    duplicate.artifacts[1].relative = duplicate.artifacts[0].relative.clone();
    assert!(validate_journal(&config, &duplicate).is_err());

    let mut reordered = valid_journal(&config, "install");
    reordered.artifacts.swap(0, 1);
    assert!(validate_journal(&config, &reordered).is_err());

    let mut mode = valid_journal(&config, "install");
    mode.artifacts[0].mode = 0o777;
    assert!(validate_journal(&config, &mode).is_err());
}

#[test]
fn recovery_accepts_a_canonical_ordered_uninstall_subset() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(&temporary);
    let mut journal = valid_journal(&config, "uninstall");
    journal.artifacts = journal
        .artifacts
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut artifact)| {
            index.is_multiple_of(3).then(|| {
                artifact.backup = String::new();
                artifact
            })
        })
        .enumerate()
        .map(|(index, mut artifact)| {
            artifact.backup = format!("artifact-{index}");
            artifact
        })
        .collect();
    validate_journal(&config, &journal).unwrap();
}

#[test]
fn rollback_refuses_post_crash_replacements_for_install_and_uninstall() {
    let old = identity('a', 0o755);
    let new = identity('b', 0o755);
    let replacement = identity('c', 0o755);
    assert!(
        classify_rollback(
            Some(replacement.clone()),
            Some(old.clone()),
            Some(new),
            "bin/shoal"
        )
        .is_err()
    );
    assert!(classify_rollback(Some(replacement), Some(old), None, "bin/shoal").is_err());
}
