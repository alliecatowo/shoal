use serde::Serialize;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct Options {
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub session: String,
}
impl Options {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
            state_dir: std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"))
                .join("shoal"),
            config_dir: std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("shoal"),
            session: std::env::var("SHOAL_SESSION").unwrap_or_else(|_| "default".into()),
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
    c.push(Check {
        name: "pty".into(),
        level: if Path::new("/dev/ptmx").exists() {
            Level::Ok
        } else {
            Level::Fail
        },
        detail: "/dev/ptmx availability".into(),
    });
    probe_dir("runtime dir", &o.runtime_dir, &mut c);
    probe_dir("state dir", &o.state_dir, &mut c);
    probe_dir("config dir", &o.config_dir, &mut c);
    probe_socket(o, &mut c);
    probe_adapters(o, &mut c);
    probe_tools(&mut c);
    probe_journal(o, &mut c);
    probe_configs(o, &mut c);
    Report { checks: c }
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
    let p = o
        .runtime_dir
        .join("shoal")
        .join(format!("{}.sock", o.session));
    let r = UnixStream::connect(&p);
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
    for tool in ["sh", "git", "rg", "cargo"] {
        let ok = Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .is_ok_and(|x| x.status.success());
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
            .map(|()| "SQLite open/write probe passed".into())
            .unwrap_or_else(|e| e),
    })
}
fn probe_configs(o: &Options, out: &mut Vec<Check>) {
    for (name, path, policy) in [
        ("config", o.config_dir.join("shoal.toml"), false),
        ("policy", o.config_dir.join("leash.toml"), true),
    ] {
        let (level, detail) = match fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Level::Warn, format!("{} absent", path.display()))
            }
            Err(e) => (Level::Fail, e.to_string()),
            Ok(s) => {
                let r = if policy {
                    shoal_leash::Policy::from_toml(&s)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                } else {
                    toml::from_str::<toml::Value>(&s)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                };
                match r {
                    Ok(()) => (Level::Ok, format!("{} valid", path.display())),
                    Err(e) => (Level::Fail, e),
                }
            }
        };
        out.push(Check {
            name: name.into(),
            level,
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temp_xdg_report_is_deterministic() {
        let t = tempfile::tempdir().unwrap();
        let o = Options {
            runtime_dir: t.path().join("run"),
            state_dir: t.path().join("state"),
            config_dir: t.path().join("config"),
            session: "none".into(),
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
            session: "s".into(),
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
}
