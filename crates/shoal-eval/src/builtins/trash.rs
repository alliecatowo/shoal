//! Preflighted removal and bounded, same-filesystem trash reporting.

use super::admission::OutputBudget;
use super::{ioerr, paths};
use shoal_value::{ErrorVal, Fs, Record, VResult, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TRASH_SEQ: AtomicU64 = AtomicU64::new(1);
static TRASH_SESSION: OnceLock<String> = OnceLock::new();
const TRASH_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const TRASH_PRUNE_SCAN_LIMIT: usize = 64;
const WARNING_REPORT_MAX_BYTES: usize = 8 * 1024;
const WARNING_MAX_UNIQUE: usize = 32;
const WARNING_MAX_MESSAGE_BYTES: usize = 1024;

struct RemovalPlan {
    path: PathBuf,
    is_dir: bool,
    entry_name: Option<String>,
    primary_target: Option<PathBuf>,
    adjacent_root: Option<PathBuf>,
}

pub(super) fn remove(
    fs: &dyn Fs,
    cwd: &Path,
    args: Vec<Value>,
    permanent: bool,
    recursive: bool,
) -> VResult<Value> {
    remove_with_budget(fs, cwd, args, permanent, recursive, OutputBudget::new())
}

fn remove_with_budget(
    fs: &dyn Fs,
    cwd: &Path,
    args: Vec<Value>,
    permanent: bool,
    recursive: bool,
    mut output_budget: OutputBudget,
) -> VResult<Value> {
    if args.is_empty() {
        return Err(ErrorVal::new(
            "no_matches",
            "rm requires at least one path; an empty glob deletes nothing",
        ));
    }
    let paths = paths(cwd, args)?;
    let primary_root = (!permanent).then(primary_trash_root);
    let primary_session = primary_root
        .as_ref()
        .map(|root| root.join(trash_session_name()));
    let plans = preflight(
        fs,
        paths,
        permanent,
        recursive,
        primary_session.as_deref(),
        &mut output_budget,
    )?;

    if permanent {
        for plan in &plans {
            if plan.is_dir {
                fs.remove_dir_all(&plan.path)
            } else {
                fs.remove_file(&plan.path)
            }
            .map_err(|error| ioerr("remove", &plan.path, error))?;
        }
        return Ok(Value::List(
            plans
                .into_iter()
                .map(|plan| Value::Path(plan.path))
                .collect(),
        ));
    }

    let mut warnings = WarningCollector::default();
    let primary_session = primary_root.as_ref().and_then(|root| {
        match prepare_trash_session(fs, root, &mut warnings) {
            Ok(path) => Some(path),
            Err(error) => {
                warnings.push(format!(
                    "central trash unavailable at {}: {error}; using a same-filesystem trash",
                    root.display()
                ));
                None
            }
        }
    });
    let mut adjacent_sessions = HashMap::<PathBuf, PathBuf>::new();
    let mut rows = Vec::<Record>::with_capacity(plans.len());
    for plan in plans {
        let entry_name = plan
            .entry_name
            .as_deref()
            .expect("trash plan has entry name");
        let target = move_to_trash(
            &plan.path,
            primary_session
                .as_ref()
                .and_then(|_| plan.primary_target.clone()),
            |source, target| fs.rename(source, target),
            || {
                let root = plan
                    .adjacent_root
                    .as_ref()
                    .expect("trash plan has adjacent root");
                let session = if let Some(session) = adjacent_sessions.get(root) {
                    // Caching skips repeated retention scans, but never skips
                    // the ownership/link/mode validation immediately before
                    // another source is moved through this directory.
                    validate_private_trash_dir(fs, root)
                        .and_then(|()| validate_private_trash_dir(fs, session))
                        .map_err(|error| ioerr("trash", root, error))?;
                    session.clone()
                } else {
                    let session = prepare_trash_session(fs, root, &mut warnings)
                        .map_err(|error| ioerr("trash", root, error))?;
                    adjacent_sessions.insert(root.clone(), session.clone());
                    session
                };
                Ok(session.join(entry_name))
            },
        )?;
        let mut row = Record::new();
        row.insert("path".into(), Value::Path(plan.path));
        row.insert("trash".into(), Value::Path(target));
        row.insert(
            "trash_retention_days".into(),
            Value::Int((TRASH_RETENTION.as_secs() / 86_400) as i64),
        );
        rows.push(row);
    }
    if let Some(summary) = warnings.summary()
        && let Some(first) = rows.first_mut()
    {
        first.insert(
            "trash_cleanup_warnings".into(),
            Value::List(vec![Value::Str(summary)]),
        );
    }
    Ok(Value::List(rows.into_iter().map(Value::Record).collect()))
}

fn preflight(
    fs: &dyn Fs,
    paths: Vec<PathBuf>,
    permanent: bool,
    recursive: bool,
    primary_session: Option<&Path>,
    budget: &mut OutputBudget,
) -> VResult<Vec<RemovalPlan>> {
    let mut plans = Vec::with_capacity(paths.len());
    for (index, path) in paths.into_iter().enumerate() {
        let metadata = fs
            .symlink_metadata(&path)
            .map_err(|error| ioerr("remove", &path, error))?;
        if metadata.is_dir() && !recursive {
            return Err(ErrorVal::arg_error("rm: directory requires --recursive"));
        }
        if permanent {
            budget.admit_value(&Value::Path(path.clone()))?;
            plans.push(RemovalPlan {
                path,
                is_dir: metadata.is_dir(),
                entry_name: None,
                primary_target: None,
                adjacent_root: None,
            });
            continue;
        }

        let sequence = TRASH_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_name()
            .unwrap_or_else(|| OsStr::new("item"))
            .to_string_lossy();
        let entry_name = format!("{sequence}-{name}");
        let primary_target = primary_session.map(|session| session.join(&entry_name));
        let parent = path.parent().ok_or_else(|| {
            ErrorVal::new(
                "io_error",
                format!("trash: {} has no parent directory", path.display()),
            )
        })?;
        let adjacent_root = parent.join(adjacent_trash_name());
        let adjacent_target = adjacent_root.join(trash_session_name()).join(&entry_name);
        let widest_target = primary_target
            .as_ref()
            .filter(|target| {
                target.as_os_str().as_encoded_bytes().len()
                    >= adjacent_target.as_os_str().as_encoded_bytes().len()
            })
            .unwrap_or(&adjacent_target)
            .clone();
        let mut report = Record::new();
        report.insert("path".into(), Value::Path(path.clone()));
        report.insert("trash".into(), Value::Path(widest_target));
        report.insert(
            "trash_retention_days".into(),
            Value::Int((TRASH_RETENTION.as_secs() / 86_400) as i64),
        );
        if index == 0 {
            // The real summary is assembled only after filesystem operations,
            // so reserve its maximum shape now. Actual targets are one of the
            // two candidates above and the actual summary is never larger.
            report.insert(
                "trash_cleanup_warnings".into(),
                Value::List(vec![Value::Str("x".repeat(WARNING_REPORT_MAX_BYTES))]),
            );
        }
        budget.admit_value(&Value::Record(report))?;
        plans.push(RemovalPlan {
            path,
            is_dir: metadata.is_dir(),
            entry_name: Some(entry_name),
            primary_target,
            adjacent_root: Some(adjacent_root),
        });
    }
    Ok(plans)
}

pub(super) fn move_to_trash(
    source: &Path,
    primary_target: Option<PathBuf>,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    mut adjacent_target: impl FnMut() -> VResult<PathBuf>,
) -> VResult<PathBuf> {
    if let Some(target) = primary_target {
        match rename(source, &target) {
            Ok(()) => return Ok(target),
            Err(error) if !is_cross_device(&error) => {
                return Err(ioerr("trash", source, error));
            }
            Err(_) => {}
        }
    }
    let target = adjacent_target()?;
    rename(source, &target).map_err(|error| ioerr("trash", source, error))?;
    Ok(target)
}

fn primary_trash_root() -> PathBuf {
    shoal_paths::ShoalPaths::discover()
        .runtime_dir()
        .join("shoal")
        .join("trash")
}

fn trash_session_name() -> &'static str {
    TRASH_SESSION.get_or_init(|| {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{started:032x}", std::process::id())
    })
}

