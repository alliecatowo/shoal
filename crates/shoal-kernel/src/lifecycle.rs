//! Managed daemon status and authenticated shutdown RPCs.

use super::*;

impl Kernel {
    pub(crate) fn handle_kernel_status(
        &self,
        attached: &Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        Ok(json!({
            "pid": std::process::id(),
            "principal": attachment.principal,
            "uptime_ms": self.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "durable": self.state_dir.is_some(),
            "state_dir": self.state_dir.as_ref().map(|path| path.display().to_string()),
            "connections": {
                "active": self.connections.active(),
                "max": self.connections.max(),
            },
            "security": {
                "epoch": ATTACH_SECURITY_EPOCH,
                "connection_trust": attachment.connection_trust.as_str(),
                "raw_local_human": attachment.connection_trust == ConnectionTrust::EmbeddedHuman,
                "bearer_establishes_human_presence": false,
                "machine_admin_credential_required": attachment.connection_trust == ConnectionTrust::Public,
            },
            "shutdown_requested": self.shutdown_requested.load(Ordering::SeqCst),
        }))
    }

    pub(crate) fn handle_kernel_shutdown(
        &self,
        attached: &Option<Attachment>,
    ) -> Result<Json, RpcError> {
        let attachment = attached.as_ref().ok_or_else(not_attached)?;
        if !attachment.can_approve {
            return Err(RpcError {
                code: LEASH_DENIED,
                message: "kernel shutdown requires an embedded human trust root or an explicit supervisor/plan.approve machine credential".into(),
                data: Some(json!({"principal": attachment.principal})),
            });
        }
        let already = self.shutdown_requested.swap(true, Ordering::SeqCst);
        Ok(json!({"stopping": true, "already_requested": already}))
    }
}
