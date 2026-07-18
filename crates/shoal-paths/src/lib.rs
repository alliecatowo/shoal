//! One platform-path contract shared by every Shoal binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    pub home: Option<OsString>,
    pub xdg_state_home: Option<OsString>,
    pub xdg_data_home: Option<OsString>,
    pub xdg_config_home: Option<OsString>,
    pub xdg_runtime_dir: Option<OsString>,
    pub tmpdir: Option<OsString>,
    pub explicit_state_dir: Option<OsString>,
    pub explicit_socket: Option<OsString>,
    pub explicit_secret_dir: Option<OsString>,
}

impl Inputs {
    pub fn from_env() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME"),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR"),
            tmpdir: std::env::var_os("TMPDIR"),
            explicit_state_dir: std::env::var_os("SHOAL_STATE_DIR"),
            explicit_socket: std::env::var_os("SHOAL_SOCKET"),
            explicit_secret_dir: std::env::var_os("SHOAL_SECRET_DIR"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShoalPaths {
    state_dir: PathBuf,
    data_dir: PathBuf,
    config_dir: PathBuf,
    runtime_dir: PathBuf,
    explicit_socket: Option<PathBuf>,
    secret_dir: PathBuf,
}

impl ShoalPaths {
    pub fn discover() -> Self {
        Self::resolve(Inputs::from_env(), effective_uid())
    }

    pub fn resolve(inputs: Inputs, uid: u32) -> Self {
        let home = inputs
            .home
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let explicit_state_dir = inputs
            .explicit_state_dir
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let state_root = inputs
            .xdg_state_home
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|path| path.join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("."));
        let data_root = inputs
            .xdg_data_home
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|path| path.join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        let config_root = inputs
            .xdg_config_home
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|path| path.join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        let runtime_dir = if let Some(path) = inputs
            .xdg_runtime_dir
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            path
        } else if let Some(path) = inputs
            .tmpdir
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            path.join(format!("shoal-{uid}"))
        } else {
            PathBuf::from(format!("/tmp/shoal-{uid}"))
        };
        let data_dir = data_root.join("shoal");
        let secret_dir = inputs
            .explicit_secret_dir
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("secrets"));
        Self {
            state_dir: explicit_state_dir.unwrap_or_else(|| state_root.join("shoal")),
            data_dir,
            config_dir: config_root.join("shoal"),
            runtime_dir,
            explicit_socket: inputs
                .explicit_socket
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            secret_dir,
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Directory containing the encrypted Shoal secret store.
    ///
    /// `SHOAL_SECRET_DIR` is an exact override shared by the evaluator and
    /// the administrative CLI. An empty override is ignored rather than
    /// accidentally selecting the process working directory.
    pub fn secret_dir(&self) -> &Path {
        &self.secret_dir
    }

    pub fn socket(&self, session: &str) -> PathBuf {
        self.explicit_socket.clone().unwrap_or_else(|| {
            self.runtime_dir
                .join("shoal")
                .join(format!("{session}.sock"))
        })
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and returns a value.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn effective_uid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_roots_are_canonical() {
        let paths = ShoalPaths::resolve(
            Inputs {
                home: Some("/home/a".into()),
                xdg_state_home: Some("/state".into()),
                xdg_data_home: Some("/data".into()),
                xdg_config_home: Some("/config".into()),
                xdg_runtime_dir: Some("/run/user/42".into()),
                ..Inputs::default()
            },
            42,
        );
        assert_eq!(paths.state_dir(), &PathBuf::from("/state/shoal"));
        assert_eq!(paths.data_dir(), &PathBuf::from("/data/shoal"));
        assert_eq!(paths.config_dir(), &PathBuf::from("/config/shoal"));
        assert_eq!(
            paths.socket("work"),
            PathBuf::from("/run/user/42/shoal/work.sock")
        );
    }

    #[test]
    fn macos_tmpdir_fallback_is_uid_qualified() {
        let paths = ShoalPaths::resolve(
            Inputs {
                home: Some("/Users/a".into()),
                tmpdir: Some("/var/folders/random/T".into()),
                ..Inputs::default()
            },
            501,
        );
        assert_eq!(
            paths.socket("default"),
            PathBuf::from("/var/folders/random/T/shoal-501/shoal/default.sock")
        );
    }

    #[test]
    fn home_fallbacks_and_explicit_socket_are_stable() {
        let paths = ShoalPaths::resolve(
            Inputs {
                home: Some("/home/a".into()),
                explicit_socket: Some("/custom/kernel.sock".into()),
                ..Inputs::default()
            },
            7,
        );
        assert_eq!(
            paths.state_dir(),
            &PathBuf::from("/home/a/.local/state/shoal")
        );
        assert_eq!(
            paths.data_dir(),
            &PathBuf::from("/home/a/.local/share/shoal")
        );
        assert_eq!(paths.config_dir(), &PathBuf::from("/home/a/.config/shoal"));
        assert_eq!(
            paths.socket("ignored"),
            PathBuf::from("/custom/kernel.sock")
        );
    }

    #[test]
    fn explicit_state_directory_is_not_rewritten() {
        let paths = ShoalPaths::resolve(
            Inputs {
                home: Some("/home/a".into()),
                explicit_state_dir: Some("/srv/shoal-state".into()),
                ..Inputs::default()
            },
            7,
        );
        assert_eq!(paths.state_dir(), Path::new("/srv/shoal-state"));
    }

    #[test]
    fn explicit_secret_directory_is_shared_and_not_rewritten() {
        let paths = ShoalPaths::resolve(
            Inputs {
                home: Some("/home/a".into()),
                xdg_data_home: Some("/data".into()),
                explicit_secret_dir: Some("relative/private-secrets".into()),
                ..Inputs::default()
            },
            7,
        );
        assert_eq!(paths.secret_dir(), Path::new("relative/private-secrets"));
        assert_eq!(paths.data_dir(), Path::new("/data/shoal"));
    }

    #[test]
    fn empty_environment_values_do_not_override_fallbacks() {
        let paths = ShoalPaths::resolve(
            Inputs {
                home: Some("/home/a".into()),
                xdg_state_home: Some(OsString::new()),
                xdg_runtime_dir: Some(OsString::new()),
                tmpdir: Some(OsString::new()),
                explicit_socket: Some(OsString::new()),
                explicit_secret_dir: Some(OsString::new()),
                ..Inputs::default()
            },
            9,
        );
        assert_eq!(
            paths.state_dir(),
            &PathBuf::from("/home/a/.local/state/shoal")
        );
        assert_eq!(
            paths.socket("s"),
            PathBuf::from("/tmp/shoal-9/shoal/s.sock")
        );
        assert_eq!(
            paths.secret_dir(),
            Path::new("/home/a/.local/share/shoal/secrets")
        );
    }
}
