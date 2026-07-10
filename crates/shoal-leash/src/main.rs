use shoal_leash::{FsSandbox, apply_sandbox as apply};
use std::path::PathBuf;

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
