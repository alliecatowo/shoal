use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline,
    component::{Component, HasSelf, Linker},
};

const MAX_COMPILATION_JOBS: usize = 2;
const MAX_COMPILATION_WAIT: Duration = Duration::from_secs(10);

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
    /// Read no more than `max_bytes`. Implementations must reject an
    /// oversized source without first materializing the whole file.
    fn read_file(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, CapabilityError>;

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

    fn read_file(&self, _path: &Path, _max_bytes: usize) -> Result<Vec<u8>, CapabilityError> {
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
    pub wasm_stack_bytes: usize,
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
    pub hostcall_total_bytes: usize,
    pub hostcall_calls: usize,
    pub value_bytes: usize,
    pub value_depth: usize,
    pub value_nodes: usize,
    pub metadata_bytes: usize,
    pub declarations: usize,
    pub arguments: usize,
    pub plugins: usize,
    pub registry_component_bytes: usize,
    pub discovery_entries: usize,
    /// Maximum component compilations admitted concurrently across this
    /// process. This may be lowered from the hard ceiling of two; Wasmtime's
    /// optional parallel-compiler feature is disabled.
    pub compilation_jobs: usize,
    /// Maximum time a registry load may wait for a compilation slot. This
    /// bounds admission latency without pretending the synchronous compiler
    /// itself can be interrupted. The hard ceiling is ten seconds.
    pub compilation_wait: Duration,
    /// Coarse wall deadline for guest code run during instantiation. Component
    /// compilation is instead bounded by `component_bytes` because Wasmtime's
    /// synchronous compiler is not epoch-interruptible.
    pub wall_time: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 10_000_000,
            wasm_stack_bytes: 512 * 1024,
            memory_bytes: 16 * 1024 * 1024,
            memories: 1,
            table_elements: 10_000,
            tables: 4,
            instances: 16,
            manifest_bytes: 256 * 1024,
            component_bytes: 16 * 1024 * 1024,
            hostcall_bytes: 4 * 1024 * 1024,
            hostcall_total_bytes: 16 * 1024 * 1024,
            hostcall_calls: 64,
            value_bytes: 4 * 1024 * 1024,
            value_depth: 64,
            value_nodes: 65_536,
            metadata_bytes: 1024 * 1024,
            declarations: 256,
            arguments: 256,
            plugins: 64,
            registry_component_bytes: 64 * 1024 * 1024,
            discovery_entries: 1024,
            compilation_jobs: MAX_COMPILATION_JOBS,
            compilation_wait: Duration::from_secs(2),
            wall_time: Duration::from_secs(2),
        }
    }
}

mod state;
use state::State;

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

mod host;
pub use host::Host;
#[cfg(test)]
use host::{CompilationAdmission, acquire_compilation};

fn bounded_text(mut message: String, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message;
    }
    if max_bytes < "…".len() {
        return ".".repeat(max_bytes);
    }
    let mut end = max_bytes.saturating_sub("…".len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push('…');
    message
}

fn validate_limits(limits: Limits) -> Result<(), PluginError> {
    let invalid = |message: &str| PluginError::Component {
        name: "engine".into(),
        message: message.into(),
    };
    if limits.fuel == 0 {
        return Err(invalid("WASM limit `fuel` must be non-zero"));
    }
    for (name, value) in [
        ("wasm_stack_bytes", limits.wasm_stack_bytes),
        ("memory_bytes", limits.memory_bytes),
        ("memories", limits.memories),
        ("table_elements", limits.table_elements),
        ("tables", limits.tables),
        ("instances", limits.instances),
        ("manifest_bytes", limits.manifest_bytes),
        ("component_bytes", limits.component_bytes),
        ("hostcall_bytes", limits.hostcall_bytes),
        ("hostcall_total_bytes", limits.hostcall_total_bytes),
        ("hostcall_calls", limits.hostcall_calls),
        ("value_bytes", limits.value_bytes),
        ("value_depth", limits.value_depth),
        ("value_nodes", limits.value_nodes),
        ("metadata_bytes", limits.metadata_bytes),
        ("declarations", limits.declarations),
        ("arguments", limits.arguments),
        ("plugins", limits.plugins),
        ("registry_component_bytes", limits.registry_component_bytes),
        ("discovery_entries", limits.discovery_entries),
        ("compilation_jobs", limits.compilation_jobs),
    ] {
        if value == 0 {
            return Err(invalid(&format!("WASM limit `{name}` must be non-zero")));
        }
    }
    limits
        .memory_bytes
        .checked_mul(limits.memories)
        .ok_or_else(|| invalid("aggregate WASM memory limit overflows usize"))?;
    limits
        .table_elements
        .checked_mul(limits.tables)
        .ok_or_else(|| invalid("aggregate WASM table limit overflows usize"))?;
    if limits.hostcall_bytes > limits.hostcall_total_bytes {
        return Err(invalid(
            "per-call hostcall byte limit exceeds aggregate hostcall budget",
        ));
    }
    if limits.component_bytes > limits.registry_component_bytes {
        return Err(invalid(
            "per-component byte limit exceeds registry component budget",
        ));
    }
    if limits.compilation_jobs > MAX_COMPILATION_JOBS {
        return Err(invalid(&format!(
            "WASM compilation_jobs exceeds the process-wide ceiling of {MAX_COMPILATION_JOBS}"
        )));
    }
    if limits.compilation_wait.is_zero() {
        return Err(invalid("WASM compilation_wait must be non-zero"));
    }
    if limits.compilation_wait > MAX_COMPILATION_WAIT {
        return Err(invalid(&format!(
            "WASM compilation_wait exceeds the process-wide ceiling of {MAX_COMPILATION_WAIT:?}"
        )));
    }
    if limits.wall_time.is_zero() {
        return Err(invalid("WASM wall_time must be non-zero"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "lib/tests.rs"]
mod tests;
