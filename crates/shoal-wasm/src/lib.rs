use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker},
};

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("manifest {path}: {message}")]
    Manifest { path: PathBuf, message: String },
    #[error("duplicate plugin `{0}`")]
    Duplicate(String),
    #[error("component `{name}` rejected: {message}")]
    Component { name: String, message: String },
    #[error("plugin `{0}` not found")]
    NotFound(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub component: PathBuf,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
    #[serde(default)]
    pub methods: Vec<MethodDecl>,
    #[serde(default)]
    pub effects: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDecl {
    pub name: String,
    pub signature: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MethodDecl {
    pub type_name: String,
    pub name: String,
    pub signature: String,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        Self::load_bounded(path, Limits::default().manifest_bytes)
    }

    fn load_bounded(path: &Path, max_bytes: usize) -> Result<Self, PluginError> {
        let bytes = read_at_most(path, max_bytes).map_err(|message| PluginError::Manifest {
            path: path.into(),
            message,
        })?;
        let src = String::from_utf8(bytes).map_err(|error| PluginError::Manifest {
            path: path.into(),
            message: format!("manifest is not UTF-8: {error}"),
        })?;
        let mut manifest: Self = toml::from_str(&src).map_err(|error| PluginError::Manifest {
            path: path.into(),
            message: error.to_string(),
        })?;
        if manifest.name.trim().is_empty() || manifest.version.trim().is_empty() {
            return Err(PluginError::Manifest {
                path: path.into(),
                message: "name and version are required".into(),
            });
        }
        if manifest.component.is_relative() {
            manifest.component = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&manifest.component);
        }
        Ok(manifest)
    }
}

fn read_at_most(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let read_limit = max_bytes.saturating_add(1);
    let mut bytes = Vec::with_capacity(read_limit.min(64 * 1024));
    file.take(read_limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "file exceeds the {max_bytes}-byte validation limit"
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub fuel: u64,
    /// Maximum bytes in each linear memory. `memories` separately bounds how
    /// many memories may exist, making the aggregate ceiling explicit.
    pub memory_bytes: usize,
    pub memories: usize,
    /// Maximum elements in each table. `tables` separately bounds the count.
    pub table_elements: usize,
    pub tables: usize,
    pub instances: usize,
    pub manifest_bytes: usize,
    pub component_bytes: usize,
    /// Coarse wall deadline for guest code run during instantiation. Component
    /// compilation is instead bounded by `component_bytes` because Wasmtime's
    /// synchronous compiler is not epoch-interruptible.
    pub wall_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            memory_bytes: 64 * 1024 * 1024,
            memories: 1,
            table_elements: 10_000,
            tables: 4,
            instances: 16,
            manifest_bytes: 256 * 1024,
            component_bytes: 16 * 1024 * 1024,
            wall_time: Duration::from_secs(2),
        }
    }
}

struct State {
    limits: StoreLimits,
}

/// A component whose exact bytes were bounded, hashed, compiled, and
/// instantiated under the configured validation limits. Invocation must use
/// `component`, never re-read `manifest.component`, so a later file or symlink
/// replacement cannot swap in unvalidated code.
pub struct ValidatedPlugin {
    manifest: Manifest,
    bytes: Arc<[u8]>,
    digest: blake3::Hash,
    component: Component,
}

impl ValidatedPlugin {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn digest(&self) -> blake3::Hash {
        self.digest
    }

    pub fn component(&self) -> &Component {
        &self.component
    }
}

pub struct Host {
    engine: Engine,
    limits: Limits,
}

