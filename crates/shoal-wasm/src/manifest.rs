//! Bounded plugin-manifest loading and semantic validation.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use shoal_leash::Effect;

use super::{ABI_VERSION, Limits, PluginError};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub abi_version: u32,
    pub component: PathBuf,
    #[serde(default)]
    pub commands: Vec<CommandDecl>,
    #[serde(default)]
    pub methods: Vec<MethodDecl>,
    #[serde(default)]
    pub effects: Vec<Effect>,
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

    pub(super) fn load_bounded(path: &Path, max_bytes: usize) -> Result<Self, PluginError> {
        let bytes = read_at_most(path, max_bytes).map_err(|message| PluginError::Manifest {
            path: path.into(),
            message,
        })?;
        let source = String::from_utf8(bytes).map_err(|error| PluginError::Manifest {
            path: path.into(),
            message: format!("manifest is not UTF-8: {error}"),
        })?;
        let mut manifest: Self =
            toml::from_str(&source).map_err(|error| PluginError::Manifest {
                path: path.into(),
                message: error.to_string(),
            })?;
        manifest.validate_shape(path)?;
        if manifest.component.is_relative() {
            manifest.component = path
                .parent()
                .unwrap_or(Path::new("."))
                .join(&manifest.component);
        }
        Ok(manifest)
    }

    pub(super) fn validate_shape(&self, path: &Path) -> Result<(), PluginError> {
        if self.name.trim().is_empty() || self.version.trim().is_empty() {
            return Err(PluginError::Manifest {
                path: path.into(),
                message: "name and version are required".into(),
            });
        }
        if self.abi_version != ABI_VERSION {
            return Err(PluginError::Manifest {
                path: path.into(),
                message: format!(
                    "unsupported plugin ABI {}; expected {ABI_VERSION}",
                    self.abi_version
                ),
            });
        }
        self.validate_declarations(path)
    }

    fn validate_declarations(&self, path: &Path) -> Result<(), PluginError> {
        let invalid = |message: String| PluginError::Manifest {
            path: path.into(),
            message,
        };
        let mut commands = BTreeSet::new();
        for command in &self.commands {
            if command.name.trim().is_empty() {
                return Err(invalid("command names must not be empty".into()));
            }
            if !commands.insert(command.name.as_str()) {
                return Err(invalid(format!(
                    "duplicate command declaration `{}`",
                    command.name
                )));
            }
            serde_json::from_str::<serde_json::Value>(&command.signature).map_err(|error| {
                invalid(format!(
                    "command `{}` has invalid signature JSON: {error}",
                    command.name
                ))
            })?;
        }
        let mut methods = BTreeSet::new();
        for method in &self.methods {
            if method.type_name.trim().is_empty() || method.name.trim().is_empty() {
                return Err(invalid("method type/name must not be empty".into()));
            }
            if !methods.insert((method.type_name.as_str(), method.name.as_str())) {
                return Err(invalid(format!(
                    "duplicate method declaration `{}.{}`",
                    method.type_name, method.name
                )));
            }
            serde_json::from_str::<serde_json::Value>(&method.signature).map_err(|error| {
                invalid(format!(
                    "method `{}.{}` has invalid signature JSON: {error}",
                    method.type_name, method.name
                ))
            })?;
        }
        for (index, effect) in self.effects.iter().enumerate() {
            if self.effects[..index].contains(effect) {
                return Err(invalid(format!("duplicate effect declaration: {effect:?}")));
            }
        }
        Ok(())
    }
}

pub(super) fn read_at_most(path: &Path, max_bytes: usize) -> Result<Vec<u8>, String> {
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
