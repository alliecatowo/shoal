//! Semantic effects and plans (TDD §8): the concrete, evaluatable actions a
//! principal's spawn can take, bundled into a [`Plan`] with a stable
//! content-addressed reference.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    FsRead { paths: Vec<PathBuf> },
    FsWrite { paths: Vec<PathBuf> },
    FsDelete { paths: Vec<PathBuf> },
    ProcSpawn { bin_hash: String, argv0: String },
    NetConnect { host: String, port: u16 },
    NetListen { port: u16 },
    EnvRead { names: Vec<String> },
    EnvWrite { names: Vec<String> },
    SecretUse { names: Vec<String> },
    SessionWrite,
    JournalRead,
    Time,
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    Reversible,
    Irreversible,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Estimates {
    pub bytes: Option<u64>,
    pub items: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub plan_ref: String,
    pub effects: Vec<Effect>,
    pub reversibility: Reversibility,
    pub estimates: Estimates,
}

impl Plan {
    pub fn new(effects: Vec<Effect>, reversibility: Reversibility, estimates: Estimates) -> Self {
        let canonical =
            serde_json::to_vec(&(&effects, reversibility, &estimates)).expect("serializable plan");
        let plan_ref = format!("plan:{}", &blake3::hash(&canonical).to_hex()[..16]);
        Self {
            plan_ref,
            effects,
            reversibility,
            estimates,
        }
    }
}
