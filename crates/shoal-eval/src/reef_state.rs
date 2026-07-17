//! Mutable reef overlay, cwd-derived cache, and lock state.

use super::*;

#[derive(Clone)]
pub(crate) struct ReefState {
    pub(crate) chain: Option<(PathBuf, shoal_reef::ScopeChain)>,
    pub(crate) chain_key: Option<shoal_reef::ChainKey>,
    pub(crate) lock: shoal_reef::Lockfile,
    pub(crate) lock_path: Option<PathBuf>,
    pub(crate) lock_load_error: Option<String>,
    pub(crate) overrides: Vec<shoal_reef::ScopeEntry>,
}

impl Default for ReefState {
    fn default() -> Self {
        Self {
            chain: None,
            chain_key: None,
            lock: shoal_reef::Lockfile::new(),
            lock_path: None,
            lock_load_error: None,
            overrides: Vec::new(),
        }
    }
}
