//! Validated plugin discovery, collision checks, and command resolution.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use shoal_leash::Effect;

use super::{
    CapabilityProvider, CommandDecl, Host, Limits, Manifest, PluginError, PluginValue,
    ValidatedPlugin,
};

pub struct Registry {
    host: Host,
    plugins: BTreeMap<String, ValidatedPlugin>,
}

/// Immutable command-resolution data retained by a validated registry.
#[derive(Debug, Clone, Copy)]
pub struct CommandMetadata<'a> {
    pub plugin: &'a str,
    pub declaration: &'a CommandDecl,
    pub effects: &'a [Effect],
    pub digest: blake3::Hash,
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
        for command in &manifest.commands {
            if self.plugins.values().any(|plugin| {
                plugin
                    .manifest
                    .commands
                    .iter()
                    .any(|existing| existing.name == command.name)
            }) {
                return Err(PluginError::Collision {
                    kind: "command",
                    name: command.name.clone(),
                });
            }
        }
        for method in &manifest.methods {
            if self.plugins.values().any(|plugin| {
                plugin.manifest.methods.iter().any(|existing| {
                    existing.type_name == method.type_name && existing.name == method.name
                })
            }) {
                return Err(PluginError::Collision {
                    kind: "method",
                    name: format!("{}.{}", method.type_name, method.name),
                });
            }
        }
        let plugin = self.host.validate(manifest)?;
        self.plugins.insert(plugin.manifest.name.clone(), plugin);
        Ok(())
    }

    /// Discover manifests in caller-provided directory order and lexical path
    /// order within each directory, accumulating malformed-plugin errors.
    pub fn discover(
        dirs: &[PathBuf],
        limits: Limits,
    ) -> Result<(Self, Vec<PluginError>), PluginError> {
        let mut registry = Self::new(limits)?;
        let mut errors = Vec::new();
        for dir in dirs {
            errors.extend(registry.load_dir(dir));
        }
        Ok((registry, errors))
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

    pub fn command(&self, name: &str) -> Option<CommandMetadata<'_>> {
        self.plugins.values().find_map(|plugin| {
            plugin
                .manifest
                .commands
                .iter()
                .find(|command| command.name == name)
                .map(|declaration| CommandMetadata {
                    plugin: &plugin.manifest.name,
                    declaration,
                    effects: &plugin.manifest.effects,
                    digest: plugin.digest,
                })
        })
    }

    pub fn command_names(&self) -> impl Iterator<Item = &str> {
        self.plugins.values().flat_map(|plugin| {
            plugin
                .manifest
                .commands
                .iter()
                .map(|command| command.name.as_str())
        })
    }

    pub fn invoke_declared_command(
        &self,
        command: &str,
        args: Vec<PluginValue>,
        capabilities: Arc<dyn CapabilityProvider>,
    ) -> Result<PluginValue, PluginError> {
        let metadata = self
            .command(command)
            .ok_or_else(|| PluginError::NotFound(command.to_string()))?;
        self.invoke_command(metadata.plugin, command, args, capabilities)
    }

    pub fn invoke_command(
        &self,
        plugin: &str,
        command: &str,
        args: Vec<PluginValue>,
        capabilities: Arc<dyn CapabilityProvider>,
    ) -> Result<PluginValue, PluginError> {
        let plugin = self
            .plugins
            .get(plugin)
            .ok_or_else(|| PluginError::NotFound(plugin.to_string()))?;
        self.host
            .invoke_command(plugin, command, args, capabilities)
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}
