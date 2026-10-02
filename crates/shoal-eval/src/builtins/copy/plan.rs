//! Effect-free copy inventory and the retained-capability plan model.

use super::admission::AdmittedDestination;
use super::path_policy::{
    map_destination_open_error, map_directory_error, map_source_open_error, validate_root_job,
};
use super::policy::{inspect_source, validate_destination};
use super::{MAX_COPY_DEPTH, MAX_RETAINED_BYTES, MAX_VALUES, work_limit};
use shoal_value::{Fs, FsCopyDestination, FsCopySource, FsCopyTarget, VResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub(super) enum CopyOp {
    CreateDir {
        destination: PathBuf,
        target: Box<dyn FsCopyTarget>,
        permissions: std::fs::Permissions,
    },
    CopyFile {
        source_path: PathBuf,
        source: Box<dyn FsCopySource>,
        destination: PathBuf,
        target: Box<dyn FsCopyTarget>,
        permissions: std::fs::Permissions,
    },
}

#[derive(Debug)]
pub(super) struct PendingPath {
    pub(super) source_path: PathBuf,
    pub(super) source: Box<dyn FsCopySource>,
    pub(super) destination: PathBuf,
    pub(super) destination_root: Arc<dyn FsCopyDestination>,
    pub(super) destination_relative: PathBuf,
    pub(super) target: Box<dyn FsCopyTarget>,
    pub(super) depth: usize,
    pub(super) retained_bytes: usize,
}

#[derive(Debug)]
pub(in crate::builtins) struct CopyPlan {
    pub(super) operations: Vec<CopyOp>,
    pub(super) retained_bytes: usize,
    pub(super) max_operations: usize,
    pub(super) max_retained_bytes: usize,
    pub(super) max_depth: usize,
    pub(super) retained_handles: usize,
    pub(super) max_retained_handles: Option<usize>,
}

impl CopyPlan {
    pub(in crate::builtins) fn build(
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

    pub(super) fn build_with_limits(
        fs: &dyn Fs,
        jobs: &[(PathBuf, PathBuf)],
        recursive: bool,
        max_operations: usize,
        max_retained_bytes: usize,
        max_depth: usize,
    ) -> VResult<Self> {
        let max_retained_handles = fs.copy_capability_budget().map_err(|error| {
            work_limit(format!(
                "recursive copy cannot determine its descriptor budget: {error}"
            ))
        })?;
        Self::build_with_all_limits(
            fs,
            jobs,
            recursive,
            max_operations,
            max_retained_bytes,
            max_depth,
            max_retained_handles,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_with_all_limits(
        fs: &dyn Fs,
        jobs: &[(PathBuf, PathBuf)],
        recursive: bool,
        max_operations: usize,
        max_retained_bytes: usize,
        max_depth: usize,
        max_retained_handles: Option<usize>,
    ) -> VResult<Self> {
        let mut plan = Self {
            operations: Vec::new(),
            retained_bytes: 0,
            max_operations,
            max_retained_bytes,
            max_depth,
            retained_handles: 0,
            max_retained_handles,
        };
        let mut pending = Vec::new();
        let mut admitted_destinations = Vec::new();
        // This is a LIFO work stack. Reverse initial jobs and sorted children
        // so the resulting operation order remains caller/lexical order.
        for (source, destination) in jobs.iter().rev() {
            // Source, destination root, and a possibly-existing final target
            // can each own one retained descriptor. Check the conservative
            // worst case before opening the first capability for this job.
            plan.ensure_handle_capacity(3)?;
            let pinned = fs
                .open_copy_source(source)
                .map_err(|error| map_source_open_error(source, error))?;
            plan.retain_handles(pinned.retained_handle_count())?;
            let destination_root: Arc<dyn FsCopyDestination> = fs
                .open_copy_destination(destination)
                .map_err(|error| map_destination_open_error(destination, error))?
                .into();
            plan.retain_handles(destination_root.retained_handle_count())?;
            let target = destination_root
                .open_target(Path::new(""))
                .map_err(|error| map_destination_open_error(destination, error))?;
            plan.retain_handles(target.retained_handle_count())?;
            validate_root_job(
                source,
                destination,
                pinned.as_ref(),
                destination_root.as_ref(),
                target.as_ref(),
            )?;
            let admitted = AdmittedDestination {
                requested: destination.clone(),
                canonical: destination_root
                    .canonical_path()
                    .map_err(|error| map_destination_open_error(destination, error))?,
                existing: target
                    .metadata()
                    .map_err(|error| map_destination_open_error(destination, error))?,
            };
            super::admission::validate_destination_job_conflicts(
                &admitted_destinations,
                &admitted,
            )?;
            admitted_destinations.push(admitted);
            plan.admit_pending(
                &mut pending,
                PendingPath {
                    source_path: source.clone(),
                    source: pinned,
                    destination: destination.clone(),
                    destination_root: Arc::clone(&destination_root),
                    destination_relative: PathBuf::new(),
                    target,
                    depth: 0,
                    retained_bytes: 0,
                },
            )?;
        }
        while let Some(path) = pending.pop() {
            plan.retained_bytes = plan
                .retained_bytes
                .checked_sub(path.retained_bytes)
                .expect("pending path charge is owned by the plan");
            plan.visit(&mut pending, path, recursive)?;
        }
        Ok(plan)
    }

    fn visit(
        &mut self,
        pending: &mut Vec<PendingPath>,
        path: PendingPath,
        recursive: bool,
    ) -> VResult<()> {
        let metadata = path
            .source
            .metadata()
            .map_err(|error| super::super::ioerr("copy", &path.source_path, error))?;
        let portable = inspect_source(path.source.as_ref(), &path.source_path, &metadata)?;
        if portable.is_dir {
            let source_handles = path.source.retained_handle_count();
            if !recursive {
                return Err(shoal_value::ErrorVal::arg_error(
                    "cp: directory requires --recursive",
                ));
            }
            validate_destination(path.target.as_ref(), &path.destination, true)?;
            self.admit(CopyOp::CreateDir {
                destination: path.destination.clone(),
                target: path.target,
                permissions: portable.permissions,
            })?;
            let remaining = self
                .max_operations
                .saturating_sub(self.operations.len())
                .saturating_sub(pending.len());
            let handle_remaining = self.max_retained_handles.map_or(usize::MAX, |limit| {
                limit.saturating_sub(self.retained_handles)
            });
            let mut entries = path
                .source
                .children_limited(
                    remaining.min(handle_remaining),
                    self.max_retained_bytes.saturating_sub(self.retained_bytes),
                )
                .map_err(|error| map_directory_error(&path.source_path, error))?;
            let child_handles = entries.iter().try_fold(0usize, |count, entry| {
                count
                    .checked_add(entry.source.retained_handle_count())
                    .ok_or_else(|| work_limit("recursive copy descriptor accounting overflowed"))
            })?;
            self.retain_handles(child_handles)?;
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            // Descendants are independently pinned now; the directory handle
            // itself is no longer needed by this plan.
            drop(path.source);
            self.release_handles(source_handles);
            for entry in entries.into_iter().rev() {
                let child_depth = path.depth.checked_add(1).ok_or_else(|| {
                    work_limit("recursive copy directory depth accounting overflowed")
                })?;
                let child_path = path.source_path.join(&entry.name);
                let child_destination = path.destination.join(&entry.name);
                let child_relative = path.destination_relative.join(&entry.name);
                let child_target = path
                    .destination_root
                    .open_target(&child_relative)
                    .map_err(|error| map_destination_open_error(&child_destination, error))?;
                self.retain_handles(child_target.retained_handle_count())?;
                self.admit_pending(
                    pending,
                    PendingPath {
                        source_path: child_path,
                        source: entry.source,
                        destination: child_destination,
                        destination_root: Arc::clone(&path.destination_root),
                        destination_relative: child_relative,
                        target: child_target,
                        depth: child_depth,
                        retained_bytes: 0,
                    },
                )?;
            }
        } else {
            validate_destination(path.target.as_ref(), &path.destination, false)?;
            self.admit(CopyOp::CopyFile {
                source_path: path.source_path,
                source: path.source,
                destination: path.destination,
                target: path.target,
                permissions: portable.permissions,
            })?;
        }
        Ok(())
    }
}
