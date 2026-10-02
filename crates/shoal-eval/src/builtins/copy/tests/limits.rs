use super::*;

#[test]
fn copy_plan_rejects_before_mutation_when_the_tree_exceeds_a_wall() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(source.join(name), name).unwrap();
    }

    let error =
        CopyPlan::build_with_limits(&StdFs, &[(source, destination.clone())], true, 3, 4096, 8)
            .unwrap_err();
    assert_eq!(error.code, "builtin_work_limit");
    assert!(!destination.exists(), "planning must be effect-free");
}

#[test]
fn copy_plan_rejects_descriptor_pressure_before_any_mutation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    for index in 0..8 {
        std::fs::write(source.join(format!("file-{index}")), b"payload").unwrap();
    }

    let error = CopyPlan::build_with_all_limits(
        &StdFs,
        &[(source, destination.clone())],
        true,
        64,
        64 * 1024,
        8,
        Some(5),
    )
    .unwrap_err();
    assert_eq!(error.code, "builtin_work_limit");
    assert!(error.msg.contains("descriptor") || error.msg.contains("admit"));
    assert!(!destination.exists(), "descriptor admission is effect-free");
}

#[test]
fn descriptor_budget_is_checked_before_opening_a_source() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let destination = root.path().join("destination");
    let error = CopyPlan::build_with_all_limits(
        &StdFs,
        &[(missing, destination.clone())],
        false,
        8,
        4096,
        8,
        Some(2),
    )
    .unwrap_err();
    assert_eq!(error.code, "builtin_work_limit");
    assert!(error.msg.contains("descriptor"));
    assert!(!destination.exists());
}

#[test]
fn copy_plan_tracks_retained_handles_within_its_budget() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    for index in 0..16 {
        std::fs::write(source.join(format!("file-{index}")), b"payload").unwrap();
    }

    let plan = CopyPlan::build_with_all_limits(
        &StdFs,
        &[(source, destination.clone())],
        true,
        64,
        64 * 1024,
        8,
        Some(24),
    )
    .unwrap();
    assert!(plan.retained_handles <= 24);
    plan.execute(&StdFs).unwrap();
    assert_eq!(std::fs::read_dir(destination).unwrap().count(), 16);
}

#[test]
fn copy_plan_executes_only_after_a_complete_recursive_inventory() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("nested/file"), b"payload").unwrap();

    CopyPlan::build(&StdFs, &[(source, destination.clone())], true)
        .unwrap()
        .execute(&StdFs)
        .unwrap();
    assert_eq!(
        std::fs::read(destination.join("nested/file")).unwrap(),
        b"payload"
    );
}
