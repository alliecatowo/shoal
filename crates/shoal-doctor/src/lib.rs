use serde::Serialize;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct Options {
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub socket: PathBuf,
    pub session: String,
    pub language_journal_enabled: bool,
    pub render_width: Option<usize>,
    pub config_error: Option<String>,
}
impl Options {
    pub fn from_env() -> Self {
        let paths = shoal_paths::ShoalPaths::discover();
        let session = std::env::var("SHOAL_SESSION").unwrap_or_else(|_| "default".into());
        let cwd = std::env::current_dir().ok();
        let loaded = cwd
            .as_deref()
            .map(shoal_config::LoadOptions::discover)
            .map(|options| shoal_config::load(&options));
        let (state_dir, language_journal_enabled, render_width, config_error) = match loaded {
            Some(Ok(loaded)) => {
                let state_dir = match loaded.config.journal.state_dir.as_deref() {
                    Some(path) if path.is_absolute() => path.to_path_buf(),
                    Some(path) => cwd.as_deref().unwrap_or_else(|| Path::new(".")).join(path),
                    None => paths.state_dir().to_path_buf(),
                };
                (
                    state_dir,
                    loaded.config.journal.enabled,
                    loaded.config.render.width,
                    None,
                )
            }
            Some(Err(error)) => (
                paths.state_dir().to_path_buf(),
                true,
                None,
                Some(error.to_string()),
            ),
            None => (
                paths.state_dir().to_path_buf(),
                true,
                None,
                Some("cannot determine cwd for layered config discovery".into()),
            ),
        };
        Self {
            runtime_dir: paths.runtime_dir().to_path_buf(),
            state_dir,
            config_dir: paths.config_dir().to_path_buf(),
            socket: paths.socket(&session),
            session,
            language_journal_enabled,
            render_width,
            config_error,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}
impl Report {
    pub fn exit_code(&self) -> i32 {
        if self.checks.iter().any(|c| c.level == Level::Fail) {
            2
        } else if self.checks.iter().any(|c| c.level == Level::Warn) {
            1
        } else {
            0
        }
    }
}
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in &self.checks {
            writeln!(
                f,
                "{:<4} {:<20} {}",
                match c.level {
                    Level::Ok => "ok",
                    Level::Warn => "warn",
                    Level::Fail => "FAIL",
                },
                c.name,
                c.detail
            )?
        }
        writeln!(f, "exit: {}", self.exit_code())
    }
}
pub fn run(o: &Options) -> Report {
    let mut c = Vec::new();
    let enforcement = shoal_leash::EnforcementStatus::detect();
    c.push(Check {
        name: "leash".into(),
        level: if enforcement.enforced {
            Level::Ok
        } else {
            Level::Warn
        },
        detail: format!(
            "available {:?}; active {:?}; enforced={}: {}",
            enforcement.available_tier,
            enforcement.active_tier,
            enforcement.enforced,
            enforcement.detail
        ),
    });
    c.push(Check {
        name: "tty".into(),
        level: if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            Level::Ok
        } else {
            Level::Warn
        },
        detail: "stdin terminal detection".into(),
    });
    probe_pty(&mut c);
    probe_dir("runtime dir", &o.runtime_dir, &mut c);
    probe_dir("state dir", &o.state_dir, &mut c);
    probe_dir("config dir", &o.config_dir, &mut c);
    probe_socket(o, &mut c);
    probe_adapters(o, &mut c);
    probe_tools(&mut c);
    probe_journal(o, &mut c);
    probe_journal_storage(o, &mut c);
    probe_effective_config(o, &mut c);
    probe_configs(o, &mut c);
    Report { checks: c }
}

