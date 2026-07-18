//! Bounded, effect-free recursive-copy planning followed by execution.

use super::admission::{MAX_RETAINED_BYTES, MAX_VALUES};
use shoal_value::{ErrorVal, Fs, OpaqueHandling, RetainedLimits, VResult, Value, retained_size};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAX_COPY_DEPTH: usize = 64;

#[derive(Debug)]
enum CopyOp {
    CreateDir {
        destination: PathBuf,
    },
    CopyFile {
        source: PathBuf,
        destination: PathBuf,
    },
}

#[derive(Debug)]
struct PendingPath {
    source: PathBuf,
    destination: PathBuf,
    depth: usize,
    retained_bytes: usize,
}

#[derive(Debug)]
pub(super) struct CopyPlan {
    operations: Vec<CopyOp>,
    retained_bytes: usize,
    max_operations: usize,
    max_retained_bytes: usize,
    max_depth: usize,
}

impl CopyPlan {
    pub(super) fn build(
        fs: &dyn Fs,
        jobs: &[(PathBuf, PathBuf)],
        recursive: bool,
    ) -> VResult<Self> {
        Self::build_with_limits(
            fs,
            jobs,
            recursive,
            MAX_VALUES,
            MAX_RETAINED_BYTES,
            MAX_COPY_DEPTH,
        )
    }

    fn build_with_limits(
        fs: &dyn Fs,
        jobs: &[(PathBuf, PathBuf)],
        recursive: bool,
        max_operations: usize,
        max_retained_bytes: usize,
        max_depth: usize,
    ) -> VResult<Self> {
        let mut plan = Self {
            operations: Vec::new(),
            retained_bytes: 0,
            max_operations,
            max_retained_bytes,
            max_depth,
        };
        let mut pending = Vec::new();
        // This is a LIFO work stack. Reverse initial jobs and sorted children
        // so the resulting operation order remains caller/lexical order.
        for (source, destination) in jobs.iter().rev() {
            validate_root_job(fs, source, destination)?;
            plan.admit_pending(&mut pending, source.clone(), destination.clone(), 0)?;
        }
        while let Some(path) = pending.pop() {
            plan.retained_bytes = plan
                .retained_bytes
                .checked_sub(path.retained_bytes)
                .expect("pending path charge is owned by the plan");
            plan.visit(fs, &mut pending, path, recursive)?;
        }
        Ok(plan)
    }

    fn visit(
        &mut self,
        fs: &dyn Fs,
        pending: &mut Vec<PendingPath>,
        path: PendingPath,
        recursive: bool,
    ) -> VResult<()> {
        let metadata = fs
            .symlink_metadata(&path.source)
            .map_err(|error| super::ioerr("copy", &path.source, error))?;
        if metadata.is_dir() {
            if !recursive {
                return Err(ErrorVal::arg_error("cp: directory requires --recursive"));
            }
            self.admit(CopyOp::CreateDir {
                destination: path.destination.clone(),
            })?;
            let remaining = self
                .max_operations
                .saturating_sub(self.operations.len())
                .saturating_sub(pending.len());
            let mut entries = fs
                .read_dir_limited(
                    &path.source,
                    remaining,
                    self.max_retained_bytes.saturating_sub(self.retained_bytes),
                )
                .map_err(|error| map_directory_error(&path.source, error))?;
            entries.sort();
            for entry in entries.into_iter().rev() {
                let name = entry.file_name().ok_or_else(|| {
                    ErrorVal::new(
                        "io_error",
                        format!("copy: directory entry {} has no filename", entry.display()),
                    )
                })?;
                let child_depth = path.depth.checked_add(1).ok_or_else(|| {
                    work_limit("recursive copy directory depth accounting overflowed")
                })?;
                self.admit_pending(
                    pending,
                    entry.clone(),
                    path.destination.join(name),
                    child_depth,
                )?;
            }
        } else {
            self.admit(CopyOp::CopyFile {
                source: path.source,
                destination: path.destination,
            })?;
        }
        Ok(())
    }

