//! Pinned, fd-relative source capabilities for recursive copy.

use std::ffi::OsString;
use std::fmt::Debug;
use std::fs;
use std::io;
use std::path::Path;

/// A child admitted from a pinned directory descriptor.
#[derive(Debug)]
pub struct FsCopyChild {
    pub name: OsString,
    pub source: Box<dyn FsCopySource>,
}

/// An opened source object retained from recursive-copy inventory through
/// execution. Implementations must never resolve descendants through the
/// original ambient pathname.
pub trait FsCopySource: Debug + Send {
    /// Number of operating-system handles retained exclusively by this
    /// capability. Shared directory roots must be charged by their owning
    /// destination capability instead of every descendant view.
    fn retained_handle_count(&self) -> usize {
        0
    }
    /// Current canonical path of the retained object, obtained from the open
    /// descriptor rather than by resolving the caller's ambient pathname.
    fn canonical_path(&self) -> io::Result<std::path::PathBuf>;
    /// Metadata for the opened object itself.
    fn metadata(&self) -> io::Result<fs::Metadata>;
    /// Whether the opened object carries non-portable extended attributes.
    fn has_extended_attributes(&self) -> io::Result<bool>;
    /// Open every admitted child relative to this retained directory.
    fn children_limited(
        &self,
        max_entries: usize,
        max_name_bytes: usize,
    ) -> io::Result<Vec<FsCopyChild>>;
    /// Copy this retained regular file into an already-opened destination.
    fn copy_contents(&self, destination: &mut dyn io::Write) -> io::Result<u64>;
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
mod unix {
    use super::{FsCopyChild, FsCopySource};
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::{self, File};
    use std::io::{self, Seek as _, SeekFrom};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    use std::path::Path;

    #[derive(Debug)]
    struct UnixCopySource {
        file: File,
    }

    impl UnixCopySource {
        fn from_fd(fd: RawFd) -> io::Result<Self> {
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                // SAFETY: a successful open/openat returns a new owned fd.
                Ok(Self {
                    file: unsafe { File::from_raw_fd(fd) },
                })
            }
        }