fn prepare_trash_session(
    fs: &dyn Fs,
    root: &Path,
    warnings: &mut WarningCollector,
) -> std::io::Result<PathBuf> {
    fs.create_private_dir_all(root)?;
    validate_private_trash_dir(fs, root)?;
    for warning in prune_stale_trash_root(
        fs,
        root,
        trash_session_name(),
        TRASH_RETENTION,
        TRASH_PRUNE_SCAN_LIMIT,
    ) {
        warnings.push(warning);
    }
    let session = root.join(trash_session_name());
    fs.create_private_dir_all(&session)?;
    validate_private_trash_dir(fs, &session)?;
    Ok(session)
}

#[cfg(unix)]
fn adjacent_trash_name() -> String {
    format!(".shoal-trash-{}", unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn adjacent_trash_name() -> String {
    ".shoal-trash".into()
}

#[cfg(unix)]
pub(super) fn validate_private_trash_dir(fs: &dyn Fs, path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs.symlink_metadata(path)?;
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "trash directory {} must be owned by uid {effective_uid} with mode 0700",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn validate_private_trash_dir(fs: &dyn Fs, path: &Path) -> std::io::Result<()> {
    if fs.symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("trash directory {} is not a directory", path.display()),
        ))
    }
}

fn is_cross_device(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EXDEV)
}

