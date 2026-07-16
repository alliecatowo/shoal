//! Discovery, layering, and validation — turns a chain of on-disk files (plus
//! the process environment) into a validated [`Config`].

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{Config, ConfigError};

/// The result of a successful [`load`]: the merged, validated config, any
/// non-fatal warnings collected along the way (unknown keys — see
/// docs/CONFIG.md §4), and the layer files that were actually found and
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
/// error (docs/CONFIG.md §1).
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub system: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
    /// The complete set of `(name, value)` pairs consulted for env overrides
    /// (docs/CONFIG.md §3). Callers normally pass `std::env::vars_os()`
    /// verbatim; tests pass a small synthetic set.
    pub env: Vec<(OsString, OsString)>,
}

impl LoadOptions {
    /// Discover the layered config locations for `cwd`, per docs/CONFIG.md
    /// §1:
    ///
    /// 1. **system** — `/etc/shoal/shoal.toml`.
    /// 2. **user** — `$XDG_CONFIG_HOME/shoal/shoal.toml`, falling back to
    ///    `~/.config/shoal/shoal.toml` when `XDG_CONFIG_HOME` is unset.
    /// 3. **project** — the nearest `.shoal.toml` walking up from `cwd` to
    ///    the filesystem root ([`find_project_config`]) — same "nearest
    ///    wins" rule `shoal-reef` uses for `.reef.toml` (REEF.md §1).
    /// 4. **env** — the live process environment, for `NO_COLOR`/`SHOAL_*`
    ///    overrides (docs/CONFIG.md §3), highest precedence.
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
/// (REEF.md §1): no special-casing of `$HOME` or a VCS boundary, so the
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
        match fs::read_to_string(path) {
            Ok(text) => {
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ConfigError::Io {
                    path: path.clone(),
                    message: e.to_string(),
                });
            }
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
/// (docs/CONFIG.md §3). Deliberately a flat static list rather than a
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
mod tests {
    use super::*;

    fn opts(
        system: Option<PathBuf>,
        user: Option<PathBuf>,
        project: Option<PathBuf>,
        env: Vec<(&str, &str)>,
    ) -> LoadOptions {
        LoadOptions {
            system,
            user,
            project,
            env: env
                .into_iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
        }
    }

    #[test]
    fn precedence_layers_override_in_order_and_warn_on_unknown_key() {
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("s");
        let u = t.path().join("u");
        let p = t.path().join("p");
        fs::write(&s, "[prompt]\ntemplate='system'").unwrap();
        fs::write(&u, "[prompt]\ntemplate='user'").unwrap();
        fs::write(&p, "[prompt]\ntemplate='project'\nwat=1").unwrap();
        let l = load(&opts(
            Some(s),
            Some(u),
            Some(p),
            vec![("SHOAL_PROMPT", "env")],
        ))
        .unwrap();
        assert_eq!(l.config.prompt.template, "env");
        assert_eq!(l.warnings.len(), 1);
        assert!(l.warnings[0].contains("unknown config key `prompt.wat`"));
        assert_eq!(l.sources.len(), 3);
    }

    #[test]
    fn a_later_layer_only_overrides_the_keys_it_sets() {
        // system sets history.enabled=false AND max_entries=5; user only
        // touches max_entries — history.enabled must survive from system.
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("s");
        let u = t.path().join("u");
        fs::write(&s, "[history]\nenabled = false\nmax_entries = 5\n").unwrap();
        fs::write(&u, "[history]\nmax_entries = 50\n").unwrap();
        let l = load(&opts(Some(s), Some(u), None, vec![])).unwrap();
        assert!(!l.config.history.enabled);
        assert_eq!(l.config.history.max_entries, 50);
    }

    #[test]
    fn missing_layers_are_not_an_error() {
        let t = tempfile::tempdir().unwrap();
        let missing = t.path().join("does-not-exist");
        let l = load(&opts(
            Some(missing.clone()),
            Some(missing.clone()),
            None,
            vec![],
        ))
        .unwrap();
        assert_eq!(l.config, Config::default());
        assert!(l.sources.is_empty());
    }

