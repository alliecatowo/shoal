//! macOS Seatbelt profile generation: a deterministic, deny-by-default
//! `(sandbox-init)` profile built from canonicalized filesystem grants.

use crate::enforce::FsSandbox;
use std::fs;
use std::path::{Path, PathBuf};

/// Deterministic deny-by-default Seatbelt profile. Grants must exist so
/// canonicalization cannot silently broaden a symlinked or lexical path.
pub fn seatbelt_profile(grants: &FsSandbox) -> Result<String, String> {
    let mut read = canonical_grants(&grants.read)?;
    let mut write = canonical_grants(&grants.write)?;
    let mut delete = canonical_grants(&grants.delete)?;
    read.sort();
    read.dedup();
    write.sort();
    write.dedup();
    delete.sort();
    delete.dedup();
    let mut out = String::from(
        "(version 1)\n(deny default)\n(allow process*)\n(allow signal (target self))\n",
    );
    for p in read {
        out.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            seatbelt_escape(&p)?
        ));
    }
    for p in write {
        out.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            seatbelt_escape(&p)?
        ));
    }
    for p in delete {
        out.push_str(&format!(
            "(allow file-read-metadata file-write-unlink (subpath \"{}\"))\n",
            seatbelt_escape(&p)?
        ));
    }
    Ok(out)
}
fn canonical_grants(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .map(|p| {
            fs::canonicalize(p)
                .map_err(|e| format!("cannot canonicalize Seatbelt grant {}: {e}", p.display()))
        })
        .collect()
}
fn seatbelt_escape(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("Seatbelt cannot encode non-UTF-8 path {}", path.display()))?;
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => return Err("Seatbelt grant contains a control character".into()),
            c => out.push(c),
        }
    }
    Ok(out)
}
