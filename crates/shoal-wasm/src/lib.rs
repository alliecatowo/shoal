use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline,
    component::{Component, HasSelf, Linker},
};

mod abi {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
    });
}

mod manifest;
mod registry;
mod value;

use manifest::read_at_most;
pub use manifest::{CommandDecl, Manifest, MethodDecl};
pub use registry::{CommandMetadata, Registry};
pub use value::PluginValue;

use abi::shoal::plugin::types::{Declaration, ErrorKind, GuestError, MethodDeclaration};
use shoal_leash::Effect;

pub const ABI_VERSION: u32 = 1;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct CapabilityError {
    pub message: String,
}

/// Explicit host capabilities available to one plugin invocation. The runtime
/// calls `authorize` immediately before every effectful operation; declaring an
/// effect in a manifest never grants it by itself.
pub trait CapabilityProvider: Send + Sync {
    fn authorize(&self, effect: &Effect) -> Result<(), CapabilityError>;
    fn now_ns(&self) -> Result<u64, CapabilityError>;
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, CapabilityError>;

    /// Whether the invocation's owning session has been cancelled. The store
    /// checks this on every epoch tick, so pure guest computation is
    /// interruptible even when it never crosses a hostcall boundary.
    fn cancelled(&self) -> bool {
        false
    }
}

#[derive(Default)]
pub struct DenyAllCapabilities;

impl CapabilityProvider for DenyAllCapabilities {
    fn authorize(&self, _effect: &Effect) -> Result<(), CapabilityError> {
        Err(CapabilityError {
            message: "plugin capability denied".into(),
        })
    }

    fn now_ns(&self) -> Result<u64, CapabilityError> {
        Err(CapabilityError {
            message: "time capability is unavailable".into(),
        })
    }