impl Host {
    pub fn new(limits: Limits) -> Result<Self, PluginError> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|error| PluginError::Component {
            name: "engine".into(),
            message: error.to_string(),
        })?;
        Ok(Self { engine, limits })
    }

    pub fn validate(&self, manifest: Manifest) -> Result<ValidatedPlugin, PluginError> {
        let bytes =
            read_at_most(&manifest.component, self.limits.component_bytes).map_err(|message| {
                PluginError::Component {
                    name: manifest.name.clone(),
                    message,
                }
            })?;
        if bytes.is_empty() {
            return Err(PluginError::Component {
                name: manifest.name.clone(),
                message: "component file is empty".into(),
            });
        }
        let digest = blake3::hash(&bytes);
        let component =
            Component::new(&self.engine, &bytes).map_err(|error| PluginError::Component {
                name: manifest.name.clone(),
                message: error.to_string(),
            })?;
        let imports = component
            .component_type()
            .imports(&self.engine)
            .map(|(name, _)| name.to_string())
            .collect::<Vec<_>>();
        if !imports.is_empty() {
            return Err(PluginError::Component {
                name: manifest.name.clone(),
                message: format!("no ambient imports are available: {}", imports.join(", ")),
            });
        }

        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_bytes)
            .memories(self.limits.memories)
            .table_elements(self.limits.table_elements)
            .tables(self.limits.tables)
            .instances(self.limits.instances)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &self.engine,
            State {
                limits: store_limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|error| PluginError::Component {
                name: manifest.name.clone(),
                message: error.to_string(),
            })?;
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);

        let linker = Linker::<State>::new(&self.engine);
        self.instantiate_with_deadline(&manifest.name, &mut store, &linker, &component)?;
        Ok(ValidatedPlugin {
            manifest,
            bytes: bytes.into(),
            digest,
            component,
        })
    }

    fn instantiate_with_deadline(
        &self,
        name: &str,
        store: &mut Store<State>,
        linker: &Linker<State>,
        component: &Component,
    ) -> Result<(), PluginError> {
        let instantiate = || {
            linker
                .instantiate(store, component)
                .map(|_| ())
                .map_err(|error| PluginError::Component {
                    name: name.to_string(),
                    message: error.to_string(),
                })
        };

        #[cfg(target_has_atomic = "64")]
        {
            let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let engine = self.engine.clone();
                let wall_time = self.limits.wall_time;
                let timer = std::thread::Builder::new()
                    .name("shoal-wasm-deadline".into())
                    .spawn_scoped(scope, move || {
                        if cancel_rx.recv_timeout(wall_time).is_err() {
                            engine.increment_epoch();
                        }
                    })
                    .map_err(|error| PluginError::Component {
                        name: name.to_string(),
                        message: format!("failed to start deadline guard: {error}"),
                    })?;
                let result = instantiate();
                let _ = cancel_tx.send(());
                timer.join().map_err(|_| PluginError::Component {
                    name: name.to_string(),
                    message: "deadline guard panicked".into(),
                })?;
                result
            })
        }

        #[cfg(not(target_has_atomic = "64"))]
        instantiate()
    }
}

pub struct Registry {
    host: Host,
    plugins: BTreeMap<String, ValidatedPlugin>,
}

impl Registry {
    pub fn new(limits: Limits) -> Result<Self, PluginError> {
        Ok(Self {
            host: Host::new(limits)?,
            plugins: BTreeMap::new(),
        })
    }

    pub fn load_manifest(&mut self, path: &Path) -> Result<(), PluginError> {
        let manifest = Manifest::load_bounded(path, self.host.limits.manifest_bytes)?;
        if self.plugins.contains_key(&manifest.name) {
            return Err(PluginError::Duplicate(manifest.name));
        }
        let plugin = self.host.validate(manifest)?;
        self.plugins.insert(plugin.manifest.name.clone(), plugin);
        Ok(())
    }

