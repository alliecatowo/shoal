use shoal_kernel::{BoundSocket, ConnectionTrust, Kernel, Limits};
use shoal_leash::Policy;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const EMBEDDED_READY_FRAME: &[u8] = b"{\"shoal_embedded\":{\"ready\":true,\"protocol\":1}}\n";
const HELP: &str = "Shoal resident kernel

Usage:
  shoal-kernel [OPTIONS]

Options:
  --session NAME                         Select the default session
  --socket PATH                          Listen on an explicit Unix socket
  --state-dir PATH                       Override durable state storage
  --token-store PATH                     Override the capability-token authority
  --policy FILE                          Load a sandbox policy
  --embedded-fd FD                       Serve one inherited connected Unix stream
  --detach-stderr-after-ready            Close inherited stderr after the readiness announcement
  --require-token                        Require a bearer on the public socket
  --require-peer-uid                     Require the public peer UID to match this process
  --max-connections N                    Bound simultaneous client connections
  --max-sessions N                       Bound resident sessions
  --max-tasks-per-session N              Bound tasks owned by one session
  --max-ptys-per-session N               Bound PTYs owned by one session
  --max-ptys-per-principal N             Bound PTYs owned by one principal
  --max-ptys-global N                    Bound all resident PTYs
  --max-subscriptions-per-session N      Bound event subscriptions per session
  --max-blob-decompressions-per-window N Bound decompression work in each window
  --blob-decompression-window-ms N       Set the decompression accounting window
  --frame-read-timeout-ms N              Bound time spent receiving one request frame
  -h, --help                             Print this help and exit
  -V, --version                          Print the version and exit

Output:
  Announces readiness on stderr. Embedded mode writes one bounded readiness frame to FD.

Errors:
  Refuses unsafe sockets/descriptors, invalid limits, conflicting transports, and invalid policy/state.

Examples:
  shoal-kernel --session default --require-peer-uid
  shoal-kernel --socket /run/user/1000/shoal/ci.sock --require-token

Exit status:
  0 after an orderly shutdown; 1 for configuration, transport, state, or serving failures.";

/// Canonical parser registry. Help/man parity tests enumerate this table, and
/// `Args::parse` uses it to reject repeated options before interpretation.
const PARSER_OPTIONS: &[(&str, bool)] = &[
    ("--session", true),
    ("--socket", true),
    ("--state-dir", true),
    ("--token-store", true),
    ("--policy", true),
    ("--embedded-fd", true),
    ("--detach-stderr-after-ready", false),
    ("--launch-guard-fd", true),
    ("--launch-guard-token-id", true),
    ("--require-token", false),
    ("--require-peer-uid", false),
    ("--max-connections", true),
    ("--max-sessions", true),
    ("--max-tasks-per-session", true),
    ("--max-ptys-per-session", true),
    ("--max-ptys-per-principal", true),
    ("--max-ptys-global", true),
    ("--max-subscriptions-per-session", true),
    ("--max-blob-decompressions-per-window", true),
    ("--blob-decompression-window-ms", true),
    ("--frame-read-timeout-ms", true),
];

#[cfg(test)]
const INTERNAL_OPTIONS: &[&str] = &["--launch-guard-fd", "--launch-guard-token-id"];