fn probe_journal_storage(o: &Options, out: &mut Vec<Check>) {
    let database = o.state_dir.join("journal.db");
    let result = if database.exists() {
        shoal_journal::Journal::open(&o.state_dir)
            .and_then(|journal| journal.storage_status())
            .map(Some)
    } else {
        Ok(None)
    };
    let (level, detail) = match result {
        Ok(Some(status)) => {
            let database_used = status.database_admission_bytes();
            let cas_used = status.cas_admission_bytes();
            let exhausted = database_used >= status.database_max_bytes
                || cas_used >= status.cas_max_bytes;
            let nearing = database_used.saturating_mul(10)
                >= status.database_max_bytes.saturating_mul(9)
                || cas_used.saturating_mul(10) >= status.cas_max_bytes.saturating_mul(9);
            (
                if exhausted {
                    Level::Fail
                } else if nearing {
                    Level::Warn
                } else {
                    Level::Ok
                },
                format!(
                    "database {database_used}/{} bytes; CAS {cas_used}/{} bytes ({} pinned); configure SHOAL_JOURNAL_DATABASE_MAX_BYTES and SHOAL_JOURNAL_CAS_MAX_BYTES",
                    status.database_max_bytes,
                    status.cas_max_bytes,
                    status.pinned_logical_bytes,
                ),
            )
        }
        Ok(None) => (
            Level::Ok,
            "no durable journal yet; storage limits come from SHOAL_JOURNAL_DATABASE_MAX_BYTES and SHOAL_JOURNAL_CAS_MAX_BYTES".into(),
        ),
        Err(error) => (Level::Fail, error.to_string()),
    };
    out.push(Check {
        name: "journal storage".into(),
        level,
        detail,
    });
}

fn probe_pty(out: &mut Vec<Check>) {
    use portable_pty::{PtySize, native_pty_system};
    let result = native_pty_system().openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    });
    out.push(Check {
        name: "pty".into(),
        level: if result.is_ok() {
            Level::Ok
        } else {
            Level::Fail
        },
        detail: result
            .map(|_| "native PTY allocation succeeded".to_string())
            .unwrap_or_else(|error| format!("native PTY allocation failed: {error}")),
    });
}
fn probe_dir(name: &str, path: &Path, out: &mut Vec<Check>) {
    let result = (|| {
        let f = tempfile::NamedTempFile::new_in(path)?;
        drop(f);
        Ok::<_, std::io::Error>(())
    })();
    out.push(Check {
        name: name.into(),
        level: if result.is_ok() {
            Level::Ok
        } else {
            Level::Fail
        },
        detail: result
            .map(|()| path.display().to_string())
            .unwrap_or_else(|e| format!("{}: {e}", path.display())),
    })
}
#[cfg(unix)]
fn probe_socket(o: &Options, out: &mut Vec<Check>) {
    use std::os::unix::net::UnixStream;
    let p = &o.socket;
    let r = UnixStream::connect(p);
    out.push(Check {
        name: "kernel socket".into(),
        level: if r.is_ok() { Level::Ok } else { Level::Warn },
        detail: if r.is_ok() {
            format!("reachable: {}", p.display())
        } else {
            format!("not reachable: {}", p.display())
        },
    })
}
#[cfg(not(unix))]
fn probe_socket(_: &Options, out: &mut Vec<Check>) {
    out.push(Check {
        name: "kernel socket".into(),
        level: Level::Warn,
        detail: "Unix sockets unsupported".into(),
    })
}
fn probe_adapters(o: &Options, out: &mut Vec<Check>) {
    let dir = o.config_dir.join("adapters");
    let (cat, warnings) = shoal_adapters::AdapterCatalog::load_dir(&dir);
    out.push(Check {
        name: "adapters".into(),
        level: if warnings.is_empty() {
            Level::Ok
        } else {
            Level::Warn
        },
        detail: format!(
            "{} loaded{}",
            cat.len(),
            if warnings.is_empty() {
                String::new()
            } else {
                format!("; {}", warnings.join("; "))
            }
        ),
    })
}
fn probe_tools(out: &mut Vec<Check>) {
    let path = std::env::var_os("PATH");
    for tool in ["sh", "git", "rg", "cargo"] {
        let ok = tool_available_on_path(tool, path.as_deref());
        out.push(Check {
            name: format!("tool {tool}"),
            level: if ok {
                Level::Ok
            } else if tool == "sh" {
                Level::Fail
            } else {
                Level::Warn
            },
            detail: if ok {
                "available".into()
            } else {
                "not found on PATH".into()
            },
        })
    }
}

