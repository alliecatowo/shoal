//! `Kernel::dispatch` — the RPC method router. See
//! `site/content/internals/kernel-protocol.md` and
//! `site/content/internals/intercrate-protocol-contracts.md`. Each arm's
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
        let method = request.method;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || -> Result<Json, RpcError> {
                // Reattachment is always available as the recovery path. All
                // other requests, including pure parse/complete calls, must
                // revalidate the authority of an established attachment.
                if method != "session.attach"
                    && let Some(attachment) = attached.as_ref()
                    && let Err(error) = self.ensure_attachment_current(attachment)
                {
                    self.events.remove_conn(client);
                    *attached = None;
                    return Err(error);
                }
                // `parse` and `complete` are session-independent, and
                // `session.attach` must remain available so this connection
                // can move to a different healthy session.
                if !matches!(method.as_str(), "parse" | "complete" | "session.attach")
                    && let Some(attachment) = attached.as_ref()
                {
                    attachment.session.ensure_healthy()?;
                    attachment.session.touch();
                }
                match method.as_str() {
                    "session.attach" => self.handle_session_attach(params, client, attached),
                    "session.env" => self.handle_session_env(attached),
                    "session.reef" => self.handle_session_reef(attached),
                    "kernel.status" => self.handle_kernel_status(attached),
                    "kernel.shutdown" => self.handle_kernel_shutdown(attached),
                    "parse" => self.handle_parse(params),
                    "exec" => self.handle_exec(params, client, attached),
                    "value.get" => self.handle_value_get(params, attached),
                    "stream.pull" => self.handle_stream_pull(params, attached),
                    "stream.close" => self.handle_stream_close(params, attached),
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
                    "cap.request" => self.handle_cap_request(params, attached),
                    "journal.query" => self.handle_journal_query(params, attached),
                    "events.read" => self.handle_events_read(params, attached),
                    "events.publish" => self.handle_events_publish(params, attached),
                    "events.subscribe" => {
                        self.handle_events_subscribe(params, client, attached, conn)
                    }
                    "events.unsubscribe" => {
                        self.handle_events_unsubscribe(params, client, attached)
                    }
                    "blob.get" => self.handle_blob_get(params, attached),
                    "complete" => self.handle_complete(params),
                    "explain" => self.handle_explain(params, attached),
                    #[cfg(test)]
                    "test.panic_evaluator" => {
                        let attachment = attached.as_ref().ok_or_else(not_attached)?;
                        let _evaluator = attachment.session.evaluator.lock().unwrap();
                        panic!("injected evaluator panic")
                    }
                    _ => Err(RpcError {
                        code: METHOD_NOT_FOUND,
                        message: "method not found".into(),
                        data: None,
                    }),
                }
            },
        ));
        let result = match result {
            Ok(result) => result,
            Err(_) => {
                let quarantined = if let Some(attachment) = attached.as_ref() {
                    attachment.session.quarantine();
                    true
                } else {
                    false
                };
                eprintln!(
                    "shoal-kernel: request handler panicked; session quarantined={quarantined}"
                );
                Err(RpcError {
                    code: INTERNAL_ERROR,
                    message: "request handler panicked".into(),
                    data: Some(json!({"session_quarantined": quarantined})),
                })
            }
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

impl Kernel {
    pub(crate) fn ensure_attachment_current(
        &self,
        attachment: &Attachment,
    ) -> Result<(), RpcError> {
        if attachment.security_epoch != ATTACH_SECURITY_EPOCH {
            return Err(RpcError {
                code: AUTH_FAILED,
                message: "attachment security epoch is no longer accepted".into(),
                data: Some(json!({
                    "attached_epoch": attachment.security_epoch,
                    "required_epoch": ATTACH_SECURITY_EPOCH,
                })),
            });
        }
        let Some(meta) = &attachment.bearer else {
            return Ok(());
        };
        let valid = self
            .auth
            .as_ref()
            .and_then(|store| store.lock().ok())
            .and_then(|store| store.refresh_authenticated(meta))
            .is_some();
        if valid {
            Ok(())
        } else {
            Err(RpcError {
                code: AUTH_FAILED,
                message: "attached bearer is expired, revoked, or unavailable".into(),
                data: Some(json!({"reauthenticate": true})),
            })
        }
    }
}
