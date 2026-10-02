use std::env;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Install,
    Check,
    Uninstall,
}

#[derive(Debug)]
pub struct Config {
    pub action: Action,
    pub force: bool,
    pub prefix: PathBuf,
    pub release_dir: PathBuf,
    pub install_dir: PathBuf,
    pub man_dir: PathBuf,
    pub lock_timeout_ms: u64,
}

impl Config {
    pub fn parse() -> io::Result<Self> {
        let mut action = Action::Install;
        let mut force = false;
        let mut clean = false;
        for arg in env::args().skip(1) {
            match arg.as_str() {
                "--check" if action == Action::Install => action = Action::Check,
                "--uninstall" if action == Action::Install => action = Action::Uninstall,
                "--force" => force = true,
                "--clean" => clean = true,
                _ => {
                    return Err(invalid(
                        "usage: shoal-install-transaction [--clean|--check|--uninstall [--force]]",
                    ));
                }
            }
        }
        if force && action != Action::Uninstall {
            return Err(invalid("--force is valid only with --uninstall"));
        }
        if clean && action != Action::Install {
            return Err(invalid("--clean cannot be combined with another operation"));
        }

        let home = env::var_os("HOME").ok_or_else(|| invalid("HOME is not set"))?;
        let cargo_home = env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".cargo"));
        let install_dir = env::var_os("SHOAL_INSTALL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| cargo_home.join("bin"));
        let prefix = install_dir
            .parent()
            .ok_or_else(|| invalid("install directory has no prefix"))?
            .to_path_buf();
        let man_dir = env::var_os("SHOAL_MAN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| prefix.join("share/man/man1"));
        let release_dir = env::var_os("SHOAL_RELEASE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/release"));

        validate_root(&prefix, "install prefix")?;
        relative_to(&prefix, &install_dir)?;
        relative_to(&prefix, &man_dir)?;
        let lock_timeout_ms = env::var("SHOAL_INSTALL_LOCK_TIMEOUT_MS")
            .ok()
            .filter(|text| !text.is_empty())
            .map(|text| {
                text.parse::<u64>()
                    .map_err(|_| invalid("invalid SHOAL_INSTALL_LOCK_TIMEOUT_MS"))
            })
            .transpose()?
            .unwrap_or(10_000)
            .min(60_000);

        Ok(Self {
            action,
            force,
            prefix,
            release_dir,
            install_dir,
            man_dir,
            lock_timeout_ms,
        })
    }

    pub fn install_relative(&self) -> io::Result<PathBuf> {
        relative_to(&self.prefix, &self.install_dir)
    }

    pub fn man_relative(&self) -> io::Result<PathBuf> {
        relative_to(&self.prefix, &self.man_dir)
    }
}

pub fn relative_to(root: &Path, child: &Path) -> io::Result<PathBuf> {
    let root = absolute_lexical(root)?;
    let child = absolute_lexical(child)?;
    let relative = child.strip_prefix(&root).map_err(|_| {
        invalid(format!(
            "managed path {} escapes prefix {}",
            child.display(),
            root.display()
        ))
    })?;
    validate_relative(relative)?;
    Ok(relative.to_path_buf())
}

pub fn validate_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(invalid("managed path cannot be the prefix itself"));
    }
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(invalid(format!("unsafe managed path: {}", path.display())))
    }
}

fn validate_root(path: &Path, label: &str) -> io::Result<()> {
    let absolute = absolute_lexical(path)?;
    if absolute.parent().is_none() {
        Err(invalid(format!("unsafe {label}: {}", path.display())))
    } else {
        Ok(())
    }
}

fn absolute_lexical(path: &Path) -> io::Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut clean = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::RootDir => clean.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return Err(invalid(format!(
                        "path escapes filesystem root: {}",
                        path.display()
                    )));
                }
            }
            Component::Normal(part) => clean.push(part),
            Component::Prefix(_) => return Err(invalid("Windows paths are unsupported")),
        }
    }
    Ok(clean)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
