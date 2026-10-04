//! Portable node/metadata admission for `cp`.

use super::*;

pub(super) struct PortableSource {
    pub(super) is_dir: bool,
    pub(super) permissions: std::fs::Permissions,
}

pub(super) fn inspect_source(
    fs: &dyn Fs,
    path: &Path,
    metadata: &std::fs::Metadata,
) -> VResult<PortableSource> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(unsupported_source(
            path,
            "symbolic links (including broken links)",
        ));
    }
    let is_dir = metadata.is_dir();
    if !is_dir && !metadata.is_file() {
        return Err(unsupported_source(
            path,
            "special files such as FIFOs, sockets, and device nodes",
        ));
    }
    if fs
        .has_extended_attributes(path)
        .map_err(|error| super::super::ioerr("copy metadata", path, error))?
    {
        return Err(unsupported_source(path, "extended attributes"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if metadata.mode() & 0o7000 != 0 {
            return Err(unsupported_source(
                path,
                "setuid, setgid, or sticky permission bits",
            ));
        }
        if !is_dir && metadata.len() > 0 && metadata.blocks().saturating_mul(512) < metadata.len() {
            return Err(unsupported_source(path, "sparse file allocation"));
        }
        Ok(PortableSource {
            is_dir,
            permissions: std::fs::Permissions::from_mode(metadata.mode() & 0o777),
        })
    }
    #[cfg(not(unix))]
    Ok(PortableSource {
        is_dir,
        permissions: metadata.permissions(),
    })
}

pub(super) fn validate_destination(fs: &dyn Fs, path: &Path, source_is_dir: bool) -> VResult<()> {
    let metadata = match fs.symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(super::super::ioerr("copy", path, error)),
    };
    if metadata.file_type().is_symlink() {
        return Err(unsupported_destination(path, "a symbolic link"));
    }
    if source_is_dir != metadata.is_dir() || (!source_is_dir && !metadata.is_file()) {
        return Err(unsupported_destination(
            path,
            "a different or special filesystem node type",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if !source_is_dir && metadata.nlink() > 1 {
            return Err(unsupported_destination(path, "a hard-linked file alias"));
        }
    }
    if fs
        .has_extended_attributes(path)
        .map_err(|error| super::super::ioerr("copy metadata", path, error))?
    {
        return Err(unsupported_destination(path, "extended attributes"));
    }
    Ok(())
}

fn unsupported_source(path: &Path, feature: &str) -> ErrorVal {
    ErrorVal::arg_error(format!(
        "cp: portable recursive copy refuses {feature} at {}",
        path.display()
    ))
    .with_hint("copy only ordinary files/directories after explicitly converting metadata")
}

fn unsupported_destination(path: &Path, feature: &str) -> ErrorVal {
    ErrorVal::arg_error(format!(
        "cp: portable recursive copy refuses destination {} because it is {feature}",
        path.display()
    ))
    .with_hint("choose a metadata-free ordinary destination path")
}