    fn admit_pending(
        &mut self,
        pending: &mut Vec<PendingPath>,
        source: PathBuf,
        destination: PathBuf,
        depth: usize,
    ) -> VResult<()> {
        if depth > self.max_depth {
            return Err(work_limit(format!(
                "recursive copy exceeds its {}-directory depth limit",
                self.max_depth
            )));
        }
        if self.operations.len().saturating_add(pending.len()) >= self.max_operations {
            return Err(work_limit(format!(
                "recursive copy reached its {}-operation limit",
                self.max_operations
            )));
        }
        let retained = self.measure_paths(&source, Some(&destination))?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(retained)
            .ok_or_else(|| work_limit("recursive copy plan accounting overflowed"))?;
        pending.push(PendingPath {
            source,
            destination,
            depth,
            retained_bytes: retained,
        });
        Ok(())
    }

    fn admit(&mut self, operation: CopyOp) -> VResult<()> {
        if self.operations.len() >= self.max_operations {
            return Err(work_limit(format!(
                "recursive copy reached its {}-operation limit",
                self.max_operations
            )));
        }
        let retained = match &operation {
            CopyOp::CreateDir { destination } => self.measure_paths(destination, None)?,
            CopyOp::CopyFile {
                source,
                destination,
            } => self.measure_paths(source, Some(destination))?,
        };
        self.retained_bytes = self
            .retained_bytes
            .checked_add(retained)
            .ok_or_else(|| work_limit("recursive copy plan accounting overflowed"))?;
        self.operations.push(operation);
        Ok(())
    }

    fn measure_paths(&self, first: &Path, second: Option<&Path>) -> VResult<usize> {
        let measured = second.map_or_else(
            || Value::Path(first.to_path_buf()),
            |second| {
                Value::List(vec![
                    Value::Path(first.to_path_buf()),
                    Value::Path(second.to_path_buf()),
                ])
            },
        );
        let retained = retained_size(
            &measured,
            RetainedLimits {
                max_bytes: self.max_retained_bytes.saturating_sub(self.retained_bytes),
                max_depth: 8,
                max_nodes: 4,
                opaque: OpaqueHandling::Reject,
                allow_secret: false,
            },
        )
        .map_err(|_| {
            work_limit(format!(
                "recursive copy exceeds its {}-byte plan limit",
                self.max_retained_bytes
            ))
        })?;
        Ok(retained)
    }

    pub(super) fn execute(self, fs: &dyn Fs) -> VResult<()> {
        for operation in self.operations {
            match operation {
                CopyOp::CreateDir { destination } => fs
                    .create_dir_all(&destination)
                    .map_err(|error| super::ioerr("copy", &destination, error))?,
                CopyOp::CopyFile {
                    source,
                    destination,
                } => {
                    fs.copy(&source, &destination)
                        .map_err(|error| super::ioerr("copy", &source, error))?;
                }
            }
        }
        Ok(())
    }
}

/// Refuse aliases of the same file and recursive destinations inside their
/// source before inventory or execution. Canonicalizing the closest existing
/// destination ancestor catches symlinked parents even when the leaf does not
/// exist yet.
fn validate_root_job(fs: &dyn Fs, source: &Path, destination: &Path) -> VResult<()> {
    let source_metadata = fs
        .metadata(source)
        .map_err(|error| super::ioerr("copy", source, error))?;
    let canonical_source = fs
        .canonicalize(source)
        .map_err(|error| super::ioerr("copy", source, error))?;
    let canonical_destination = canonicalize_with_missing_tail(fs, destination)?;

    if canonical_source == canonical_destination
        || existing_files_are_identical(fs, &source_metadata, destination)?
    {
        return Err(copy_relation_error(format!(
            "source and destination are the same file: {}",
            source.display()
        )));
    }
    if source_metadata.is_dir() && canonical_destination.starts_with(&canonical_source) {
        return Err(copy_relation_error(format!(
            "cannot copy directory {} into itself at {}",
            source.display(),
            destination.display()
        )));
    }
    Ok(())
}

