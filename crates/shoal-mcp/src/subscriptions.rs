//! One multiplexed kernel connection/thread for all MCP resource subscriptions.

use crate::{BridgeError, Config, KernelClient, write_stdout_frame};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::Duration;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(6);

type Reply = SyncSender<Result<(), String>>;

enum Command {
    Add {
        uri: String,
        channel: String,
        reply: Reply,
    },
    Remove {
        uri: String,
        channel: String,
        reply: Reply,
    },
}

struct SubscriptionFailure {
    message: String,
    fatal: bool,
}

/// Owns the facade's single subscription transport and forwarding worker.
/// Commands are synchronous from the facade's request loop, so the bounded
/// queue can never retain more than one lifecycle mutation.
pub(crate) struct SubscriptionHub {
    commands: SyncSender<Command>,
    wake: UnixStream,
    interrupt: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl SubscriptionHub {
    pub(crate) fn connect(config: &Config) -> Result<Self, String> {
        let client = KernelClient::connect(config)
            .map_err(|error| safe_bridge_error(&error, "subscription connection"))?;
        let interrupt = client
            .shutdown_handle()
            .map_err(|_| "kernel subscription shutdown handle failed".to_string())?;
        let (wake_reader, wake) =
            UnixStream::pair().map_err(|_| "kernel subscription wakeup pair failed".to_string())?;
        wake_reader
            .set_nonblocking(true)
            .map_err(|_| "kernel subscription wakeup reader failed".to_string())?;
        wake.set_nonblocking(true)
            .map_err(|_| "kernel subscription wakeup writer failed".to_string())?;
        let (commands, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("shoal-mcp-subscriptions".into())
            .spawn(move || run(client, receiver, wake_reader))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            commands,
            wake,
            interrupt,
            thread: Some(thread),
        })
    }

    pub(crate) fn add(&self, uri: String, channel: String) -> Result<(), String> {
        self.command(|reply| Command::Add {
            uri,
            channel,
            reply,
        })
    }