fn main() {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.as_slice() == ["-h"] || args.as_slice() == ["--help"] {
        println!("{HELP}");
        return;
    }
    if args.as_slice() == ["-V"] || args.as_slice() == ["--version"] {
        println!("shoal-kernel {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(error) = run() {
        eprintln!("shoal-kernel: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    let limits = args.resolved_limits();
    let paths = shoal_paths::ShoalPaths::discover();
    let state = args
        .state_dir
        .as_ref()
        .cloned()
        .unwrap_or_else(|| paths.state_dir().to_path_buf());
    let token_store = args
        .token_store
        .as_ref()
        .cloned()
        .unwrap_or_else(|| paths.token_store(&state));
    if let Some((fd, token_id)) = args.launch_guard.as_ref() {
        await_launch_release(*fd, token_id, &token_store)?;
    }
    let mut kernel_builder = Kernel::builder()
        .durable(&state)
        .token_store(&token_store)
        .limits(limits)
        .listener_security(args.require_token, args.require_peer_uid);
    if let Some(path) = args.policy.as_ref() {
        kernel_builder = kernel_builder.policy(Policy::load(path)?);
    }
    let kernel = kernel_builder.build()?;
    if let Some(fd) = args.embedded_fd {
        if args.socket.is_some() {
            return Err("--embedded-fd and --socket are mutually exclusive".into());
        }
        if fd < 3 {
            return Err("--embedded-fd must name a non-stdio descriptor".into());
        }
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
            return Err(format!(
                "--embedded-fd {fd} is not an inherited open descriptor: {}",
                io::Error::last_os_error()
            )
            .into());
        }
        validate_embedded_socket(fd).map_err(|error| {
            format!("--embedded-fd {fd} is not a connected Unix stream socket: {error}")
        })?;
        // Keep the private kernel alive when the terminal delivers Ctrl-C to
        // the foreground process group. This is a caught handler (not
        // SIG_IGN), so exec restores SIG_DFL in command children.
        ctrlc::set_handler(|| {})?;
        // SAFETY: the spawning parent passes ownership of this descriptor
        // exactly once; fcntl above proved it is open in this process.
        let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
        // This transport is private and the host consumes this versioned
        // prelude before constructing its JSON-RPC client. Emitting it only
        // after state/configuration and fd validation makes readiness
        // deterministic and turns early child death into a startup error.
        stream.write_all(EMBEDDED_READY_FRAME)?;
        stream.flush()?;
        kernel.handle_stream_with_trust(stream, ConnectionTrust::EmbeddedHuman)?;
        return Ok(());
    }

    let socket = args.socket.unwrap_or_else(|| paths.socket(&args.session));
    let bound = BoundSocket::bind(&socket)?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
    eprintln!("shoal-kernel: ready {}", socket.display());
    if args.detach_stderr_after_ready {
        detach_stderr()?;
    }
    kernel.serve_bound_until(bound, stop)?;
    Ok(())
}

fn detach_stderr() -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let null = fs::OpenOptions::new().write(true).open("/dev/null")?;
    // SAFETY: both descriptors are valid. `dup2` atomically replaces only
    // this process's stderr; the retained `File` is dropped after duplication.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn validate_embedded_socket(fd: i32) -> io::Result<()> {
    let mut socket_type: libc::c_int = 0;
    let mut socket_type_len = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    // SAFETY: `fd` was proven open above; both output pointers refer to live,
    // correctly-sized stack values. A non-socket fails with ENOTSOCK.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut socket_type).cast(),
            &raw mut socket_type_len,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor is not a stream socket",
        ));
    }

    // `SO_TYPE` alone also accepts listeners and unrelated network sockets.
    // Requiring a connected AF_UNIX peer pins the private transport shape
    // before `from_raw_fd` turns it into trusted `UnixStream` ownership.
    let mut peer = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: the sockaddr storage and length slots are valid writable outputs.
    if unsafe { libc::getpeername(fd, peer.as_mut_ptr().cast(), &raw mut peer_len) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful getpeername initialized at least the family field.
    let peer = unsafe { peer.assume_init() };
    if peer.ss_family as libc::c_int != libc::AF_UNIX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor peer is not AF_UNIX",
        ));
    }
    Ok(())
}

