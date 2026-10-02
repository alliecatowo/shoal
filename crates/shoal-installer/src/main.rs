mod config;
mod model;
mod safe_fs;
mod transaction;

use std::process::{Command, ExitCode};

use config::Config;
use safe_fs::SafeRoot;

fn main() -> ExitCode {
    if let Some(exit) = supervise_kill_injection() {
        return exit;
    }
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("shoal installer: {error}");
            ExitCode::FAILURE
        }
    }
}

// Shoal deliberately mirrors a directly signalled child's termination signal.
// Put the fault-injected transaction in a grandchild so the Shoal lifecycle
// harness observes an ordinary failed outcome while the transaction worker
// still receives a real, uncatchable SIGKILL.
fn supervise_kill_injection() -> Option<ExitCode> {
    let requested = std::env::var("SHOAL_INSTALL_TEST_KILL_AFTER")
        .ok()
        .is_some_and(|value| !value.is_empty());
    if !requested || std::env::var_os("SHOAL_INSTALL_TEST_KILL_WORKER").is_some() {
        return None;
    }
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("shoal installer: cannot resolve fault-injection worker: {error}");
            return Some(ExitCode::FAILURE);
        }
    };
    let status = Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env("SHOAL_INSTALL_TEST_KILL_WORKER", "1")
        .status();
    match status {
        Ok(status) if status.success() => Some(ExitCode::SUCCESS),
        Ok(_) => Some(ExitCode::FAILURE),
        Err(error) => {
            eprintln!("shoal installer: cannot launch fault-injection worker: {error}");
            Some(ExitCode::FAILURE)
        }
    }
}

fn run() -> std::io::Result<String> {
    let config = Config::parse()?;
    let root = SafeRoot::open(&config.prefix)?;
    let _lock = root.lock(config.lock_timeout_ms)?;
    match transaction::execute(&config, &root) {
        Ok(message) => Ok(message),
        Err(primary) => match transaction::recover(&config, &root) {
            Ok(()) => Err(primary),
            Err(recovery) => Err(std::io::Error::other(format!(
                "{primary}; automatic recovery also failed: {recovery}"
            ))),
        },
    }
}
