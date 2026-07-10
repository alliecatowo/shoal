use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
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
        let src = fs::read_to_string(path).map_err(|e| PluginError::Manifest {
            path: path.into(),
            message: e.to_string(),
        })?;
        let mut m: Self = toml::from_str(&src).map_err(|e| PluginError::Manifest {
            path: path.into(),
            message: e.to_string(),
        })?;
        if m.name.is_empty() || m.version.is_empty() {
            return Err(PluginError::Manifest {
                path: path.into(),
                message: "name and version are required".into(),
            });
        }
        if m.component.is_relative() {
            m.component = path.parent().unwrap_or(Path::new(".")).join(&m.component)
        }
        Ok(m)
    }
}
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub table_elements: usize,
    pub instances: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            memory_bytes: 64 * 1024 * 1024,
            table_elements: 10_000,
            instances: 16,
        }
    }
}
struct State {
    limits: StoreLimits,
}
pub struct Host {
    engine: Engine,
    limits: Limits,
}
impl Host {
    pub fn new(limits: Limits) -> Result<Self, PluginError> {
        let mut c = Config::new();
        c.wasm_component_model(true).consume_fuel(true);
        let engine = Engine::new(&c).map_err(|e| PluginError::Component {
            name: "engine".into(),
            message: e.to_string(),
        })?;
        Ok(Self { engine, limits })
    }
    pub fn validate(&self, manifest: &Manifest) -> Result<(), PluginError> {
        let bytes = fs::read(&manifest.component).map_err(|e| PluginError::Component {
            name: manifest.name.clone(),
            message: e.to_string(),
        })?;
        let component =
            Component::new(&self.engine, bytes).map_err(|e| PluginError::Component {
                name: manifest.name.clone(),
                message: e.to_string(),
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
        let limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_bytes)
            .table_elements(self.limits.table_elements)
            .instances(self.limits.instances)
            .build();
        let mut store = Store::new(&self.engine, State { limits });
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|e| PluginError::Component {
                name: manifest.name.clone(),
                message: e.to_string(),
            })?;
        let linker = Linker::<State>::new(&self.engine);
        linker
            .instantiate(&mut store, &component)
            .map_err(|e| PluginError::Component {
                name: manifest.name.clone(),
                message: format!("no ambient imports are available: {e}"),
            })?;
        Ok(())
    }
}
pub struct Registry {
    host: Host,
    plugins: BTreeMap<String, Manifest>,
}
impl Registry {
    pub fn new(limits: Limits) -> Result<Self, PluginError> {
        Ok(Self {
            host: Host::new(limits)?,
            plugins: BTreeMap::new(),
        })
    }
    pub fn load_manifest(&mut self, path: &Path) -> Result<(), PluginError> {
        let m = Manifest::load(path)?;
        if self.plugins.contains_key(&m.name) {
            return Err(PluginError::Duplicate(m.name));
        }
        self.host.validate(&m)?;
        self.plugins.insert(m.name.clone(), m);
        Ok(())
    }
    pub fn load_dir(&mut self, dir: &Path) -> Vec<PluginError> {
        let mut paths = match fs::read_dir(dir) {
            Ok(x) => x
                .filter_map(Result::ok)
                .map(|x| x.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect::<Vec<_>>(),
            Err(e) => {
                return vec![PluginError::Manifest {
                    path: dir.into(),
                    message: e.to_string(),
                }];
            }
        };
        paths.sort();
        paths
            .into_iter()
            .filter_map(|p| self.load_manifest(&p).err())
            .collect()
    }
    pub fn get(&self, name: &str) -> Option<&Manifest> {
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
    fn fixture(wat: &str) -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let component = t.path().join("p.wasm");
        fs::write(&component, wat).unwrap();
        let manifest = t.path().join("p.toml");
        fs::write(
            &manifest,
            "name='test'\nversion='0.1'\ncomponent='p.wasm'\neffects=[]\n",
        )
        .unwrap();
        (t, manifest)
    }
    #[test]
    fn accepts_empty_component_without_imports() {
        let (_t, p) = fixture("(component)");
        let mut r = Registry::new(Limits::default()).unwrap();
        r.load_manifest(&p).unwrap();
        assert_eq!(r.len(), 1)
    }
    #[test]
    fn rejects_ambient_import() {
        let (_t, p) = fixture("(component (import \"wasi:filesystem/types@0.2.0\" (instance)))");
        let mut r = Registry::new(Limits::default()).unwrap();
        let e = r.load_manifest(&p).unwrap_err().to_string();
        assert!(e.contains("no ambient imports"))
    }
    #[test]
    fn rejects_core_module_and_bad_manifest() {
        let (_t, p) = fixture("(module)");
        let mut r = Registry::new(Limits::default()).unwrap();
        assert!(r.load_manifest(&p).is_err());
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.toml");
        fs::write(&p, "name='x'\nunknown=1").unwrap();
        assert!(Manifest::load(&p).is_err())
    }
    #[test]
    fn duplicate_is_deterministic() {
        let (_t, p) = fixture("(component)");
        let mut r = Registry::new(Limits::default()).unwrap();
        r.load_manifest(&p).unwrap();
        assert!(matches!(
            r.load_manifest(&p),
            Err(PluginError::Duplicate(_))
        ))
    }
}
