//! Discovery, layering, and validation — turns a chain of on-disk files (plus
//! the process environment) into a validated [`Config`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Config, ConfigError};

mod input;

use input::{check_toml_nesting, read_config_file};

/// Maximum bytes retained from any one layered `shoal.toml`. Configuration is
/// a control-plane input, not a data transport; one MiB leaves ample room for
/// large alias/environment maps while bounding startup memory and TOML work.
pub const CONFIG_FILE_MAX_BYTES: usize = 1024 * 1024;
/// Maximum bracket/inline-table nesting admitted before TOML parsing.
pub const CONFIG_TOML_MAX_NESTING: usize = 64;
/// Maximum UTF-8 bytes copied from one recognized environment override.
pub const CONFIG_ENV_VALUE_MAX_BYTES: usize = 64 * 1024;

/// The result of a successful [`load`]: the merged, validated config, any
/// non-fatal warnings collected along the way (unknown keys — see
/// `site/content/internals/configuration-reference.md`), and the layer files that were actually found and
/// merged (in precedence order, lowest first) — handy for a `shoal doctor`
/// style "here's what I read" report.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
    pub sources: Vec<PathBuf>,
}

/// The four layers `load` merges, in precedence order (later wins). Every
/// field is optional — a `None`/missing file is simply skipped, never an
/// error (site/content/internals/configuration-reference.md).
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub system: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
    /// The complete set of `(name, value)` pairs consulted for env overrides
    /// (site/content/internals/configuration-reference.md). Callers normally pass `std::env::vars_os()`
    /// verbatim; tests pass a small synthetic set.
    pub env: Vec<(OsString, OsString)>,
}

impl LoadOptions {
    /// Discover the layered config locations for `cwd`, per
    /// `site/content/internals/configuration-reference.md`:
    ///
    /// 1. **system** — `/etc/shoal/shoal.toml`.
    /// 2. **user** — `$XDG_CONFIG_HOME/shoal/shoal.toml`, falling back to
    ///    `~/.config/shoal/shoal.toml` when `XDG_CONFIG_HOME` is unset.
    /// 3. **project** — the nearest `.shoal.toml` walking up from `cwd` to
    ///    the filesystem root ([`find_project_config`]) — same "nearest
    ///    wins" rule `shoal-reef` uses for `.reef.toml` (site/content/internals/reef-resolution.md).
    /// 4. **env** — the live process environment, for `NO_COLOR`/`SHOAL_*`
    ///    overrides (site/content/internals/configuration-reference.md), highest precedence.
    pub fn discover(cwd: &Path) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        // $XDG_CONFIG_HOME is already a config root (no extra `.config`);
        // only the $HOME fallback needs it appended.
        let user = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".config")))
            .map(|p| p.join("shoal/shoal.toml"));
        Self {
            system: Some(PathBuf::from("/etc/shoal/shoal.toml")),
            user,
            project: find_project_config(cwd),
            env: std::env::vars_os().collect(),
        }
    }
}

/// Walk up from `start` (inclusive) toward the filesystem root looking for
/// the nearest `.shoal.toml`; `None` if none exists anywhere in the
/// ancestry. Pure function of `(start, filesystem)` — no caching, no env
/// reads. Mirrors `ScopeChain::discover`'s "nearest manifest wins" walk
/// (site/content/internals/reef-resolution.md): no special-casing of `$HOME` or a VCS boundary, so the
/// search is simple and total (a finite ancestor chain always terminates).
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".shoal.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Load + layer + validate a [`Config`] from `o`. Never panics: a missing
/// file is skipped, malformed TOML/an unknown-typed value/a bad env override
/// all come back as an `Err(ConfigError)` instead.
pub fn load(o: &LoadOptions) -> Result<Loaded, ConfigError> {
    let mut merged = toml::Value::try_from(Config::default()).map_err(|e| ConfigError::Value {
        source: None,
        key: "<default>".into(),
        message: e.to_string(),
    })?;
    let mut warnings = Vec::new();
    let mut sources = Vec::new();

    for path in [&o.system, &o.user, &o.project].into_iter().flatten() {
        if let Some(text) = read_config_file(path)? {
            check_toml_nesting(path, &text)?;
            let value: toml::Value = toml::from_str(&text).map_err(|e| ConfigError::Parse {
                path: path.clone(),
                message: e.to_string(),
            })?;
            let mut layer_warnings = Vec::new();
            crate::schema::check(&value, crate::schema::ROOT, "", &mut layer_warnings)
                .map_err(|e| e.with_source(path))?;
            for w in layer_warnings {
                warnings.push(format!("{}: {w}", path.display()));
            }
            merge(&mut merged, value);
            sources.push(path.clone());
        }
    }

    apply_env(&mut merged, &o.env)?;

    let config: Config = merged
        .try_into()
        .map_err(|e: toml::de::Error| ConfigError::Value {
            source: None,
            key: "<config>".into(),
            message: e.to_string(),
        })?;
    validate(&config)?;
    Ok(Loaded {
        config,
        warnings,
        sources,
    })
}

