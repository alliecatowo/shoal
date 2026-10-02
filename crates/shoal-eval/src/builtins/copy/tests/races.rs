use super::*;

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn recursive_copy_keeps_the_admitted_tree_when_a_source_ancestor_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let ancestor = root.path().join("ancestor");
    let source = ancestor.join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("nested/file"), b"admitted").unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source.clone(), destination.clone())], true).unwrap();
    let displaced = root.path().join("displaced");
    std::fs::rename(&ancestor, &displaced).unwrap();
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("nested/file"), b"replacement").unwrap();

    plan.execute(&StdFs).unwrap();
    assert_eq!(
        std::fs::read(destination.join("nested/file")).unwrap(),
        b"admitted",
        "an ancestor swap must not redirect retained descendant handles"
    );
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn root_relation_uses_the_retained_source_after_its_path_is_rebound() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("admitted"), b"payload").unwrap();
    let pinned = StdFs.open_copy_source(&source).unwrap();

    let displaced = root.path().join("displaced");
    std::fs::rename(&source, &displaced).unwrap();
    std::fs::create_dir(&source).unwrap();
    let destination = source.join("backup");
    let destination_root = StdFs.open_copy_destination(&destination).unwrap();
    let destination_target = destination_root.open_target(Path::new("")).unwrap();

    validate_root_job(
        &source,
        &destination,
        pinned.as_ref(),
        destination_root.as_ref(),
        destination_target.as_ref(),
    )
    .expect("replacement directory is not inside the retained source object");
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn recursive_copy_keeps_the_admitted_file_when_a_descendant_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    let file = source.join("file");
    std::fs::write(&file, b"admitted").unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source, destination.clone())], true).unwrap();
    std::fs::rename(&file, root.path().join("displaced-file")).unwrap();
    std::fs::write(&file, b"replacement").unwrap();

    plan.execute(&StdFs).unwrap();
    assert_eq!(
        std::fs::read(destination.join("file")).unwrap(),
        b"admitted",
        "a descendant swap must not replace the retained source handle"
    );
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn copy_execution_reports_a_raced_in_destination_symlink_as_failure() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    let victim = root.path().join("victim");
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&victim, b"old").unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source, destination.clone())], false).unwrap();
    symlink(&victim, &destination).unwrap();

    let error = plan.execute(&StdFs).unwrap_err();
    assert_eq!(error.code, "custom");
    assert_eq!(std::fs::read(victim).unwrap(), b"old");
    assert!(
        std::fs::symlink_metadata(destination)
            .unwrap()
            .file_type()
            .is_symlink(),
        "failure must not replace or follow the raced-in destination"
    );
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn copy_keeps_the_admitted_destination_parent_after_an_ancestor_swap() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let ancestor = root.path().join("ancestor");
    let destination = ancestor.join("destination");
    std::fs::write(&source, b"payload").unwrap();
    std::fs::create_dir(&ancestor).unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source, destination)], false).unwrap();
    let displaced = root.path().join("displaced");
    std::fs::rename(&ancestor, &displaced).unwrap();
    std::fs::create_dir(&ancestor).unwrap();
    plan.execute(&StdFs).unwrap();

    assert_eq!(
        std::fs::read(displaced.join("destination")).unwrap(),
        b"payload"
    );
    assert!(
        !ancestor.join("destination").exists(),
        "ambient ancestor replacement must not redirect publication"
    );
}

#[cfg(unix)]
#[test]
fn copy_fails_closed_when_a_missing_destination_parent_becomes_a_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let missing_parent = root.path().join("missing");
    let destination = missing_parent.join("destination");
    let victim = root.path().join("victim");
    std::fs::write(&source, b"payload").unwrap();
    std::fs::create_dir(&victim).unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source, destination)], false).unwrap();
    symlink(&victim, &missing_parent).unwrap();
    let error = plan.execute(&StdFs).unwrap_err();
    assert_eq!(error.code, "custom");
    assert!(!victim.join("destination").exists());
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn copy_does_not_replace_a_destination_file_that_changed_after_preflight() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    let admitted = root.path().join("admitted");
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&destination, b"old").unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source, destination.clone())], false).unwrap();
    std::fs::rename(&destination, &admitted).unwrap();
    std::fs::write(&destination, b"replacement").unwrap();
    let error = plan.execute(&StdFs).unwrap_err();
    assert_eq!(error.code, "custom");
    assert_eq!(std::fs::read(destination).unwrap(), b"replacement");
    assert_eq!(std::fs::read(admitted).unwrap(), b"old");
    assert_no_copy_temporaries(root.path());
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn copy_does_not_replace_a_destination_hardlinked_after_preflight() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    let alias = root.path().join("alias");
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&destination, b"old").unwrap();

    let plan = CopyPlan::build(&StdFs, &[(source, destination.clone())], false).unwrap();
    std::fs::hard_link(&destination, &alias).unwrap();
    let error = plan.execute(&StdFs).unwrap_err();
    assert_eq!(error.code, "custom");
    assert_eq!(std::fs::read(destination).unwrap(), b"old");
    assert_eq!(std::fs::read(alias).unwrap(), b"old");
    assert_no_copy_temporaries(root.path());
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[test]
fn copy_atomically_overwrites_the_admitted_ordinary_file() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::write(&source, b"new payload").unwrap();
    std::fs::write(&destination, b"old").unwrap();

    CopyPlan::build(&StdFs, &[(source, destination.clone())], false)
        .unwrap()
        .execute(&StdFs)
        .unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), b"new payload");
    assert_no_copy_temporaries(root.path());
}
