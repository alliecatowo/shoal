//! Manifest model: the native `.reef.toml` / `[reef]`-in-`shoal.toml` format and
//! read-only adapters for foreign manifests (`mise.toml`, `.tool-versions`).
//!
//! Parsing is pure and total over the input text: a malformed manifest yields a
//! [`ManifestError`] rather than panicking, and unknown keys are ignored so the
//! format can grow.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::runner::{Invocation, RunnerTable};
use crate::version::Constraint;

/// A single tool requirement from a `[tools]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReq {
    pub constraint: Constraint,
    /// A forced provider (`{ provider = "mise" }`), if the entry pinned one.
    pub provider: Option<String>,
}

impl ToolReq {
    pub fn new(constraint: Constraint) -> ToolReq {
        ToolReq {
            constraint,
            provider: None,
        }
    }
}

/// A parsed reef manifest (native or adapted from a foreign format).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReefManifest {
    pub tools: BTreeMap<String, ToolReq>,
    pub runners: RunnerTable,
    /// Child PATH is synthesized-only when true (no system tail).
    pub hermetic: bool,
}

/// The kind of manifest a scope came from — surfaced in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    /// Native `.reef.toml`.
    Reef,
    /// `[reef]` table in a `shoal.toml` (user scope).
    ShoalUser,
    /// Foreign `mise.toml` / `.mise.toml`.
    Mise,
    /// Foreign `.tool-versions`.
    ToolVersions,
}

impl ManifestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ManifestKind::Reef => "reef",
            ManifestKind::ShoalUser => "user",
            ManifestKind::Mise => "mise",
            ManifestKind::ToolVersions => "tool-versions",
        }
    }
}

/// Error parsing a manifest.
#[derive(Debug, Clone)]
pub struct ManifestError {
    pub msg: String,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "manifest error: {}", self.msg)
    }
}
impl std::error::Error for ManifestError {}

impl ReefManifest {
    /// Parse a native `.reef.toml`.
    pub fn parse_reef(text: &str) -> Result<ReefManifest, ManifestError> {
        crate::input::validate_toml_text(text).map_err(manifest_error)?;
        let raw: RawReef =
            toml::from_str(text).map_err(|e| ManifestError { msg: e.to_string() })?;
        validate_raw_reef(&raw).map_err(manifest_error)?;
        let manifest = raw.into_manifest();
        validate_manifest(&manifest).map_err(manifest_error)?;
        Ok(manifest)
    }

    /// Parse the `[reef]` table out of a `shoal.toml` (user scope). A file with
    /// no `[reef]` table yields an empty manifest.
    pub fn parse_shoal_reef(text: &str) -> Result<ReefManifest, ManifestError> {
        crate::input::validate_toml_text(text).map_err(manifest_error)?;
        let raw: RawShoal =
            toml::from_str(text).map_err(|e| ManifestError { msg: e.to_string() })?;
        let raw = raw.reef.unwrap_or_default();
        validate_raw_reef(&raw).map_err(manifest_error)?;
        let manifest = raw.into_manifest();
        validate_manifest(&manifest).map_err(manifest_error)?;
        Ok(manifest)
    }

    /// Adapt a foreign `mise.toml` / `.mise.toml` (read-only). Reads the
    /// `[tools]` table; values may be a string, an array (first entry wins), or
    /// a table with a `version` key. Runners/hermetic are never inferred.
    pub fn parse_mise(text: &str) -> Result<ReefManifest, ManifestError> {
        crate::input::validate_toml_text(text).map_err(manifest_error)?;
        let raw: RawMise =
            toml::from_str(text).map_err(|e| ManifestError { msg: e.to_string() })?;
        if raw.tools.len() > crate::input::REEF_MAX_TOOLS {
            return Err(manifest_error(format!(
                "manifest has {} tools; maximum is {}",
                raw.tools.len(),
                crate::input::REEF_MAX_TOOLS
            )));
        }
        let mut tools = BTreeMap::new();
        for (name, val) in raw.tools {
            if let Some(constraint) = val.constraint() {
                tools.insert(name, ToolReq::new(constraint));
            }
        }
        let manifest = ReefManifest {
            tools,
            runners: RunnerTable::default(),
            hermetic: false,
        };
        validate_manifest(&manifest).map_err(manifest_error)?;
        Ok(manifest)
    }

