use crate::args::KernelAction;

pub(crate) fn run(action: KernelAction) -> Result<i32, String> {
    let config = shoal_mcp::Config::from_env()?;
    // Retain ownership until the newly started daemon answers a real request.
    // On failure, dropping the guard cleans up the process group. On success,
    // transfer it out of the short-lived CLI so the daemon stays running.
    let autostart =
        matches!(action, KernelAction::Start { .. }).then(|| shoal_mcp::start_kernel(&config));
    let mut client = shoal_mcp::KernelClient::connect(&config).map_err(|error| {
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
        .map_err(|error| error.to_string())?;
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
            result["pid"].as_u64().unwrap_or_default(),
            result["uptime_ms"].as_u64().unwrap_or_default(),
            config.socket.display(),
            result["principal"].as_str().unwrap_or("unknown"),
        );
    }
    Ok(0)
}