    pub fn load_dir(&mut self, dir: &Path) -> Vec<PluginError> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                return vec![PluginError::Manifest {
                    path: dir.into(),
                    message: error.to_string(),
                }];
            }
        };
        let mut paths = Vec::new();
        let mut errors = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "toml")
                    {
                        paths.push(path);
                    }
                }
                Err(error) => errors.push(PluginError::Manifest {
                    path: dir.into(),
                    message: format!("cannot read directory entry: {error}"),
                }),
            }
        }
        paths.sort();
        errors.extend(
            paths
                .into_iter()
                .filter_map(|path| self.load_manifest(&path).err()),
        );
        errors
    }

    pub fn get(&self, name: &str) -> Option<&ValidatedPlugin> {
        self.plugins.get(name)
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture(wat: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let component = temp.path().join("p.wasm");
        fs::write(&component, wat).unwrap();
        let manifest = temp.path().join("p.toml");
        fs::write(
            &manifest,
            "name='test'\nversion='0.1'\ncomponent='p.wasm'\neffects=[]\n",
        )
        .unwrap();
        (temp, manifest)
    }

    #[test]
    fn accepts_component_without_imports() {
        let (_temp, path) = fixture("(component)");
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(!registry.get("test").unwrap().bytes().is_empty());
    }

    #[test]
    fn rejects_ambient_import() {
        let (_temp, path) =
            fixture("(component (import \"wasi:filesystem/types@0.2.0\" (instance)))");
        let mut registry = Registry::new(Limits::default()).unwrap();
        let error = registry.load_manifest(&path).unwrap_err().to_string();
        assert!(error.contains("no ambient imports"));
    }

    #[test]
    fn rejects_core_module_bad_manifest_and_empty_file() {
        let (_temp, path) = fixture("(module)");
        let mut registry = Registry::new(Limits::default()).unwrap();
        assert!(registry.load_manifest(&path).is_err());

        let temp = tempfile::tempdir().unwrap();
        let bad_manifest = temp.path().join("x.toml");
        fs::write(&bad_manifest, "name='x'\nunknown=1").unwrap();
        assert!(Manifest::load(&bad_manifest).is_err());

        let (_temp, path) = fixture("");
        assert!(registry.load_manifest(&path).is_err());
    }

    #[test]
    fn duplicate_is_deterministic() {
        let (_temp, path) = fixture("(component)");
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();
        assert!(matches!(
            registry.load_manifest(&path),
            Err(PluginError::Duplicate(_))
        ));
    }

    #[test]
    fn artifact_is_bound_to_the_validated_bytes_and_digest() {
        let (temp, path) = fixture("(component)");
        let original = fs::read(temp.path().join("p.wasm")).unwrap();
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();
        fs::write(temp.path().join("p.wasm"), b"not wasm anymore").unwrap();

        let plugin = registry.get("test").unwrap();
        assert_eq!(plugin.bytes(), original);
        assert_eq!(plugin.digest(), blake3::hash(&original));
        assert_eq!(
            plugin
                .component()
                .component_type()
                .imports(plugin.component().engine())
                .count(),
            0
        );
    }

    #[test]
    fn manifest_and_component_reads_are_bounded() {
        let (_temp, path) = fixture("(component)");
        let mut registry = Registry::new(Limits {
            manifest_bytes: 8,
            ..Limits::default()
        })
        .unwrap();
        assert!(registry.load_manifest(&path).is_err());

        let (_temp, path) = fixture("(component)");
        let mut registry = Registry::new(Limits {
            component_bytes: 4,
            ..Limits::default()
        })
        .unwrap();
        let error = registry.load_manifest(&path).unwrap_err().to_string();
        assert!(error.contains("4-byte validation limit"), "{error}");
    }

    #[test]
    fn aggregate_memory_count_and_per_memory_size_are_enforced() {
        let (_temp, path) =
            fixture("(component (core module $m (memory 2)) (core instance (instantiate $m)))");
        let mut registry = Registry::new(Limits {
            memory_bytes: 64 * 1024,
            ..Limits::default()
        })
        .unwrap();
        assert!(registry.load_manifest(&path).is_err());

        let (_temp, path) = fixture(
            "(component
                (core module $m (memory 1))
                (core instance (instantiate $m))
                (core instance (instantiate $m)))",
        );
        let mut registry = Registry::new(Limits {
            memories: 1,
            instances: 4,
            ..Limits::default()
        })
        .unwrap();
        assert!(registry.load_manifest(&path).is_err());
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn wall_deadline_interrupts_nonterminating_start_code() {
        let (_temp, path) = fixture(
            "(component
                (core module $m
                    (func $start (loop $forever (br $forever)))
                    (start $start))
                (core instance (instantiate $m)))",
        );
        let mut registry = Registry::new(Limits {
            fuel: u64::MAX,
            wall_time: Duration::from_millis(20),
            ..Limits::default()
        })
        .unwrap();
        let start = Instant::now();
        assert!(registry.load_manifest(&path).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