/// Deep-merge `src` into `dst`: a table merges key-by-key (so setting one
/// field in a later layer doesn't blank out its siblings from an earlier
/// one); anything else (scalar, array) in `src` replaces `dst` outright.
fn merge(dst: &mut toml::Value, src: toml::Value) {
    match (dst, src) {
        (toml::Value::Table(d), toml::Value::Table(s)) => {
            for (k, v) in s {
                if let Some(old) = d.get_mut(&k) {
                    merge(old, v)
                } else {
                    d.insert(k, v);
                }
            }
        }
        (d, s) => *d = s,
    }
}

/// How to coerce an env var's string value onto the TOML tree.
#[derive(Clone, Copy)]
enum EnvKind {
    Bool,
    UInt,
    Str,
}

/// Explicit `env var name -> (dotted key path, expected kind)` table
/// (site/content/internals/configuration-reference.md). Deliberately a flat static list rather than a
/// derived `SHOAL_SECTION_FIELD` split — an automatic split is ambiguous the
/// moment a field name itself contains an underscore (`max_entries`,
/// `bracketed_paste`, …), so every override is spelled out once, here, and
/// in the docs table.
const ENV_OVERRIDES: &[(&str, &[&str], EnvKind)] = &[
    (
        "SHOAL_PROMPT_TEMPLATE",
        &["prompt", "template"],
        EnvKind::Str,
    ),
    ("SHOAL_PROMPT", &["prompt", "template"], EnvKind::Str), // legacy alias
    (
        "SHOAL_HISTORY_ENABLED",
        &["history", "enabled"],
        EnvKind::Bool,
    ),
    ("SHOAL_HISTORY", &["history", "enabled"], EnvKind::Bool), // legacy alias
    (
        "SHOAL_HISTORY_MAX_ENTRIES",
        &["history", "max_entries"],
        EnvKind::UInt,
    ),
    ("SHOAL_HISTORY_FILE", &["history", "path"], EnvKind::Str),
    ("SHOAL_HISTORY_DEDUP", &["history", "dedup"], EnvKind::Bool),
    ("SHOAL_RENDER_COLOR", &["render", "color"], EnvKind::Bool),
    ("SHOAL_RENDER_WIDTH", &["render", "width"], EnvKind::UInt),
    ("SHOAL_RENDER_PAGING", &["render", "paging"], EnvKind::Str),
    ("SHOAL_RENDER_PAGER", &["render", "pager"], EnvKind::Str),
    ("SHOAL_RENDER_ECHO", &["render", "echo"], EnvKind::Str),
    ("SHOAL_EDITOR_MODE", &["editor", "mode"], EnvKind::Str),
    (
        "SHOAL_EDITOR_BRACKETED_PASTE",
        &["editor", "bracketed_paste"],
        EnvKind::Bool,
    ),
    (
        "SHOAL_KERNEL_ENABLED",
        &["kernel", "enabled"],
        EnvKind::Bool,
    ),
    ("SHOAL_KERNEL", &["kernel", "enabled"], EnvKind::Bool), // legacy alias
    ("SHOAL_KERNEL_SESSION", &["kernel", "session"], EnvKind::Str),
    (
        "SHOAL_JOURNAL_ENABLED",
        &["journal", "enabled"],
        EnvKind::Bool,
    ),
    ("SHOAL_LEASH_POLICY", &["leash", "policy"], EnvKind::Str),
    (
        "SHOAL_COMPLETION_FUZZY",
        &["completion", "fuzzy"],
        EnvKind::Bool,
    ),
    (
        "SHOAL_COMPLETION_CASE_INSENSITIVE",
        &["completion", "case_insensitive"],
        EnvKind::Bool,
    ),
    (
        "SHOAL_COMPLETION_MAX_RESULTS",
        &["completion", "max_results"],
        EnvKind::UInt,
    ),
    (
        "SHOAL_COMPLETION_MENU",
        &["completion", "menu"],
        EnvKind::Bool,
    ),
];

