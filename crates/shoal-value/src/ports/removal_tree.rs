//! Bounded, pinned, fd-relative permanent recursive removal.

use super::FsEntryIdentity;
use std::fmt::Debug;
use std::io;
use std::path::Path;

/// A directory tree admitted before mutation and anchored by an open root
/// directory descriptor until permanent deletion commits.
pub trait FsRemovalTree: Debug + Send {
    /// Validate the complete admitted tree at its quarantined name, then
    /// remove its descendants relative to retained directory descriptors.
    fn remove(&self, quarantined: &Path) -> io::Result<()>;
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
mod unix {
    use super::{FsEntryIdentity, FsRemovalTree};
    use std::collections::HashSet;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    use std::path::Path;

    // These files are included into one private platform module so the low-level
    // ownership types stay sealed while each security responsibility has one owner.
    include!("removal_tree/model.rs");
    include!("removal_tree/inventory.rs");
    include!("removal_tree/mutation.rs");
    include!("removal_tree/syscalls.rs");
    include!("removal_tree/entry.rs");

    #[cfg(test)]
    mod tests {
        include!("removal_tree/unix/tests.rs");
    }
}

pub(crate) fn open(path: &Path, expected: &FsEntryIdentity) -> io::Result<Box<dyn FsRemovalTree>> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        unix::open(path, expected)
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let _ = (path, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "pinned recursive removal requires openat/fdopendir/unlinkat support",
        ))
    }
}

pub(crate) fn remove_leaf(path: &Path, expected: &FsEntryIdentity) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        unix::remove_leaf(path, expected)
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let _ = (path, expected);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "conditional leaf removal requires fd-relative no-replace rename support",
        ))
    }
}
