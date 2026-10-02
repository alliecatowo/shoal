use serde::{Deserialize, Serialize};

pub const MANIFEST_NAME: &str = ".shoal-install-manifest.json";
pub const JOURNAL_NAME: &str = "journal.json";
pub const LOCK_NAME: &str = ".shoal-install.lock";
pub const TRANSACTION_NAME: &str = ".shoal-install-transaction";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Identity {
    pub bytes: u64,
    pub sha256: String,
    pub mode: u32,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JournalArtifact {
    pub name: String,
    pub relative: String,
    pub backup: String,
    pub mode: u32,
    pub old: Option<Identity>,
    pub new: Option<Identity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Phase {
    Snapshot,
    Prepared,
    Committing { next: usize },
    RollingBack,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Journal {
    pub schema: u32,
    pub operation: String,
    pub phase: Phase,
    pub artifacts: Vec<JournalArtifact>,
    pub old_manifest: Option<Identity>,
    pub new_manifest: Option<Identity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedArtifact {
    pub name: String,
    pub relative: String,
    pub bytes: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ManagedManifest {
    pub schema: u32,
    pub generation: String,
    pub artifacts: Vec<ManagedArtifact>,
}
