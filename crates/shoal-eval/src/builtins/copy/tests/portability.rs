use super::*;

#[cfg(unix)]
#[test]
fn recursive_copy_rejects_live_and_broken_symlinks_before_mutation() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ordinary"), b"payload").unwrap();
    symlink("ordinary", source.join("live-link")).unwrap();
    symlink("missing", source.join("broken-link")).unwrap();

    let error = CopyPlan::build(&StdFs, &[(source, destination.clone())], true).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("symbolic links"));
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn copy_rejects_a_symlink_destination_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    let destination = root.path().join("destination");
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&target, b"old").unwrap();
    symlink(&target, &destination).unwrap();

    let error = CopyPlan::build(&StdFs, &[(source, destination.clone())], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("destination"));
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(
        std::fs::symlink_metadata(destination)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn recursive_copy_rejects_sparse_files_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    let sparse = std::fs::File::create(source.join("sparse")).unwrap();
    sparse.set_len(8 * 1024 * 1024).unwrap();

    let error = CopyPlan::build(&StdFs, &[(source, destination.clone())], true).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("sparse"));
    assert!(!destination.exists());
}

#[cfg(unix)]
#[test]
fn recursive_copy_rejects_fifo_without_opening_or_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ordinary"), b"payload").unwrap();
    let fifo = CString::new(source.join("fifo").as_os_str().as_bytes()).unwrap();
    // SAFETY: `fifo` is a live NUL-terminated path and mode contains only
    // ordinary permission bits.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let started = std::time::Instant::now();
    let error = CopyPlan::build(&StdFs, &[(source, destination.clone())], true).unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("special files"));
    assert!(!destination.exists());
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
))]
#[test]
fn recursive_copy_rejects_source_and_destination_xattrs_before_mutation() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::write(&source, b"payload").unwrap();
    set_xattr(&source);
    let error =
        CopyPlan::build(&StdFs, &[(source.clone(), destination.clone())], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("extended attributes"));
    assert!(!destination.exists());

    std::fs::remove_file(&source).unwrap();
    std::fs::write(&source, b"new").unwrap();
    std::fs::write(&destination, b"old").unwrap();
    set_xattr(&destination);
    let error = CopyPlan::build(&StdFs, &[(source, destination.clone())], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("destination"));
    assert_eq!(std::fs::read(destination).unwrap(), b"old");
}

#[cfg(unix)]
#[test]
fn copy_preserves_rwx_modes_but_rejects_special_permission_bits() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751)).unwrap();
    let file = source.join("tool");
    std::fs::write(&file, b"payload").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();

    CopyPlan::build(&StdFs, &[(source.clone(), destination.clone())], true)
        .unwrap()
        .execute(&StdFs)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        std::fs::metadata(destination.join("tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o4640)).unwrap();
    let rejected = root.path().join("rejected");
    let error = CopyPlan::build(&StdFs, &[(file, rejected.clone())], false).unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("setuid"));
    assert!(!rejected.exists());
}

#[test]
fn copy_intentionally_does_not_preserve_modification_times() {
    use std::fs::FileTimes;
    use std::time::{Duration, UNIX_EPOCH};

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::write(&source, b"payload").unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(946_684_800);
    std::fs::File::open(&source)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old))
        .unwrap();

    CopyPlan::build(&StdFs, &[(source.clone(), destination.clone())], false)
        .unwrap()
        .execute(&StdFs)
        .unwrap();
    assert_eq!(std::fs::metadata(source).unwrap().modified().unwrap(), old);
    assert_ne!(
        std::fs::metadata(destination).unwrap().modified().unwrap(),
        old,
        "copy timestamps are intentionally publication time, not source metadata"
    );
}
