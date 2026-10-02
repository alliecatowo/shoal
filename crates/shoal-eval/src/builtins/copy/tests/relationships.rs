use super::*;

#[test]
fn multi_source_copy_preflights_every_source_before_the_first_effect() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let missing = root.path().join("missing");
    let destination = root.path().join("destination");
    std::fs::write(&source, b"payload").unwrap();
    std::fs::create_dir(&destination).unwrap();

    let error = super::super::super::copy_move(
        &StdFs,
        root.path(),
        vec![
            Value::Path(source),
            Value::Path(missing),
            Value::Path(destination.clone()),
        ],
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, "custom");
    assert!(
        !destination.join("source").exists(),
        "a later preflight failure must leave earlier sources untouched"
    );
}

#[test]
fn multi_source_copy_rejects_duplicate_destination_jobs_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let left_dir = root.path().join("left");
    let right_dir = root.path().join("right");
    let destination = root.path().join("destination");
    std::fs::create_dir(&left_dir).unwrap();
    std::fs::create_dir(&right_dir).unwrap();
    std::fs::create_dir(&destination).unwrap();
    let left = left_dir.join("same");
    let right = right_dir.join("same");
    std::fs::write(&left, b"left").unwrap();
    std::fs::write(&right, b"right").unwrap();

    let error = super::super::super::copy_move(
        &StdFs,
        root.path(),
        vec![
            Value::Path(left),
            Value::Path(right),
            Value::Path(destination.clone()),
        ],
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("overlap or alias"));
    assert!(!destination.join("same").exists());
}

#[test]
fn copy_plan_rejects_lexical_and_ancestor_destination_overlaps() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let destination = root.path().join("destination");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();

    for conflicting in [
        destination.clone(),
        root.path().join("missing/../destination"),
        destination.join("child"),
    ] {
        let error = CopyPlan::build(
            &StdFs,
            &[
                (first.clone(), destination.clone()),
                (second.clone(), conflicting),
            ],
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("overlap or alias"));
        assert!(!destination.exists());
    }
}

#[cfg(unix)]
#[test]
fn copy_plan_rejects_hard_linked_destination_aliases() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let destination = root.path().join("destination");
    let alias = root.path().join("alias");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    std::fs::write(&destination, b"old").unwrap();
    std::fs::hard_link(&destination, &alias).unwrap();

    let error = CopyPlan::build(
        &StdFs,
        &[(first, destination.clone()), (second, alias.clone())],
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("overlap or alias"));
    assert_eq!(std::fs::read(destination).unwrap(), b"old");
    assert_eq!(std::fs::read(alias).unwrap(), b"old");
}

#[test]
fn copy_rejects_the_same_file_before_truncation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("same");
    std::fs::write(&source, b"payload").unwrap();

    let error = CopyPlan::build(&StdFs, &[(source.clone(), source.clone())], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("same file"));
    assert_eq!(std::fs::read(source).unwrap(), b"payload");
}

#[cfg(unix)]
#[test]
fn copy_rejects_a_hard_link_alias() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let alias = root.path().join("alias");
    std::fs::write(&source, b"payload").unwrap();
    std::fs::hard_link(&source, &alias).unwrap();

    let error = CopyPlan::build(&StdFs, &[(source.clone(), alias)], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("same file"));
    assert_eq!(std::fs::read(source).unwrap(), b"payload");
}

#[cfg(unix)]
#[test]
fn copy_rejects_an_unrelated_hard_link_destination_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let aliased = root.path().join("aliased");
    let destination = root.path().join("destination");
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&aliased, b"old").unwrap();
    std::fs::hard_link(&aliased, &destination).unwrap();

    let error = CopyPlan::build(&StdFs, &[(source, destination)], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("hard-linked"));
    assert_eq!(std::fs::read(aliased).unwrap(), b"old");
}

#[test]
fn recursive_copy_rejects_a_missing_destination_inside_source() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = source.join("missing/../backup");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"payload").unwrap();

    let error =
        CopyPlan::build(&StdFs, &[(source.clone(), destination.clone())], true).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("into itself"));
    assert!(!source.join("backup").exists());
}

#[cfg(unix)]
#[test]
fn recursive_copy_rejects_destination_through_a_symlinked_parent() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let alias = root.path().join("alias");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("file"), b"payload").unwrap();
    symlink(&source, &alias).unwrap();
    let destination = alias.join("backup");

    let error = CopyPlan::build(&StdFs, &[(source.clone(), destination)], true).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("symbolic-link"));
    assert!(!source.join("backup").exists());
}
