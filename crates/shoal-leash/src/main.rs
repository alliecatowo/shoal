use shoal_leash::FsSandbox;
use std::path::PathBuf;

/// Apply the strongest OS backend this crate has for the current platform.
/// Mirrors the dispatch `shoal-sandbox-exec` uses, so this helper's own
/// pass/fail behavior is representative of what the real spawn path does.
#[cfg(target_os = "linux")]
fn apply(s: &FsSandbox) -> Result<shoal_leash::EnforcementStatus, String> {
    shoal_leash::apply_landlock(s)
}
#[cfg(target_os = "macos")]
fn apply(s: &FsSandbox) -> Result<shoal_leash::EnforcementStatus, String> {
    shoal_leash::apply_macos_sandbox(s)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn apply(_: &FsSandbox) -> Result<shoal_leash::EnforcementStatus, String> {
    Err("no OS sandbox backend for this platform".into())
}

fn main() {
    let a: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if a.len() != 2 {
        std::process::exit(64)
    };
    if apply(&FsSandbox {
        read: vec![a[0].clone()],
        write: vec![],
        delete: vec![],
    })
    .is_err()
    {
        std::process::exit(77)
    };
    if std::fs::read(&a[0]).is_err() {
        std::process::exit(2)
    };
    if std::fs::read(&a[1]).is_ok() {
        std::process::exit(3)
    }
}