    /// Adapt a foreign `.tool-versions` file (read-only). Each non-comment line
    /// is `tool version [fallback…]`; the first version token is the constraint.
    pub fn parse_tool_versions(text: &str) -> Result<ReefManifest, ManifestError> {
        if text.len() > crate::input::REEF_MANIFEST_MAX_BYTES {
            return Err(manifest_error(format!(
                "manifest exceeds the {}-byte limit",
                crate::input::REEF_MANIFEST_MAX_BYTES
            )));
        }
        let mut tools = BTreeMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(name), Some(ver)) = (it.next(), it.next()) else {
                continue;
            };
            crate::input::validate_string("tool name", name).map_err(manifest_error)?;
            crate::input::validate_string("tool constraint", ver).map_err(manifest_error)?;
            if !tools.contains_key(name) && tools.len() >= crate::input::REEF_MAX_TOOLS {
                return Err(manifest_error(format!(
                    "manifest exceeds the {}-tool limit",
                    crate::input::REEF_MAX_TOOLS
                )));
            }
            tools.insert(name.to_string(), ToolReq::new(Constraint::parse(ver)));
        }
        Ok(ReefManifest {
            tools,
            runners: RunnerTable::default(),
            hermetic: false,
        })
    }
}

fn manifest_error(message: impl Into<String>) -> ManifestError {
    ManifestError {
        msg: message.into(),
    }
}

fn validate_manifest(manifest: &ReefManifest) -> Result<(), String> {
    if manifest.tools.len() > crate::input::REEF_MAX_TOOLS {
        return Err(format!(
            "manifest has {} tools; maximum is {}",
            manifest.tools.len(),
            crate::input::REEF_MAX_TOOLS
        ));
    }
    if manifest.runners.len() > crate::input::REEF_MAX_RUNNERS {
        return Err(format!(
            "manifest has {} runners; maximum is {}",
            manifest.runners.len(),
            crate::input::REEF_MAX_RUNNERS
        ));
    }
    for (name, requirement) in &manifest.tools {
        crate::input::validate_string("tool name", name)?;
        crate::input::validate_string("tool constraint", &requirement.constraint.to_string())?;
        if let Some(provider) = &requirement.provider {
            crate::input::validate_string("provider", provider)?;
        }
    }
    for (extension, invocation) in manifest.runners.iter() {
        crate::input::validate_string("runner extension", extension)?;
        crate::input::validate_string("runner tool", &invocation.tool)?;
        if invocation.args_template.len() > crate::input::REEF_MAX_RUNNER_ARGS {
            return Err(format!(
                "runner {extension:?} has {} arguments; maximum is {}",
                invocation.args_template.len(),
                crate::input::REEF_MAX_RUNNER_ARGS
            ));
        }
        for argument in &invocation.args_template {
            crate::input::validate_string("runner argument", argument)?;
        }
    }
    Ok(())
}

fn validate_raw_reef(raw: &RawReef) -> Result<(), String> {
    if raw.tools.len() > crate::input::REEF_MAX_TOOLS {
        return Err(format!(
            "manifest has {} tools; maximum is {}",
            raw.tools.len(),
            crate::input::REEF_MAX_TOOLS
        ));
    }
    if raw.runners.len() > crate::input::REEF_MAX_RUNNERS {
        return Err(format!(
            "manifest has {} runners; maximum is {}",
            raw.runners.len(),
            crate::input::REEF_MAX_RUNNERS
        ));
    }
    Ok(())
}

// --- raw serde layer -------------------------------------------------------

#[derive(Deserialize, Default)]
struct RawShoal {
    reef: Option<RawReef>,
}

#[derive(Deserialize, Default)]
struct RawReef {
    #[serde(default)]
    tools: BTreeMap<String, RawToolSpec>,
    #[serde(default)]
    runners: BTreeMap<String, RawRunnerSpec>,
    #[serde(default)]
    options: RawOptions,
}