    fn read_file(&self, _path: &Path) -> Result<Vec<u8>, CapabilityError> {
        Err(CapabilityError {
            message: "filesystem capability is unavailable".into(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("manifest {path}: {message}")]
    Manifest { path: PathBuf, message: String },
    #[error("duplicate plugin `{0}`")]
    Duplicate(String),
    #[error("plugin declaration collides with existing {kind} `{name}`")]
    Collision { kind: &'static str, name: String },
    #[error("component `{name}` rejected: {message}")]
    Component { name: String, message: String },
    #[error("plugin `{0}` not found")]
    NotFound(String),
    #[error("plugin invocation cancelled")]
    Cancelled,
    #[error("plugin value rejected: {0}")]
    Value(String),
    #[error("plugin `{name}` returned {kind}: {message}")]
    Guest {
        name: String,
        kind: String,
        message: String,
        details_json: Option<String>,
    },
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
    pub hostcall_bytes: usize,
    pub value_bytes: usize,
    pub metadata_bytes: usize,
    pub declarations: usize,
    pub arguments: usize,
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
            hostcall_bytes: 4 * 1024 * 1024,
            value_bytes: 4 * 1024 * 1024,
            metadata_bytes: 1024 * 1024,
            declarations: 256,
            arguments: 256,
            wall_time: Duration::from_secs(2),
        }
    }
}

struct State {
    limits: StoreLimits,
    capabilities: Arc<dyn CapabilityProvider>,
    hostcall_bytes: usize,
    declared_effects: Vec<Effect>,
}

impl abi::shoal::plugin::types::Host for State {}

impl abi::shoal::plugin::host::Host for State {
    fn now_ns(&mut self) -> Result<u64, GuestError> {
        let effect = Effect::Time;
        self.authorize(&effect)?;
        self.capabilities
            .now_ns()
            .map_err(|error| guest_error(ErrorKind::Internal, error.message))
    }

    fn read_file(&mut self, path: String) -> Result<Vec<u8>, GuestError> {
        let path = PathBuf::from(path);
        let effect = Effect::FsRead {
            paths: vec![path.clone()],
        };
        self.authorize(&effect)?;
        let bytes = self
            .capabilities
            .read_file(&path)
            .map_err(|error| guest_error(ErrorKind::Internal, error.message))?;
        if bytes.len() > self.hostcall_bytes {
            Err(guest_error(
                ErrorKind::ResourceLimit,
                format!(
                    "hostcall output exceeds the {}-byte limit",
                    self.hostcall_bytes
                ),
            ))
        } else {
            Ok(bytes)
        }
    }
}

impl State {
    fn authorize(&self, effect: &Effect) -> Result<(), GuestError> {
        if !self
            .declared_effects
            .iter()
            .any(|declared| effect_covers(declared, effect))
        {
            return Err(guest_error(
                ErrorKind::PermissionDenied,
                format!("plugin attempted undeclared effect {effect:?}"),
            ));
        }
        self.capabilities
            .authorize(effect)
            .map_err(|error| guest_error(ErrorKind::PermissionDenied, error.message))
    }
}

fn effect_covers(declared: &Effect, actual: &Effect) -> bool {
    match (declared, actual) {
        (Effect::FsRead { paths: allowed }, Effect::FsRead { paths: requested })
        | (Effect::FsWrite { paths: allowed }, Effect::FsWrite { paths: requested })
        | (Effect::FsDelete { paths: allowed }, Effect::FsDelete { paths: requested }) => {
            requested.iter().all(|path| allowed.contains(path))
        }
        (Effect::EnvRead { names: allowed }, Effect::EnvRead { names: requested })
        | (Effect::EnvWrite { names: allowed }, Effect::EnvWrite { names: requested })
        | (Effect::SecretUse { names: allowed }, Effect::SecretUse { names: requested }) => {
            requested.iter().all(|name| allowed.contains(name))
        }
        _ => declared == actual,
    }
}

fn guest_error(kind: ErrorKind, message: String) -> GuestError {
    GuestError {
        kind,
        message,
        details_json: None,
    }
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
}

pub struct Host {
    engine: Engine,
    limits: Limits,
    #[cfg(target_has_atomic = "64")]
    _deadline_ticker: DeadlineTicker,
}

#[cfg(target_has_atomic = "64")]
struct DeadlineTicker {
    cancel: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_has_atomic = "64")]
impl DeadlineTicker {
    fn new(engine: Engine, wall_time: Duration) -> Result<Self, PluginError> {
        let tick = wall_time
            .min(Duration::from_millis(10))
            .max(Duration::from_millis(1));
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("shoal-wasm-epoch".into())
            .spawn(move || {
                while cancel_rx.recv_timeout(tick).is_err() {
                    engine.increment_epoch();
                }
            })
            .map_err(|error| PluginError::Component {
                name: "engine".into(),
                message: format!("failed to start WASM deadline ticker: {error}"),
            })?;
        Ok(Self {
            cancel: Some(cancel_tx),
            thread: Some(thread),
        })
    }
}

#[cfg(target_has_atomic = "64")]
impl Drop for DeadlineTicker {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
        #[cfg(target_has_atomic = "64")]
        let deadline_ticker = DeadlineTicker::new(engine.clone(), limits.wall_time)?;
        Ok(Self {
            engine,
            limits,
            #[cfg(target_has_atomic = "64")]
            _deadline_ticker: deadline_ticker,
        })
    }

