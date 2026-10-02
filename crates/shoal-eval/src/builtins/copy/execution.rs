//! Execution of a completely admitted copy plan.

use super::path_policy::map_copy_execution_error;
use super::plan::{CopyOp, CopyPlan};
use shoal_value::{Fs, VResult};

impl CopyPlan {
    pub(in crate::builtins) fn execute(self, _fs: &dyn Fs) -> VResult<()> {
        let mut directories = Vec::new();
        for operation in self.operations {
            match operation {
                CopyOp::CreateDir {
                    destination,
                    target,
                    permissions,
                } => {
                    target
                        .create_directory()
                        .map_err(|error| super::super::ioerr("copy", &destination, error))?;
                    directories.push((destination, target, permissions));
                }
                CopyOp::CopyFile {
                    source_path,
                    source,
                    destination: _,
                    target,
                    permissions,
                } => {
                    target
                        .copy_from(source.as_ref(), permissions)
                        .map_err(|error| map_copy_execution_error(&source_path, error))?;
                }
            }
        }
        // Apply directory modes deepest-first only after children exist. A
        // read-only source directory must not make its destination
        // unpopulatable halfway through execution.
        for (destination, target, permissions) in directories.into_iter().rev() {
            target
                .set_permissions(permissions)
                .map_err(|error| super::super::ioerr("copy", &destination, error))?;
        }
        Ok(())
    }
}