#[derive(Deserialize, Default)]
struct RawOptions {
    #[serde(default)]
    hermetic: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawToolSpec {
    Str(String),
    Table {
        version: Option<String>,
        provider: Option<String>,
    },
}

impl RawToolSpec {
    fn into_req(self) -> ToolReq {
        match self {
            RawToolSpec::Str(s) => ToolReq::new(Constraint::parse(&s)),
            RawToolSpec::Table { version, provider } => {
                let constraint = version
                    .map(|v| Constraint::parse(&v))
                    .unwrap_or(Constraint::Any);
                ToolReq {
                    constraint,
                    provider,
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRunnerSpec {
    Str(String),
    Table {
        tool: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl RawRunnerSpec {
    fn into_invocation(self) -> Invocation {
        match self {
            RawRunnerSpec::Str(tool) => Invocation {
                tool,
                args_template: Vec::new(),
            },
            RawRunnerSpec::Table { tool, args } => Invocation {
                tool,
                args_template: args,
            },
        }
    }
}

impl RawReef {
    fn into_manifest(self) -> ReefManifest {
        let tools = self
            .tools
            .into_iter()
            .map(|(k, v)| (k, v.into_req()))
            .collect();
        let mut runners = RunnerTable::empty();
        for (ext, spec) in self.runners {
            runners.insert(ext, spec.into_invocation());
        }
        ReefManifest {
            tools,
            runners,
            hermetic: self.options.hermetic,
        }
    }
}

// mise.toml [tools] values.
#[derive(Deserialize, Default)]
struct RawMise {
    #[serde(default)]
    tools: BTreeMap<String, RawMiseSpec>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawMiseSpec {
    Str(String),
    List(Vec<String>),
    Table { version: Option<String> },
}

impl RawMiseSpec {
    fn constraint(&self) -> Option<Constraint> {
        match self {
            RawMiseSpec::Str(s) => Some(Constraint::parse(s)),
            RawMiseSpec::List(v) => v.first().map(|s| Constraint::parse(s)),
            RawMiseSpec::Table { version } => version.as_ref().map(|s| Constraint::parse(s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_native_reef() {
        let text = r#"
[tools]
node = "22"
python = "3.12"
rg = "*"
go = { provider = "mise" }
deno = { version = "1.4", provider = "mise" }

[runners]
py = "python"
ts = { tool = "deno", args = ["run"] }

[options]
hermetic = true
"#;
        let m = ReefManifest::parse_reef(text).unwrap();
        assert_eq!(m.tools["node"].constraint, Constraint::parse("22"));
        assert_eq!(m.tools["rg"].constraint, Constraint::Any);
        assert_eq!(m.tools["go"].provider.as_deref(), Some("mise"));
        assert_eq!(m.tools["go"].constraint, Constraint::Any);
        assert_eq!(m.tools["deno"].provider.as_deref(), Some("mise"));
        assert_eq!(m.tools["deno"].constraint, Constraint::parse("1.4"));
        assert!(m.hermetic);
        let ts = m.runners.get("ts").unwrap();
        assert_eq!(ts.tool, "deno");
        assert_eq!(ts.args_template, vec!["run".to_string()]);
    }

    #[test]
    fn default_hermetic_is_false() {
        let m = ReefManifest::parse_reef("[tools]\nnode = \"22\"\n").unwrap();
        assert!(!m.hermetic);
    }

    #[test]
    fn parse_shoal_user_reef() {
        let text = r#"
[other]
theme = "dark"

[reef.tools]
node = "20"

[reef.options]
hermetic = false
"#;
        let m = ReefManifest::parse_shoal_reef(text).unwrap();
        assert_eq!(m.tools["node"].constraint, Constraint::parse("20"));
        assert!(!m.hermetic);
    }

    #[test]
    fn shoal_without_reef_is_empty() {
        let m = ReefManifest::parse_shoal_reef("[ui]\ntheme=\"x\"\n").unwrap();
        assert!(m.tools.is_empty());
    }

    #[test]
    fn parse_foreign_mise() {
        let text = r#"
[tools]
node = "22"
python = ["3.12", "3.11"]
go = { version = "1.21" }
"#;
        let m = ReefManifest::parse_mise(text).unwrap();
        assert_eq!(m.tools["node"].constraint, Constraint::parse("22"));
        assert_eq!(m.tools["python"].constraint, Constraint::parse("3.12"));
        assert_eq!(m.tools["go"].constraint, Constraint::parse("1.21"));
    }

    #[test]
    fn parse_foreign_tool_versions() {
        let text = "# a comment\nnodejs 22.3.0\npython 3.12.4 3.11.9\n\nruby 3.3\n";
        let m = ReefManifest::parse_tool_versions(text).unwrap();
        assert_eq!(m.tools["nodejs"].constraint, Constraint::parse("22.3.0"));
        assert_eq!(m.tools["python"].constraint, Constraint::parse("3.12.4"));
        assert_eq!(m.tools["ruby"].constraint, Constraint::parse("3.3"));
    }

    #[test]
    fn manifest_width_and_strings_are_bounded_but_unknown_fields_remain_forward_compatible() {
        let tools = (0..=crate::input::REEF_MAX_TOOLS)
            .map(|index| format!("t{index}='1'\n"))
            .collect::<String>();
        assert!(ReefManifest::parse_reef(&format!("[tools]\n{tools}")).is_err());

        let huge = "x".repeat(crate::input::REEF_MAX_STRING_BYTES + 1);
        assert!(ReefManifest::parse_reef(&format!("[tools]\nnode='{huge}'\n")).is_err());

        let compatible = ReefManifest::parse_reef(
            "unknown_future_key=true\n[tools]\nnode='22'\n[options]\nfuture=false\n",
        )
        .unwrap();
        assert!(compatible.tools.contains_key("node"));
    }

    #[test]
    fn malformed_toml_errors() {
        assert!(ReefManifest::parse_reef("[tools\nnode = ").is_err());
    }
}
