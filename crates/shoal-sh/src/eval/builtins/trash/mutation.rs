//! Identity-guarded trash and permanent-removal commit paths.

use super::*;

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
        let result = if plan.is_dir {
            fs.remove_dir_all(&target)
        } else {
            fs.remove_file(&target)
        };
        result.map_err(|error| {
            ioerr("remove quarantined entry", &target, error).with_hint(format!(
                "the identity-verified entry remains contained at {}; inspect or remove it manually",
                target.display()
            ))
        })?;
    }
    Ok(())
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

fn removal_mutation_error(operation: &str, path: &Path, error: std::io::Error) -> ErrorVal {
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
