//! The `[prompt]` config schema (site/content/internals/prompt-editor-lsp.md): parse, validate, defaults, theme
//! layering, and the format-string / unknown-key warnings of site/content/internals/prompt-editor-lsp.md.
//!
//! Every struct here derives `serde(default)` so a partial or absent config
//! degrades to defaults with a warning rather than failing the shell to start
//! (site/content/internals/prompt-editor-lsp.md: a broken prompt config must never be a broken shell).

use serde::{Deserialize, Serialize};

use crate::format::{FormatToken, parse_format, referenced_ids};

mod module_config;
mod schema;

pub use module_config::{
    BatteryModule, CharacterModule, CmdDurationModule, CustomModule, DirectoryModule,
    ExitStatusModule, GitBranchModule, GitStateModule, GitStatusModule, HostnameModule, JobsModule,
    LanguageModule, LeashModule, ModuleConfig, PrincipalModule, ReefModule, TimeModule,
    UsernameModule,
};
use schema::validate_keys;

/// Admission limits for advisory prompt configuration. Invalid layers are
/// ignored with warnings so a hostile prompt file cannot prevent shell use.
pub const PROMPT_MAX_SOURCE_BYTES: usize = 1024 * 1024;
pub const PROMPT_MAX_LAYERS: usize = 16;
pub const PROMPT_MAX_NESTING: usize = 64;
pub const PROMPT_MAX_NODES: usize = 16 * 1024;
pub const PROMPT_MAX_STRING_BYTES: usize = 64 * 1024;
pub const PROMPT_MAX_DYNAMIC_MODULES: usize = 128;
pub const PROMPT_MAX_ENV_VALUE_BYTES: usize = 64 * 1024;

/// The full parsed prompt configuration. Shaped identically to the `[prompt]`
/// table's *contents* (the dedicated `prompt.toml` file / theme fragment form).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    pub theme: String,
    pub nerd_font: String,
    pub unicode: bool,
    pub right_prompt_on_last_line: bool,
    pub format: FormatConfig,
    pub transient: TransientConfig,
    pub budget: BudgetConfig,
    pub style: StylePalette,
    pub module: ModuleConfig,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            theme: String::new(),
            nerd_font: "auto".into(),
            unicode: true,
            right_prompt_on_last_line: false,
            format: FormatConfig::default(),
            transient: TransientConfig::default(),
            budget: BudgetConfig::default(),
            style: StylePalette::default(),
            module: ModuleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatConfig {
    pub left: String,
    pub right: String,
    pub continuation: String,
    pub transient: String,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            left: "$directory$git_branch$git_status$git_state$reef$character".into(),
            right: "$cmd_duration $jobs $time".into(),
            continuation: "... ".into(),
            transient: "$character ".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransientConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub render_deadline_ms: u64,
    pub warn_on_exceed: bool,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            render_deadline_ms: 5,
            warn_on_exceed: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StylePalette {
    pub ok: String,
    pub error: String,
    pub warn: String,
    pub info: String,
    pub muted: String,
    pub accent: String,
}

impl Default for StylePalette {
    fn default() -> Self {
        Self {
            ok: "green bold".into(),
            error: "red bold".into(),
            warn: "yellow bold".into(),
            info: "blue".into(),
            muted: "8".into(),
            accent: "purple".into(),
        }
    }
}

impl StylePalette {
    /// Resolve a style spec against the palette: when the whole spec names a
    /// palette token (`"muted"`, `"accent"`, …) expand it to that token's value;
    /// otherwise return the spec verbatim for [`crate::style::parse_style`].
    pub fn resolve<'a>(&'a self, spec: &'a str) -> &'a str {
        match spec.trim() {
            "ok" => &self.ok,
            "error" => &self.error,
            "warn" => &self.warn,
            "info" => &self.info,
            "muted" => &self.muted,
            "accent" => &self.accent,
            other => other,
        }
    }
}

impl PromptConfig {
    /// The known module ids referenced-able from a `format.*` string, given the
    /// configured dynamic language/custom tables.
    pub fn known_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = STATIC_MODULE_IDS.iter().map(|s| s.to_string()).collect();
        for name in self.module.language.keys() {
            ids.push(format!("language_{name}"));
        }
        for name in self.module.custom.keys() {
            ids.push(format!("custom_{name}"));
        }
        ids
    }

    /// Parse every `format.*` string, warning (site/content/internals/prompt-editor-lsp.md) about placeholders that
    /// match no known module id. Returns the parsed token streams so the caller
    /// can cache them for the process lifetime (site/content/internals/prompt-editor-lsp.md).
    pub fn parse_formats(&self, warnings: &mut Vec<String>) -> ParsedFormats {
        let known = self.known_ids();
        let parse_side = |side: &str, src: &str, warnings: &mut Vec<String>| {
            let toks = parse_format(src);
            for id in referenced_ids(&toks) {
                if id != "indent" && !known.contains(&id) {
                    warnings.push(format!(
                        "prompt: unknown module id '{id}' in format string '{side}'"
                    ));
                }
            }
            toks
        };
        ParsedFormats {
            left: parse_side("left", &self.format.left, warnings),
            right: parse_side("right", &self.format.right, warnings),
            continuation: parse_side("continuation", &self.format.continuation, warnings),
            transient: parse_side("transient", &self.format.transient, warnings),
        }
    }
}