fn tool_available_on_path(tool: &str, path: Option<&std::ffi::OsStr>) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = path else { return false };
    std::env::split_paths(path).any(|directory| {
        fs::metadata(directory.join(tool))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}
fn probe_journal(o: &Options, out: &mut Vec<Check>) {
    let result = (|| {
        let d = tempfile::Builder::new()
            .prefix("doctor-journal-")
            .tempdir_in(&o.state_dir)
            .map_err(|e| e.to_string())?;
        let j = shoal_journal::Journal::open(d.path()).map_err(|e| e.to_string())?;
        let id = j
            .append(&shoal_journal::EntryRecord {
                session: "doctor".into(),
                principal: "human".into(),
                ts_ns: 0,
                cwd: b"/".to_vec(),
                src: "null".into(),
                ast_json: "{}".into(),
                effects_json: "[]".into(),
                opaque: false,
            })
            .map_err(|e| e.to_string())?;
        j.finish(id, Some(0), true, 0).map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })();
    out.push(Check {
        name: "journal".into(),
        level: if result.is_ok() {
            Level::Ok
        } else {
            Level::Fail
        },
        detail: result
            .map(|()| {
                format!(
                    "mandatory kernel audit SQLite open/write passed at {}; language history={}",
                    o.state_dir.display(),
                    if o.language_journal_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )
            })
            .unwrap_or_else(|e| e),
    })
}

fn probe_effective_config(o: &Options, out: &mut Vec<Check>) {
    let (level, detail) = match &o.config_error {
        Some(error) => (Level::Fail, error.clone()),
        None => (
            Level::Ok,
            format!(
                "render width={}; language history={}; kernel security audit=mandatory; state={}",
                o.render_width
                    .map_or_else(|| "terminal".into(), |width| width.to_string()),
                if o.language_journal_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                o.state_dir.display()
            ),
        ),
    };
    out.push(Check {
        name: "effective config".into(),
        level,
        detail,
    });
}
fn probe_configs(o: &Options, out: &mut Vec<Check>) {
    let config_path = o.config_dir.join("shoal.toml");
    let policy_path = o.config_dir.join("leash.toml");
    for (name, path, result) in [
        ("config", &config_path, probe_config_file(&config_path)),
        ("policy", &policy_path, probe_policy_file(&policy_path)),
    ] {
        let (level, detail) = match result {
            Ok(ProbeFile::Absent) => (Level::Warn, format!("{} absent", path.display())),
            Ok(ProbeFile::Valid(detail)) => (Level::Ok, detail),
            Err(error) => (Level::Fail, error),
        };
        out.push(Check {
            name: name.into(),
            level,
            detail,
        })
    }
}

#[derive(Debug)]
enum ProbeFile {
    Absent,
    Valid(String),
}

fn probe_config_file(path: &Path) -> Result<ProbeFile, String> {
    let loaded = shoal_config::load(&shoal_config::LoadOptions {
        system: None,
        user: Some(path.to_path_buf()),
        project: None,
        env: Vec::new(),
    })
    .map_err(|error| error.to_string())?;
    if loaded.sources.is_empty() {
        return Ok(ProbeFile::Absent);
    }
    let suffix = if loaded.warnings.is_empty() {
        String::new()
    } else {
        format!("; {} schema warning(s)", loaded.warnings.len())
    };
    Ok(ProbeFile::Valid(format!(
        "{} valid{suffix}",
        path.display()
    )))
}

fn probe_policy_file(path: &Path) -> Result<ProbeFile, String> {
    match shoal_leash::Policy::load(path) {
        Ok(_) => Ok(ProbeFile::Valid(format!("{} valid", path.display()))),
        Err(shoal_leash::PolicyLoadError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(ProbeFile::Absent)
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn temp_xdg_report_is_deterministic() {
        let t = tempfile::tempdir().unwrap();
        let o = Options {
            runtime_dir: t.path().join("run"),
            state_dir: t.path().join("state"),
            config_dir: t.path().join("config"),
            socket: t.path().join("run/shoal/none.sock"),
            session: "none".into(),
            language_journal_enabled: false,
            render_width: Some(100),
            config_error: None,
        };
        for p in [&o.runtime_dir, &o.state_dir, &o.config_dir] {
            fs::create_dir_all(p).unwrap()
        }
        let r = run(&o);
        assert!(
            r.checks
                .iter()
                .any(|c| c.name == "journal" && c.level == Level::Ok)
        );
        assert!(
            r.checks
                .iter()
                .any(|c| c.name == "kernel socket" && c.level == Level::Warn)
        );
        assert!(r.checks.iter().any(|c| {
            c.name == "effective config"
                && c.detail.contains("language history=disabled")
                && c.detail.contains("kernel security audit=mandatory")
        }));
        assert_eq!(r.exit_code(), 1)
    }
    #[cfg(unix)]
    #[test]
    fn mock_socket_reachable() {
        use std::os::unix::net::UnixListener;
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("shoal");
        fs::create_dir_all(&sock).unwrap();
        let _l = UnixListener::bind(sock.join("s.sock")).unwrap();
        let o = Options {
            runtime_dir: t.path().into(),
            state_dir: t.path().join("state"),
            config_dir: t.path().join("config"),
            socket: sock.join("s.sock"),
            session: "s".into(),
            language_journal_enabled: true,
            render_width: None,
            config_error: None,
        };
        for p in [&o.state_dir, &o.config_dir] {
            fs::create_dir_all(p).unwrap()
        }
        let r = run(&o);
        assert!(
            r.checks
                .iter()
                .any(|c| c.name == "kernel socket" && c.level == Level::Ok)
        )
    }

    #[test]
    fn tool_probe_requires_an_executable_regular_file() {
        let t = tempfile::tempdir().unwrap();
        let tool = t.path().join("audit-tool");
        fs::write(&tool, b"#!/bin/sh\n").unwrap();
        let path = std::env::join_paths([t.path()]).unwrap();

        assert!(!tool_available_on_path("audit-tool", Some(&path)));
        let mut permissions = fs::metadata(&tool).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&tool, permissions).unwrap();
        assert!(tool_available_on_path("audit-tool", Some(&path)));
        assert!(!tool_available_on_path("missing", Some(&path)));
        assert!(!tool_available_on_path("audit-tool", None));
    }

    #[test]
    fn config_probes_use_authoritative_bounded_loaders() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("shoal.toml");
        let policy = directory.path().join("leash.toml");

        assert!(matches!(probe_config_file(&config), Ok(ProbeFile::Absent)));
        assert!(matches!(probe_policy_file(&policy), Ok(ProbeFile::Absent)));

        let sparse = fs::File::create(&config).unwrap();
        sparse
            .set_len((shoal_config::CONFIG_FILE_MAX_BYTES + 1) as u64)
            .unwrap();
        assert!(
            probe_config_file(&config)
                .unwrap_err()
                .contains("configuration exceeds")
        );

        fs::write(&config, b"[history]\nenabled = 'yes'\n").unwrap();
        assert!(
            probe_config_file(&config)
                .unwrap_err()
                .contains("expected a boolean")
        );

        fs::write(&policy, [0xff]).unwrap();
        assert!(
            probe_policy_file(&policy)
                .unwrap_err()
                .contains("not valid UTF-8")
        );
    }

    #[test]
    fn production_config_probe_does_not_bypass_core_loaders() {
        let source = include_str!("lib.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("fs::read_to_string"));
        assert!(production.contains("shoal_config::load"));
        assert!(production.contains("shoal_leash::Policy::load"));
    }
}