fn await_launch_release(
    fd: i32,
    token_id: &str,
    token_store: &std::path::Path,
) -> Result<(), String> {
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        return Err(format!(
            "launch guard descriptor {fd} is not an inherited open descriptor"
        ));
    }
    // SAFETY: fcntl proved this inherited descriptor open, and the argument
    // transfers its ownership to this process exactly once.
    let mut guard = unsafe { fs::File::from_raw_fd(fd) };
    let mut release = [0_u8; 1];
    let result = guard.read_exact(&mut release);
    if result.is_ok() && release[0] == shoal_mcp_release_byte() {
        return Ok(());
    }

    let reason = match result {
        Ok(()) => "supervisor sent an invalid release frame".to_string(),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            "launch supervisor exited before lifecycle commit".to_string()
        }
        Err(error) => format!("cannot read launch supervisor gate: {error}"),
    };
    let cleanup = shoal_auth::TokenStore::open(token_store)
        .and_then(|mut store| store.revoke(token_id).map(|_| ()))
        .map_err(|error| format!("cannot revoke unreleased managed authority: {error}"));
    match cleanup {
        Ok(()) => Err(reason),
        Err(cleanup) => Err(format!("{reason}; {cleanup}")),
    }
}

const fn shoal_mcp_release_byte() -> u8 {
    // Kept local so the daemon does not depend on the MCP facade crate.
    1
}

