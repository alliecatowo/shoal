use crate::cli::args::KernelAction;

pub(crate) fn run(action: KernelAction) -> Result<i32, String> {
    let config = crate::mcp::Config::from_env()?;
    // Retain ownership until the newly started daemon answers a real request.
    // On failure, dropping the guard cleans up the process group. On success,
    // transfer it out of the short-lived CLI so the daemon stays running.
    let autostart =
        matches!(action, KernelAction::Start { .. }).then(|| crate::mcp::start_kernel(&config));
    let mut client = crate::mcp::KernelClient::connect(&config).map_err(|error| {
        format!(
            "kernel is not reachable at {}: {error}",
            config.socket.display()
        )
    })?;
    let (method, json_output) = match action {
        KernelAction::Start { json } | KernelAction::Status { json } => ("kernel.status", json),
        KernelAction::Stop { json } => ("kernel.shutdown", json),
    };
    let result = client
        .call(method, serde_json::json!({}))
        .map_err(|error| describe_call_error(method, &error))?;
    if let Some(autostart) = autostart {
        // Dropping a Child handle does not kill the process. Once this command
        // exits the durable daemon is adopted by the user's process manager.
        drop(autostart.into_child());
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?
        );
    } else if method == "kernel.shutdown" {
        println!("kernel stopping (pid authority authenticated)");
    } else {
        println!(
            "kernel running: pid={} uptime={}ms socket={} principal={}",
            result["pid"]
                .as_u64()
                .map_or_else(|| "hidden".to_string(), |pid| pid.to_string()),
            result["uptime_ms"].as_u64().unwrap_or_default(),
            config.socket.display(),
            result["principal"].as_str().unwrap_or("unknown"),
        );
    }
    Ok(0)
}

/// Turn a raw kernel RPC error into something the user can act on. The CLI
/// attaches as a restricted agent unless `SHOAL_TOKEN` names a stronger
/// credential, so `stop` is refused by default; say how to fix that.
fn describe_call_error(method: &str, error: &crate::mcp::BridgeError) -> String {
    if method == "kernel.shutdown"
        && let crate::mcp::BridgeError::Kernel(value) = error
        && value.to_string().contains("kernel shutdown requires")
    {
        return "the kernel refused to stop: this client is attached with a restricted credential.\n\
                Create a supervisor token and retry:\n  \
                SHOAL_TOKEN=$(shoal-token create supervisor-cli supervisor --ttl 600) shoal kernel stop\n\
                (or stop the daemon through the process manager that started it)"
            .to_string();
    }
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_stop_explains_how_to_authenticate() {
        let denied = crate::mcp::BridgeError::Kernel(serde_json::json!({
            "message": "kernel shutdown requires an embedded human trust root or an explicit supervisor/plan.approve machine credential"
        }));
        let text = describe_call_error("kernel.shutdown", &denied);
        assert!(text.contains("supervisor"), "{text}");
        assert!(text.contains("SHOAL_TOKEN"), "{text}");
        let other = describe_call_error("kernel.status", &denied);
        assert!(other.starts_with("kernel error"), "{other}");
    }
}
