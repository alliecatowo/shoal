//! Journal validation and identity-aware crash recovery.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::manifest::{
    canonical_layout, committed_generation_matches, path_text, validate_identity,
};
use super::state::{TransactionDir, cleanup_transaction, other, persist_journal, same_content};
use crate::config::{Config, validate_relative};
use crate::model::{Identity, JOURNAL_NAME, Journal, MANIFEST_NAME, Phase, TRANSACTION_NAME};
use crate::safe_fs::{SafeRoot, identity_for_path};

pub fn recover(config: &Config, root: &SafeRoot) -> io::Result<()> {
    let transaction_path = root.path().join(TRANSACTION_NAME);
    let metadata = match fs::symlink_metadata(&transaction_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(other("installer transaction path is not a real directory"));
    }
    // SAFETY: `geteuid` has no pointer preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(other(
            "installer transaction directory is not owner-private",
        ));
    }
    let transaction = TransactionDir::open(transaction_path)?;
    let journal = match fs::read(transaction.join(JOURNAL_NAME)) {
        Ok(bytes) => serde_json::from_slice::<Journal>(&bytes)
            .map_err(|e| other(format!("unreadable installer recovery journal: {e}")))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return cleanup_transaction(root, &transaction);
        }
        Err(error) => return Err(error),
    };
    if journal.schema != 1 {
        return Err(other(format!(
            "unsupported installer journal schema {}",
            journal.schema
        )));
    }
    validate_journal(config, &journal)?;
    match journal.phase {
        Phase::Snapshot => cleanup_transaction(root, &transaction),
        Phase::Committed if committed_generation_matches(root, &journal)? => {
            cleanup_transaction(root, &transaction)
        }
        Phase::Prepared | Phase::Committing { .. } | Phase::RollingBack | Phase::Committed => {
            rollback(root, &transaction, journal)
        }
    }
}

pub(super) fn validate_journal(config: &Config, journal: &Journal) -> io::Result<()> {
    let layout = canonical_layout(config)?;
    let valid_count = match journal.operation.as_str() {
        "install" => journal.artifacts.len() == layout.len(),
        "uninstall" => journal.artifacts.len() <= layout.len(),
        _ => false,
    };
    if !valid_count {
        return Err(other(
            "installer journal has an invalid operation or artifact count",
        ));
    }
    if let Phase::Committing { next } = journal.phase
        && next > journal.artifacts.len()
    {
        return Err(other("installer journal commit cursor is out of bounds"));
    }
    match journal.operation.as_str() {
        "install" if journal.new_manifest.is_none() => {
            return Err(other(
                "install journal lacks its intended manifest identity",
            ));
        }
        "uninstall" if journal.new_manifest.is_some() => {
            return Err(other("uninstall journal unexpectedly publishes a manifest"));
        }
        _ => {}
    }
    let mut destinations = HashSet::new();
    let mut backups = HashSet::new();
    let mut next_layout_index = 0;
    for (index, artifact) in journal.artifacts.iter().enumerate() {
        validate_relative(Path::new(&artifact.relative))?;
        validate_relative(Path::new(&artifact.backup))?;
        if Path::new(&artifact.backup).components().count() != 1
            || artifact.backup != format!("artifact-{index}")
            || !destinations.insert(&artifact.relative)
            || !backups.insert(&artifact.backup)
        {
            return Err(other(
                "installer journal contains duplicate or nested artifact paths",
            ));
        }
        let expected = if journal.operation == "install" {
            layout.get(index)
        } else {
            let found = layout[next_layout_index..].iter().position(|candidate| {
                candidate.name == artifact.name
                    && path_text(&candidate.relative) == artifact.relative
            });
            found.map(|offset| {
                next_layout_index += offset + 1;
                &layout[next_layout_index - 1]
            })
        }
        .ok_or_else(|| other("installer journal targets a noncanonical managed artifact"))?;
        if artifact.name != expected.name
            || artifact.relative != path_text(&expected.relative)
            || artifact.mode != expected.mode
        {
            return Err(other(format!(
                "installer journal artifact {} disagrees with the canonical managed layout",
                artifact.name
            )));
        }
        if let Some(old) = &artifact.old {
            validate_identity(old, "journal old identity")?;
        }
        match (journal.operation.as_str(), &artifact.new) {
            ("install", Some(new)) => {
                validate_identity(new, "journal new identity")?;
                if new.mode != expected.mode {
                    return Err(other("journal new identity has a noncanonical mode"));
                }
            }
            ("install", None) => return Err(other("install journal lacks a new identity")),
            ("uninstall", None) => {}
            ("uninstall", Some(_)) => {
                return Err(other("uninstall journal unexpectedly installs an artifact"));
            }
            _ => unreachable!("operation was validated above"),
        }
    }
    if let Some(identity) = &journal.old_manifest {
        validate_identity(identity, "journal old manifest identity")?;
    }
    if let Some(identity) = &journal.new_manifest {
        validate_identity(identity, "journal new manifest identity")?;
        if identity.mode != 0o600 {
            return Err(other("journal manifest identity has a noncanonical mode"));
        }
    }
    Ok(())
}

