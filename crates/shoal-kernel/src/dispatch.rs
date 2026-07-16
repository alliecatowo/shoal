//! `Kernel::dispatch` — the RPC method router (TDD §10, AGENT-SURFACE §5).
//! Split out of `lib.rs` (docs/ROADMAP.md wave R4, scratch/audit-arch.md
//! §3c/W1.3): pure mechanical move, zero wire/behavior change. Each arm's
//! body now lives in a `handle_<method>` function (see `handlers_session.rs`,
//! `handlers_exec.rs`, `handlers_value.rs`, `handlers_task.rs`) so this stays
//! a thin router over the method name.
use super::*;

impl Kernel {
    pub(crate) fn dispatch(
        self: &Arc<Self>,
        request: Request,
        client: u64,
        attached: &mut Option<Attachment>,
        conn: Option<&SharedWriter>,
    ) -> Response {
        let id = request.id;
        let params = request.params;
        let result: Result<Json, RpcError> = match request.method.as_str() {
            "session.attach" => self.handle_session_attach(params, attached),
            "session.env" => self.handle_session_env(attached),
            "session.reef" => self.handle_session_reef(attached),
            "parse" => self.handle_parse(params),
            "exec" => self.handle_exec(params, client, attached),
            "value.get" => self.handle_value_get(params, attached),
            "task.list" => self.handle_task_list(attached),
            "task.get" => self.handle_task_get(params, attached),
            "task.await" => self.handle_task_await(params, attached),
            "task.cancel" => self.handle_task_cancel(params, attached),
            "task.suspend" => self.handle_task_suspend(params, attached),
            "task.resume" => self.handle_task_resume(params, attached),
            "pty.open" => self.handle_pty_open(params, attached),
            "pty.send" => self.handle_pty_send(params, attached),
            "pty.read" => self.handle_pty_read(params, attached),
            "pty.resize" => self.handle_pty_resize(params, attached),
            "pty.close" => self.handle_pty_close(params, attached),
            "pty.list" => self.handle_pty_list(attached),
            "plan.get" => self.handle_plan_get(params, attached),
            "plan.list" => self.handle_plan_list(attached),
            "plan.apply" => self.handle_plan_apply(params, client, attached, conn),
            "cap.request" => self.handle_cap_request(params),
            "journal.query" => self.handle_journal_query(params),
            "events.read" => self.handle_events_read(params, attached),
            "events.publish" => self.handle_events_publish(params, attached),
            "events.subscribe" => self.handle_events_subscribe(params, client, attached, conn),
            "events.unsubscribe" => self.handle_events_unsubscribe(params, client, attached),
            "blob.get" => self.handle_blob_get(params, attached),
            "complete" => self.handle_complete(params),
            "explain" => self.handle_explain(params, attached),
            _ => Err(RpcError {
                code: METHOD_NOT_FOUND,
                message: "method not found".into(),
                data: None,
            }),
        };
        match result {
            Ok(value) => Response::ok(id, value),
            Err(error) => Response {
                jsonrpc: JSONRPC.into(),
                id,
                result: None,
                error: Some(error),
            },
        }
    }
}
