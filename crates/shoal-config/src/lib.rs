use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub prompt: Prompt,
    pub history: History,
    pub render: Render,
    pub editor: Editor,
    pub kernel: Kernel,
    pub adapters: Adapters,
    pub journal: Journal,
    pub leash: Leash,
    pub init: Init,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompt {
    pub template: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub enabled: bool,
    pub max_entries: usize,
    pub path: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Render {
    pub width: Option<usize>,
    pub color: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    pub mode: String,
    pub bracketed_paste: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Kernel {
    pub enabled: bool,
    pub session: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Adapters {
    pub dirs: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Journal {
    pub enabled: bool,
    pub state_dir: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Leash {
    pub policy: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Init {
    pub files: Vec<PathBuf>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            prompt: Prompt::default(),
            history: History::default(),
            render: Render::default(),
            editor: Editor::default(),
            kernel: Kernel::default(),
            adapters: Adapters::default(),
            journal: Journal::default(),
            leash: Leash::default(),
            init: Init::default(),
        }
    }
}
impl Default for Prompt {
    fn default() -> Self {
        Self {
            template: "{cwd}".into(),
        }
    }
}
impl Default for History {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 10_000,
            path: None,
        }
    }
}
impl Default for Render {
    fn default() -> Self {
        Self {
            width: None,
            color: true,
        }
    }
}
impl Default for Editor {
    fn default() -> Self {
        Self {
            mode: "emacs".into(),
            bracketed_paste: true,
        }
    }
}
impl Default for Kernel {
    fn default() -> Self {
        Self {
            enabled: true,
            session: "default".into(),
        }
    }
}
impl Default for Journal {
    fn default() -> Self {
        Self {
            enabled: true,
            state_dir: None,
        }
    }
}
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
    pub sources: Vec<PathBuf>,
}
#[derive(Debug)]
pub struct LoadOptions {
    pub system: Option<PathBuf>,
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}
impl LoadOptions {
    pub fn discover(cwd: &Path) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let user = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".config")))
            .map(|p| p.join("shoal/shoal.toml"));
        Self {
            system: Some("/etc/shoal/shoal.toml".into()),
            user,
            project: Some(cwd.join(".shoal.toml")),
            env: std::env::vars_os().collect(),
        }
    }
}
pub fn load(o: &LoadOptions) -> Result<Loaded, String> {
    let mut merged = toml::Value::try_from(Config::default()).map_err(|e| e.to_string())?;
    let mut warnings = vec![];
    let mut sources = vec![];
    for path in [&o.system, &o.user, &o.project].into_iter().flatten() {
        match fs::read_to_string(path) {
            Ok(s) => {
                let value: toml::Value =
                    toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
                unknowns(&value, "", &mut warnings);
                merge(&mut merged, value);
                sources.push(path.clone())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    apply_env(&mut merged, &o.env)?;
    let config: Config = merged
        .try_into()
        .map_err(|e| format!("invalid configuration: {e}"))?;
    validate(&config)?;
    Ok(Loaded {
        config,
        warnings,
        sources,
    })
}
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
fn apply_env(v: &mut toml::Value, env: &[(OsString, OsString)]) -> Result<(), String> {
    for (k, val) in env {
        let Some(k) = k.to_str() else { continue };
        let Some(val) = val.to_str() else {
            return Err(format!("{k} is not UTF-8"));
        };
        let path = match k {
            "SHOAL_PROMPT" => Some(["prompt", "template"]),
            "SHOAL_HISTORY" => Some(["history", "enabled"]),
            "SHOAL_KERNEL" => Some(["kernel", "enabled"]),
            _ => None,
        };
        if let Some([a, b]) = path {
            let table = v
                .as_table_mut()
                .unwrap()
                .get_mut(a)
                .unwrap()
                .as_table_mut()
                .unwrap();
            table.insert(
                b.into(),
                if b == "enabled" {
                    toml::Value::Boolean(
                        val.parse().map_err(|_| format!("{k} expects true/false"))?,
                    )
                } else {
                    toml::Value::String(val.into())
                },
            );
        }
    }
    Ok(())
}
fn validate(c: &Config) -> Result<(), String> {
    if c.version != 1 {
        return Err(format!("unsupported config version {}", c.version));
    }
    if c.history.max_entries == 0 {
        return Err("history.max_entries must be positive".into());
    }
    if !matches!(c.editor.mode.as_str(), "emacs" | "vi") {
        return Err("editor.mode must be emacs or vi".into());
    }
    Ok(())
}
fn unknowns(v: &toml::Value, prefix: &str, out: &mut Vec<String>) {
    let allowed: BTreeSet<&str> = match prefix {
        "" => [
            "version", "prompt", "history", "render", "editor", "kernel", "adapters", "journal",
            "leash", "init",
        ]
        .into_iter()
        .collect(),
        "prompt" => ["template"].into_iter().collect(),
        "history" => ["enabled", "max_entries", "path"].into_iter().collect(),
        "render" => ["width", "color"].into_iter().collect(),
        "editor" => ["mode", "bracketed_paste"].into_iter().collect(),
        "kernel" => ["enabled", "session"].into_iter().collect(),
        "adapters" => ["dirs"].into_iter().collect(),
        "journal" => ["enabled", "state_dir"].into_iter().collect(),
        "leash" => ["policy"].into_iter().collect(),
        "init" => ["files"].into_iter().collect(),
        _ => BTreeSet::new(),
    };
    if let Some(t) = v.as_table() {
        for (k, x) in t {
            if !allowed.contains(k.as_str()) {
                out.push(format!(
                    "unknown config key {}{}",
                    if prefix.is_empty() { "" } else { prefix },
                    if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!(".{k}")
                    }
                ))
            } else if x.is_table() {
                unknowns(x, k, out)
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn precedence_and_warning() {
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("s");
        let u = t.path().join("u");
        let p = t.path().join("p");
        fs::write(&s, "[prompt]\ntemplate='system'").unwrap();
        fs::write(&u, "[prompt]\ntemplate='user'").unwrap();
        fs::write(&p, "[prompt]\ntemplate='project'\nwat=1").unwrap();
        let l = load(&LoadOptions {
            system: Some(s),
            user: Some(u),
            project: Some(p),
            env: vec![("SHOAL_PROMPT".into(), "env".into())],
        })
        .unwrap();
        assert_eq!(l.config.prompt.template, "env");
        assert_eq!(l.warnings.len(), 1)
    }
    #[test]
    fn invalid_version() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, "version=9").unwrap();
        assert!(
            load(&LoadOptions {
                system: None,
                user: Some(p),
                project: None,
                env: vec![]
            })
            .unwrap_err()
            .contains("unsupported")
        )
    }
}
