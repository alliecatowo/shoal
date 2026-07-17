//! Discovery, layering, and validation — turns a chain of on-disk files (plus
//! the process environment) into a validated [`Config`].

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::{Config, ConfigError};

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
        match read_config_file(path)? {
            Some(text) => {
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
            None => {}
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

fn io_error(path: &Path, error: io::Error) -> ConfigError {
    ConfigError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

/// Read one optional layer without trusting metadata length for allocation.
/// The preliminary metadata check rejects ordinary directories/devices/FIFOs
/// before open and follows symlinks, preserving symlink-to-file support. The
/// bounded reader remains authoritative if the file grows after that check.
fn read_config_file(path: &Path) -> Result<Option<String>, ConfigError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(path, error)),
    };
    if !metadata.is_file() {
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            message: "configuration layer is not a regular file".into(),
        });
    }
    let file = fs::File::open(path).map_err(|error| io_error(path, error))?;
    read_config_utf8(path, file).map(Some)
}

fn read_config_utf8(path: &Path, reader: impl Read) -> Result<String, ConfigError> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    reader
        .take((CONFIG_FILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() > CONFIG_FILE_MAX_BYTES {
        return Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
            max_bytes: CONFIG_FILE_MAX_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| ConfigError::Utf8 {
        path: path.to_path_buf(),
    })
}

/// Reject bracket-shaped TOML recursion before invoking the TOML parser. This
/// scanner deliberately tracks quoted strings/comments so data such as
/// `template = "[[["` does not consume the structure budget.
fn check_toml_nesting(path: &Path, text: &str) -> Result<(), ConfigError> {
    #[derive(Clone, Copy)]
    enum Quote {
        Basic { triple: bool, escaped: bool },
        Literal { triple: bool },
    }

    let bytes = text.as_bytes();
    let mut quote = None;
    let mut comment = false;
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if comment {
            if byte == b'\n' {
                comment = false;
            }
            index += 1;
            continue;
        }
        match quote {
            Some(Quote::Basic {
                triple,
                mut escaped,
            }) => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"'
                    && (!triple || bytes.get(index..index + 3) == Some(b"\"\"\""))
                {
                    quote = None;
                    index += if triple { 3 } else { 1 };
                    continue;
                }
                quote = Some(Quote::Basic { triple, escaped });
            }
            Some(Quote::Literal { triple }) => {
                if byte == b'\'' && (!triple || bytes.get(index..index + 3) == Some(b"'''")) {
                    quote = None;
                    index += if triple { 3 } else { 1 };
                    continue;
                }
            }
            None => match byte {
                b'#' => comment = true,
                b'"' => {
                    let triple = bytes.get(index..index + 3) == Some(b"\"\"\"");
                    quote = Some(Quote::Basic {
                        triple,
                        escaped: false,
                    });
                    if triple {
                        index += 2;
                    }
                }
                b'\'' => {
                    let triple = bytes.get(index..index + 3) == Some(b"'''");
                    quote = Some(Quote::Literal { triple });
                    if triple {
                        index += 2;
                    }
                }
                b'[' | b'{' => {
                    depth += 1;
                    if depth > CONFIG_TOML_MAX_NESTING {
                        return Err(ConfigError::Complexity {
                            path: path.to_path_buf(),
                            max_nesting: CONFIG_TOML_MAX_NESTING,
                        });
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
        index += 1;
    }
    Ok(())
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
mod tests {
    use super::*;

    struct GrowingReader {
        remaining: usize,
    }

    impl Read for GrowingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = self.remaining.min(buf.len());
            buf[..count].fill(b'x');
            self.remaining -= count;
            Ok(count)
        }
    }

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
    fn oversized_sparse_layer_is_rejected_before_toml() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sparse.toml");
        let file = fs::File::create(&p).unwrap();
        file.set_len((CONFIG_FILE_MAX_BYTES + 1) as u64).unwrap();
        let err = load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err();
        assert_eq!(
            err,
            ConfigError::TooLarge {
                path: p,
                max_bytes: CONFIG_FILE_MAX_BYTES,
            }
        );
    }

    #[test]
    fn growing_reader_stops_after_the_limit_sentinel() {
        let p = Path::new("growing.toml");
        let err = read_config_utf8(
            p,
            GrowingReader {
                remaining: CONFIG_FILE_MAX_BYTES * 4,
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            ConfigError::TooLarge {
                path: p.to_path_buf(),
                max_bytes: CONFIG_FILE_MAX_BYTES,
            }
        );
    }

    #[test]
    fn non_utf8_layer_has_a_path_aware_error() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("binary.toml");
        fs::write(&p, [b'v', 0xff]).unwrap();
        assert_eq!(
            load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err(),
            ConfigError::Utf8 { path: p }
        );
    }

    #[test]
    fn deeply_nested_toml_is_rejected_before_deserialization() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("deep.toml");
        fs::write(
            &p,
            format!(
                "unknown = {}0{}\n",
                "[".repeat(CONFIG_TOML_MAX_NESTING + 1),
                "]".repeat(CONFIG_TOML_MAX_NESTING + 1)
            ),
        )
        .unwrap();
        assert_eq!(
            load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err(),
            ConfigError::Complexity {
                path: p,
                max_nesting: CONFIG_TOML_MAX_NESTING,
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_regular_layer_remains_supported() {
        use std::os::unix::fs::symlink;

        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("target.toml");
        let link = t.path().join("link.toml");
        fs::write(&target, "[prompt]\ntemplate = 'linked'\n").unwrap();
        symlink(&target, &link).unwrap();
        let loaded = load(&opts(None, Some(link.clone()), None, vec![])).unwrap();
        assert_eq!(loaded.config.prompt.template, "linked");
        assert_eq!(loaded.sources, vec![link]);
    }

    #[test]
    fn recognized_env_string_cannot_amplify_config_without_bound() {
        let huge = "x".repeat(CONFIG_ENV_VALUE_MAX_BYTES + 1);
        let err = load(&opts(
            None,
            None,
            None,
            vec![("SHOAL_PROMPT_TEMPLATE", &huge)],
        ))
        .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Env { ref var, ref message }
                if var == "SHOAL_PROMPT_TEMPLATE" && message.contains("maximum")
        ));
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
    fn render_width_must_be_positive() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[render]\nwidth = 0\n").unwrap();
        let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Value {
                source: None,
                key: "render.width".into(),
                message: "must be greater than 0".into(),
            }
        );
    }

    /// `render.echo` defaults to unset (`None` — each host surface picks its
    /// own fallback: `-c`/scripts default to `quiet`, the REPL to `all`) and
    /// is settable via either the config file or `SHOAL_RENDER_ECHO`.
    #[test]
    fn render_echo_defaults_to_none_and_is_env_overridable() {
        assert_eq!(Config::default().render.echo, None);

        let l = load(&opts(
            None,
            None,
            None,
            vec![("SHOAL_RENDER_ECHO", "commands")],
        ))
        .unwrap();
        assert_eq!(l.config.render.echo.as_deref(), Some("commands"));
    }

    #[test]
    fn render_echo_rejects_anything_other_than_quiet_commands_or_all() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "[render]\necho = \"loud\"\n").unwrap();
        let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Value {
                source: None,
                key: "render.echo".into(),
                message: "must be `quiet`, `commands`, or `all`".into(),
            }
        );
    }

    /// All three legal values load cleanly from the config file.
    #[test]
    fn render_echo_accepts_each_legal_value() {
        for value in ["quiet", "commands", "all"] {
            let t = tempfile::tempdir().unwrap();
            let p = t.path().join("c");
            fs::write(&p, format!("[render]\necho = \"{value}\"\n")).unwrap();
            let l = load(&opts(None, Some(p), None, vec![])).unwrap();
            assert_eq!(l.config.render.echo.as_deref(), Some(value));
        }
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
    /// needs an update, site/content/internals/configuration-reference.md's worked example almost certainly
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
echo = "quiet"

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
        assert_eq!(c.render.echo.as_deref(), Some("quiet"));
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