/// Cached, parsed `format.*` token streams (parsed once, site/content/internals/prompt-editor-lsp.md).
#[derive(Debug, Clone)]
pub struct ParsedFormats {
    pub left: Vec<FormatToken>,
    pub right: Vec<FormatToken>,
    pub continuation: Vec<FormatToken>,
    pub transient: Vec<FormatToken>,
}

/// The fixed module ids with a dedicated `[prompt.module.<id>]` table.
pub const STATIC_MODULE_IDS: &[&str] = &[
    "character",
    "directory",
    "git_branch",
    "git_status",
    "git_state",
    "cmd_duration",
    "exit_status",
    "jobs",
    "time",
    "username",
    "hostname",
    "reef",
    "principal",
    "leash",
    "battery",
];

// ---------------------------------------------------------------------------
// Loading, merging, theme layering
// ---------------------------------------------------------------------------

/// Deep-merge `src` into `dst` (src wins), mirroring `shoal_config::merge`.
pub fn merge(dst: &mut toml::Value, src: toml::Value) {
    match (dst, src) {
        (toml::Value::Table(d), toml::Value::Table(s)) => {
            for (k, v) in s {
                if let Some(old) = d.get_mut(&k) {
                    merge(old, v);
                } else {
                    d.insert(k, v);
                }
            }
        }
        (d, s) => *d = s,
    }
}

/// Parse one prompt layer after applying the same bounded-input checks used by
/// the binary loader. This remains I/O-free; callers retain responsibility for
/// bounded file reads.
pub fn parse_layer(source: &str) -> Result<toml::Value, String> {
    if source.len() > PROMPT_MAX_SOURCE_BYTES {
        return Err(format!(
            "prompt config exceeds the {PROMPT_MAX_SOURCE_BYTES}-byte limit"
        ));
    }
    validate_toml_source(source)?;
    let value = toml::from_str(source).map_err(|error| error.to_string())?;
    validate_layer(&value)?;
    Ok(value)
}

