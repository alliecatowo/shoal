//! Typed protocol session used by the kernel-backed interactive surface.
//!
//! Interactive evaluation is submitted as a kernel task even when the user
//! typed a foreground command. That keeps this connection responsive between
//! short `task.get` polls, so Ctrl-C can send `task.cancel` on the same trusted
//! connection without requiring a second human-authorized socket.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

pub(crate) trait KernelRpc {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

impl KernelRpc for shoal_mcp::KernelClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        shoal_mcp::KernelClient::call(self, method, params).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolOutcome {
    pub value_ref: Option<String>,
    pub render: Option<String>,
    pub state: String,
    pub exit_code: Option<i32>,
}

pub(crate) struct ProtocolSession<R> {
    rpc: R,
    poll_interval: Duration,
}

impl<R: KernelRpc> ProtocolSession<R> {
    pub(crate) fn new(rpc: R) -> Self {
        Self {
            rpc,
            poll_interval: Duration::from_millis(20),
        }
    }

    #[cfg(test)]
    fn with_poll_interval(rpc: R, poll_interval: Duration) -> Self {
        Self { rpc, poll_interval }
    }

    pub(crate) fn execute(
        &mut self,
        src: &str,
        interrupt: &AtomicBool,
        width: usize,
    ) -> Result<ProtocolOutcome, String> {
        let submitted = self.rpc.call(
            "exec",
            json!({
                "src": src,
                "mode": "run",
                "position": "stmt",
                "async": true,
            }),
        )?;
        let task = submitted
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| "kernel async exec response omitted task ref".to_string())?
            .to_string();
        let mut cancellation_sent = false;
        loop {
            if interrupt.swap(false, Ordering::SeqCst) && !cancellation_sent {
                self.rpc.call("task.cancel", json!({"task": task}))?;
                cancellation_sent = true;
            }
            let record = self.rpc.call("task.get", json!({"task": task}))?;
            let state = record
                .get("state")
                .and_then(Value::as_str)
                .ok_or_else(|| "kernel task record omitted state".to_string())?;
            if matches!(state, "running" | "cancelling") {
                std::thread::sleep(self.poll_interval);
                continue;
            }
            if state != "cancelled"
                && let Some(error) = record.get("error").filter(|error| !error.is_null())
            {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("kernel task failed");
                return Err(message.to_string());
            }
            let value_ref = record
                .get("result_ref")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let render = if let Some(value_ref) = &value_ref {
                self.rpc
                    .call(
                        "value.get",
                        json!({
                            "ref": value_ref,
                            "path": null,
                            "slice": null,
                            "format": "render",
                            "width": width,
                        }),
                    )?
                    .get("render")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            } else {
                None
            };
            return Ok(ProtocolOutcome {
                value_ref,
                render,
                state: state.to_string(),
                exit_code: record
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok()),
            });
        }
    }

    pub(crate) fn snapshot(&mut self) -> Result<Value, String> {
        self.rpc.call("session.snapshot", json!({}))
    }

    #[cfg(test)]
    fn into_inner(self) -> R {
        self.rpc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeRpc {
        responses: VecDeque<Result<Value, String>>,
        calls: Vec<(String, Value)>,
    }

    impl KernelRpc for FakeRpc {
        fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.push((method.to_string(), params));
            self.responses
                .pop_front()
                .expect("test supplied a response for every call")
        }
    }

    #[test]
    fn foreground_execution_uses_pollable_task_and_fetches_render() {
        let rpc = FakeRpc {
            responses: VecDeque::from([
                Ok(json!({"task":"task:1"})),
                Ok(json!({"state":"running"})),
                Ok(json!({
                    "state":"completed",
                    "result_ref":"out:1",
                    "error":null
                })),
                Ok(json!({"ref":"out:1","render":"42"})),
            ]),
            ..FakeRpc::default()
        };
        let mut session = ProtocolSession::with_poll_interval(rpc, Duration::ZERO);
        let outcome = session
            .execute("40 + 2", &AtomicBool::new(false), 132)
            .unwrap();
        assert_eq!(
            outcome,
            ProtocolOutcome {
                value_ref: Some("out:1".into()),
                render: Some("42".into()),
                state: "completed".into(),
                exit_code: None,
            }
        );
        let rpc = session.into_inner();
        assert_eq!(
            rpc.calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["exec", "task.get", "task.get", "value.get"]
        );
        assert_eq!(rpc.calls[0].1["async"], true);
        assert_eq!(rpc.calls[3].1["width"], 132);
    }

    #[test]
    fn interrupt_cancels_on_the_same_connection_before_next_poll() {
        let rpc = FakeRpc {
            responses: VecDeque::from([
                Ok(json!({"task":"task:9"})),
                Ok(json!({"cancelled":true})),
                Ok(json!({
                    "state":"cancelled",
                    "result_ref":null,
                    "error":{"message":"execution cancelled"}
                })),
            ]),
            ..FakeRpc::default()
        };
        let mut session = ProtocolSession::with_poll_interval(rpc, Duration::ZERO);
        let outcome = session
            .execute("sleep 10s", &AtomicBool::new(true), 80)
            .unwrap();
        assert_eq!(outcome.state, "cancelled");
        assert_eq!(outcome.exit_code, None);
        let rpc = session.into_inner();
        assert_eq!(
            rpc.calls
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["exec", "task.cancel", "task.get"]
        );
    }

    #[test]
    fn task_error_is_returned_without_attempting_value_fetch() {
        let rpc = FakeRpc {
            responses: VecDeque::from([
                Ok(json!({"task":"task:2"})),
                Ok(json!({
                    "state":"failed",
                    "result_ref":null,
                    "error":{"message":"raised boom"}
                })),
            ]),
            ..FakeRpc::default()
        };
        let mut session = ProtocolSession::with_poll_interval(rpc, Duration::ZERO);
        assert_eq!(
            session
                .execute("raise error(\"boom\")", &AtomicBool::new(false), 80)
                .unwrap_err(),
            "raised boom"
        );
        assert_eq!(session.into_inner().calls.len(), 2);
    }

    #[test]
    fn exit_code_is_preserved_for_the_interactive_driver() {
        let rpc = FakeRpc {
            responses: VecDeque::from([
                Ok(json!({"task":"task:3"})),
                Ok(json!({
                    "state":"completed",
                    "result_ref":null,
                    "error":null,
                    "exit_code":7
                })),
            ]),
            ..FakeRpc::default()
        };
        let mut session = ProtocolSession::with_poll_interval(rpc, Duration::ZERO);
        let outcome = session
            .execute("exit 7", &AtomicBool::new(false), 80)
            .unwrap();
        assert_eq!(outcome.exit_code, Some(7));
    }
}
