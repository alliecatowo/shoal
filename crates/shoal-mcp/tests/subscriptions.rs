//! Live ownership and scaling proof for the multiplexed MCP subscription hub.

use serde_json::{Value, json};
use shoal_kernel::{Kernel, Limits};
use shoal_leash::Policy;
use shoal_mcp::{Config, Facade, LocalAuthMode};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct LiveKernel {
    socket: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl LiveKernel {
    fn start(max_connections: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/kernel.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let kernel = Kernel::with_policy(Policy::permissive("agent:mcp"));
        kernel.configure_limits(Limits {
            max_connections,
            ..Limits::default()
        });
        let serve_socket = socket.clone();
        let serve_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            kernel.serve_until(&serve_socket, serve_stop).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if UnixStream::connect(&socket).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "kernel must accept its socket");
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            socket,
            stop,
            handle: Some(handle),
            _dir: dir,
        }
    }

    fn config(&self) -> Config {
        Config {
            socket: self.socket.clone(),
            session: Some("default".into()),
            token: None,
            local_auth: LocalAuthMode::RestrictedAgent,
        }
    }
}

impl Drop for LiveKernel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn resource_subscription(facade: &mut Facade, method: &str, uri: &str) -> Value {
    facade
        .handle(&json!({
            "jsonrpc":"2.0",
            "id":77,
            "method":method,
            "params":{"uri":uri}
        }))
        .expect("subscription request has a response")
}

#[test]
fn unsubscribe_reclaims_exact_routes_across_heavy_churn() {
    let live = LiveKernel::start(Limits::default().max_connections);
    let mut facade = Facade::connect(&live.config()).unwrap();
    let uri = "shoal://events/user.lifecycle";

    for _ in 0..160 {
        let subscribed = resource_subscription(&mut facade, "resources/subscribe", uri);
        assert!(subscribed.get("error").is_none(), "{subscribed}");
        assert_eq!(facade.active_subscriptions(), 1);

        let duplicate = resource_subscription(&mut facade, "resources/subscribe", uri);
        assert!(duplicate.get("error").is_none(), "{duplicate}");
        assert_eq!(facade.active_subscriptions(), 1);

        let unsubscribed = resource_subscription(&mut facade, "resources/unsubscribe", uri);
        assert!(unsubscribed.get("error").is_none(), "{unsubscribed}");
        assert_eq!(facade.active_subscriptions(), 0);
    }

    let absent = resource_subscription(&mut facade, "resources/unsubscribe", uri);
    assert!(absent.get("error").is_none(), "{absent}");
}

#[test]
fn multiple_resource_uris_share_one_kernel_connection_and_worker() {
    // One request/response connection plus exactly one subscription hub. The
    // old per-URI connection model cannot admit the second URI at this limit.
    let live = LiveKernel::start(2);
    let mut facade = Facade::connect(&live.config()).unwrap();
    for uri in ["shoal://events/user.one", "shoal://events/user.two"] {
        let response = resource_subscription(&mut facade, "resources/subscribe", uri);
        assert!(response.get("error").is_none(), "{response}");
    }
    assert_eq!(facade.active_subscriptions(), 2);

    let removed = resource_subscription(
        &mut facade,
        "resources/unsubscribe",
        "shoal://events/user.one",
    );
    assert!(removed.get("error").is_none(), "{removed}");
    assert_eq!(facade.active_subscriptions(), 1);

    let third = resource_subscription(
        &mut facade,
        "resources/subscribe",
        "shoal://events/user.three",
    );
    assert!(third.get("error").is_none(), "{third}");
    assert_eq!(facade.active_subscriptions(), 2);
}