        fn open_child(&self, name: &OsStr) -> io::Result<Self> {
            let name = cstring(name)?;
            // O_NONBLOCK prevents a raced-in FIFO from hanging admission.
            // fstat via `metadata` subsequently rejects every special type.
            // SAFETY: the name is NUL-terminated and the retained fd is live.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                )
            };
            Self::from_fd(fd).map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "symbolic link appeared while admitting {}",
                            name.to_string_lossy()
                        ),
                    )
                } else {
                    error
                }
            })
        }
    }

    impl FsCopySource for UnixCopySource {
        fn retained_handle_count(&self) -> usize {
            1
        }

        fn canonical_path(&self) -> io::Result<std::path::PathBuf> {
            descriptor_path(self.file.as_raw_fd())
        }

        fn metadata(&self) -> io::Result<fs::Metadata> {
            self.file.metadata()
        }

        fn has_extended_attributes(&self) -> io::Result<bool> {
            has_extended_attributes(self.file.as_raw_fd())
        }

        fn children_limited(
            &self,
            max_entries: usize,
            max_name_bytes: usize,
        ) -> io::Result<Vec<FsCopyChild>> {
            if !self.file.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "copy source is not a directory",
                ));
            }
            // fdopendir takes ownership, so give it a duplicate. The original
            // descriptor remains the stable base for every openat below.
            // SAFETY: fcntl duplicates a live fd and sets close-on-exec.
            let duplicate = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if duplicate < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: duplicate is an owned directory fd on this path.
            let directory = unsafe { libc::fdopendir(duplicate) };
            if directory.is_null() {
                let error = io::Error::last_os_error();
                // SAFETY: fdopendir did not take ownership on failure.
                unsafe { libc::close(duplicate) };
                return Err(error);
            }

            let result = self.collect_children(directory, max_entries, max_name_bytes);
            // SAFETY: directory is a successful fdopendir result and uniquely
            // owns the duplicate descriptor.
            let close_status = unsafe { libc::closedir(directory) };
            if let Err(error) = result {
                return Err(error);
            }
            if close_status != 0 {
                return Err(io::Error::last_os_error());
            }
            result
        }

        fn copy_contents(&self, destination: &mut dyn io::Write) -> io::Result<u64> {
            let before = self.file.metadata()?;
            if !before.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "copy source is not a regular file",
                ));
            }
            let mut source = self.file.try_clone()?;
            source.seek(SeekFrom::Start(0))?;
            let copied = io::copy(&mut source, destination)?;
            let after = self.file.metadata()?;
            if copied != before.len() || !stable_file_metadata(&before, &after) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "copy source changed while its contents were being read",
                ));
            }
            Ok(copied)
        }
    }

    fn stable_file_metadata(before: &fs::Metadata, after: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }

    impl UnixCopySource {
        fn collect_children(
            &self,
            directory: *mut libc::DIR,
            max_entries: usize,
            max_name_bytes: usize,
        ) -> io::Result<Vec<FsCopyChild>> {
            let mut children = Vec::new();
            let mut retained_name_bytes = 0usize;
            loop {
                clear_errno();
                // SAFETY: directory remains live and is used on this thread.
                let entry = unsafe { libc::readdir(directory) };
                if entry.is_null() {
                    let error = current_errno();
                    return if error == 0 {
                        Ok(children)
                    } else {
                        Err(io::Error::from_raw_os_error(error))
                    };
                }
                // SAFETY: readdir returned a live dirent whose d_name is a
                // NUL-terminated array for the duration of this iteration.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                if children.len() >= max_entries {
                    return Err(limit_error(format!(
                        "directory contains more than {max_entries} admitted entries"
                    )));
                }
                retained_name_bytes = retained_name_bytes
                    .checked_add(name.len())
                    .ok_or_else(|| limit_error("directory name accounting overflowed"))?;
                if retained_name_bytes > max_name_bytes {
                    return Err(limit_error(format!(
                        "directory names exceed {max_name_bytes} admitted bytes"
                    )));
                }
                let name = OsString::from_vec(name.to_vec());
                let source = self.open_child(&name)?;
                children.push(FsCopyChild {
                    name,
                    source: Box::new(source),
                });
            }
        }
    }

    pub(super) fn open(path: &Path) -> io::Result<Box<dyn FsCopySource>> {
        let path = cstring(path.as_os_str())?;
        // O_NONBLOCK closes the special-file admission hang described above.
        // SAFETY: path is NUL-terminated and these flags require no mode.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        UnixCopySource::from_fd(fd).map(|source| Box::new(source) as Box<dyn FsCopySource>)
    }

    pub(super) fn retained_handle_budget() -> io::Result<Option<usize>> {
        let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: `limit` points to writable storage for one rlimit value.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful getrlimit initialized `limit`.
        let soft = unsafe { limit.assume_init() }.rlim_cur;
        if soft == libc::RLIM_INFINITY {
            return Ok(None);
        }
        let soft = usize::try_from(soft).unwrap_or(usize::MAX);
        let descriptor_directory = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        // The iterator's own descriptor is intentionally counted. It closes
        // before admission, leaving one extra slot in the safety reserve.
        let open = std::fs::read_dir(descriptor_directory)?.try_fold(0usize, |count, entry| {
            entry.map(|_| count.saturating_add(1))
        })?;
        const TRANSIENT_HEADROOM: usize = 32;
        Ok(Some(
            soft.saturating_sub(open).saturating_sub(TRANSIENT_HEADROOM),
        ))
    }

    fn cstring(value: &OsStr) -> io::Result<CString> {
        CString::new(value.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem path contains an interior NUL",
            )
        })
    }

    #[cfg(target_os = "linux")]
    fn descriptor_path(fd: RawFd) -> io::Result<std::path::PathBuf> {
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
    }

    #[cfg(target_vendor = "apple")]
    fn descriptor_path(fd: RawFd) -> io::Result<std::path::PathBuf> {
        let mut bytes = vec![0_u8; libc::PATH_MAX as usize];
        // SAFETY: `bytes` is writable for PATH_MAX bytes and fd is live.
        let status = unsafe { libc::fcntl(fd, libc::F_GETPATH, bytes.as_mut_ptr()) };
        if status < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        bytes.truncate(length);
        Ok(std::path::PathBuf::from(OsString::from_vec(bytes)))
    }

    fn limit_error(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.into())
    }

    #[cfg(target_os = "linux")]
    fn has_extended_attributes(fd: RawFd) -> io::Result<bool> {
        // SAFETY: a null buffer with length zero requests only the list size.
        let count = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
        if count <= 0 {
            return xattr_count_result(count);
        }
        if count as usize > 64 * 1024 {
            return Ok(true);
        }
        let mut names = vec![0u8; count as usize];
        // SAFETY: names is writable for its advertised length and fd is live.
        let read = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len()) };
        if read < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ERANGE) {
                Ok(true)
            } else {
                Err(error)
            };
        }
        names.truncate(read as usize);
        Ok(names
            .split(|byte| *byte == 0)
            .any(|name| !name.is_empty() && name != b"security.selinux"))
    }

    #[cfg(target_vendor = "apple")]
    fn has_extended_attributes(fd: RawFd) -> io::Result<bool> {
        // SAFETY: a null buffer with length zero requests only the list size.
        let count = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
        xattr_count_result(count)
    }

    fn xattr_count_result(count: isize) -> io::Result<bool> {
        if count >= 0 {
            Ok(count != 0)
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOTSUP) {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn clear_errno() {
        // SAFETY: errno location is thread-local and writable.
        unsafe { *libc::__errno_location() = 0 };
    }

    #[cfg(target_os = "linux")]
    fn current_errno() -> i32 {
        // SAFETY: errno location is thread-local and readable.
        unsafe { *libc::__errno_location() }
    }

    #[cfg(target_vendor = "apple")]
    fn clear_errno() {
        // SAFETY: errno location is thread-local and writable.
        unsafe { *libc::__error() = 0 };
    }

    #[cfg(target_vendor = "apple")]
    fn current_errno() -> i32 {
        // SAFETY: errno location is thread-local and readable.
        unsafe { *libc::__error() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct MutatingWriter {
            source: std::path::PathBuf,
            bytes: Vec<u8>,
            mutated: bool,
        }

        impl io::Write for MutatingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                if !self.mutated {
                    self.mutated = true;
                    fs::OpenOptions::new()
                        .append(true)
                        .open(&self.source)?
                        .write_all(b" raced")?;
                }
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn copy_contents_rejects_in_place_source_mutation() {
            let unique = format!(
                "shoal-copy-source-stability-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("test")
            );
            let path = std::env::temp_dir().join(unique);
            fs::write(&path, b"admitted").unwrap();
            let source = open(&path).unwrap();
            let mut destination = MutatingWriter {
                source: path.clone(),
                bytes: Vec::new(),
                mutated: false,
            };

            let error = source.copy_contents(&mut destination).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("changed"));
            drop(source);
            fs::remove_file(path).unwrap();
        }
    }
}

pub(crate) fn open(path: &Path) -> io::Result<Box<dyn FsCopySource>> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        unix::open(path)
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "pinned recursive-copy sources require openat/fdopendir support",
        ))
    }
}

pub(crate) fn retained_handle_budget() -> io::Result<Option<usize>> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        unix::retained_handle_budget()
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        Ok(None)
    }
}
