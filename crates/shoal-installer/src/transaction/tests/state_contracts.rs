#[test]
fn private_transaction_directory_survives_permissive_umask_and_immediate_crash() {
    let temporary = tempfile::tempdir().unwrap();
    let transaction = temporary.path().join("transaction");
    let encoded = std::ffi::CString::new(transaction.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: after fork the child performs only async-signal-safe syscalls
    // over memory prepared above, then terminates without unwinding shared
    // parent state.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
    if child == 0 {
        // SAFETY: umask changes only this forked child process.
        unsafe { libc::umask(0) };
        // SAFETY: encoded is a live NUL-terminated path and 0700 is the
        // exact atomic mode used by create_private_dir's DirBuilder.
        if unsafe { libc::mkdir(encoded.as_ptr(), 0o700) } != 0 {
            // SAFETY: the forked test child owns no cleanup obligations.
            unsafe { libc::_exit(2) };
        }
        // SAFETY: deliberately model a crash immediately after mkdir.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        // SAFETY: reachable only if SIGKILL unexpectedly failed.
        unsafe { libc::_exit(3) };
    }
    let mut status = 0;
    // SAFETY: child is the live PID returned by fork and status is writable.
    assert_eq!(unsafe { libc::waitpid(child, &raw mut status, 0) }, child);
    assert!(libc::WIFSIGNALED(status));
    assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
    let metadata = fs::symlink_metadata(&transaction).unwrap();
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert_eq!(metadata.mode() & 0o777, 0o700);
    // SAFETY: geteuid has no pointer preconditions.
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
}

#[test]
fn pinned_transaction_directory_rejects_a_same_name_replacement() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("transaction");
    create_private_dir(&path).unwrap();
    let pinned = TransactionDir::open(path.clone()).unwrap();
    fs::rename(&path, temporary.path().join("displaced")).unwrap();
    create_private_dir(&path).unwrap();
    let error = pinned.revalidate().unwrap_err();
    assert!(error.to_string().contains("changed identity"), "{error}");
    assert!(path.is_dir(), "replacement must remain untouched");
}