    pub(crate) fn remove(&self, uri: String, channel: String) -> Result<(), String> {
        self.command(|reply| Command::Remove {
            uri,
            channel,
            reply,
        })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn command(&self, build: impl FnOnce(Reply) -> Command) -> Result<(), String> {
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .send(build(reply))
            .map_err(|_| "kernel subscription worker disconnected".to_string())?;
        (&self.wake)
            .write_all(&[1])
            .map_err(|_| "kernel subscription worker wakeup failed".to_string())?;
        response.recv_timeout(CONTROL_REPLY_TIMEOUT).map_err(|_| {
            "kernel subscription worker did not acknowledge lifecycle change".to_string()
        })?
    }
}

impl Drop for SubscriptionHub {
    fn drop(&mut self) {
        let _ = self.interrupt.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(mut client: KernelClient, commands: Receiver<Command>, mut wake: UnixStream) {
    let mut resources: HashMap<String, BTreeSet<String>> = HashMap::new();
    loop {
        let mut ready = [
            libc::pollfd {
                fd: client.read_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `ready` points to two initialized pollfd values for the
        // duration of the call. Both descriptors remain owned by this worker.
        let polled = unsafe { libc::poll(ready.as_mut_ptr(), ready.len() as libc::nfds_t, -1) };
        if polled < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if ready[1].revents != 0 {
            drain_wakeup(&mut wake);
            loop {
                match commands.try_recv() {
                    Ok(command) => {
                        if !handle_command(&mut client, &mut resources, command) {
                            return;
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            // A lifecycle call may consume kernel frames that made the old
            // readiness bit true. Repoll instead of risking a blocking read.
            continue;
        }
        if ready[0].revents != 0 {
            match client.read_frame() {
                Ok(Some(frame)) => forward_event(&frame, &resources),
                Ok(None) | Err(_) => return,
            }
        }
    }
}

fn drain_wakeup(wake: &mut UnixStream) {
    let mut bytes = [0_u8; 64];
    loop {
        match wake.read(&mut bytes) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(_) => return,
        }
    }
}

fn handle_command(
    client: &mut KernelClient,
    resources: &mut HashMap<String, BTreeSet<String>>,
    command: Command,
) -> bool {
    match command {
        Command::Add {
            uri,
            channel,
            reply,
        } => {
            if resources
                .get(&channel)
                .is_some_and(|uris| uris.contains(&uri))
            {
                let _ = reply.send(Ok(()));
                return true;
            }
            let first = !resources.contains_key(&channel);
            resources
                .entry(channel.clone())
                .or_default()
                .insert(uri.clone());
            let result = if first {
                subscription_call(client, resources, "events.subscribe", &channel)
            } else {
                Ok(())
            };
            if result.is_err()
                && let Some(uris) = resources.get_mut(&channel)
            {
                uris.remove(&uri);
                if uris.is_empty() {
                    resources.remove(&channel);
                }
            }
            reply_with_lifecycle(result, reply)
        }
        Command::Remove {
            uri,
            channel,
            reply,
        } => {
            let Some(uris) = resources.get(&channel) else {
                let _ = reply.send(Ok(()));
                return true;
            };
            if !uris.contains(&uri) {
                let _ = reply.send(Ok(()));
                return true;
            }
            let last = uris.len() == 1;
            let result = if last {
                subscription_call(client, resources, "events.unsubscribe", &channel)
            } else {
                Ok(())
            };
            if result.is_ok()
                && let Some(uris) = resources.get_mut(&channel)
            {
                uris.remove(&uri);
                if uris.is_empty() {
                    resources.remove(&channel);
                }
            }
            reply_with_lifecycle(result, reply)
        }
    }
}

fn reply_with_lifecycle(result: Result<(), SubscriptionFailure>, reply: Reply) -> bool {
    let fatal = result.as_ref().is_err_and(|error| error.fatal);
    let _ = reply.send(result.map_err(|error| error.message));
    !fatal
}

fn subscription_call(
    client: &mut KernelClient,
    resources: &HashMap<String, BTreeSet<String>>,
    method: &str,
    channel: &str,
) -> Result<(), SubscriptionFailure> {
    client
        .set_read_timeout(Some(CONTROL_TIMEOUT))
        .map_err(|_| SubscriptionFailure {
            message: "kernel subscription timeout configuration failed".into(),
            fatal: true,
        })?;
    let result = client
        .call_with_notifications(method, json!({"channel": channel}), |frame| {
            forward_event(frame, resources);
        })
        .map(|_| ())
        .map_err(|error| SubscriptionFailure {
            message: safe_bridge_error(&error, "subscription"),
            fatal: !matches!(error, BridgeError::Kernel(_)),
        });
    let restore = client
        .set_read_timeout(None)
        .map_err(|_| SubscriptionFailure {
            message: "kernel subscription timeout restoration failed".into(),
            fatal: true,
        });
    result.and(restore)
}

fn forward_event(frame: &Value, resources: &HashMap<String, BTreeSet<String>>) {
    for note in resource_updates(frame, resources) {
        let _ = write_stdout_frame(&note);
    }
}

fn resource_updates(frame: &Value, resources: &HashMap<String, BTreeSet<String>>) -> Vec<Value> {
    if frame.get("method").and_then(Value::as_str) != Some("event") {
        return Vec::new();
    }
    let params = frame.get("params").cloned().unwrap_or(Value::Null);
    let Some(channel) = params.get("channel").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(uris) = resources.get(channel) else {
        return Vec::new();
    };
    uris.iter()
        .map(|uri| {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/updated",
                "params": {
                    "uri": uri,
                    "seq": params.get("seq"),
                    "payload": params.get("payload"),
                }
            })
        })
        .collect()
}

fn safe_bridge_error(error: &BridgeError, operation: &str) -> String {
    match error {
        BridgeError::Kernel(error) => match error.get("code").and_then(Value::as_i64) {
            Some(code) => format!("kernel rejected {operation} request (code {code})"),
            None => format!("kernel rejected {operation} request"),
        },
        _ => format!("kernel {operation} failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_kernel_event_routes_to_every_exact_uri_in_stable_order() {
        let resources = HashMap::from([
            (
                "task.7".into(),
                BTreeSet::from(["shoal://events/task.7".into(), "shoal://task/7".into()]),
            ),
            (
                "user.other".into(),
                BTreeSet::from(["shoal://events/user.other".into()]),
            ),
        ]);
        let frame = json!({
            "method": "event",
            "params": {
                "channel": "task.7",
                "seq": 7,
                "payload": {"$":"str", "v":"ready"}
            }
        });

        let updates = resource_updates(&frame, &resources);
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates
                .iter()
                .map(|update| update["params"]["uri"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["shoal://events/task.7", "shoal://task/7"]
        );
        assert!(updates.iter().all(|update| {
            update["params"]["seq"] == 7
                && update["params"]["payload"] == json!({"$":"str", "v":"ready"})
        }));
    }
}
