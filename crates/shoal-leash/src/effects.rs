//! Semantic effects and plans (site/content/internals/language-conformance-contract.md): the concrete, evaluatable actions a
//! principal's spawn can take, bundled into a [`Plan`] with a stable
//! content-addressed reference.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    FsRead {
        paths: Vec<PathBuf>,
    },
    FsWrite {
        paths: Vec<PathBuf>,
    },
    FsDelete {
        paths: Vec<PathBuf>,
        /// `false` means a recoverable delete (Shoal trash or a reversible
        /// move source); `true` means the caller explicitly requested
        /// destructive deletion. Omitted on the wire for compatibility with
        /// plans produced before this distinction was represented.
        #[serde(default, skip_serializing_if = "is_false")]
        permanent: bool,
    },
    ProcSpawn {
        bin_hash: String,
        argv0: String,
    },
    NetConnect {
        host: String,
        port: u16,
    },
    NetListen {
        port: u16,
    },
    EnvRead {
        names: Vec<String>,
    },
    EnvWrite {
        names: Vec<String>,
    },
    SecretUse {
        names: Vec<String>,
    },
    SessionWrite,
    JournalRead,
    Time,
    Opaque,
}

impl Effect {
    /// Whether this effect explicitly destroys filesystem state without a
    /// trash/move inverse. Kept on the semantic type so evaluators, policy,
    /// kernels, and renderers cannot invent divergent flag heuristics.
    #[must_use]
    pub fn is_permanent_delete(&self) -> bool {
        matches!(
            self,
            Self::FsDelete {
                permanent: true,
                ..
            }
        )
    }
}

fn is_false(value: &bool) -> bool {
    !*value
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