fn validate_toml_source(source: &str) -> Result<(), String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Basic,
        Literal,
        MultiBasic,
        MultiLiteral,
    }

    let bytes = source.as_bytes();
    let mut index = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut comment = false;
    let mut depth = 0usize;
    let mut dots_on_line = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if comment {
            if byte == b'\n' {
                comment = false;
                dots_on_line = 0;
            }
            index += 1;
            continue;
        }
        if let Some(kind) = quote {
            match kind {
                Quote::Basic => {
                    if escaped {
                        escaped = false;
                    } else if byte == b'\\' {
                        escaped = true;
                    } else if byte == b'"' {
                        quote = None;
                    }
                    index += 1;
                }
                Quote::Literal => {
                    if byte == b'\'' {
                        quote = None;
                    }
                    index += 1;
                }
                Quote::MultiBasic | Quote::MultiLiteral => {
                    let delimiter = if kind == Quote::MultiBasic {
                        b'"'
                    } else {
                        b'\''
                    };
                    if byte == delimiter
                        && bytes.get(index + 1) == Some(&delimiter)
                        && bytes.get(index + 2) == Some(&delimiter)
                    {
                        quote = None;
                        index += 3;
                    } else {
                        index += 1;
                    }
                }
            }
            continue;
        }
        match byte {
            b'#' => comment = true,
            b'"' | b'\'' => {
                let multi =
                    bytes.get(index + 1) == Some(&byte) && bytes.get(index + 2) == Some(&byte);
                quote = Some(match (byte, multi) {
                    (b'"', true) => Quote::MultiBasic,
                    (b'\'', true) => Quote::MultiLiteral,
                    (b'"', false) => Quote::Basic,
                    _ => Quote::Literal,
                });
                if multi {
                    index += 2;
                }
            }
            b'[' | b'{' => {
                depth += 1;
                if depth > PROMPT_MAX_NESTING {
                    return Err(format!(
                        "prompt config exceeds the {PROMPT_MAX_NESTING}-level TOML nesting limit"
                    ));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            b'.' => {
                dots_on_line += 1;
                if dots_on_line >= PROMPT_MAX_NESTING {
                    return Err(format!(
                        "prompt config exceeds the {PROMPT_MAX_NESTING}-segment dotted-key limit"
                    ));
                }
            }
            b'\n' => dots_on_line = 0,
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

fn validate_layer(value: &toml::Value) -> Result<(), String> {
    for (kind, table) in ["language", "custom"].into_iter().filter_map(|kind| {
        value
            .get("module")
            .and_then(|module| module.get(kind))
            .and_then(toml::Value::as_table)
            .map(|table| (kind, table))
    }) {
        if table.len() > PROMPT_MAX_DYNAMIC_MODULES {
            return Err(format!(
                "prompt module.{kind} has {} entries; maximum is {PROMPT_MAX_DYNAMIC_MODULES}",
                table.len()
            ));
        }
    }

    let mut stack = vec![(value, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > PROMPT_MAX_NODES {
            return Err(format!(
                "prompt config exceeds the {PROMPT_MAX_NODES}-node limit"
            ));
        }
        if depth > PROMPT_MAX_NESTING {
            return Err(format!(
                "prompt config exceeds the {PROMPT_MAX_NESTING}-level value-depth limit"
            ));
        }
        match value {
            toml::Value::String(string) => validate_prompt_string(string)?,
            toml::Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            toml::Value::Table(values) => {
                for (key, value) in values {
                    validate_prompt_string(key)?;
                    stack.push((value, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_prompt_string(string: &str) -> Result<(), String> {
    if string.len() > PROMPT_MAX_STRING_BYTES {
        return Err(format!(
            "prompt config string is {} UTF-8 bytes; maximum is {PROMPT_MAX_STRING_BYTES}",
            string.len()
        ));
    }
    Ok(())
}

/// Build the final [`PromptConfig`] from an ordered (lowest → highest
/// precedence) list of `[prompt]`-contents-shaped TOML values. Applies the
/// named theme preset as an extra lowest-precedence layer and
/// collects unknown-key + format warnings (site/content/internals/prompt-editor-lsp.md).
pub fn load(layers: Vec<toml::Value>, warnings: &mut Vec<String>) -> PromptConfig {
    let omitted = layers.len().saturating_sub(PROMPT_MAX_LAYERS);
    if omitted > 0 {
        warnings.push(format!(
            "prompt: {omitted} lowest-precedence layers omitted above the {PROMPT_MAX_LAYERS}-layer limit"
        ));
    }
    let mut merged = toml::Value::Table(toml::map::Map::new());
    let mut accepted_layers = Vec::new();
    for (index, layer) in layers.into_iter().skip(omitted).enumerate() {
        if let Err(error) = validate_layer(&layer) {
            warnings.push(format!("prompt: layer {} ignored: {error}", index + 1));
            continue;
        }
        let mut candidate = merged.clone();
        merge(&mut candidate, layer.clone());
        if let Err(error) = validate_layer(&candidate) {
            warnings.push(format!(
                "prompt: layer {} ignored after merge: {error}",
                index + 1
            ));
            continue;
        }
        merged = candidate;
        accepted_layers.push(layer);
    }

    // Resolve the theme name from the merged layers, then re-merge with the
    // theme fragment underneath everything (theme = lowest precedence).
    let theme_name = merged
        .get("theme")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    if !theme_name.is_empty() {
        if let Some(theme_src) = crate::themes::get(&theme_name) {
            match parse_layer(theme_src) {
                Ok(theme_val) => {
                    let mut base = theme_val;
                    for (index, layer) in accepted_layers.iter().enumerate() {
                        let mut candidate = base.clone();
                        merge(&mut candidate, layer.clone());
                        if let Err(error) = validate_layer(&candidate) {
                            warnings.push(format!(
                                "prompt: layer {} ignored with theme '{theme_name}': {error}",
                                index + 1
                            ));
                        } else {
                            base = candidate;
                        }
                    }
                    merged = base;
                }
                Err(e) => {
                    warnings.push(format!("prompt: theme '{theme_name}' failed to parse: {e}"))
                }
            }
        } else {
            warnings.push(format!("prompt: unknown theme '{theme_name}'"));
        }
    }

    validate_keys(&merged, warnings);

    let config: PromptConfig = merged.try_into().unwrap_or_else(|e| {
        warnings.push(format!(
            "prompt: config did not deserialize ({e}); using defaults"
        ));
        PromptConfig::default()
    });
    config
}

/// Build a `[prompt]`-contents-shaped TOML value carrying the `SHOAL_PROMPT_*`
/// environment overrides (site/content/internals/prompt-editor-lsp.md). Returned as the highest-precedence layer.
pub fn env_overrides(vars: &[(String, String)]) -> toml::Value {
    env_overrides_checked(vars, &mut Vec::new())
}

/// Like [`env_overrides`], but reports and ignores hostile oversized values.
pub fn env_overrides_checked(vars: &[(String, String)], warnings: &mut Vec<String>) -> toml::Value {
    let mut table = toml::map::Map::new();
    let mut format = toml::map::Map::new();
    let mut deprecated_bare: Option<String> = None;
    for (k, val) in vars {
        if val.len() > PROMPT_MAX_ENV_VALUE_BYTES {
            warnings.push(format!(
                "prompt: environment override {k} ignored because it exceeds the {PROMPT_MAX_ENV_VALUE_BYTES}-byte limit"
            ));
            continue;
        }
        match k.as_str() {
            "SHOAL_PROMPT_LEFT" => {
                format.insert("left".into(), toml::Value::String(val.clone()));
            }
            "SHOAL_PROMPT_RIGHT" => {
                format.insert("right".into(), toml::Value::String(val.clone()));
            }
            "SHOAL_PROMPT_THEME" => {
                table.insert("theme".into(), toml::Value::String(val.clone()));
            }
            "SHOAL_PROMPT" => deprecated_bare = Some(val.clone()),
            "SHOAL_NERD_FONT" => {
                let mode = match val.as_str() {
                    "1" => "always",
                    "0" => "never",
                    other => other,
                };
                table.insert("nerd_font".into(), toml::Value::String(mode.to_string()));
            }
            _ => {}
        }
    }
    // Bare SHOAL_PROMPT is a deprecated alias for _LEFT; _LEFT wins if both set.
    if let Some(bare) = deprecated_bare {
        format.entry("left").or_insert(toml::Value::String(bare));
    }
    if !format.is_empty() {
        table.insert("format".into(), toml::Value::Table(format));
    }
    toml::Value::Table(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let cfg = PromptConfig::default();
        assert_eq!(cfg.nerd_font, "auto");
        assert_eq!(cfg.module.character.success_symbol, "❯");
        assert_eq!(cfg.module.git_status.ahead, "⇡${count}");
    }

    #[test]
    fn unknown_key_warns() {
        let v: toml::Value = toml::from_str("wat = 1\n[module.directory]\nbogus = 2").unwrap();
        let mut w = Vec::new();
        load(vec![v], &mut w);
        assert!(w.iter().any(|x| x.contains("prompt.wat")));
        assert!(
            w.iter()
                .any(|x| x.contains("prompt.module.directory.bogus"))
        );
    }

    #[test]
    fn removed_speculative_prompt_keys_warn() {
        let v: toml::Value = toml::from_str(
            "[module.git_status]\nengine = 'gix'\n[module.language.rust]\nprobe_ttl_s = 30",
        )
        .unwrap();
        let mut warnings = Vec::new();
        load(vec![v], &mut warnings);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.ends_with("git_status.engine"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.ends_with("<name>.probe_ttl_s"))
        );
    }

    #[test]
    fn theme_is_lowest_precedence() {
        // minimal theme sets unicode=false; a user override of unicode=true wins.
        let user: toml::Value = toml::from_str("theme = 'minimal'\nunicode = true").unwrap();
        let mut w = Vec::new();
        let cfg = load(vec![user], &mut w);
        assert!(cfg.unicode, "user override must beat the theme");
        // but the theme's other keys still apply
        assert_eq!(cfg.format.left, "$directory$git_branch$character");
    }

    #[test]
    fn unknown_placeholder_warns() {
        let mut cfg = PromptConfig::default();
        cfg.format.left = "$directory$nope".into();
        let mut w = Vec::new();
        cfg.parse_formats(&mut w);
        assert!(w.iter().any(|x| x.contains("unknown module id 'nope'")));
    }

    #[test]
    fn language_and_custom_ids_known() {
        let v: toml::Value = toml::from_str(
            "[module.language.python]\ntool='python'\n[module.custom.k8s]\ncommand='x'",
        )
        .unwrap();
        let mut w = Vec::new();
        let mut cfg = load(vec![v], &mut w);
        cfg.format.left = "$language_python$custom_k8s".into();
        let mut w2 = Vec::new();
        cfg.parse_formats(&mut w2);
        assert!(
            w2.is_empty(),
            "configured language/custom ids are known: {w2:?}"
        );
    }

    #[test]
    fn env_overrides_left_and_theme() {
        let v = env_overrides(&[
            ("SHOAL_PROMPT_LEFT".into(), "$character".into()),
            ("SHOAL_PROMPT_THEME".into(), "rich".into()),
        ]);
        let mut w = Vec::new();
        let cfg = load(vec![v], &mut w);
        assert_eq!(cfg.format.left, "$character");
    }

    #[test]
    fn hostile_layers_are_ignored_with_visible_warnings() {
        let mut warnings = Vec::new();
        let wide = toml::Value::Array(
            (0..=PROMPT_MAX_NODES)
                .map(|_| toml::Value::Integer(1))
                .collect(),
        );
        let cfg = load(vec![wide], &mut warnings);
        assert_eq!(cfg.format.left, PromptConfig::default().format.left);
        assert!(warnings.iter().any(|warning| warning.contains("ignored")));

        let deep = format!(
            "x = {}0{}",
            "[".repeat(PROMPT_MAX_NESTING + 1),
            "]".repeat(PROMPT_MAX_NESTING + 1)
        );
        assert!(parse_layer(&deep).unwrap_err().contains("nesting"));
    }

    #[test]
    fn multiline_strings_do_not_count_as_toml_structure() {
        let layer = parse_layer("format.left = '''[[[...]]]'''\n").unwrap();
        assert_eq!(
            layer.get("format").unwrap().get("left").unwrap().as_str(),
            Some("[[[...]]]")
        );
    }

    #[test]
    fn oversized_environment_override_is_ignored() {
        let mut warnings = Vec::new();
        let layer = env_overrides_checked(
            &[(
                "SHOAL_PROMPT_LEFT".into(),
                "x".repeat(PROMPT_MAX_ENV_VALUE_BYTES + 1),
            )],
            &mut warnings,
        );
        assert!(layer.as_table().unwrap().is_empty());
        assert!(warnings.iter().any(|warning| warning.contains("ignored")));
    }
}