fn canonicalize_with_missing_tail(fs: &dyn Fs, path: &Path) -> VResult<PathBuf> {
    let normalized = normalize_lexically(path);
    let mut existing = normalized.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs.canonicalize(existing) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    super::ioerr(
                        "copy",
                        path,
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "destination has no existing ancestor",
                        ),
                    )
                })?;
                missing.push(name.to_owned());
                existing = existing.parent().ok_or_else(|| {
                    super::ioerr(
                        "copy",
                        path,
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "destination has no existing ancestor",
                        ),
                    )
                })?;
            }
            Err(error) => return Err(super::ioerr("copy", path, error)),
        }
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn existing_files_are_identical(
    fs: &dyn Fs,
    source: &std::fs::Metadata,
    destination: &Path,
) -> VResult<bool> {
    let destination = match fs.metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(super::ioerr("copy", destination, error)),
    };
    Ok(same_file_identity(source, &destination))
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    false
}

fn copy_relation_error(message: impl Into<String>) -> ErrorVal {
    ErrorVal::arg_error(message).with_hint("choose a destination outside the source tree")
}

fn map_directory_error(source: &Path, error: std::io::Error) -> ErrorVal {
    if error.kind() == std::io::ErrorKind::InvalidData {
        work_limit(format!(
            "recursive copy cannot admit every entry under {}: {error}",
            source.display()
        ))
    } else {
        super::ioerr("copy", source, error)
    }
}

fn work_limit(message: impl Into<String>) -> ErrorVal {
    ErrorVal::new("builtin_work_limit", message)
        .with_hint("copy a narrower tree or split the operation into bounded subtrees")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_value::StdFs;

    #[test]
    fn copy_plan_rejects_before_mutation_when_the_tree_exceeds_a_wall() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        std::fs::create_dir(&source).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(source.join(name), name).unwrap();
        }

        let error =
            CopyPlan::build_with_limits(&StdFs, &[(source, destination.clone())], true, 3, 4096, 8)
                .unwrap_err();
        assert_eq!(error.code, "builtin_work_limit");
        assert!(!destination.exists(), "planning must be effect-free");
    }

    #[test]
    fn copy_plan_executes_only_after_a_complete_recursive_inventory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("nested/file"), b"payload").unwrap();

        CopyPlan::build(&StdFs, &[(source, destination.clone())], true)
            .unwrap()
            .execute(&StdFs)
            .unwrap();
        assert_eq!(
            std::fs::read(destination.join("nested/file")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn multi_source_copy_preflights_every_source_before_the_first_effect() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let missing = root.path().join("missing");
        let destination = root.path().join("destination");
        std::fs::write(&source, b"payload").unwrap();
        std::fs::create_dir(&destination).unwrap();

        let error = super::super::copy_move(
            &StdFs,
            root.path(),
            vec![
                Value::Path(source),
                Value::Path(missing),
                Value::Path(destination.clone()),
            ],
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, "custom");
        assert!(
            !destination.join("source").exists(),
            "a later preflight failure must leave earlier sources untouched"
        );
    }

    #[test]
    fn copy_rejects_the_same_file_before_truncation() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("same");
        std::fs::write(&source, b"payload").unwrap();

        let error =
            CopyPlan::build(&StdFs, &[(source.clone(), source.clone())], false).unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("same file"));
        assert_eq!(std::fs::read(source).unwrap(), b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn copy_rejects_a_hard_link_alias() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let alias = root.path().join("alias");
        std::fs::write(&source, b"payload").unwrap();
        std::fs::hard_link(&source, &alias).unwrap();

        let error = CopyPlan::build(&StdFs, &[(source.clone(), alias)], false).unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("same file"));
        assert_eq!(std::fs::read(source).unwrap(), b"payload");
    }

    #[test]
    fn recursive_copy_rejects_a_missing_destination_inside_source() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = source.join("missing/../backup");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file"), b"payload").unwrap();

        let error =
            CopyPlan::build(&StdFs, &[(source.clone(), destination.clone())], true).unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("into itself"));
        assert!(!source.join("backup").exists());
    }

    #[cfg(unix)]
    #[test]
    fn recursive_copy_rejects_destination_through_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let alias = root.path().join("alias");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file"), b"payload").unwrap();
        symlink(&source, &alias).unwrap();
        let destination = alias.join("backup");

        let error = CopyPlan::build(&StdFs, &[(source.clone(), destination)], true).unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("into itself"));
        assert!(!source.join("backup").exists());
    }
}
