//! Public and inherited transport lifecycle for a configured kernel.

use super::*;

impl Kernel {
    pub(crate) fn reserve_blob_decompression(&self, session: &Session) -> Result<(), RpcError> {
        session.reserve_blob_decompression(
            self.admission
                .max_blob_decompressions_per_window
                .load(Ordering::Relaxed),
            std::time::Duration::from_millis(
                self.admission
                    .blob_decompression_window_ms
                    .load(Ordering::Relaxed),
            ),
        )
    }

    pub(crate) fn reserve_connection_slot(&self) -> Result<ConnectionPermit, ()> {
        self.admission.connections.reserve()
    }

    pub fn serve(self: Arc<Self>, path: impl AsRef<Path>) -> io::Result<()> {
        self.serve_until(path, Arc::new(AtomicBool::new(false)))
    }

    pub fn serve_until(
        self: Arc<Self>,
        path: impl AsRef<Path>,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        let bound = BoundSocket::bind(path.as_ref())?;
        self.serve_bound_until(bound, stop)
    }

    /// Serve an already-bound public socket. This permits daemon frontends to
    /// announce readiness only after secure atomic socket publication.
    pub fn serve_bound_until(
        self: Arc<Self>,
        bound: BoundSocket,
        stop: Arc<AtomicBool>,
    ) -> io::Result<()> {
        self.serve_bound_until_with_spawner(bound, stop, &ThreadSpawner)
    }

    pub(crate) fn serve_bound_until_with_spawner(
        self: Arc<Self>,
        bound: BoundSocket,
        stop: Arc<AtomicBool>,
        spawner: &dyn ConnectionSpawner,
    ) -> io::Result<()> {
        if self.authority.require_peer_uid.load(Ordering::SeqCst) && !peer::supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "--require-peer-uid is not supported on this platform",
            ));
        }
        let listener = bound.listener();
        listener.set_nonblocking(true)?;
        let mut consecutive_spawn_failures = 0_u32;
        while !stop.load(Ordering::SeqCst)
            && !self.lifecycle.shutdown_requested.load(Ordering::SeqCst)
        {
            let kernel = self.clone();
            match listener.accept() {
                Ok((stream, _)) => {
                    // The listener is non-blocking so the accept loop can poll
                    // `stop`, but that non-blocking flag is inherited by the
                    // accepted stream on some platforms (e.g. macOS) and not
                    // others (e.g. Linux, where accepted sockets are always
                    // blocking regardless of the listener's flag). Explicitly
                    // force the accepted connection back into blocking mode so
                    // per-connection reads in `handle_stream` block as intended
                    // on every platform, instead of racing the client's next
                    // write and getting a transient `WouldBlock` misread as EOF.
                    if let Err(error) = stream.set_nonblocking(false) {
                        eprintln!("shoal-kernel: rejected public connection: {error}");
                        continue;
                    }
                    if kernel.authority.require_peer_uid.load(Ordering::SeqCst)
                        && let Err(error) = peer::require_matching_effective_uid(&stream)
                    {
                        eprintln!("shoal-kernel: rejected public peer: {error}");
                        continue;
                    }
                    let slot = match kernel.reserve_connection_slot() {
                        Ok(slot) => slot,
                        Err(()) => {
                            let max = kernel.admission.connections.max();
                            let _ = reject_connection_over_quota(stream, max);
                            continue;
                        }
                    };
                    let job = Box::new(move || {
                        let _slot = slot;
                        let _ = kernel.handle_stream_with_trust(stream, ConnectionTrust::Public);
                    });
                    match spawner.spawn(job) {
                        Ok(()) => consecutive_spawn_failures = 0,
                        Err(error) => {
                            consecutive_spawn_failures =
                                consecutive_spawn_failures.saturating_add(1);
                            eprintln!("shoal-kernel: connection worker spawn failed: {error}");
                            std::thread::sleep(failure_backoff(consecutive_spawn_failures));
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(25))
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn handle_stream(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
        self.handle_stream_with_trust(stream, ConnectionTrust::Public)
    }

    /// Service one already-connected stream under server-selected trust.
    /// Public listeners must always pass [`ConnectionTrust::Public`].
    pub fn handle_stream_with_trust(
        self: &Arc<Self>,
        stream: UnixStream,
        trust: ConnectionTrust,
    ) -> io::Result<()> {
        let client = self.admission.connections.next_client();
        let mut reader = BufReader::new(stream.try_clone()?);
        let writer: SharedWriter = Arc::new(Mutex::new(stream));
        let mut attached: Option<Attachment> = None;
        let result = (|| -> io::Result<()> {
            loop {
                let timeout_ms = self.admission.connections.frame_read_timeout_ms();
                // Before authentication, a client must begin its first frame
                // within the deadline. Once attached, an entirely idle client
                // may remain subscribed indefinitely; after the first byte of
                // any new frame, however, the same deadline bounds completion.
                let bearer_idle_recheck = attached
                    .as_ref()
                    .is_some_and(|attachment| attachment.bearer.is_some());
                let wait_timeout = if attached.is_none() && timeout_ms != 0 {
                    Some(timeout_ms)
                } else if bearer_idle_recheck {
                    // Disabling the frame deadline must not disable bearer
                    // revocation. Otherwise use the configured deadline as
                    // the maximum stale-authority window as well.
                    Some(if timeout_ms == 0 { 10_000 } else { timeout_ms })
                } else {
                    None
                };
                set_read_deadline(reader.get_ref(), wait_timeout)?;
                match reader.fill_buf() {
                    Ok([]) => break,
                    Ok(_) => {}
                    Err(error) if bearer_idle_recheck && is_read_timeout(&error) => {
                        let validity = attached
                            .as_ref()
                            .expect("bearer recheck requires an attachment");
                        if let Err(error) = self.ensure_attachment_current(validity) {
                            self.runtime.events.remove_conn(client);
                            attached = None;
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                error.message,
                            ));
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                }
                set_read_deadline(reader.get_ref(), (timeout_ms != 0).then_some(timeout_ms))?;
                let Some(request) = read_frame(&mut reader)? else {
                    break;
                };
                let id = request.id.clone();
                let response = if request.jsonrpc != JSONRPC {
                    Response::err(id, INVALID_REQUEST, "invalid JSON-RPC version", None)
                } else {
                    self.dispatch(request, client, &mut attached, Some(&writer), trust)
                };
                // A poisoned writer may contain a partially-written JSON
                // frame. Close this connection rather than recovering the
                // guard and corrupting framing for subsequent responses.
                let mut writer = writer
                    .lock()
                    .map_err(|_| io::Error::other("connection writer poisoned"))?;
                write_frame(&mut *writer, &response)?;
            }
            Ok(())
        })();
        // On disconnect, drop this connection's subscriptions so publish never
        // writes to a dead fd.
        self.runtime.events.remove_conn(client);
        normalize_attached_disconnect(result, attached.is_some())
    }
}

fn reject_connection_over_quota(mut stream: UnixStream, max_connections: usize) -> io::Result<()> {
    stream.set_write_timeout(Some(std::time::Duration::from_millis(100)))?;
    write_frame(
        &mut stream,
        &Response::err(
            Json::Null,
            QUOTA_EXCEEDED,
            format!("kernel connection limit ({max_connections}) reached"),
            Some(json!({"limit":"connections", "max":max_connections})),
        ),
    )
}
