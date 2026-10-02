//! Identity-guarded trash and permanent-removal commit paths.

use super::*;
use sha2::{Digest, Sha256};
use std::io::Read as _;

pub(super) fn permanently_remove(fs: &dyn Fs, plans: &[RemovalPlan]) -> VResult<()> {
    let mut warnings = WarningCollector::default();
    let mut sessions = HashMap::<PathBuf, PathBuf>::new();
    for plan in plans {
        let root = plan
            .adjacent_root
            .as_ref()
            .expect("permanent removal has adjacent quarantine root");
        let session = if let Some(session) = sessions.get(root) {
            validate_private_trash_dir(fs, root)
                .and_then(|()| validate_private_trash_dir(fs, session))
                .map_err(|error| ioerr("remove", root, error))?;
            session.clone()
        } else {
            let session = prepare_trash_session(fs, root, &mut warnings)
                .map_err(|error| ioerr("remove", root, error))?;
            sessions.insert(root.clone(), session.clone());
            session
        };
        let target = session.join(
            plan.entry_name
                .as_deref()
                .expect("permanent removal has quarantine entry name"),
        );
        fs.rename_if_unchanged(&plan.action_path, &target, &plan.identity)
            .map_err(|error| removal_mutation_error("remove", &plan.path, error))?;
        if let Some(expected) = &plan.expected_content {
            verify_quarantined_content(fs, plan, &target, expected)?;
        }
        let result = if plan.is_dir {
            plan.removal_tree
                .as_ref()
                .expect("permanent directory removal has pinned tree")
                .remove(&target)
        } else {
            fs.remove_entry_if_unchanged(&target, &plan.identity)
        };
        result.map_err(|error| {
            let mut error = removal_mutation_error("remove quarantined entry", &target, error);
            error.hint = Some(format!(
                "the identity-verified entry remains contained at {}; inspect or remove it manually",
                target.display()
            ));
            error
        })?;
    }
    Ok(())
}

fn verify_quarantined_content(
    fs: &dyn Fs,
    plan: &RemovalPlan,
    target: &Path,
    expected: &ExpectedContent,
) -> VResult<()> {
    let observed = hash_stable_file(fs, target);
    let matches = observed
        .as_ref()
        .is_ok_and(|(bytes, sha256)| *bytes == expected.bytes && sha256 == &expected.sha256);
    if matches {
        return Ok(());
    }

    let detail = match observed {
        Ok((bytes, sha256)) => format!(
            "expected {} bytes with SHA-256 {}, found {bytes} bytes with SHA-256 {sha256}",
            expected.bytes, expected.sha256
        ),
        Err(error) => format!("could not verify quarantined content: {error}"),
    };
    let restoration = fs.rename_if_unchanged(target, &plan.action_path, &plan.identity);
    let (message, hint) = match restoration {
        Ok(()) => (
            format!(
                "conditional remove refused {} because its content drifted: {detail}; the entry was restored",
                plan.path.display()
            ),
            "refresh the inventory and prune plan before retrying".to_string(),
        ),
        Err(error) => (
            format!(
                "conditional remove refused {} because its content drifted: {detail}; restoration failed ({error})",
                plan.path.display()
            ),
            format!(
                "the changed entry remains safely contained at {}; inspect it and any replacement at the original path",
                target.display()
            ),
        ),
    };
    Err(ErrorVal::new("rm_content_changed", message).with_hint(hint))
}

fn hash_stable_file(fs: &dyn Fs, path: &Path) -> std::io::Result<(u64, String)> {
    let before = fs.file_snapshot(path)?;
    let mut reader = fs.open_read(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("file byte count overflowed"))?;
        hasher.update(&buffer[..read]);
    }
    let after = fs.file_snapshot(path)?;
    if before != after {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file changed while hashing quarantined content",
        ));
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

pub(crate) fn move_to_trash(
    source: &Path,
    primary_target: Option<PathBuf>,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    mut adjacent_target: impl FnMut() -> VResult<PathBuf>,
) -> VResult<PathBuf> {
    if let Some(target) = primary_target {
        match rename(source, &target) {
            Ok(()) => return Ok(target),
            Err(error) if !is_cross_device(&error) => {
                return Err(removal_mutation_error("trash", source, error));
            }
            Err(_) => {}
        }
    }
    let target = adjacent_target()?;
    rename(source, &target).map_err(|error| removal_mutation_error("trash", source, error))?;
    Ok(target)
}

pub(super) fn removal_mutation_error(
    operation: &str,
    path: &Path,
    error: std::io::Error,
) -> ErrorVal {
    if error.kind() == std::io::ErrorKind::InvalidData {
        ErrorVal::new(
            "rm_path_changed",
            format!(
                "{operation}: {} changed after removal preflight: {error}",
                path.display()
            ),
        )
        .with_hint(
            "inspect the path and retry; Shoal did not delete an object whose identity differed",
        )
    } else {
        ioerr(operation, path, error)
    }
}

fn is_cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EXDEV)
}
