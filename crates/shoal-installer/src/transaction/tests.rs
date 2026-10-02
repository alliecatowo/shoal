use super::*;
use tempfile::TempDir;

fn config(temporary: &TempDir) -> Config {
    Config {
        action: Action::Install,
        force: false,
        prefix: temporary.path().to_path_buf(),
        release_dir: temporary.path().join("release"),
        install_dir: temporary.path().join("bin"),
        man_dir: temporary.path().join("share/man/man1"),
        lock_timeout_ms: 100,
    }
}

fn identity(seed: char, mode: u32) -> Identity {
    Identity {
        bytes: 1,
        sha256: seed.to_string().repeat(64),
        mode,
        device: 1,
        inode: 1,
    }
}

fn valid_manifest(config: &Config) -> ManagedManifest {
    let artifacts = canonical_layout(config)
        .unwrap()
        .into_iter()
        .map(|layout| ManagedArtifact {
            name: layout.name,
            relative: path_text(&layout.relative),
            bytes: 1,
            sha256: "a".repeat(64),
            mode: layout.mode,
        })
        .collect::<Vec<_>>();
    ManagedManifest {
        schema: 1,
        generation: manifest_generation(&artifacts),
        artifacts,
    }
}

fn valid_journal(config: &Config, operation: &str) -> Journal {
    let artifacts = canonical_layout(config)
        .unwrap()
        .into_iter()
        .enumerate()
        .map(|(index, layout)| JournalArtifact {
            name: layout.name,
            relative: path_text(&layout.relative),
            backup: format!("artifact-{index}"),
            mode: layout.mode,
            old: Some(identity('a', layout.mode)),
            new: (operation == "install").then(|| identity('b', layout.mode)),
        })
        .collect();
    Journal {
        schema: 1,
        operation: operation.to_owned(),
        phase: Phase::Prepared,
        artifacts,
        old_manifest: Some(identity('c', 0o600)),
        new_manifest: (operation == "install").then(|| identity('d', 0o600)),
    }
}

mod manifest_contracts {
    use super::*;
    include!("tests/manifest_contracts.rs");
}

mod recovery_contracts {
    use super::*;
    include!("tests/recovery_contracts.rs");
}

mod state_contracts {
    use super::*;
    include!("tests/state_contracts.rs");
}
