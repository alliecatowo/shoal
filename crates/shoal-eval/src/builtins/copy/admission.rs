//! Copy-plan resource accounting and cross-job destination admission.

use super::plan::{CopyOp, CopyPlan, PendingPath};
use super::work_limit;
use shoal_value::{ErrorVal, OpaqueHandling, RetainedLimits, VResult, Value, retained_size};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct AdmittedDestination {
    pub(super) requested: PathBuf,
    pub(super) canonical: PathBuf,
    pub(super) existing: Option<std::fs::Metadata>,
}

impl CopyPlan {
    pub(super) fn ensure_handle_capacity(&self, additional: usize) -> VResult<()> {
        if self
            .max_retained_handles
            .is_some_and(|limit| self.retained_handles.saturating_add(additional) > limit)
        {
            return Err(work_limit(format!(
                "recursive copy requires more retained descriptors than its {}-handle budget",
                self.max_retained_handles.unwrap_or(0)
            )));
        }
        Ok(())
    }

    pub(super) fn retain_handles(&mut self, additional: usize) -> VResult<()> {
        self.ensure_handle_capacity(additional)?;
        self.retained_handles = self
            .retained_handles
            .checked_add(additional)
            .ok_or_else(|| work_limit("recursive copy descriptor accounting overflowed"))?;
        Ok(())
    }

    pub(super) fn release_handles(&mut self, released: usize) {
        self.retained_handles = self
            .retained_handles
            .checked_sub(released)
            .expect("released copy handles were charged during admission");
    }

    pub(super) fn admit_pending(
        &mut self,
        pending: &mut Vec<PendingPath>,
        mut path: PendingPath,
    ) -> VResult<()> {
        if path.depth > self.max_depth {
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
        let retained = self.measure_paths(&path.source_path, Some(&path.destination))?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(retained)
            .ok_or_else(|| work_limit("recursive copy plan accounting overflowed"))?;
        path.retained_bytes = retained;
        pending.push(path);
        Ok(())
    }

    pub(super) fn admit(&mut self, operation: CopyOp) -> VResult<()> {
        if self.operations.len() >= self.max_operations {
            return Err(work_limit(format!(
                "recursive copy reached its {}-operation limit",
                self.max_operations
            )));
        }
        let retained = match &operation {
            CopyOp::CreateDir { destination, .. } => self.measure_paths(destination, None)?,
            CopyOp::CopyFile {
                source_path,
                destination,
                ..
            } => self.measure_paths(source_path, Some(destination))?,
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
        retained_size(
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
        })
    }
}

pub(super) fn validate_destination_job_conflicts(
    admitted: &[AdmittedDestination],
    candidate: &AdmittedDestination,
) -> VResult<()> {
    let candidate_path = normalize_path(&candidate.canonical);
    for prior in admitted {
        let prior_path = normalize_path(&prior.canonical);
        let path_overlap = candidate_path == prior_path
            || candidate_path.starts_with(&prior_path)
            || prior_path.starts_with(&candidate_path);
        let identity_alias = prior.existing.as_ref().is_some_and(|left| {
            candidate
                .existing
                .as_ref()
                .is_some_and(|right| same_file_identity(left, right))
        });
        if path_overlap || identity_alias {
            return Err(ErrorVal::arg_error(format!(
                "copy destinations overlap or alias: {} and {}",
                prior.requested.display(),
                candidate.requested.display()
            ))
            .with_hint("choose distinct destination trees for every source"));
        }
    }
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    false
}
