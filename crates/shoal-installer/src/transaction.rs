//! Crash-consistent installer transaction orchestration.
//!
//! The facade deliberately owns only dispatch. Layout/manifest trust,
//! durable staging, commit phases, and recovery each have a separate owner.

mod manifest;
mod operations;
mod recovery;
mod staging;
mod state;

use std::env;
use std::io;
use std::thread;
use std::time::Duration;

use crate::config::{Action, Config};
use crate::safe_fs::SafeRoot;

pub use recovery::recover;

pub fn execute(config: &Config, root: &SafeRoot) -> io::Result<String> {
    recover(config, root)?;
    if let Ok(delay) = env::var("SHOAL_INSTALL_TEST_HOLD_LOCK_MS")
        && !delay.is_empty()
    {
        let milliseconds = delay.parse::<u64>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid SHOAL_INSTALL_TEST_HOLD_LOCK_MS",
            )
        })?;
        thread::sleep(Duration::from_millis(milliseconds.min(30_000)));
    }
    match config.action {
        Action::Install => operations::install(config, root),
        Action::Check => operations::check(config, root),
        Action::Uninstall => operations::uninstall(config, root),
    }
}

#[cfg(test)]
use crate::model::*;
#[cfg(test)]
use manifest::{canonical_layout, manifest_generation, path_text, validate_manifest};
#[cfg(test)]
use operations::uninstall;
#[cfg(test)]
use recovery::{classify_rollback, validate_journal};
#[cfg(test)]
use state::{TransactionDir, create_private_dir};
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
#[cfg(test)]
use std::{fs, path::Path};

#[cfg(test)]
mod tests;