pub(super) fn prune_stale_trash_root(
    fs: &dyn Fs,
    root: &Path,
    current_session: &str,
    retention: Duration,
    scan_limit: usize,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let entries = match fs.read_dir_prefix(root, scan_limit) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!(
                "cannot scan trash retention at {}: {error}",
                root.display()
            ));
            return warnings;
        }
    };
    let now = SystemTime::now();
    for entry in entries {
        if entry.file_name() == Some(OsStr::new(current_session)) {
            continue;
        }
        let metadata = match fs.symlink_metadata(&entry) {
            Ok(metadata) => metadata,
            Err(error) => {
                warnings.push(format!(
                    "cannot inspect trash entry {}: {error}",
                    entry.display()
                ));
                continue;
            }
        };
        if !metadata.is_dir()
            || metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age < retention)
        {
            continue;
        }
        if let Err(error) = fs.remove_dir_all(&entry) {
            warnings.push(format!(
                "cannot prune trash entry {}: {error}",
                entry.display()
            ));
        }
    }
    warnings
}

#[derive(Default)]
struct WarningCollector {
    seen: HashSet<String>,
    messages: Vec<String>,
    suppressed: usize,
}

impl WarningCollector {
    fn push(&mut self, mut message: String) {
        truncate_utf8(&mut message, WARNING_MAX_MESSAGE_BYTES);
        if self.seen.contains(&message) {
            return;
        }
        if self.messages.len() >= WARNING_MAX_UNIQUE {
            self.suppressed = self.suppressed.saturating_add(1);
            return;
        }
        self.seen.insert(message.clone());
        self.messages.push(message);
    }

    fn summary(&self) -> Option<String> {
        if self.messages.is_empty() && self.suppressed == 0 {
            return None;
        }
        const SUFFIX_RESERVE: usize = 96;
        let message_budget = WARNING_REPORT_MAX_BYTES.saturating_sub(SUFFIX_RESERVE);
        let mut summary = String::new();
        let mut included = 0;
        for message in &self.messages {
            let separator = usize::from(!summary.is_empty()) * 2;
            if summary
                .len()
                .saturating_add(separator)
                .saturating_add(message.len())
                > message_budget
            {
                break;
            }
            if !summary.is_empty() {
                summary.push_str("; ");
            }
            summary.push_str(message);
            included += 1;
        }
        let omitted = self
            .messages
            .len()
            .saturating_sub(included)
            .saturating_add(self.suppressed);
        if omitted != 0 {
            if !summary.is_empty() {
                summary.push_str("; ");
            }
            summary.push_str(&format!(
                "{omitted} additional warning occurrence(s) suppressed"
            ));
        }
        truncate_utf8(&mut summary, WARNING_REPORT_MAX_BYTES);
        Some(summary)
    }
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_value::StdFs;

    #[test]
    fn later_recursive_error_precedes_every_removal() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file");
        let directory = root.path().join("directory");
        std::fs::write(&file, b"keep").unwrap();
        std::fs::create_dir(&directory).unwrap();

        let error = remove(
            &StdFs,
            root.path(),
            vec![Value::Path(file.clone()), Value::Path(directory)],
            true,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert_eq!(std::fs::read(file).unwrap(), b"keep");
    }

    #[test]
    fn report_limit_error_precedes_every_removal() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let error = remove_with_budget(
            &StdFs,
            root.path(),
            vec![Value::Path(first.clone()), Value::Path(second.clone())],
            true,
            false,
            OutputBudget::with_limits(1, 4096),
        )
        .unwrap_err();
        assert_eq!(error.code, "builtin_output_limit");
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(second).unwrap(), b"second");
    }

    #[test]
    fn warning_summary_is_deduplicated_and_bounded() {
        let mut warnings = WarningCollector::default();
        for _ in 0..100 {
            warnings.push("duplicate".into());
        }
        for index in 0..100 {
            warnings.push(format!("{index}-{}", "x".repeat(2048)));
        }
        let summary = warnings.summary().unwrap();
        assert_eq!(summary.matches("duplicate").count(), 1);
        assert!(summary.len() <= WARNING_REPORT_MAX_BYTES);
        assert!(summary.contains("suppressed"));
    }

    #[test]
    fn warning_summary_is_attached_to_only_one_result_row() {
        let mut rows = [Record::new(), Record::new(), Record::new()];
        let mut warnings = WarningCollector::default();
        warnings.push("one warning".into());
        if let Some(summary) = warnings.summary()
            && let Some(first) = rows.first_mut()
        {
            first.insert(
                "trash_cleanup_warnings".into(),
                Value::List(vec![Value::Str(summary)]),
            );
        }
        assert!(rows[0].contains_key("trash_cleanup_warnings"));
        assert!(
            rows[1..]
                .iter()
                .all(|row| !row.contains_key("trash_cleanup_warnings"))
        );
    }
}
