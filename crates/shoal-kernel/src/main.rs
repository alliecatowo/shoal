use shoal_kernel::Kernel;
use shoal_leash::Policy;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn main() {
    if let Err(error) = run() {
        eprintln!("shoal-kernel: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    let socket = args.socket.unwrap_or_else(|| runtime_socket(&args.session));
    prepare_socket(&socket)?;
    let state = args.state_dir.unwrap_or_else(state_dir);
    let kernel = if let Some(path) = args.policy {
        Kernel::open_with_policy(&state, Policy::load(&path)?)?
    } else {
        Kernel::open(&state)?
    };
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))?;
    eprintln!("shoal-kernel: ready {}", socket.display());
    kernel.serve_until(&socket, stop)?;
    Ok(())
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket needs parent"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "kernel already listening",
            ));
        }
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_socket() || meta.uid() != unsafe { geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to remove unowned non-socket path",
            ));
        }
        fs::remove_file(path)?;
    }
    Ok(())
}
unsafe extern "C" {
    fn geteuid() -> u32;
}

fn runtime_socket(session: &str) -> PathBuf {
    runtime_dir().join("shoal").join(format!("{session}.sock"))
}

/// The runtime directory for the kernel socket. `$XDG_RUNTIME_DIR` when set;
/// otherwise `$TMPDIR/shoal-{uid}` (macOS exports `TMPDIR` but not
/// `XDG_RUNTIME_DIR`), else the hard `/tmp/shoal-{uid}` fallback. `shoal-mcp`
/// mirrors this exactly so socket discovery agrees on every platform.
fn runtime_dir() -> PathBuf {
    let uid = unsafe { geteuid() };
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg);
    }
    if let Some(tmp) = std::env::var_os("TMPDIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(tmp).join(format!("shoal-{uid}"));
    }
    PathBuf::from(format!("/tmp/shoal-{uid}"))
}
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("shoal")
}

struct Args {
    session: String,
    socket: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    policy: Option<PathBuf>,
}
impl Args {
    fn parse(mut it: impl Iterator<Item = std::ffi::OsString>) -> Result<Self, String> {
        let mut a = Self {
            session: "default".into(),
            socket: None,
            state_dir: None,
            policy: None,
        };
        while let Some(k) = it.next() {
            let missing = || format!("{} requires a value", k.to_string_lossy());
            match k.to_str() {
                Some("--session") => a.session = it.next().ok_or_else(&missing)?.into_string().map_err(|_| "invalid session")?,
                Some("--socket") => a.socket = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--state-dir") => a.state_dir = Some(it.next().ok_or_else(&missing)?.into()),
                Some("--policy") => a.policy = Some(it.next().ok_or_else(&missing)?.into()),
                Some("-h" | "--help") => return Err("usage: shoal-kernel [--session NAME] [--socket PATH] [--state-dir PATH] [--policy FILE]".into()),
                _ => return Err(format!("unknown argument {}", k.to_string_lossy())),
            }
        }
        Ok(a)
    }
}
