//! Source/destination relationship validation and copy-specific error mapping.

use super::policy::inspect_source;
use super::work_limit;
use shoal_value::{ErrorVal, FsCopyDestination, FsCopySource, FsCopyTarget, VResult};
use std::path::Path;
/// Refuse aliases of the same file and recursive destinations inside their
/// source before inventory or execution. Both paths come from retained
/// descriptors/capabilities rather than a second ambient pathname lookup.
pub(super) fn validate_root_job(
    source: &Path,
    destination: &Path,
    pinned: &dyn FsCopySource,
    destination_root: &dyn FsCopyDestination,
    destination_target: &dyn FsCopyTarget,
) -> VResult<()> {
    let source_metadata = pinned
        .metadata()
        .map_err(|error| super::super::ioerr("copy", source, error))?;
    inspect_source(pinned, source, &source_metadata)?;
    let canonical_source = pinned
        .canonical_path()
        .map_err(|error| super::super::ioerr("copy", source, error))?;
    let canonical_destination = destination_root
        .canonical_path()
        .map_err(|error| super::super::ioerr("copy", destination, error))?;

    if canonical_source == canonical_destination
        || existing_files_are_identical(&source_metadata, destination_target)
            .map_err(|error| super::super::ioerr("copy", destination, error))?
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

fn existing_files_are_identical(
    source: &std::fs::Metadata,
    destination: &dyn FsCopyTarget,
) -> std::io::Result<bool> {
    destination
        .metadata()
        .map(|destination| destination.is_some_and(|entry| same_file_identity(source, &entry)))
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

fn copy_relation_error(message: impl Into<String>) -> ErrorVal {
    ErrorVal::arg_error(message).with_hint("choose a destination outside the source tree")
}

pub(super) fn map_directory_error(source: &Path, error: std::io::Error) -> ErrorVal {
    if descriptor_limit(&error) {
        work_limit(format!(
            "recursive copy exhausted its admitted descriptor budget under {}",
            source.display()
        ))
    } else if error.kind() == std::io::ErrorKind::InvalidData {
        work_limit(format!(
            "recursive copy cannot admit every entry under {}: {error}",
            source.display()
        ))
    } else if error.kind() == std::io::ErrorKind::InvalidInput
        && error.to_string().contains("symbolic link")
    {
        ErrorVal::arg_error(format!(
            "cp: portable recursive copy refuses symbolic links under {}",
            source.display()
        ))
        .with_hint("copy only ordinary files/directories after removing symbolic links")
    } else if error.kind() == std::io::ErrorKind::Unsupported {
        unsupported_capability_error(source, error)
    } else {
        super::super::ioerr("copy", source, error)
    }
}

pub(super) fn map_source_open_error(source: &Path, error: std::io::Error) -> ErrorVal {
    if descriptor_limit(&error) {
        work_limit(format!(
            "recursive copy cannot retain another source descriptor at {}",
            source.display()
        ))
    } else if error.raw_os_error() == Some(libc::ELOOP) {
        ErrorVal::arg_error(format!(
            "cp: portable recursive copy refuses symbolic links at {}",
            source.display()
        ))
        .with_hint("copy only ordinary files/directories after removing symbolic links")
    } else if error.kind() == std::io::ErrorKind::Unsupported {
        unsupported_capability_error(source, error)
    } else {
        super::super::ioerr("copy", source, error)
    }
}

pub(super) fn map_destination_open_error(destination: &Path, error: std::io::Error) -> ErrorVal {
    if descriptor_limit(&error) {
        work_limit(format!(
            "recursive copy cannot retain another destination descriptor at {}",
            destination.display()
        ))
    } else if error.raw_os_error() == Some(libc::ELOOP)
        || error.kind() == std::io::ErrorKind::NotADirectory
    {
        ErrorVal::arg_error(format!(
            "cp: portable recursive copy refuses a symbolic-link or non-directory ancestor at {}",
            destination.display()
        ))
        .with_hint("choose a destination whose ancestors are ordinary directories")
    } else if error.kind() == std::io::ErrorKind::Unsupported {
        unsupported_capability_error(destination, error)
    } else {
        super::super::ioerr("copy", destination, error)
    }
}

pub(super) fn map_copy_execution_error(source: &Path, error: std::io::Error) -> ErrorVal {
    if descriptor_limit(&error) {
        work_limit(format!(
            "recursive copy has insufficient descriptor headroom to publish {}",
            source.display()
        ))
    } else if error.kind() == std::io::ErrorKind::Unsupported {
        unsupported_capability_error(source, error)
    } else {
        super::super::ioerr("copy", source, error)
    }
}

fn descriptor_limit(error: &std::io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| code == libc::EMFILE || code == libc::ENFILE)
}

fn unsupported_capability_error(source: &Path, error: std::io::Error) -> ErrorVal {
    ErrorVal::new(
        "unsupported",
        format!(
            "copy {} requires a pinned recursive-copy capability: {error}",
            source.display()
        ),
    )
}
