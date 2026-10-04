//! Mutable reef overlay, cwd-derived cache, and lock state.

use super::*;

#[derive(Clone)]
pub(crate) struct ReefState {
    pub(crate) chain: Option<(PathBuf, crate::reef::ScopeChain)>,
    pub(crate) chain_key: Option<crate::reef::ChainKey>,
    pub(crate) lock: crate::reef::Lockfile,
    pub(crate) lock_path: Option<PathBuf>,
    pub(crate) lock_load_error: Option<String>,
    pub(crate) discovery_error: Option<String>,
    pub(crate) overrides: Vec<crate::reef::ScopeEntry>,
}

impl Default for ReefState {
    fn default() -> Self {
        Self {
            chain: None,
            chain_key: None,
            lock: crate::reef::Lockfile::new(),
            lock_path: None,
            lock_load_error: None,
            discovery_error: None,
            overrides: Vec::new(),
        }
    }
}
