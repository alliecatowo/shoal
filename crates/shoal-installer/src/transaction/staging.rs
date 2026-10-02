//! Release staging, snapshots, and pre-mutation backups.

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::manifest::{ArtifactSource, canonical_layout, path_text};
use super::state::{
    ArtifactSpec, TransactionDir, create_private_dir, maybe_stage_fail, other, write_durable,
};
use crate::config::Config;
use crate::model::{Journal, JournalArtifact, MANIFEST_NAME};
use crate::safe_fs::{SafeRoot, identity_for_path, sync_directory};

pub(super) fn stage_release(
    config: &Config,
    root: &SafeRoot,
    transaction: &TransactionDir,
) -> io::Result<Vec<ArtifactSpec>> {
    transaction.revalidate()?;
    let stage = transaction.join("stage");
    let backup = transaction.join("backup");
    create_private_dir(&stage)?;
    create_private_dir(&backup)?;
    let source_man = if config.release_dir.join("man").is_dir() {
        config.release_dir.join("man")
    } else {
        PathBuf::from("man")
    };
    let mut specs = Vec::new();
    for artifact in canonical_layout(config)? {
        match artifact.source {
            source_kind @ (ArtifactSource::Binary | ArtifactSource::ManPage) => {
                let source = match source_kind {
                    ArtifactSource::Binary => config.release_dir.join(&artifact.name),
                    ArtifactSource::ManPage => source_man.join(&artifact.name),
                    ArtifactSource::Completion(_) => unreachable!(),
                };
                specs.push(stage_one(
                    &artifact.name,
                    &source,
                    artifact.relative,
                    artifact.mode,
                    specs.len(),
                    &stage,
                    &backup,
                )?);
            }
            ArtifactSource::Completion(shell) => {
                let output = Command::new(config.release_dir.join("shoal"))
                    .args(["completions", shell])
                    .output()?;
                if !output.status.success() || output.stdout.is_empty() {
                    return Err(other(format!(
                        "generate {shell} completion failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                let index = specs.len();
                let staged = stage.join(format!("artifact-{index}"));
                write_durable(&staged, &output.stdout, artifact.mode)?;
                specs.push(ArtifactSpec {
                    name: artifact.name,
                    relative: artifact.relative,
                    staged,
                    backup: backup.join(format!("artifact-{index}")),
                    mode: artifact.mode,
                });
            }
        }
    }
    sync_directory(&stage)?;
    root.sync()?;
    transaction.revalidate()?;
    Ok(specs)
}

fn stage_one(
    name: &str,
    source: &Path,
    relative: PathBuf,
    mode: u32,
    index: usize,
    stage: &Path,
    backup: &Path,
) -> io::Result<ArtifactSpec> {
    let metadata = fs::symlink_metadata(source).map_err(|e| {
        other(format!(
            "missing release artifact {}: {e}",
            source.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(other(format!(
            "release artifact is not a regular file: {}",
            source.display()
        )));
    }
    let staged = stage.join(format!("artifact-{index}"));
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&staged)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    maybe_stage_fail(index + 1)?;
    Ok(ArtifactSpec {
        name: name.into(),
        relative,
        staged,
        backup: backup.join(format!("artifact-{index}")),
        mode,
    })
}

pub(super) fn snapshot_artifacts(
    root: &SafeRoot,
    specs: &[ArtifactSpec],
    include_new: bool,
) -> io::Result<Vec<JournalArtifact>> {
    specs
        .iter()
        .map(|spec| {
            Ok(JournalArtifact {
                name: spec.name.clone(),
                relative: path_text(&spec.relative),
                backup: spec
                    .backup
                    .file_name()
                    .ok_or_else(|| other("backup path has no name"))?
                    .to_string_lossy()
                    .into_owned(),
                mode: spec.mode,
                old: root.identity(&spec.relative)?,
                new: include_new
                    .then(|| identity_for_path(&spec.staged, spec.mode))
                    .transpose()?,
            })
        })
        .collect()
}

pub(super) fn backup_generation(
    root: &SafeRoot,
    transaction: &TransactionDir,
    journal: &Journal,
) -> io::Result<()> {
    transaction.revalidate()?;
    let backup_directory = transaction.join("backup");
    if !backup_directory.exists() {
        create_private_dir(&backup_directory)?;
    }
    for artifact in &journal.artifacts {
        if let Some(old) = &artifact.old {
            let backup = backup_directory.join(&artifact.backup);
            root.copy_out(Path::new(&artifact.relative), &backup, old.mode)?;
            let copied = identity_for_path(&backup, old.mode)?;
            if copied.bytes != old.bytes || copied.sha256 != old.sha256 {
                return Err(other(format!(
                    "managed artifact changed while it was being backed up: {}",
                    artifact.relative
                )));
            }
        }
    }
    if journal.old_manifest.is_some() {
        root.copy_out(
            Path::new(MANIFEST_NAME),
            &backup_directory.join("managed-manifest.json"),
            0o600,
        )?;
    }
    sync_directory(&backup_directory)?;
    transaction.revalidate()
}