    #[test]
    fn unsupported_version_is_a_precise_error() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "version=9").unwrap();
        let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Value {
                source: None,
                key: "version".into(),
                message: "unsupported config version 9 (expected 1)".into(),
            }
        );
    }

    #[test]
    fn malformed_toml_never_panics_and_is_a_structured_parse_error() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[history\nenabled = true").unwrap();
        let err = load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err();
        match err {
            ConfigError::Parse { path, message } => {
                assert_eq!(path, p);
                assert!(!message.is_empty());
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn type_mismatch_names_key_path_and_expected_type() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[history]\nmax_entries = \"lots\"").unwrap();
        let err = load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Type {
                source: Some(p),
                key: "history.max_entries".into(),
                expected: "a non-negative integer",
                found: "string",
            }
        );
    }

    #[test]
    fn env_override_bad_bool_is_a_precise_error() {
        let err = load(&opts(
            None,
            None,
            None,
            vec![("SHOAL_HISTORY_ENABLED", "maybe")],
        ))
        .unwrap_err();
        assert_eq!(
            err,
            ConfigError::Env {
                var: "SHOAL_HISTORY_ENABLED".into(),
                message: "expected true/false, got `maybe`".into(),
            }
        );
    }

    #[test]
    fn env_override_scalar_leaves() {
        let l = load(&opts(
            None,
            None,
            None,
            vec![
                ("SHOAL_HISTORY_MAX_ENTRIES", "42"),
                ("SHOAL_EDITOR_MODE", "vi"),
                ("SHOAL_KERNEL_SESSION", "work"),
            ],
        ))
        .unwrap();
        assert_eq!(l.config.history.max_entries, 42);
        assert_eq!(l.config.editor.mode, "vi");
        assert_eq!(l.config.kernel.session, "work");
    }

    /// `render.paging` defaults to `"never"` (identical behavior to before
    /// this knob existed — an unconfigured shoal never pages) and is
    /// settable via either the config file or `SHOAL_RENDER_PAGING`/
    /// `SHOAL_RENDER_PAGER`.
    #[test]
    fn render_paging_defaults_to_never_and_is_env_overridable() {
        assert_eq!(Config::default().render.paging, "never");
        assert_eq!(Config::default().render.pager, None);

        let l = load(&opts(
            None,
            None,
            None,
            vec![
                ("SHOAL_RENDER_PAGING", "auto"),
                ("SHOAL_RENDER_PAGER", "bat --paging=always"),
            ],
        ))
        .unwrap();
        assert_eq!(l.config.render.paging, "auto");
        assert_eq!(
            l.config.render.pager.as_deref(),
            Some("bat --paging=always")
        );
    }

    #[test]
    fn render_paging_rejects_anything_other_than_never_or_auto() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[render]\npaging = \"always\"\n").unwrap();
        let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Value {
                source: None,
                key: "render.paging".into(),
                message: "must be `never` or `auto`".into(),
            }
        );
    }

    #[test]
    fn no_color_env_forces_render_color_off_even_if_config_says_true() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[render]\ncolor = true\n").unwrap();
        let l = load(&opts(None, Some(p), None, vec![("NO_COLOR", "")])).unwrap();
        assert!(!l.config.render.color);
    }

    #[test]
    fn no_color_wins_over_an_explicit_shoal_render_color_override() {
        let l = load(&opts(
            None,
            None,
            None,
            vec![("SHOAL_RENDER_COLOR", "true"), ("NO_COLOR", "1")],
        ))
        .unwrap();
        assert!(!l.config.render.color);
    }

    #[test]
    fn project_walk_up_finds_nearest_dot_shoal_toml() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(".shoal.toml"), "[prompt]\ntemplate='root'").unwrap();
        assert_eq!(find_project_config(&nested), Some(root.join(".shoal.toml")));

        // A closer one wins over the root's.
        fs::write(root.join("a").join(".shoal.toml"), "[prompt]\ntemplate='a'").unwrap();
        assert_eq!(
            find_project_config(&nested),
            Some(root.join("a").join(".shoal.toml"))
        );
    }

    #[test]
    fn project_walk_up_finds_nothing_when_absent() {
        let t = tempfile::tempdir().unwrap();
        let nested = t.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_config(&nested), None);
    }

    #[test]
    fn discover_wires_all_four_layers() {
        let t = tempfile::tempdir().unwrap();
        let o = LoadOptions::discover(t.path());
        assert_eq!(o.system, Some(PathBuf::from("/etc/shoal/shoal.toml")));
        // project: none present in a fresh tempdir tree (well, unless the
        // real filesystem above it happens to have one — exercise the
        // plumbing, not the outcome).
        let _ = o.project;
        assert!(!o.env.is_empty() || std::env::vars_os().next().is_none());
    }

    #[test]
    fn unknown_key_deep_in_a_table_still_recurses_and_warns() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[editor]\nbracketde_paste = true\n").unwrap();
        let l = load(&opts(None, Some(p), None, vec![])).unwrap();
        assert_eq!(l.warnings.len(), 1);
        assert!(l.warnings[0].contains("editor.bracketde_paste"));
        assert!(l.warnings[0].contains("did you mean `editor.bracketed_paste`?"));
    }

    #[test]
    fn opaque_reef_table_layers_and_validates_shape_only() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(
            &p,
            "[reef.tools]\nnode = \"22\"\n[reef.options]\nhermetic = true\n",
        )
        .unwrap();
        let l = load(&opts(None, Some(p), None, vec![])).unwrap();
        assert!(l.warnings.is_empty());
        assert_eq!(
            l.config.reef.tools.get("node"),
            Some(&toml::Value::String("22".into()))
        );
        assert!(l.config.reef.options.hermetic);
    }

    #[test]
    fn aliases_and_env_layer_as_string_maps() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(
            &p,
            "[aliases]\ngs = \"git status\"\n[env]\nEDITOR = \"hx\"\n",
        )
        .unwrap();
        let l = load(&opts(None, Some(p), None, vec![])).unwrap();
        assert_eq!(l.config.aliases.get("gs"), Some(&"git status".to_string()));
        assert_eq!(l.config.env.get("EDITOR"), Some(&"hx".to_string()));
    }

    #[test]
    fn invalid_alias_name_is_rejected() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[aliases]\n\"g s\" = \"git status\"\n").unwrap();
        let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
        assert!(matches!(err, ConfigError::Value { ref key, .. } if key == "aliases"));
    }

    /// A "golden" full config exercising every documented key at once —
    /// round-trips through `load` unchanged and un-warned. If this test
    /// needs an update, docs/CONFIG.md's worked example almost certainly
    /// needs the same update.
    #[test]
    fn golden_full_config_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("shoal.toml");
        fs::write(
            &p,
            r#"
version = 1

[prompt]
template = "{cwd} $"

[history]
enabled = true
max_entries = 5000
path = "/home/dev/.local/state/shoal/history"
dedup = true
ignore = ["ls", "cd *"]
ignore_space = true

[render]
width = 120
color = true
paging = "auto"
pager = "less -R"

[editor]
mode = "vi"
bracketed_paste = true
[editor.keybindings]
"ctrl-r" = "history_search_backward"

[kernel]
enabled = true
session = "default"

[adapters]
dirs = ["/home/dev/.config/shoal/adapters"]

[journal]
enabled = true
state_dir = "/home/dev/.local/share/shoal"

[leash]
policy = "/home/dev/.config/shoal/leash.toml"

[init]
files = ["/home/dev/.config/shoal/init.shl"]

[completion]
fuzzy = true
case_insensitive = true
max_results = 200
menu = true

[aliases]
gs = "git status"
gd = "git diff"

[env]
EDITOR = "hx"

[reef.tools]
node = "22"
python = "3.12"
go = { provider = "mise" }

[reef.runners]
py = "python"
ts = { tool = "deno", args = ["run"] }

[reef.options]
hermetic = false
"#,
        )
        .unwrap();

        let l = load(&opts(None, Some(p), None, vec![])).unwrap();
        assert!(
            l.warnings.is_empty(),
            "unexpected warnings: {:?}",
            l.warnings
        );

        let c = &l.config;
        assert_eq!(c.version, 1);
        assert_eq!(c.prompt.template, "{cwd} $");
        assert_eq!(c.history.max_entries, 5000);
        assert_eq!(
            c.history.path,
            Some(PathBuf::from("/home/dev/.local/state/shoal/history"))
        );
        assert!(c.history.dedup);
        assert_eq!(c.history.ignore, vec!["ls".to_string(), "cd *".to_string()]);
        assert_eq!(c.render.width, Some(120));
        assert_eq!(c.render.paging, "auto");
        assert_eq!(c.render.pager.as_deref(), Some("less -R"));
        assert_eq!(c.editor.mode, "vi");
        assert_eq!(
            c.editor.keybindings.get("ctrl-r").map(String::as_str),
            Some("history_search_backward")
        );
        assert_eq!(
            c.adapters.dirs,
            vec![PathBuf::from("/home/dev/.config/shoal/adapters")]
        );
        assert_eq!(c.completion.max_results, 200);
        assert_eq!(c.aliases.get("gs").map(String::as_str), Some("git status"));
        assert_eq!(c.env.get("EDITOR").map(String::as_str), Some("hx"));
        assert!(!c.reef.options.hermetic);
        assert_eq!(
            c.reef.tools.get("node"),
            Some(&toml::Value::String("22".into()))
        );

        // And round-tripping the *typed* Config back through TOML (not the
        // original file text) must reproduce the same Config bit-for-bit —
        // the golden property that makes this a round-trip test rather than
        // just a parse test.
        let text = toml::to_string(&l.config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, l.config);
    }
}
