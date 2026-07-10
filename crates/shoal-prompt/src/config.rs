//! The `[prompt]` config schema (§3): parse, validate, defaults, theme
//! layering, and the format-string / unknown-key warnings of §11.
//!
//! Every struct here derives `serde(default)` so a partial or absent config
//! degrades to defaults with a warning rather than failing the shell to start
//! (§11: a broken prompt config must never be a broken shell).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::format::{FormatToken, parse_format, referenced_ids};

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModuleConfig {
    pub character: CharacterModule,
    pub directory: DirectoryModule,
    pub git_branch: GitBranchModule,
    pub git_status: GitStatusModule,
    pub git_state: GitStateModule,
    pub cmd_duration: CmdDurationModule,
    pub exit_status: ExitStatusModule,
    pub jobs: JobsModule,
    pub time: TimeModule,
    pub username: UsernameModule,
    pub hostname: HostnameModule,
    pub reef: ReefModule,
    pub principal: PrincipalModule,
    pub leash: LeashModule,
    pub battery: BatteryModule,
    pub language: BTreeMap<String, LanguageModule>,
    pub custom: BTreeMap<String, CustomModule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CharacterModule {
    pub enabled: bool,
    pub success_symbol: String,
    pub error_symbol: String,
    pub vicmd_symbol: String,
    pub success_style: String,
    pub error_style: String,
    pub vicmd_style: String,
}
impl Default for CharacterModule {
    fn default() -> Self {
        Self {
            enabled: true,
            success_symbol: "❯".into(),
            error_symbol: "❯".into(),
            vicmd_symbol: "❮".into(),
            success_style: "green bold".into(),
            error_style: "red bold".into(),
            vicmd_style: "yellow".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectoryModule {
    pub enabled: bool,
    pub truncate_to: usize,
    pub truncate_style: String,
    pub repo_relative: bool,
    pub read_only_symbol: String,
    pub symbol: String,
    pub style: String,
    pub home_symbol: String,
}
impl Default for DirectoryModule {
    fn default() -> Self {
        Self {
            enabled: true,
            truncate_to: 3,
            truncate_style: "middle".into(),
            repo_relative: true,
            read_only_symbol: "🔒".into(),
            symbol: String::new(),
            style: "cyan bold".into(),
            home_symbol: "~".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitBranchModule {
    pub enabled: bool,
    pub symbol: String,
    pub ascii_symbol: String,
    pub style: String,
    pub truncate_to: usize,
    pub truncate_symbol: String,
    pub format: String,
}
impl Default for GitBranchModule {
    fn default() -> Self {
        Self {
            enabled: true,
            symbol: " ".into(),
            ascii_symbol: "git:".into(),
            style: "purple".into(),
            truncate_to: 32,
            truncate_symbol: "…".into(),
            format: "${symbol}${branch}".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitStatusModule {
    pub enabled: bool,
    pub style: String,
    pub ahead: String,
    pub behind: String,
    pub diverged: String,
    pub staged: String,
    pub unstaged: String,
    pub untracked: String,
    pub conflicted: String,
    pub stashed: String,
    pub stale_symbol: String,
    pub engine: String,
}
impl Default for GitStatusModule {
    fn default() -> Self {
        Self {
            enabled: true,
            style: "red".into(),
            ahead: "⇡${count}".into(),
            behind: "⇣${count}".into(),
            diverged: "⇕⇡${ahead}⇣${behind}".into(),
            staged: "+${count}".into(),
            unstaged: "!${count}".into(),
            untracked: "?${count}".into(),
            conflicted: "=${count}".into(),
            stashed: "*${count}".into(),
            stale_symbol: "…".into(),
            engine: "gix".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitStateModule {
    pub enabled: bool,
    pub rebase: String,
    pub merge: String,
    pub cherry_pick: String,
    pub bisect: String,
    pub revert: String,
    pub style: String,
}
impl Default for GitStateModule {
    fn default() -> Self {
        Self {
            enabled: true,
            rebase: "REBASING".into(),
            merge: "MERGING".into(),
            cherry_pick: "CHERRY-PICKING".into(),
            bisect: "BISECTING".into(),
            revert: "REVERTING".into(),
            style: "yellow bold".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CmdDurationModule {
    pub enabled: bool,
    pub min_ms: u64,
    pub style: String,
}
impl Default for CmdDurationModule {
    fn default() -> Self {
        Self {
            enabled: true,
            min_ms: 500,
            style: "yellow".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExitStatusModule {
    pub enabled: bool,
    pub show_on_success: bool,
    pub format: String,
    pub style: String,
}
impl Default for ExitStatusModule {
    fn default() -> Self {
        Self {
            enabled: false,
            show_on_success: false,
            format: "[${status}]".into(),
            style: "red bold".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JobsModule {
    pub enabled: bool,
    pub symbol: String,
    pub style: String,
    pub threshold: usize,
    pub format: String,
}
impl Default for JobsModule {
    fn default() -> Self {
        Self {
            enabled: true,
            symbol: "✦".into(),
            style: "blue".into(),
            threshold: 1,
            format: "${symbol}${total}".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeModule {
    pub enabled: bool,
    pub format: String,
    pub style: String,
}
impl Default for TimeModule {
    fn default() -> Self {
        Self {
            enabled: true,
            format: "%H:%M:%S".into(),
            style: "dim".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsernameModule {
    pub enabled: bool,
    pub show_always: bool,
    pub style: String,
    pub root_style: String,
}
impl Default for UsernameModule {
    fn default() -> Self {
        Self {
            enabled: true,
            show_always: false,
            style: "yellow bold".into(),
            root_style: "red bold".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostnameModule {
    pub enabled: bool,
    pub show_always: bool,
    pub symbol: String,
    pub style: String,
}
impl Default for HostnameModule {
    fn default() -> Self {
        Self {
            enabled: true,
            show_always: false,
            symbol: "@".into(),
            style: "green".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReefModule {
    pub enabled: bool,
    pub style: String,
    pub format: String,
    pub show_when_empty: bool,
    pub show_ambient: bool,
}
impl Default for ReefModule {
    fn default() -> Self {
        Self {
            enabled: true,
            style: "green".into(),
            format: "reef:${tools}".into(),
            show_when_empty: false,
            show_ambient: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrincipalModule {
    pub enabled: bool,
    pub human_symbol: String,
    pub agent_symbol: String,
    pub show_agent_name: bool,
    pub style: String,
    pub agent_style: String,
}
impl Default for PrincipalModule {
    fn default() -> Self {
        Self {
            enabled: false,
            human_symbol: String::new(),
            agent_symbol: "🤖".into(),
            show_agent_name: true,
            style: "muted".into(),
            agent_style: "cyan".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeashModule {
    pub enabled: bool,
    pub style_by_tier: BTreeMap<String, String>,
    pub symbol_by_tier: BTreeMap<String, String>,
    pub hide_when_enforced: bool,
}
impl Default for LeashModule {
    fn default() -> Self {
        Self {
            enabled: false,
            style_by_tier: [
                ("A", "green"),
                ("B", "yellow"),
                ("C", "yellow"),
                ("D", "red bold"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            symbol_by_tier: [("A", "🔒"), ("B", "🔓"), ("C", "🔓"), ("D", "⚠")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            hide_when_enforced: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatteryModule {
    pub enabled: bool,
    pub charging_symbol: String,
    pub discharging_symbol: String,
    pub low_threshold: u8,
    pub low_style: String,
    pub style: String,
    pub sample_interval_s: u64,
}
impl Default for BatteryModule {
    fn default() -> Self {
        Self {
            enabled: false,
            charging_symbol: "⚡".into(),
            discharging_symbol: String::new(),
            low_threshold: 20,
            low_style: "red bold".into(),
            style: "green".into(),
            sample_interval_s: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageModule {
    pub enabled: bool,
    pub tool: String,
    pub symbol: String,
    pub ascii_symbol: String,
    pub style: String,
    pub when: String,
    pub probe_ttl_s: u64,
    pub format: String,
}
impl Default for LanguageModule {
    fn default() -> Self {
        Self {
            enabled: true,
            tool: String::new(),
            symbol: String::new(),
            ascii_symbol: String::new(),
            style: "green".into(),
            when: "constrained".into(),
            probe_ttl_s: 30,
            format: "${symbol}${version}".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomModule {
    pub enabled: bool,
    pub command: String,
    pub when: String,
    pub cache_ttl: String,
    pub style: String,
    pub format: String,
}
impl Default for CustomModule {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
            when: String::new(),
            cache_ttl: "5s".into(),
            style: "blue".into(),
            format: "${output}".into(),
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

    /// Parse every `format.*` string, warning (§11) about placeholders that
    /// match no known module id. Returns the parsed token streams so the caller
    /// can cache them for the process lifetime (§3.4).
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

/// Cached, parsed `format.*` token streams (parsed once, §3.4).
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

/// Build the final [`PromptConfig`] from an ordered (lowest → highest
/// precedence) list of `[prompt]`-contents-shaped TOML values. Applies the
/// named theme preset as an extra lowest-precedence layer (§3.1 step 1) and
/// collects unknown-key + format warnings (§11).
pub fn load(layers: Vec<toml::Value>, warnings: &mut Vec<String>) -> PromptConfig {
    let mut merged = toml::Value::Table(toml::map::Map::new());
    for layer in &layers {
        merge(&mut merged, layer.clone());
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
            match toml::from_str::<toml::Value>(theme_src) {
                Ok(theme_val) => {
                    let mut base = theme_val;
                    for layer in &layers {
                        merge(&mut base, layer.clone());
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

/// Warn about unknown keys anywhere in the prompt table (§3.2, §11). Dynamic
/// `module.language.<tool>` / `module.custom.<name>` inner keys are validated
/// against their own schema; the user-chosen table name itself is never flagged.
fn validate_keys(v: &toml::Value, warnings: &mut Vec<String>) {
    walk(v, "prompt", warnings);
}

fn allowed_for(prefix: &str) -> Option<&'static [&'static str]> {
    Some(match prefix {
        "prompt" => &[
            "theme",
            "nerd_font",
            "unicode",
            "right_prompt_on_last_line",
            "format",
            "transient",
            "budget",
            "style",
            "module",
        ],
        "prompt.format" => &["left", "right", "continuation", "transient"],
        "prompt.transient" => &["enabled"],
        "prompt.budget" => &["render_deadline_ms", "warn_on_exceed"],
        "prompt.style" => &["ok", "error", "warn", "info", "muted", "accent"],
        "prompt.module" => &[
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
            "language",
            "custom",
        ],
        "prompt.module.character" => &[
            "enabled",
            "success_symbol",
            "error_symbol",
            "vicmd_symbol",
            "success_style",
            "error_style",
            "vicmd_style",
        ],
        "prompt.module.directory" => &[
            "enabled",
            "truncate_to",
            "truncate_style",
            "repo_relative",
            "read_only_symbol",
            "symbol",
            "style",
            "home_symbol",
        ],
        "prompt.module.git_branch" => &[
            "enabled",
            "symbol",
            "ascii_symbol",
            "style",
            "truncate_to",
            "truncate_symbol",
            "format",
        ],
        "prompt.module.git_status" => &[
            "enabled",
            "style",
            "ahead",
            "behind",
            "diverged",
            "staged",
            "unstaged",
            "untracked",
            "conflicted",
            "stashed",
            "stale_symbol",
            "engine",
        ],
        "prompt.module.git_state" => &[
            "enabled",
            "rebase",
            "merge",
            "cherry_pick",
            "bisect",
            "revert",
            "style",
        ],
        "prompt.module.cmd_duration" => &["enabled", "min_ms", "style"],
        "prompt.module.exit_status" => &["enabled", "show_on_success", "format", "style"],
        "prompt.module.jobs" => &["enabled", "symbol", "style", "threshold", "format"],
        "prompt.module.time" => &["enabled", "format", "style"],
        "prompt.module.username" => &["enabled", "show_always", "style", "root_style"],
        "prompt.module.hostname" => &["enabled", "show_always", "symbol", "style"],
        "prompt.module.reef" => &[
            "enabled",
            "style",
            "format",
            "show_when_empty",
            "show_ambient",
        ],
        "prompt.module.principal" => &[
            "enabled",
            "human_symbol",
            "agent_symbol",
            "show_agent_name",
            "style",
            "agent_style",
        ],
        "prompt.module.leash" => &[
            "enabled",
            "style_by_tier",
            "symbol_by_tier",
            "hide_when_enforced",
        ],
        "prompt.module.battery" => &[
            "enabled",
            "charging_symbol",
            "discharging_symbol",
            "low_threshold",
            "low_style",
            "style",
            "sample_interval_s",
        ],
        _ => return None,
    })
}

fn walk(v: &toml::Value, prefix: &str, warnings: &mut Vec<String>) {
    // The two dynamic tables: their direct children are user-named, so we
    // validate one level deeper against the language/custom module schema.
    if prefix == "prompt.module.language" || prefix == "prompt.module.custom" {
        let inner_allowed: &[&str] = if prefix.ends_with("language") {
            &[
                "enabled",
                "tool",
                "symbol",
                "ascii_symbol",
                "style",
                "when",
                "probe_ttl_s",
                "format",
            ]
        } else {
            &["enabled", "command", "when", "cache_ttl", "style", "format"]
        };
        if let Some(t) = v.as_table() {
            for (_name, entry) in t {
                if let Some(inner) = entry.as_table() {
                    for k in inner.keys() {
                        if !inner_allowed.contains(&k.as_str()) {
                            warnings.push(format!("unknown config key {prefix}.<name>.{k}"));
                        }
                    }
                }
            }
        }
        return;
    }

    let Some(allowed) = allowed_for(prefix) else {
        return;
    };
    if let Some(t) = v.as_table() {
        for (k, x) in t {
            if !allowed.contains(&k.as_str()) {
                warnings.push(format!("unknown config key {prefix}.{k}"));
            } else if x.is_table() {
                walk(x, &format!("{prefix}.{k}"), warnings);
            }
        }
    }
}

/// Build a `[prompt]`-contents-shaped TOML value carrying the `SHOAL_PROMPT_*`
/// environment overrides (§3.8). Returned as the highest-precedence layer.
pub fn env_overrides(vars: &[(String, String)]) -> toml::Value {
    let mut table = toml::map::Map::new();
    let mut format = toml::map::Map::new();
    let mut deprecated_bare: Option<String> = None;
    for (k, val) in vars {
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
}
