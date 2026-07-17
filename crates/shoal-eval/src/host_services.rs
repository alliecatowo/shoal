//! Immutable, shareable host capability bundle.

use super::*;
use std::sync::OnceLock;

#[derive(Clone)]
pub(crate) struct HostServices {
    pub(crate) fs: Arc<dyn Fs>,
    pub(crate) exec: Arc<dyn Exec>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) opener: Arc<dyn Opener>,
    pub(crate) secrets: Arc<dyn SecretPort>,
    pub(crate) config: Arc<dyn ConfigPort>,
    pub(crate) adapters: AdapterCatalog,
    pub(crate) bus: Arc<channels::EventBus>,
    pub(crate) reef_resolver: OnceLock<Arc<shoal_reef::Resolver>>,
    pub(crate) reef_user_manifest: Option<PathBuf>,
}

impl Default for HostServices {
    fn default() -> Self {
        Self {
            fs: Arc::new(StdFs),
            exec: Arc::new(StdExec),
            clock: Arc::new(StdClock),
            opener: Arc::new(StdOpener),
            secrets: Arc::new(StdSecret),
            config: Arc::new(ConfigSnapshot::default()),
            adapters: AdapterCatalog::empty(),
            bus: channels::EventBus::shared(),
            reef_resolver: OnceLock::new(),
            reef_user_manifest: None,
        }
    }
}