fn apply_env(v: &mut toml::Value, env: &[(OsString, OsString)]) -> Result<(), ConfigError> {
    for (k, val) in env {
        let Some(k) = k.to_str() else { continue };
        let Some((_, path, kind)) = ENV_OVERRIDES.iter().find(|(name, _, _)| *name == k) else {
            continue;
        };
        let Some(val) = val.to_str() else {
            return Err(ConfigError::Env {
                var: k.to_string(),
                message: "value is not UTF-8".into(),
            });
        };
        if val.len() > CONFIG_ENV_VALUE_MAX_BYTES {
            return Err(ConfigError::Env {
                var: k.to_string(),
                message: format!(
                    "value is {} UTF-8 bytes; maximum is {CONFIG_ENV_VALUE_MAX_BYTES}",
                    val.len()
                ),
            });
        }
        let parsed = coerce_env(k, val, *kind)?;
        set_path(v, path, parsed).map_err(|message| ConfigError::Env {
            var: k.to_string(),
            message,
        })?;
    }
    // NO_COLOR (https://no-color.org): presence — regardless of value, even
    // an empty string — disables color. Applied last so it always wins over
    // SHOAL_RENDER_COLOR too, matching the convention every other tool
    // honors: NO_COLOR is the one override nothing else is allowed to undo.
    if env.iter().any(|(k, _)| k == "NO_COLOR") {
        set_path(v, &["render", "color"], toml::Value::Boolean(false)).map_err(|message| {
            ConfigError::Env {
                var: "NO_COLOR".into(),
                message,
            }
        })?;
    }
    Ok(())
}

fn coerce_env(var: &str, val: &str, kind: EnvKind) -> Result<toml::Value, ConfigError> {
    match kind {
        EnvKind::Bool => {
            parse_bool(val)
                .map(toml::Value::Boolean)
                .ok_or_else(|| ConfigError::Env {
                    var: var.to_string(),
                    message: format!("expected true/false, got `{val}`"),
                })
        }
        EnvKind::UInt => {
            let n: i64 = val.parse().map_err(|_| ConfigError::Env {
                var: var.to_string(),
                message: format!("expected a non-negative integer, got `{val}`"),
            })?;
            if n < 0 {
                return Err(ConfigError::Env {
                    var: var.to_string(),
                    message: format!("expected a non-negative integer, got `{val}`"),
                });
            }
            Ok(toml::Value::Integer(n))
        }
        EnvKind::Str => Ok(toml::Value::String(val.to_string())),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "true" | "TRUE" | "True" | "yes" | "on" => Some(true),
        "0" | "false" | "FALSE" | "False" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Set `value` at the end of `path` inside `v`, creating intermediate tables
/// as needed. Errors (rather than panics) if an intermediate segment is
/// already present as something other than a table.
fn set_path(v: &mut toml::Value, path: &[&str], value: toml::Value) -> Result<(), String> {
    let mut cur = v;
    for (i, seg) in path.iter().enumerate() {
        let Some(table) = cur.as_table_mut() else {
            return Err(format!(
                "cannot set `{}`: `{}` is not a table",
                path.join("."),
                path[..i].join(".")
            ));
        };
        if i == path.len() - 1 {
            table.insert((*seg).to_string(), value);
            return Ok(());
        }
        cur = table
            .entry((*seg).to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
    }
    Ok(())
}

fn validate(c: &Config) -> Result<(), ConfigError> {
    if c.version != 1 {
        return Err(value_err(
            "version",
            format!("unsupported config version {} (expected 1)", c.version),
        ));
    }
    if c.history.max_entries == 0 {
        return Err(value_err("history.max_entries", "must be greater than 0"));
    }
    if !matches!(c.editor.mode.as_str(), "emacs" | "vi") {
        return Err(value_err("editor.mode", "must be `emacs` or `vi`"));
    }
    if !matches!(c.render.paging.as_str(), "never" | "auto") {
        return Err(value_err("render.paging", "must be `never` or `auto`"));
    }
    if c.render.width == Some(0) {
        return Err(value_err("render.width", "must be greater than 0"));
    }
    if let Some(echo) = &c.render.echo
        && !matches!(echo.as_str(), "quiet" | "commands" | "all")
    {
        return Err(value_err(
            "render.echo",
            "must be `quiet`, `commands`, or `all`",
        ));
    }
    if c.completion.max_results == 0 {
        return Err(value_err(
            "completion.max_results",
            "must be greater than 0",
        ));
    }
    for name in c.aliases.keys() {
        if name.is_empty() {
            return Err(value_err("aliases", "alias name must not be empty"));
        }
        if name.chars().any(char::is_whitespace) {
            return Err(value_err(
                "aliases",
                format!("alias name `{name}` must not contain whitespace"),
            ));
        }
    }
    for name in c.env.keys() {
        if name.is_empty() {
            return Err(value_err(
                "env",
                "environment variable name must not be empty",
            ));
        }
    }
    for pat in &c.history.ignore {
        if pat.is_empty() {
            return Err(value_err("history.ignore", "pattern must not be empty"));
        }
    }
    Ok(())
}

fn value_err(key: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Value {
        source: None,
        key: key.to_string(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;
