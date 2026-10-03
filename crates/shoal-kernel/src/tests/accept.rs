use super::*;

/// Regression (audit L2): any accept error used to terminate the daemon.
#[test]
fn transient_accept_errors_do_not_stop_the_listener() {
    for kind in [io::ErrorKind::ConnectionAborted, io::ErrorKind::Interrupted] {
        assert!(
            is_transient_accept_error(&io::Error::from(kind)),
            "{kind:?}"
        );
    }
    assert!(is_transient_accept_error(&io::Error::from_raw_os_error(
        libc::EMFILE
    )));
    assert!(!is_transient_accept_error(&io::Error::from(
        io::ErrorKind::PermissionDenied
    )));
}

/// Regression (audit L1): the socket is created under a 0177 umask.
#[test]
fn serve_binds_a_private_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("k.sock");
    let kernel = Kernel::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (k, s, p) = (kernel.clone(), stop.clone(), socket.clone());
    let server = std::thread::spawn(move || k.serve_until(p, s));
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap().unwrap();
}
