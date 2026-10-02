use shoal_leash::{FsSandbox, apply_sandbox as apply};
use std::path::PathBuf;

const HELP: &str = "Probe Shoal Landlock enforcement

Usage:
  shoal-landlock-helper ALLOWED_PATH DENIED_PATH

Arguments:
  ALLOWED_PATH  A readable file that must remain accessible
  DENIED_PATH   A file that the sandbox must make inaccessible

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Output:
  No output on success; this is an internal installation probe used by Shoal Doctor.

Errors:
  Distinct statuses identify unavailable Landlock, unreadable allowed input, or ineffective denial.

Examples:
  shoal-landlock-helper ./allowed.txt ./denied.txt

Exit status:
  0 on success; 64 for usage; 77 when unavailable; 2 if allowed read fails; 3 if denial fails.";

fn main() {
    let a: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if a.as_slice() == [PathBuf::from("-h")] || a.as_slice() == [PathBuf::from("--help")] {
        println!("{HELP}");
        return;
    }
    if a.as_slice() == [PathBuf::from("-V")] || a.as_slice() == [PathBuf::from("--version")] {
        println!("shoal-landlock-helper {}", env!("CARGO_PKG_VERSION"));
        return;
    }
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
