use crate::args::KernelAction;
use std::os::unix::net::UnixStream;

mod managed;

pub(crate) fn run(action: KernelAction) -> Result<i32, String> {
    let mut config = shoal_mcp::Config::from_env()?;
    let already_running = UnixStream::connect(&config.socket).is_ok();
    let mut lifecycle_credential = match action {
        KernelAction::Start { .. } if !already_running && config.token.is_none() => {
            managed::Credential::provision(&config.socket)?
        }
        KernelAction::Stop { .. } if config.token.is_none() => {
            managed::Credential::load(&config.socket)?
        }
        _ => None,
    };
    if let Some(credential) = &lifecycle_credential {
        config.token = Some(credential.token().into());
    }
    // Retain ownership until the newly started daemon answers a real request.
    // On failure, dropping the guard cleans up the process group. On success,
    // transfer it out of the short-lived CLI so the daemon stays running.
    let autostart = if matches!(action, KernelAction::Start { .. }) {
        if let Some(credential) = lifecycle_credential.as_mut() {
            let id = credential.id().to_owned();
            match shoal_mcp::start_managed_kernel(&config, &id, |pid| credential.bind_pid(pid)) {
                Ok(guard) => Some(guard),
                Err(error) => {
                    return Err(cleanup_managed_start_error(
                        error,
                        lifecycle_credential.take(),
                    ));
                }
            }
        } else {
            Some(shoal_mcp::start_kernel(&config))
        }
    } else {
        None
    };
    let mut client = match shoal_mcp::KernelClient::connect(&config) {
        Ok(client) => client,
        Err(error) => {
            let primary = format!(
                "kernel is not reachable at {}: {error}",
                config.socket.display()
            );
            return if matches!(action, KernelAction::Start { .. }) {
                Err(cleanup_managed_start_error(
                    primary,
                    lifecycle_credential.take(),
                ))
            } else {
                Err(primary)
            };
        }
    };
    let (method, json_output) = match action {
        KernelAction::Start { json } | KernelAction::Status { json } => ("kernel.status", json),
        KernelAction::Stop { json } => ("kernel.shutdown", json),
    };
    if matches!(action, KernelAction::Stop { .. })
        && let Some(credential) = &lifecycle_credential
    {
        let status = client
            .call("kernel.status", serde_json::json!({}))
            .map_err(|error| error.to_string())?;
        let answering_pid = status["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok());
        if !managed_start_pids_match(credential.pid(), answering_pid) {
            let credential = lifecycle_credential
                .take()
                .ok_or_else(|| "managed kernel stop lost its lifecycle credential".to_string())?;
            let cleanup = credential.revoke_and_remove();
            let detail = cleanup.err().map_or_else(String::new, |error| {
                format!("; cleanup also failed: {error}")
            });
            return Err(format!(
                "managed kernel credential belongs to pid {:?}, but socket answered as pid {answering_pid:?}; refusing shutdown and revoking stale credential{detail}",
                credential.pid()
            ));
        }
    }
    let result = match client.call(method, serde_json::json!({})) {
        Ok(result) => result,
        Err(error) => {
            if matches!(action, KernelAction::Start { .. }) {
                return Err(cleanup_managed_start_error(
                    error.to_string(),
                    lifecycle_credential.take(),
                ));
            }
            return Err(error.to_string());
        }
    };
    if matches!(action, KernelAction::Start { .. }) && lifecycle_credential.is_some() {
        let owned_pid = autostart
            .as_ref()
            .and_then(shoal_mcp::KernelAutostart::owned_pid);
        let answering_pid = result["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok());
        if !managed_start_pids_match(owned_pid, answering_pid) {
            let Some(credential) = lifecycle_credential.take() else {
                return Err("managed kernel start lost its lifecycle credential".into());
            };
            let cleanup = credential.revoke_and_remove();
            let detail = cleanup.err().map_or_else(String::new, |error| {
                format!("; cleanup also failed: {error}")
            });
            return Err(format!(
                "managed kernel start lost ownership to a concurrent listener (owned pid {owned_pid:?}, answering pid {answering_pid:?}); credential revoked{detail}"
            ));
        }
        // PID authority was durably committed before the child received its
        // release byte and was therefore able to publish this socket.
    }
    if let Some(autostart) = autostart {
        // Dropping a Child handle does not kill the process. Once this command
        // exits the durable daemon is adopted by the user's process manager.
        drop(autostart.into_child());
    }
    if matches!(action, KernelAction::Stop { .. })
        && let Some(credential) = lifecycle_credential
    {
        credential.revoke_and_remove()?;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|error| error.to_string())?
        );
    } else if method == "kernel.shutdown" {
        println!("kernel stopping (supervisor authority authenticated)");
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

fn managed_start_pids_match(owned_pid: Option<u32>, answering_pid: Option<u32>) -> bool {
    owned_pid.is_some() && owned_pid == answering_pid
}

fn cleanup_managed_start_error(primary: String, credential: Option<managed::Credential>) -> String {
    let cleanup = credential.map_or(Ok(()), |credential| credential.revoke_and_remove());
    combine_cleanup_error(primary, cleanup)
}

fn combine_cleanup_error(primary: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; managed credential cleanup also failed: {cleanup}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{combine_cleanup_error, managed_start_pids_match};

    #[test]
    fn managed_start_never_adopts_a_concurrent_socket_winner() {
        assert!(managed_start_pids_match(Some(42), Some(42)));
        assert!(!managed_start_pids_match(None, Some(42)));
        assert!(!managed_start_pids_match(Some(41), Some(42)));
        assert!(!managed_start_pids_match(Some(42), None));
    }

    #[test]
    fn managed_start_reports_primary_and_cleanup_failures() {
        assert_eq!(
            combine_cleanup_error("connect failed".into(), Ok(())),
            "connect failed"
        );
        assert_eq!(
            combine_cleanup_error("connect failed".into(), Err("revoke failed".into())),
            "connect failed; managed credential cleanup also failed: revoke failed"
        );
    }
}