    pub fn validate(&self, manifest: Manifest) -> Result<ValidatedPlugin, PluginError> {
        manifest.validate_shape(&manifest.component)?;
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
        let mut store = self.new_store(
            &manifest.name,
            &manifest.effects,
            Arc::new(DenyAllCapabilities),
        )?;
        let linker = self.new_linker(&manifest.name)?;
        let bindings =
            self.instantiate_with_deadline(&manifest.name, &mut store, &linker, &component)?;
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);
        let commands = self.with_deadline(&manifest.name, "commands metadata", || {
            bindings.call_commands(&mut store)
        })?;
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);
        let methods = self.with_deadline(&manifest.name, "methods metadata", || {
            bindings.call_methods(&mut store)
        })?;
        self.validate_metadata(&manifest, &commands, &methods)?;
        Ok(ValidatedPlugin {
            manifest,
            bytes: bytes.into(),
            digest,
            component,
        })
    }

    pub fn invoke_command(
        &self,
        plugin: &ValidatedPlugin,
        command: &str,
        args: Vec<PluginValue>,
        capabilities: Arc<dyn CapabilityProvider>,
    ) -> Result<PluginValue, PluginError> {
        if !plugin
            .manifest
            .commands
            .iter()
            .any(|declaration| declaration.name == command)
        {
            return Err(PluginError::NotFound(format!(
                "{}::{command}",
                plugin.manifest.name
            )));
        }
        if args.len() > self.limits.arguments {
            return Err(PluginError::Value(format!(
                "argument count exceeds the {}-argument limit",
                self.limits.arguments
            )));
        }
        let args = args
            .into_iter()
            .map(|value| value.into_guest(self.limits.value_bytes))
            .collect::<Result<Vec<_>, _>>()?;
        let argument_bytes = args.iter().map(|value| value.payload.len()).sum::<usize>();
        if argument_bytes > self.limits.value_bytes {
            return Err(PluginError::Value(format!(
                "arguments exceed the {}-byte aggregate limit",
                self.limits.value_bytes
            )));
        }
        let cancellation = capabilities.clone();
        let mut store = self.new_store(
            &plugin.manifest.name,
            &plugin.manifest.effects,
            capabilities,
        )?;
        let linker = self.new_linker(&plugin.manifest.name)?;
        let bindings = self.instantiate_with_deadline(
            &plugin.manifest.name,
            &mut store,
            &linker,
            &plugin.component,
        );
        let bindings = match bindings {
            Ok(bindings) => bindings,
            Err(_) if cancellation.cancelled() => return Err(PluginError::Cancelled),
            Err(error) => return Err(error),
        };
        #[cfg(target_has_atomic = "64")]
        store.set_epoch_deadline(1);
        let result = self.with_deadline(&plugin.manifest.name, "command invocation", || {
            bindings.call_invoke_command(&mut store, command, &args)
        });
        let result = match result {
            Ok(result) => result,
            Err(_) if cancellation.cancelled() => return Err(PluginError::Cancelled),
            Err(error) => return Err(error),
        };
        match result {
            Ok(value) => PluginValue::from_guest(value, self.limits.value_bytes),
            Err(error) => {
                let error_bytes =
                    error.message.len() + error.details_json.as_ref().map_or(0, String::len);
                if error_bytes > self.limits.metadata_bytes {
                    return Err(PluginError::Value(format!(
                        "guest error exceeds the {}-byte limit",
                        self.limits.metadata_bytes
                    )));
                }
                Err(PluginError::Guest {
                    name: plugin.manifest.name.clone(),
                    kind: format!("{:?}", error.kind),
                    message: error.message,
                    details_json: error.details_json,
                })
            }
        }
    }

    fn new_store(
        &self,
        name: &str,
        declared_effects: &[Effect],
        capabilities: Arc<dyn CapabilityProvider>,
    ) -> Result<Store<State>, PluginError> {
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
                capabilities,
                hostcall_bytes: self.limits.hostcall_bytes,
                declared_effects: declared_effects.to_vec(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|error| PluginError::Component {
                name: name.to_string(),
                message: error.to_string(),
            })?;
        #[cfg(target_has_atomic = "64")]
        {
            let wall_time = self.limits.wall_time;
            let started = std::time::Instant::now();
            let cancellation = store.data().capabilities.clone();
            store.epoch_deadline_callback(move |_| {
                Ok(
                    if cancellation.cancelled() || started.elapsed() >= wall_time {
                        UpdateDeadline::Interrupt
                    } else {
                        UpdateDeadline::Continue(1)
                    },
                )
            });
            store.set_epoch_deadline(1);
        }
        Ok(store)
    }

    fn new_linker(&self, name: &str) -> Result<Linker<State>, PluginError> {
        let mut linker = Linker::new(&self.engine);
        abi::Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).map_err(
            |error| PluginError::Component {
                name: name.to_string(),
                message: format!("cannot configure Shoal ABI v1 imports: {error}"),
            },
        )?;
        Ok(linker)
    }

    fn instantiate_with_deadline(
        &self,
        name: &str,
        store: &mut Store<State>,
        linker: &Linker<State>,
        component: &Component,
    ) -> Result<abi::Plugin, PluginError> {
        self.with_deadline(name, "Shoal ABI v1 instantiation", || {
            abi::Plugin::instantiate(store, component, linker)
        })
    }

    fn with_deadline<T>(
        &self,
        name: &str,
        operation: &str,
        call: impl FnOnce() -> wasmtime::Result<T>,
    ) -> Result<T, PluginError> {
        call().map_err(|error| PluginError::Component {
            name: name.to_string(),
            message: format!("{operation} failed: {error}"),
        })
    }

    fn validate_metadata(
        &self,
        manifest: &Manifest,
        commands: &[Declaration],
        methods: &[MethodDeclaration],
    ) -> Result<(), PluginError> {
        let reject = |message: String| PluginError::Component {
            name: manifest.name.clone(),
            message,
        };
        if commands.len() > self.limits.declarations || methods.len() > self.limits.declarations {
            return Err(reject(format!(
                "guest metadata exceeds the {}-declaration limit",
                self.limits.declarations
            )));
        }
        let metadata_bytes = commands
            .iter()
            .map(|declaration| declaration.name.len() + declaration.signature_json.len())
            .chain(methods.iter().map(|declaration| {
                declaration.type_name.len()
                    + declaration.name.len()
                    + declaration.signature_json.len()
            }))
            .sum::<usize>();
        if metadata_bytes > self.limits.metadata_bytes {
            return Err(reject(format!(
                "guest metadata exceeds the {}-byte limit",
                self.limits.metadata_bytes
            )));
        }
        let guest_commands = commands
            .iter()
            .map(|declaration| {
                let signature =
                    serde_json::from_str::<serde_json::Value>(&declaration.signature_json)
                        .map_err(|error| {
                            reject(format!(
                                "guest command `{}` returned invalid signature JSON: {error}",
                                declaration.name
                            ))
                        })?;
                Ok((declaration.name.as_str(), signature))
            })
            .collect::<Result<BTreeMap<_, _>, PluginError>>()?;
        if guest_commands.len() != commands.len() {
            return Err(reject("guest returned duplicate command metadata".into()));
        }
        let manifest_commands = manifest
            .commands
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    serde_json::from_str::<serde_json::Value>(&declaration.signature).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if guest_commands != manifest_commands {
            return Err(reject(
                "guest command metadata does not match its manifest".into(),
            ));
        }
        let guest_methods = methods
            .iter()
            .map(|declaration| {
                let signature =
                    serde_json::from_str::<serde_json::Value>(&declaration.signature_json)
                        .map_err(|error| {
                            reject(format!(
                                "guest method `{}.{}` returned invalid signature JSON: {error}",
                                declaration.type_name, declaration.name
                            ))
                        })?;
                Ok((
                    (declaration.type_name.as_str(), declaration.name.as_str()),
                    signature,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, PluginError>>()?;
        if guest_methods.len() != methods.len() {
            return Err(reject("guest returned duplicate method metadata".into()));
        }
        let manifest_methods = manifest
            .methods
            .iter()
            .map(|declaration| {
                (
                    (declaration.type_name.as_str(), declaration.name.as_str()),
                    serde_json::from_str::<serde_json::Value>(&declaration.signature).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if guest_methods != manifest_methods {
            return Err(reject(
                "guest method metadata does not match its manifest".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::shoal::plugin::types::{GuestValue, ValueKind};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;
    use wit_component::{ComponentEncoder, StringEncoding, dummy_module, embed_component_metadata};
    use wit_parser::{ManglingAndAbi, Resolve};

    fn fixture_bytes(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let component = temp.path().join("p.wasm");
        fs::write(&component, bytes).unwrap();
        let manifest = temp.path().join("p.toml");
        fs::write(
            &manifest,
            "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n",
        )
        .unwrap();
        (temp, manifest)
    }

    fn fixture(wat: &str) -> (tempfile::TempDir, PathBuf) {
        fixture_bytes(wat.as_bytes())
    }

    fn valid_component(edit_core_wat: impl FnOnce(&mut String)) -> Vec<u8> {
        let mut resolve = Resolve::new();
        let package = resolve
            .push_str("shoal-plugin.wit", include_str!("../wit/shoal-plugin.wit"))
            .unwrap();
        let world = resolve.select_world(&[package], Some("plugin")).unwrap();
        let core = dummy_module(&resolve, world, ManglingAndAbi::Standard32);
        let mut core_wat = wasmprinter::print_bytes(core).unwrap();
        core_wat = core_wat
            .replace("(memory (;0;) 0)", "(memory (;0;) 1)")
            .replace("unreachable", "i32.const 0");
        edit_core_wat(&mut core_wat);
        let mut core = wat::parse_str(core_wat).unwrap();
        embed_component_metadata(&mut core, &resolve, world, StringEncoding::UTF8).unwrap();
        ComponentEncoder::default()
            .module(&core)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap()
    }

    fn valid_fixture() -> (tempfile::TempDir, PathBuf) {
        fixture_bytes(&valid_component(|_| {}))
    }

    fn set_func_i32_result(wat: &mut String, index: usize, result: u32) {
        let function = wat.find(&format!("(func (;{index};)")).unwrap();
        let body = wat[function..].find("i32.const 0").unwrap() + function;
        wat.replace_range(
            body..body + "i32.const 0".len(),
            &format!("i32.const {result}"),
        );
    }

    fn invokable_component(call_time: bool) -> Vec<u8> {
        valid_component(|wat| {
            set_func_i32_result(wat, 4, 16);
            set_func_i32_result(wat, 6, 128);
            if call_time {
                let function = wat.find("(func (;6;)").unwrap();
                let result = wat[function..].find("i32.const 128").unwrap() + function;
                wat.insert_str(result, "i32.const 200\ncall 0\n");
            }
            let end = wat.rfind(')').unwrap();
            wat.insert_str(
                end,
                concat!(
                    "(data (i32.const 0) \"\\20\\00\\00\\00\\01\\00\\00\\00\")\n",
                    "(data (i32.const 32) \"\\40\\00\\00\\00\\04\\00\\00\\00\\44\\00\\00\\00\\02\\00\\00\\00\")\n",
                    "(data (i32.const 64) \"test{}\")\n",
                    "(data (i32.const 128) \"\\00\\00\\00\\00\\01\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\\00\")\n",
                ),
            );
        })
    }

    struct RecordingCapabilities {
        allow: bool,
        effects: std::sync::Mutex<Vec<Effect>>,
        now_calls: AtomicUsize,
        read_calls: AtomicUsize,
        read_result: Vec<u8>,
    }

    struct CancelledCapabilities;

    impl CapabilityProvider for CancelledCapabilities {
        fn authorize(&self, _effect: &Effect) -> Result<(), CapabilityError> {
            Ok(())
        }

        fn now_ns(&self) -> Result<u64, CapabilityError> {
            Ok(0)
        }

        fn read_file(&self, _path: &Path) -> Result<Vec<u8>, CapabilityError> {
            Ok(Vec::new())
        }

        fn cancelled(&self) -> bool {
            true
        }
    }

    impl RecordingCapabilities {
        fn new(allow: bool) -> Self {
            Self {
                allow,
                effects: std::sync::Mutex::new(Vec::new()),
                now_calls: AtomicUsize::new(0),
                read_calls: AtomicUsize::new(0),
                read_result: b"content".to_vec(),
            }
        }
    }

    impl CapabilityProvider for RecordingCapabilities {
        fn authorize(&self, effect: &Effect) -> Result<(), CapabilityError> {
            self.effects.lock().unwrap().push(effect.clone());
            if self.allow {
                Ok(())
            } else {
                Err(CapabilityError {
                    message: "policy denied".into(),
                })
            }
        }

        fn now_ns(&self) -> Result<u64, CapabilityError> {
            self.now_calls.fetch_add(1, Ordering::Relaxed);
            Ok(42)
        }

        fn read_file(&self, _path: &Path) -> Result<Vec<u8>, CapabilityError> {
            self.read_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.read_result.clone())
        }
    }

    fn state(
        declared_effects: Vec<Effect>,
        capabilities: Arc<dyn CapabilityProvider>,
        hostcall_bytes: usize,
    ) -> State {
        State {
            limits: StoreLimitsBuilder::new().build(),
            capabilities,
            hostcall_bytes,
            declared_effects,
        }
    }

    #[test]
    fn accepts_component_implementing_the_versioned_world() {
        let (_temp, path) = valid_fixture();
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
        assert!(error.contains("Shoal ABI v1"), "{error}");
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

        let (_temp, path) = fixture("(component)");
        let error = registry.load_manifest(&path).unwrap_err().to_string();
        assert!(error.contains("Shoal ABI v1"), "{error}");
    }

    #[test]
    fn manifest_metadata_must_match_guest_exports() {
        let (temp, path) = valid_fixture();
        fs::write(
            &path,
            "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='claimed'\nsignature='{}'\n",
        )
        .unwrap();
        let mut registry = Registry::new(Limits::default()).unwrap();
        let error = registry.load_manifest(&path).unwrap_err().to_string();
        assert!(error.contains("does not match its manifest"), "{error}");
        drop(temp);
    }

    #[test]
    fn command_invocation_uses_the_retained_component_and_typed_value_abi() {
        let (temp, path) = fixture_bytes(&invokable_component(false));
        fs::write(
            &path,
            "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
        )
        .unwrap();
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();
        let metadata = registry.command("test").unwrap();
        assert_eq!(metadata.plugin, "test");
        assert_eq!(metadata.declaration.name, "test");
        assert_eq!(registry.command_names().collect::<Vec<_>>(), vec!["test"]);
        fs::write(temp.path().join("p.wasm"), b"replaced after validation").unwrap();
        let value = registry
            .invoke_declared_command("test", Vec::new(), Arc::new(DenyAllCapabilities))
            .unwrap();
        assert_eq!(value, PluginValue::Null);
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn session_cancellation_interrupts_compute_only_guest_code() {
        let component = valid_component(|wat| {
            set_func_i32_result(wat, 4, 16);
            set_func_i32_result(wat, 6, 128);
            let function = wat.find("(func (;6;)").unwrap();
            let result = wat[function..].find("i32.const 128").unwrap() + function;
            wat.insert_str(result, "(loop $forever (br $forever))\n");
            let end = wat.rfind(')').unwrap();
            wat.insert_str(
                end,
                concat!(
                    "(data (i32.const 0) \"\\20\\00\\00\\00\\01\\00\\00\\00\")\n",
                    "(data (i32.const 32) \"\\40\\00\\00\\00\\04\\00\\00\\00\\44\\00\\00\\00\\02\\00\\00\\00\")\n",
                    "(data (i32.const 64) \"test{}\")\n",
                ),
            );
        });
        let (_temp, path) = fixture_bytes(&component);
        fs::write(
            &path,
            "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\neffects=[]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
        )
        .unwrap();
        let mut registry = Registry::new(Limits {
            fuel: u64::MAX,
            wall_time: Duration::from_secs(5),
            ..Limits::default()
        })
        .unwrap();
        registry.load_manifest(&path).unwrap();
        let started = Instant::now();
        let error = registry
            .invoke_declared_command("test", Vec::new(), Arc::new(CancelledCapabilities))
            .unwrap_err();
        assert!(matches!(error, PluginError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn guest_hostcall_cannot_exceed_declared_and_authorized_effects() {
        let (_temp, path) = fixture_bytes(&invokable_component(true));
        fs::write(
            &path,
            "name='test'\nversion='0.1'\nabi_version=1\ncomponent='p.wasm'\n\
             effects=[{kind='time'}]\n\
             [[commands]]\nname='test'\nsignature='{}'\n",
        )
        .unwrap();
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();

        let denied = Arc::new(RecordingCapabilities::new(false));
        assert_eq!(
            registry
                .invoke_command("test", "test", Vec::new(), denied.clone())
                .unwrap(),
            PluginValue::Null
        );
        assert_eq!(denied.effects.lock().unwrap().as_slice(), &[Effect::Time]);
        assert_eq!(denied.now_calls.load(Ordering::Relaxed), 0);

        let allowed = Arc::new(RecordingCapabilities::new(true));
        assert_eq!(
            registry
                .invoke_command("test", "test", Vec::new(), allowed.clone())
                .unwrap(),
            PluginValue::Null
        );
        assert_eq!(allowed.now_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn typed_effects_and_abi_version_are_strict_manifest_data() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("plugin.toml");
        fs::write(
            &path,
            "name='p'\nversion='1'\nabi_version=1\ncomponent='p.wasm'\neffects=[{kind='time'}]\n",
        )
        .unwrap();
        let manifest = Manifest::load(&path).unwrap();
        assert_eq!(manifest.effects, vec![Effect::Time]);

        fs::write(
            &path,
            "name='p'\nversion='1'\nabi_version=9\ncomponent='p.wasm'\neffects=[]\n",
        )
        .unwrap();
        assert!(Manifest::load(&path).is_err());
    }

    #[test]
    fn value_envelope_is_versioned_typed_and_bounded() {
        let values = [
            PluginValue::Null,
            PluginValue::Bool(true),
            PluginValue::Signed(-7),
            PluginValue::Unsigned(9),
            PluginValue::Float(1.25),
            PluginValue::Text("hello".into()),
            PluginValue::Bytes(vec![0, 255]),
            PluginValue::Json(serde_json::json!({"a": [1, true]})),
        ];
        for value in values {
            let guest = value.clone().into_guest(1024).unwrap();
            assert_eq!(PluginValue::from_guest(guest, 1024).unwrap(), value);
        }
        assert!(PluginValue::Bytes(vec![0; 5]).into_guest(4).is_err());
        assert!(
            PluginValue::from_guest(
                GuestValue {
                    abi_version: 99,
                    kind: ValueKind::Null,
                    payload: Vec::new(),
                },
                4,
            )
            .is_err()
        );
    }

    #[test]
    fn hostcalls_require_both_declaration_and_runtime_authorization() {
        let allowed = Arc::new(RecordingCapabilities::new(true));
        let mut undeclared = state(Vec::new(), allowed.clone(), 1024);
        let error = <State as abi::shoal::plugin::host::Host>::now_ns(&mut undeclared).unwrap_err();
        assert_eq!(error.kind, ErrorKind::PermissionDenied);
        assert!(allowed.effects.lock().unwrap().is_empty());
        assert_eq!(allowed.now_calls.load(Ordering::Relaxed), 0);

        let denied = Arc::new(RecordingCapabilities::new(false));
        let mut declared = state(vec![Effect::Time], denied.clone(), 1024);
        let error = <State as abi::shoal::plugin::host::Host>::now_ns(&mut declared).unwrap_err();
        assert_eq!(error.kind, ErrorKind::PermissionDenied);
        assert_eq!(denied.effects.lock().unwrap().as_slice(), &[Effect::Time]);
        assert_eq!(denied.now_calls.load(Ordering::Relaxed), 0);

        let allowed = Arc::new(RecordingCapabilities::new(true));
        let mut declared = state(vec![Effect::Time], allowed.clone(), 1024);
        assert_eq!(
            <State as abi::shoal::plugin::host::Host>::now_ns(&mut declared).unwrap(),
            42
        );
        assert_eq!(allowed.now_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn filesystem_hostcall_authorizes_exact_path_and_bounds_output() {
        let capabilities = Arc::new(RecordingCapabilities::new(true));
        let effect = Effect::FsRead {
            paths: vec![PathBuf::from("/work/input")],
        };
        let mut state = state(vec![effect.clone()], capabilities.clone(), 4);
        let error =
            <State as abi::shoal::plugin::host::Host>::read_file(&mut state, "/work/input".into())
                .unwrap_err();
        assert_eq!(error.kind, ErrorKind::ResourceLimit);
        assert_eq!(capabilities.effects.lock().unwrap().as_slice(), &[effect]);
        assert_eq!(capabilities.read_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn duplicate_is_deterministic() {
        let (_temp, path) = valid_fixture();
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();
        assert!(matches!(
            registry.load_manifest(&path),
            Err(PluginError::Duplicate(_))
        ));
    }

    #[test]
    fn artifact_is_bound_to_the_validated_bytes_and_digest() {
        let (temp, path) = valid_fixture();
        let original = fs::read(temp.path().join("p.wasm")).unwrap();
        let mut registry = Registry::new(Limits::default()).unwrap();
        registry.load_manifest(&path).unwrap();
        fs::write(temp.path().join("p.wasm"), b"not wasm anymore").unwrap();

        let plugin = registry.get("test").unwrap();
        assert_eq!(plugin.bytes(), original);
        assert_eq!(plugin.digest(), blake3::hash(&original));
        assert_eq!(
            plugin
                .component
                .component_type()
                .imports(plugin.component.engine())
                .count(),
            2
        );
    }

    #[test]
    fn manifest_and_component_reads_are_bounded() {
        let (_temp, path) = valid_fixture();
        let mut registry = Registry::new(Limits {
            manifest_bytes: 8,
            ..Limits::default()
        })
        .unwrap();
        assert!(registry.load_manifest(&path).is_err());

        let (_temp, path) = valid_fixture();
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
        let component = valid_component(|wat| {
            *wat = wat.replace("(memory (;0;) 1)", "(memory (;0;) 2)");
        });
        let (_temp, path) = fixture_bytes(&component);
        let mut registry = Registry::new(Limits {
            memory_bytes: 64 * 1024,
            ..Limits::default()
        })
        .unwrap();
        assert!(registry.load_manifest(&path).is_err());

        let component = valid_component(|wat| {
            let end = wat.rfind(')').unwrap();
            wat.insert_str(end, "(memory 1)\n");
        });
        let (_temp, path) = fixture_bytes(&component);
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
        let component = valid_component(|wat| {
            let end = wat.rfind(')').unwrap();
            wat.insert_str(
                end,
                "(func $start (loop $forever (br $forever)))\n(start $start)\n",
            );
        });
        let (_temp, path) = fixture_bytes(&component);
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