fn rollback(root: &SafeRoot, transaction: &TransactionDir, mut journal: Journal) -> io::Result<()> {
    transaction.revalidate()?;
    let actions = journal
        .artifacts
        .iter()
        .map(|artifact| {
            classify_rollback(
                root.identity(Path::new(&artifact.relative))?,
                artifact.old.clone(),
                artifact.new.clone(),
                &artifact.relative,
            )
        })
        .collect::<io::Result<Vec<_>>>()?;
    let manifest_action = classify_rollback(
        root.identity(Path::new(MANIFEST_NAME))?,
        journal.old_manifest.clone(),
        journal.new_manifest.clone(),
        MANIFEST_NAME,
    )?;
    journal.phase = Phase::RollingBack;
    persist_journal(transaction, &journal)?;
    for (index, (artifact, action)) in journal.artifacts.iter().zip(&actions).enumerate() {
        apply_rollback_action(
            root,
            Path::new(&artifact.relative),
            &transaction.join("backup").join(&artifact.backup),
            action,
            &format!("rollback-{}-{index}", std::process::id()),
        )?;
    }
    apply_rollback_action(
        root,
        Path::new(MANIFEST_NAME),
        &transaction.join("backup/managed-manifest.json"),
        &manifest_action,
        &format!("rollback-manifest-{}", std::process::id()),
    )?;
    root.sync()?;
    cleanup_transaction(root, transaction)
}

#[derive(Clone, Debug)]
pub(super) enum RollbackAction {
    Leave,
    Restore {
        old: Identity,
        expected_current: Option<Identity>,
    },
    Remove {
        expected_current: Identity,
    },
}

pub(super) fn classify_rollback(
    current: Option<Identity>,
    old: Option<Identity>,
    new: Option<Identity>,
    label: &str,
) -> io::Result<RollbackAction> {
    if identities_match(current.as_ref(), old.as_ref()) {
        return Ok(RollbackAction::Leave);
    }
    if identities_match(current.as_ref(), new.as_ref()) {
        return match old {
            Some(old) => Ok(RollbackAction::Restore {
                old,
                expected_current: new,
            }),
            None => new
                .map(|expected_current| RollbackAction::Remove { expected_current })
                .ok_or_else(|| other(format!("rollback state is ambiguous for {label}"))),
        };
    }
    Err(other(format!(
        "refusing crash recovery because {label} was replaced after the transaction stopped"
    )))
}

fn identities_match(left: Option<&Identity>, right: Option<&Identity>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => same_content(left, right),
        _ => false,
    }
}

fn apply_rollback_action(
    root: &SafeRoot,
    relative: &Path,
    backup: &Path,
    action: &RollbackAction,
    nonce: &str,
) -> io::Result<()> {
    match action {
        RollbackAction::Leave => Ok(()),
        RollbackAction::Restore {
            old,
            expected_current,
        } => {
            let backup_identity = identity_for_path(backup, old.mode)?;
            if !same_content(&backup_identity, old) {
                return Err(other(format!(
                    "refusing crash recovery because the backup for {} changed",
                    relative.display()
                )));
            }
            assert_current_matches(root, relative, expected_current.as_ref())?;
            root.install_file(relative, backup, old.mode, nonce)
        }
        RollbackAction::Remove { expected_current } => {
            assert_current_matches(root, relative, Some(expected_current))?;
            root.remove_file(relative)
        }
    }
}

fn assert_current_matches(
    root: &SafeRoot,
    relative: &Path,
    expected: Option<&Identity>,
) -> io::Result<()> {
    let current = root.identity(relative)?;
    if identities_match(current.as_ref(), expected) {
        Ok(())
    } else {
        Err(other(format!(
            "refusing crash recovery because {} changed during recovery",
            relative.display()
        )))
    }
}
