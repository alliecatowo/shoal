//! Pinned transaction-directory state and durable journal primitives.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::model::{
    Identity, JOURNAL_NAME, Journal, JournalArtifact, MANIFEST_NAME, TRANSACTION_NAME,
};
use crate::safe_fs::{SafeRoot, sync_directory};

pub(super) struct ArtifactSpec {
    pub(super) name: String,
    pub(super) relative: PathBuf,
    pub(super) staged: PathBuf,
    pub(super) backup: PathBuf,
    pub(super) mode: u32,
}

pub(super) struct TransactionDir {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl TransactionDir {
    pub(super) fn open(path: PathBuf) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        // SAFETY: `geteuid` has no pointer preconditions.
        let current_uid = unsafe { libc::geteuid() };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != current_uid
            || metadata.mode() & 0o077 != 0
        {
            return Err(other(
                "installer transaction directory is not owner-private",
            ));
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.path.join(path)
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.path)?;
        // SAFETY: `geteuid` has no pointer preconditions.
        let current_uid = unsafe { libc::geteuid() };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != current_uid
            || metadata.mode() & 0o077 != 0
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(other(
                "installer transaction directory changed identity during the operation",
            ));
        }
        Ok(())
    }
}

pub(super) fn create_transaction(root: &SafeRoot) -> io::Result<TransactionDir> {
    let transaction = root.path().join(TRANSACTION_NAME);
    create_private_dir(&transaction)?;
    root.sync()?;
    TransactionDir::open(transaction)
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    // SAFETY: `geteuid` has no pointer preconditions.
    let current_uid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != current_uid
    {
        return Err(other(format!(
            "private installer directory failed ownership/mode validation: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(super) fn cleanup_transaction(root: &SafeRoot, transaction: &TransactionDir) -> io::Result<()> {
    transaction.revalidate()?;
    fs::remove_dir_all(transaction.path())?;
    root.sync()
}

pub(super) fn persist_journal(transaction: &TransactionDir, journal: &Journal) -> io::Result<()> {
    transaction.revalidate()?;
    let bytes = serde_json::to_vec_pretty(journal).map_err(other)?;
    let temporary = transaction.join(".journal.next");
    let final_path = transaction.join(JOURNAL_NAME);
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    write_durable(&temporary, &bytes, 0o600)?;
    fs::rename(&temporary, &final_path)?;
    sync_directory(transaction.path())?;
    transaction.revalidate()
}

pub(super) fn write_durable(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub(super) fn same_content(left: &Identity, right: &Identity) -> bool {
    left.bytes == right.bytes && left.sha256 == right.sha256 && left.mode == right.mode
}

pub(super) fn assert_snapshot_unchanged(
    root: &SafeRoot,
    artifact: &JournalArtifact,
) -> io::Result<()> {
    let current = root.identity(Path::new(&artifact.relative))?;
    let unchanged = match (&artifact.old, current) {
        (None, None) => true,
        (Some(old), Some(current)) => old == &current,
        _ => false,
    };
    unchanged.then_some(()).ok_or_else(|| {
        other(format!(
            "managed artifact changed after the transaction snapshot: {}",
            artifact.relative
        ))
    })
}

pub(super) fn assert_manifest_snapshot_unchanged(
    root: &SafeRoot,
    journal: &Journal,
) -> io::Result<()> {
    let current = root.identity(Path::new(MANIFEST_NAME))?;
    let unchanged = match (&journal.old_manifest, current) {
        (None, None) => true,
        (Some(old), Some(current)) => old == &current,
        _ => false,
    };
    unchanged
        .then_some(())
        .ok_or_else(|| other("managed manifest changed after the transaction snapshot"))
}

pub(super) fn kill_after() -> io::Result<Option<usize>> {
    env::var("SHOAL_INSTALL_TEST_KILL_AFTER")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.parse::<usize>()
                .map_err(|_| invalid("invalid SHOAL_INSTALL_TEST_KILL_AFTER"))
        })
        .transpose()
}

pub(super) fn maybe_kill(kill_after: Option<usize>, committed: usize) {
    if kill_after == Some(committed) {
        // SAFETY: this deliberately terminates only the fault-injection worker.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
    }
}

pub(super) fn maybe_fail(committed: usize) -> io::Result<()> {
    injected_failure(
        "SHOAL_INSTALL_TEST_FAIL_AFTER",
        committed,
        "committed artifacts",
    )
}

pub(super) fn maybe_stage_fail(staged: usize) -> io::Result<()> {
    injected_failure(
        "SHOAL_INSTALL_TEST_FAIL_STAGE_AFTER",
        staged,
        "staged artifacts",
    )
}

fn injected_failure(variable: &str, count: usize, label: &str) -> io::Result<()> {
    if let Some(value) = env::var(variable).ok().filter(|v| !v.is_empty()) {
        let expected = value
            .parse::<usize>()
            .map_err(|_| invalid(format!("invalid {variable}")))?;
        if expected == count {
            return Err(other(format!("injected failure after {count} {label}")));
        }
    }
    Ok(())
}

pub(super) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(super) fn other(error: impl ToString) -> io::Error {
    io::Error::other(error.to_string())
}
