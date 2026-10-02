//! Canonical artifact layout and managed-manifest trust boundary.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use super::state::{other, same_content};
use crate::config::{Config, validate_relative};
use crate::model::{Identity, MANIFEST_NAME, ManagedArtifact, ManagedManifest};
use crate::safe_fs::SafeRoot;

pub(super) const BINARIES: [&str; 10] = [
    "shoal",
    "shoal-kernel",
    "shoal-mcp",
    "shoal-lsp",
    "shoal-token",
    "shoal-secret",
    "shoal-history",
    "shoal-doctor",
    "shoal-sandbox-exec",
    "shoal-landlock-helper",
];

#[derive(Clone)]
pub(super) struct ArtifactLayout {
    pub(super) name: String,
    pub(super) relative: PathBuf,
    pub(super) mode: u32,
    pub(super) source: ArtifactSource,
}

#[derive(Clone)]
pub(super) enum ArtifactSource {
    Binary,
    ManPage,
    Completion(&'static str),
}

pub(super) fn canonical_layout(config: &Config) -> io::Result<Vec<ArtifactLayout>> {
    let install = config.install_relative()?;
    let man = config.man_relative()?;
    let mut layout = Vec::with_capacity(23);
    for binary in BINARIES {
        layout.push(ArtifactLayout {
            name: binary.into(),
            relative: install.join(binary),
            mode: 0o755,
            source: ArtifactSource::Binary,
        });
    }
    for binary in BINARIES {
        let page = format!("{binary}.1");
        layout.push(ArtifactLayout {
            name: page.clone(),
            relative: man.join(&page),
            mode: 0o644,
            source: ArtifactSource::ManPage,
        });
    }
    for (name, shell, relative) in [
        (
            "completion-bash",
            "bash",
            PathBuf::from("share/bash-completion/completions/shoal"),
        ),
        (
            "completion-zsh",
            "zsh",
            PathBuf::from("share/zsh/site-functions/_shoal"),
        ),
        (
            "completion-fish",
            "fish",
            PathBuf::from("share/fish/vendor_completions.d/shoal.fish"),
        ),
    ] {
        layout.push(ArtifactLayout {
            name: name.into(),
            relative,
            mode: 0o644,
            source: ArtifactSource::Completion(shell),
        });
    }
    Ok(layout)
}

pub(super) fn manifest_from(artifacts: &[crate::model::JournalArtifact]) -> ManagedManifest {
    let managed = artifacts
        .iter()
        .map(|artifact| {
            let identity = artifact
                .new
                .as_ref()
                .expect("install artifacts have new identities");
            ManagedArtifact {
                name: artifact.name.clone(),
                relative: artifact.relative.clone(),
                bytes: identity.bytes,
                sha256: identity.sha256.clone(),
                mode: artifact.mode,
            }
        })
        .collect::<Vec<_>>();
    ManagedManifest {
        schema: 1,
        generation: manifest_generation(&managed),
        artifacts: managed,
    }
}

pub(super) fn manifest_generation(artifacts: &[ManagedArtifact]) -> String {
    let mut digest = Sha256::new();
    for artifact in artifacts {
        digest.update(artifact.name.as_bytes());
        digest.update(artifact.relative.as_bytes());
        digest.update(artifact.bytes.to_le_bytes());
        digest.update(artifact.sha256.as_bytes());
        digest.update(artifact.mode.to_le_bytes());
    }
    format!("{:x}", digest.finalize())
}

pub(super) fn validate_identity(identity: &Identity, label: &str) -> io::Result<()> {
    validate_sha256(&identity.sha256, label)?;
    if identity.mode & !0o7777 != 0 {
        return Err(other(format!("{label} has invalid permission bits")));
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> io::Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(other(format!("{label} is not a lowercase SHA-256 digest")))
    }
}

pub(super) fn read_manifest(config: &Config, root: &SafeRoot) -> io::Result<ManagedManifest> {
    let bytes = root.read_file(Path::new(MANIFEST_NAME))?;
    let manifest = serde_json::from_slice(&bytes)
        .map_err(|e| other(format!("invalid managed manifest: {e}")))?;
    validate_manifest(config, &manifest)?;
    Ok(manifest)
}

pub(super) fn validate_manifest(config: &Config, manifest: &ManagedManifest) -> io::Result<()> {
    let layout = canonical_layout(config)?;
    if manifest.schema != 1 || manifest.artifacts.len() != layout.len() {
        return Err(other(
            "managed manifest has an unsupported schema or artifact count",
        ));
    }
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for (managed, expected) in manifest.artifacts.iter().zip(&layout) {
        validate_relative(Path::new(&managed.relative))?;
        if managed.name != expected.name
            || managed.relative != path_text(&expected.relative)
            || managed.mode != expected.mode
            || !names.insert(&managed.name)
            || !paths.insert(&managed.relative)
        {
            return Err(other(format!(
                "managed manifest artifact {} disagrees with the canonical layout",
                managed.name
            )));
        }
        validate_sha256(&managed.sha256, "managed artifact SHA-256")?;
    }
    if manifest.generation != manifest_generation(&manifest.artifacts) {
        return Err(other("managed manifest generation digest is invalid"));
    }
    Ok(())
}

pub(super) fn legacy_manifest(config: &Config, root: &SafeRoot) -> io::Result<ManagedManifest> {
    let install = config.install_relative()?;
    let man = config.man_relative()?;
    let mut artifacts = Vec::new();
    for binary in BINARIES {
        push_existing(root, &mut artifacts, binary, install.join(binary))?;
    }
    for binary in BINARIES {
        let name = format!("{binary}.1");
        push_existing(root, &mut artifacts, &name, man.join(&name))?;
    }
    for (name, relative) in [
        (
            "completion-bash",
            PathBuf::from("share/bash-completion/completions/shoal"),
        ),
        (
            "completion-zsh",
            PathBuf::from("share/zsh/site-functions/_shoal"),
        ),
        (
            "completion-fish",
            PathBuf::from("share/fish/vendor_completions.d/shoal.fish"),
        ),
    ] {
        push_existing(root, &mut artifacts, name, relative)?;
    }
    Ok(ManagedManifest {
        schema: 1,
        generation: "legacy-force-uninstall".into(),
        artifacts,
    })
}

fn push_existing(
    root: &SafeRoot,
    artifacts: &mut Vec<ManagedArtifact>,
    name: &str,
    relative: PathBuf,
) -> io::Result<()> {
    if let Some(identity) = root.identity(&relative)? {
        artifacts.push(ManagedArtifact {
            name: name.into(),
            relative: path_text(&relative),
            bytes: identity.bytes,
            sha256: identity.sha256,
            mode: identity.mode,
        });
    }
    Ok(())
}

pub(super) fn committed_generation_matches(
    root: &SafeRoot,
    journal: &crate::model::Journal,
) -> io::Result<bool> {
    for artifact in &journal.artifacts {
        let current = root.identity(Path::new(&artifact.relative))?;
        match (&artifact.new, current) {
            (Some(expected), Some(current)) if same_content(expected, &current) => {}
            (None, None) => {}
            _ => return Ok(false),
        }
    }
    let current = root.identity(Path::new(MANIFEST_NAME))?;
    Ok(match (&journal.new_manifest, current) {
        (Some(expected), Some(current)) => same_content(expected, &current),
        (None, None) => true,
        _ => false,
    })
}

pub(super) fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