#[derive(Debug)]
struct Args {
    session: String,
    socket: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    token_store: Option<PathBuf>,
    policy: Option<PathBuf>,
    embedded_fd: Option<i32>,
    launch_guard: Option<(i32, String)>,
    detach_stderr_after_ready: bool,
    require_token: bool,
    require_peer_uid: bool,
    max_connections: Option<usize>,
    max_sessions: Option<usize>,
    max_tasks_per_session: Option<usize>,
    max_ptys_per_session: Option<usize>,
    max_ptys_per_principal: Option<usize>,
    max_ptys_global: Option<usize>,
    max_subscriptions_per_session: Option<usize>,
    max_blob_decompressions_per_window: Option<usize>,
    blob_decompression_window_ms: Option<u64>,
    frame_read_timeout_ms: Option<u64>,
}
impl Args {
    fn parse(mut it: impl Iterator<Item = std::ffi::OsString>) -> Result<Self, String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut a = Self {
            session: "default".into(),
            socket: None,
            state_dir: None,
            token_store: None,
            policy: None,
            embedded_fd: None,
            launch_guard: None,
            detach_stderr_after_ready: false,
            require_token: false,
            require_peer_uid: false,
            max_connections: None,
            max_sessions: None,
            max_tasks_per_session: None,
            max_ptys_per_session: None,
            max_ptys_per_principal: None,
            max_ptys_global: None,
            max_subscriptions_per_session: None,
            max_blob_decompressions_per_window: None,
            blob_decompression_window_ms: None,
            frame_read_timeout_ms: None,
        };
        let parse_usize = |key: &std::ffi::OsString,
                           value: std::ffi::OsString|
         -> Result<usize, String> {
            value
                .to_str()
                .and_then(|text| text.parse().ok())
                .ok_or_else(|| format!("{} requires a non-negative integer", key.to_string_lossy()))
        };
        while let Some(k) = it.next() {
            let missing = || format!("{} requires a value", k.to_string_lossy());
            let key = k
                .to_str()
                .ok_or_else(|| format!("unknown non-UTF-8 argument {}", k.to_string_lossy()))?;
            if !PARSER_OPTIONS.iter().any(|(name, _)| *name == key) {
                return Err(format!("unknown argument {key}"));
            }
            if !seen.insert(key.to_string()) {
                return Err(format!("{key} may be specified only once"));
            }
            match Some(key) {
                Some("--session") => {
                    a.session = it
                        .next()
                        .ok_or_else(&missing)?
                        .into_string()
                        .map_err(|_| "invalid session")?
                }
                Some("--socket") => a.socket = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--state-dir") => a.state_dir = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--token-store") => {
                    a.token_store = Some(it.next().ok_or_else(&missing)?.into())
                }
                Some("--policy") => a.policy = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--embedded-fd") => {
                    let fd = it
                        .next()
                        .ok_or_else(&missing)?
                        .to_str()
                        .and_then(|text| text.parse().ok())
                        .ok_or_else(|| "--embedded-fd requires an integer".to_string())?;
                    if a.embedded_fd.replace(fd).is_some() {
                        return Err("--embedded-fd may be specified only once".into());
                    }
                }
                Some("--detach-stderr-after-ready") => a.detach_stderr_after_ready = true,
                Some("--launch-guard-fd") => {
                    let fd = it
                        .next()
                        .ok_or_else(&missing)?
                        .to_str()
                        .and_then(|text| text.parse::<i32>().ok())
                        .ok_or_else(|| "--launch-guard-fd requires an integer".to_string())?;
                    if a.launch_guard.replace((fd, String::new())).is_some() {
                        return Err("--launch-guard-fd may be specified only once".into());
                    }
                }
                Some("--launch-guard-token-id") => {
                    let id = it
                        .next()
                        .ok_or_else(&missing)?
                        .into_string()
                        .map_err(|_| "--launch-guard-token-id must be UTF-8")?;
                    if id.is_empty() || id.len() > 256 {
                        return Err("--launch-guard-token-id must be 1..=256 bytes".into());
                    }
                    let Some((_, current)) = a.launch_guard.as_mut() else {
                        return Err(
                            "--launch-guard-token-id requires --launch-guard-fd first".into()
                        );
                    };
                    if !current.is_empty() {
                        return Err("--launch-guard-token-id may be specified only once".into());
                    }
                    *current = id;
                }
                Some("--require-token") => a.require_token = true,
                Some("--require-peer-uid") => a.require_peer_uid = true,
                Some("--max-connections") => {
                    a.max_connections = Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-sessions") => {
                    a.max_sessions = Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-tasks-per-session") => {
                    a.max_tasks_per_session =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-ptys-per-session") => {
                    a.max_ptys_per_session = Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-ptys-per-principal") => {
                    a.max_ptys_per_principal =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-ptys-global") => {
                    a.max_ptys_global = Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-subscriptions-per-session") => {
                    a.max_subscriptions_per_session =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--max-blob-decompressions-per-window") => {
                    a.max_blob_decompressions_per_window =
                        Some(parse_usize(&k, it.next().ok_or_else(&missing)?)?)
                }
                Some("--blob-decompression-window-ms") => {
                    a.blob_decompression_window_ms = Some(
                        it.next()
                            .ok_or_else(&missing)?
                            .to_str()
                            .and_then(|text| text.parse().ok())
                            .ok_or_else(|| {
                                "--blob-decompression-window-ms requires a non-negative integer"
                                    .to_string()
                            })?,
                    )
                }
                Some("--frame-read-timeout-ms") => {
                    a.frame_read_timeout_ms = Some(
                        it.next()
                            .ok_or_else(&missing)?
                            .to_str()
                            .and_then(|text| text.parse().ok())
                            .ok_or_else(|| {
                                "--frame-read-timeout-ms requires a non-negative integer"
                                    .to_string()
                            })?,
                    )
                }
                _ => unreachable!("registry and parser match arms must remain in parity"),
            }
        }
        if a.embedded_fd.is_some() && a.socket.is_some() {
            return Err("--embedded-fd and --socket are mutually exclusive".into());
        }
        if a.embedded_fd.is_some() && (a.require_token || a.require_peer_uid) {
            return Err(
                "--require-token and --require-peer-uid apply only to a named public socket".into(),
            );
        }
        if a.embedded_fd.is_some() && a.detach_stderr_after_ready {
            return Err("--detach-stderr-after-ready applies only to a named public socket".into());
        }
        if a.launch_guard.as_ref().is_some_and(|(_, id)| id.is_empty()) {
            return Err("--launch-guard-fd requires --launch-guard-token-id".into());
        }
        if a.launch_guard.is_some() && a.embedded_fd.is_some() {
            return Err("--launch-guard-fd applies only to a named public socket".into());
        }
        Ok(a)
    }

    fn resolved_limits(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_connections: self.max_connections.unwrap_or(defaults.max_connections),
            max_sessions: self.max_sessions.unwrap_or(defaults.max_sessions),
            max_tasks_per_session: self
                .max_tasks_per_session
                .unwrap_or(defaults.max_tasks_per_session),
            max_ptys_per_session: self
                .max_ptys_per_session
                .unwrap_or(defaults.max_ptys_per_session),
            max_ptys_per_principal: self
                .max_ptys_per_principal
                .unwrap_or(defaults.max_ptys_per_principal),
            max_ptys_global: self.max_ptys_global.unwrap_or(defaults.max_ptys_global),
            max_subscriptions_per_session: self
                .max_subscriptions_per_session
                .unwrap_or(defaults.max_subscriptions_per_session),
            max_blob_decompressions_per_window: self
                .max_blob_decompressions_per_window
                .unwrap_or(defaults.max_blob_decompressions_per_window),
            blob_decompression_window_ms: self
                .blob_decompression_window_ms
                .unwrap_or(defaults.blob_decompression_window_ms),
            frame_read_timeout_ms: self
                .frame_read_timeout_ms
                .unwrap_or(defaults.frame_read_timeout_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd as _;
    use std::path::Path;

    #[test]
    fn public_help_and_man_cover_public_options_while_supervisor_options_stay_internal() {
        let documented = HELP
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("--"))
            .filter_map(|line| line.split_whitespace().next())
            .collect::<std::collections::BTreeSet<_>>();
        let registered = PARSER_OPTIONS
            .iter()
            .filter(|(name, _)| !INTERNAL_OPTIONS.contains(name))
            .map(|(name, _)| *name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(documented, registered, "public help and options diverged");

        let man = include_str!("../../../man/shoal-kernel.1");
        for (name, takes_value) in PARSER_OPTIONS {
            let documented = man.contains(&name.replace("--", "\\-\\-"));
            assert_eq!(
                documented,
                !INTERNAL_OPTIONS.contains(name),
                "man visibility is wrong for {name}"
            );
            let mut invocation = vec![std::ffi::OsString::from(*name)];
            if *takes_value {
                invocation.push(std::ffi::OsString::from(match *name {
                    "--launch-guard-token-id" => "managed-token",
                    _ => "3",
                }));
            }
            let result = Args::parse(invocation.into_iter());
            if let Err(error) = result {
                assert!(
                    !error.contains("unknown"),
                    "registered option was not recognized: {name}: {error}"
                );
            }
        }
        for name in INTERNAL_OPTIONS {
            assert!(!HELP.contains(name), "ordinary help exposed {name}");
        }
    }

    #[test]
    fn parser_rejects_repeated_options_instead_of_silently_overwriting_them() {
        for (name, takes_value) in PARSER_OPTIONS {
            if *name == "--launch-guard-token-id" {
                continue;
            }
            let mut invocation = Vec::new();
            for _ in 0..2 {
                invocation.push(std::ffi::OsString::from(*name));
                if *takes_value {
                    invocation.push(std::ffi::OsString::from("3"));
                }
            }
            let error = Args::parse(invocation.into_iter()).unwrap_err();
            assert!(
                error.contains("only once"),
                "duplicate {name} produced the wrong error: {error}"
            );
        }
        let error = Args::parse(
            [
                "--launch-guard-fd",
                "7",
                "--launch-guard-token-id",
                "first",
                "--launch-guard-token-id",
                "second",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap_err();
        assert!(error.contains("only once"), "{error}");
    }

    #[test]
    fn quota_flags_override_only_the_named_limits() {
        let args = Args::parse(
            [
                "--max-connections",
                "10",
                "--max-sessions",
                "12",
                "--max-ptys-per-session",
                "3",
                "--max-ptys-per-principal",
                "5",
                "--max-ptys-global",
                "20",
                "--max-blob-decompressions-per-window",
                "7",
                "--blob-decompression-window-ms",
                "9000",
                "--frame-read-timeout-ms",
                "2500",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        let limits = args.resolved_limits();
        assert_eq!(limits.max_connections, 10);
        assert_eq!(limits.max_sessions, 12);
        assert_eq!(limits.max_ptys_per_session, 3);
        assert_eq!(limits.max_ptys_per_principal, 5);
        assert_eq!(limits.max_ptys_global, 20);
        assert_eq!(limits.max_blob_decompressions_per_window, 7);
        assert_eq!(limits.blob_decompression_window_ms, 9000);
        assert_eq!(limits.frame_read_timeout_ms, 2500);
        assert_eq!(
            limits.max_tasks_per_session,
            Limits::default().max_tasks_per_session
        );
    }

    #[test]
    fn quota_flags_reject_non_numeric_values() {
        let result = Args::parse(
            ["--max-connections", "many"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert!(result.unwrap_err().contains("--max-connections"));
    }

    #[test]
    fn embedded_fd_may_only_be_supplied_once() {
        let result = Args::parse(
            ["--embedded-fd", "3", "--embedded-fd", "4"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(
            result.unwrap_err(),
            "--embedded-fd may be specified only once"
        );
    }

    #[test]
    fn daemon_stderr_detach_is_listener_only() {
        let listener = Args::parse(
            ["--detach-stderr-after-ready"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert!(listener.detach_stderr_after_ready);

        let embedded = Args::parse(
            ["--embedded-fd", "3", "--detach-stderr-after-ready"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(
            embedded.unwrap_err(),
            "--detach-stderr-after-ready applies only to a named public socket"
        );
    }

    #[test]
    fn launch_guard_requires_an_exact_fd_and_token_id_pair() {
        let missing_id = Args::parse(
            ["--launch-guard-fd", "7"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(
            missing_id.unwrap_err(),
            "--launch-guard-fd requires --launch-guard-token-id"
        );

        let missing_fd = Args::parse(
            ["--launch-guard-token-id", "token-id"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(
            missing_fd.unwrap_err(),
            "--launch-guard-token-id requires --launch-guard-fd first"
        );

        let parsed = Args::parse(
            [
                "--launch-guard-fd",
                "7",
                "--launch-guard-token-id",
                "token-id",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert_eq!(parsed.launch_guard, Some((7, "token-id".into())));
    }

    #[test]
    fn explicit_token_store_is_parsed_without_changing_state_root() {
        let args = Args::parse(
            [
                "--state-dir",
                "/state",
                "--token-store",
                "/authority/tokens.json",
            ]
            .into_iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert_eq!(args.state_dir.as_deref(), Some(Path::new("/state")));
        assert_eq!(
            args.token_store.as_deref(),
            Some(Path::new("/authority/tokens.json"))
        );
    }

    #[test]
    fn embedded_fd_excludes_a_named_socket() {
        let result = Args::parse(
            ["--embedded-fd", "3", "--socket", "/tmp/shoal.sock"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert_eq!(
            result.unwrap_err(),
            "--embedded-fd and --socket are mutually exclusive"
        );
    }

    #[test]
    fn named_listener_security_flags_are_explicit_and_exclude_embedded_mode() {
        let args = Args::parse(
            ["--require-token", "--require-peer-uid"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap();
        assert!(args.require_token);
        assert!(args.require_peer_uid);

        let error = Args::parse(
            ["--embedded-fd", "3", "--require-token"]
                .into_iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap_err();
        assert!(error.contains("named public socket"));
    }

    #[test]
    fn embedded_fd_requires_a_connected_unix_stream() {
        let file = tempfile::tempfile().unwrap();
        assert!(validate_embedded_socket(file.as_raw_fd()).is_err());

        let datagram = std::os::unix::net::UnixDatagram::unbound().unwrap();
        assert!(validate_embedded_socket(datagram.as_raw_fd()).is_err());

        let (stream, _peer) = UnixStream::pair().unwrap();
        assert!(validate_embedded_socket(stream.as_raw_fd()).is_ok());
    }
}
