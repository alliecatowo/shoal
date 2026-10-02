//! Install, check, and uninstall phase machines.

use std::io;
use std::path::Path;

use super::manifest::{
    canonical_layout, committed_generation_matches, legacy_manifest, manifest_from, path_text,
    read_manifest,
};
use super::recovery::validate_journal;
use super::staging::{backup_generation, snapshot_artifacts, stage_release};
use super::state::{
    assert_manifest_snapshot_unchanged, assert_snapshot_unchanged, cleanup_transaction,
    create_transaction, kill_after, maybe_fail, maybe_kill, other, persist_journal, same_content,
    write_durable,
};
use crate::config::Config;
use crate::model::{Journal, JournalArtifact, MANIFEST_NAME, Phase};
use crate::safe_fs::{SafeRoot, identity_for_path};

pub(super) fn install(config: &Config, root: &SafeRoot) -> io::Result<String> {
    let transaction = create_transaction(root)?;
    let specs = stage_release(config, root, &transaction)?;
    let artifacts = snapshot_artifacts(root, &specs, true)?;
    let old_manifest = root.identity(Path::new(MANIFEST_NAME))?;
    let manifest = manifest_from(&artifacts);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(other)?;
    let staged_manifest = transaction.join("stage/managed-manifest.json");
    write_durable(&staged_manifest, &manifest_bytes, 0o600)?;
    let new_manifest = identity_for_path(&staged_manifest, 0o600)?;
    let mut journal = Journal {
        schema: 1,
        operation: "install".into(),
        phase: Phase::Snapshot,
        artifacts,
        old_manifest,
        new_manifest: Some(new_manifest),
    };
    validate_journal(config, &journal)?;
    persist_journal(&transaction, &journal)?;
    backup_generation(root, &transaction, &journal)?;
    journal.phase = Phase::Prepared;
    persist_journal(&transaction, &journal)?;

    let kill_after = kill_after()?;
    for (index, artifact) in journal.artifacts.iter().enumerate() {
        journal.phase = Phase::Committing { next: index };
        persist_journal(&transaction, &journal)?;
        assert_snapshot_unchanged(root, artifact)?;
        root.install_file(
            Path::new(&artifact.relative),
            &transaction.join("stage").join(&artifact.backup),
            artifact.mode,
            &format!("{}-{index}", std::process::id()),
        )?;
        maybe_kill(kill_after, index + 1);
        maybe_fail(index + 1)?;
    }
    journal.phase = Phase::Committing {
        next: journal.artifacts.len(),
    };
    persist_journal(&transaction, &journal)?;
    assert_manifest_snapshot_unchanged(root, &journal)?;
    root.install_bytes(
        Path::new(MANIFEST_NAME),
        &manifest_bytes,
        0o600,
        &format!("{}-manifest", std::process::id()),
    )?;
    root.sync()?;
    maybe_kill(kill_after, journal.artifacts.len() + 1);
    journal.phase = Phase::Committed;
    persist_journal(&transaction, &journal)?;
    if !committed_generation_matches(root, &journal)? {
        return Err(other("post-install generation verification failed"));
    }
    cleanup_transaction(root, &transaction)?;
    Ok(format!(
        "installed {} managed Shoal artifacts under {}",
        journal.artifacts.len(),
        root.path().display()
    ))
}

pub(super) fn check(config: &Config, root: &SafeRoot) -> io::Result<String> {
    let transaction = create_transaction(root)?;
    let result = (|| {
        let specs = stage_release(config, root, &transaction)?;
        let manifest = read_manifest(config, root)?;
        if manifest.artifacts.len() != specs.len() {
            return Err(other("installed manifest has the wrong artifact count"));
        }
        for (spec, managed) in specs.iter().zip(&manifest.artifacts) {
            if spec.name != managed.name || path_text(&spec.relative) != managed.relative {
                return Err(other(format!(
                    "installed manifest layout differs at {}",
                    spec.name
                )));
            }
            let staged = identity_for_path(&spec.staged, spec.mode)?;
            let installed = root.identity(&spec.relative)?.ok_or_else(|| {
                other(format!(
                    "missing installed artifact: {}",
                    spec.relative.display()
                ))
            })?;
            if !same_content(&staged, &installed)
                || staged.sha256 != managed.sha256
                || staged.bytes != managed.bytes
                || staged.mode != managed.mode
            {
                return Err(other(format!(
                    "installed artifact differs from release or manifest: {}",
                    spec.relative.display()
                )));
            }
        }
        Ok(format!(
            "verified {} managed Shoal artifacts under {}",
            specs.len(),
            root.path().display()
        ))
    })();
    cleanup_transaction(root, &transaction)?;
    result
}

pub(super) fn uninstall(config: &Config, root: &SafeRoot) -> io::Result<String> {
    let manifest = match read_manifest(config, root) {
        Ok(manifest) => manifest,
        Err(error) if config.force && error.kind() == io::ErrorKind::NotFound => {
            legacy_manifest(config, root)?
        }
        Err(error) => return Err(error),
    };
    for artifact in &manifest.artifacts {
        if let Some(current) = root.identity(Path::new(&artifact.relative))? {
            let matches = current.sha256 == artifact.sha256
                && current.bytes == artifact.bytes
                && current.mode == artifact.mode;
            if !matches && !config.force {
                return Err(other(format!(
                    "refusing to uninstall user-replaced managed artifact {}; rerun with --uninstall --force",
                    artifact.relative
                )));
            }
        }
    }
    let transaction = create_transaction(root)?;
    let layout = canonical_layout(config)?;
    let artifacts = manifest
        .artifacts
        .iter()
        .enumerate()
        .map(|(index, artifact)| {
            let expected = layout
                .iter()
                .find(|expected| {
                    expected.name == artifact.name
                        && path_text(&expected.relative) == artifact.relative
                })
                .ok_or_else(|| other("uninstall manifest targets a noncanonical artifact"))?;
            Ok(JournalArtifact {
                name: artifact.name.clone(),
                relative: artifact.relative.clone(),
                backup: format!("artifact-{index}"),
                mode: expected.mode,
                old: root.identity(Path::new(&artifact.relative))?,
                new: None,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut journal = Journal {
        schema: 1,
        operation: "uninstall".into(),
        phase: Phase::Snapshot,
        artifacts,
        old_manifest: root.identity(Path::new(MANIFEST_NAME))?,
        new_manifest: None,
    };
    validate_journal(config, &journal)?;
    persist_journal(&transaction, &journal)?;
    backup_generation(root, &transaction, &journal)?;
    journal.phase = Phase::Prepared;
    persist_journal(&transaction, &journal)?;
    let kill_after = kill_after()?;
    for (index, artifact) in journal.artifacts.iter().enumerate() {
        journal.phase = Phase::Committing { next: index };
        persist_journal(&transaction, &journal)?;
        assert_snapshot_unchanged(root, artifact)?;
        root.remove_file(Path::new(&artifact.relative))?;
        maybe_kill(kill_after, index + 1);
    }
    journal.phase = Phase::Committing {
        next: journal.artifacts.len(),
    };
    persist_journal(&transaction, &journal)?;
    assert_manifest_snapshot_unchanged(root, &journal)?;
    root.remove_file(Path::new(MANIFEST_NAME))?;
    root.sync()?;
    maybe_kill(kill_after, journal.artifacts.len() + 1);
    journal.phase = Phase::Committed;
    persist_journal(&transaction, &journal)?;
    cleanup_transaction(root, &transaction)?;
    Ok(format!(
        "uninstalled {} managed Shoal artifacts under {}",
        journal.artifacts.len(),
        root.path().display()
    ))
}
