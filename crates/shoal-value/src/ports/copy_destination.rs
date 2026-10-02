//! Pinned, fd-relative destination capabilities for recursive copy.

use super::FsCopySource;
use std::fmt::Debug;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A destination tree pinned at its deepest existing directory.
pub trait FsCopyDestination: Debug + Send + Sync {
    /// Number of unique operating-system handles retained by this root.
    fn retained_handle_count(&self) -> usize {
        0
    }
    /// Canonical destination root derived from the retained directory.
    fn canonical_path(&self) -> io::Result<PathBuf>;
    /// Admit one root-relative target and retain its observed route/identity.
    fn open_target(&self, relative: &Path) -> io::Result<Box<dyn FsCopyTarget>>;
}

/// One preflighted destination entry. Mutation must fail if any existing
/// route component or final entry differs from the admitted identity.
pub trait FsCopyTarget: Debug + Send {
    /// Number of handles retained only by this target, excluding a shared
    /// destination-root handle.
    fn retained_handle_count(&self) -> usize {
        0
    }
    /// Metadata for an existing final entry, or `None` when it was absent.
    fn metadata(&self) -> io::Result<Option<fs::Metadata>>;
    /// Inspect portable extended attributes through the retained final fd.
    fn has_extended_attributes(&self) -> io::Result<bool>;
    /// Create (or validate) this directory relative to the pinned root.
    fn create_directory(&self) -> io::Result<()>;
    /// Atomically publish this retained source file at the admitted entry.
    fn copy_from(&self, source: &dyn FsCopySource, permissions: fs::Permissions)
    -> io::Result<u64>;
    /// Apply directory permissions through the verified opened descriptor.
    fn set_permissions(&self, permissions: fs::Permissions) -> io::Result<()>;
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
mod unix {
    use super::{FsCopyDestination, FsCopySource, FsCopyTarget};
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr, OsString};
    use std::fs::{self, File};
    use std::io::{self, Write as _};
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
    use std::os::unix::ffi::OsStrExt as _;
    #[cfg(target_vendor = "apple")]
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Component, Path, PathBuf};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Identity {
        device: u64,
        inode: u64,
        file_type: u32,
    }

    impl Identity {
        #[allow(clippy::unnecessary_cast)]
        fn from_stat(stat: &libc::stat) -> Self {
            Self {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
                file_type: stat.st_mode as u32 & libc::S_IFMT as u32,
            }
        }

        fn is_directory(self) -> bool {
            self.file_type == libc::S_IFDIR
        }
    }

    #[derive(Debug, Clone)]
    struct RouteStep {
        name: OsString,
        expected: Option<Identity>,
    }

    #[derive(Debug)]
    struct UnixCopyDestination {
        base: Arc<File>,
        base_path: PathBuf,
        root: Vec<OsString>,
        created: Arc<Mutex<HashMap<Vec<OsString>, Identity>>>,
    }

    #[derive(Debug)]
    struct UnixCopyTarget {
        base: Arc<File>,
        route: Vec<RouteStep>,
        existing: Option<File>,
        created: Arc<Mutex<HashMap<Vec<OsString>, Identity>>>,
    }

    impl FsCopyDestination for UnixCopyDestination {
        fn retained_handle_count(&self) -> usize {
            1
        }

        fn canonical_path(&self) -> io::Result<PathBuf> {
            let mut path = self.base_path.clone();
            for name in &self.root {
                path.push(name);
            }
            Ok(path)
        }

        fn open_target(&self, relative: &Path) -> io::Result<Box<dyn FsCopyTarget>> {
            let mut names = self.root.clone();
            names.extend(relative_names(relative)?);
            if names.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "copy destination has no final component",
                ));
            }
            let (route, existing) = admit_route(self.base.as_ref(), &names)?;
            Ok(Box::new(UnixCopyTarget {
                base: Arc::clone(&self.base),
                route,
                existing,
                created: Arc::clone(&self.created),
            }))
        }
    }

    impl FsCopyTarget for UnixCopyTarget {
        fn retained_handle_count(&self) -> usize {
            usize::from(self.existing.is_some())
        }

        fn metadata(&self) -> io::Result<Option<fs::Metadata>> {
            self.existing.as_ref().map(File::metadata).transpose()
        }

        fn has_extended_attributes(&self) -> io::Result<bool> {
            self.existing
                .as_ref()
                .map_or(Ok(false), |file| has_extended_attributes(file.as_raw_fd()))
        }

        fn create_directory(&self) -> io::Result<()> {
            self.open_final_directory(true).map(drop)
        }

        fn copy_from(
            &self,
            source: &dyn FsCopySource,
            permissions: fs::Permissions,
        ) -> io::Result<u64> {
            let (parents, leaf) = self.route.split_at(self.route.len() - 1);
            let parent = self.walk_directories(parents, false)?;
            let leaf = &leaf[0];
            let leaf_name = cstring(&leaf.name)?;
            let (mut temporary, temporary_name) = create_temporary(parent.as_raw_fd())?;
            let result = (|| {
                let copied = source.copy_contents(&mut temporary)?;
                temporary.flush()?;
                set_mode(temporary.as_raw_fd(), permissions.mode() & 0o777)?;
                temporary.sync_all()?;
                publish(
                    parent.as_raw_fd(),
                    &temporary_name,
                    &leaf_name,
                    leaf.expected,
                )?;
                Ok(copied)
            })();
            if result.is_err() {
                let _ = unlink(parent.as_raw_fd(), &temporary_name);
            }
            result
        }

        fn set_permissions(&self, permissions: fs::Permissions) -> io::Result<()> {
            let directory = self.open_final_directory(false)?;
            set_mode(directory.as_raw_fd(), permissions.mode() & 0o777)
        }
    }

    impl UnixCopyTarget {
        fn open_final_directory(&self, create: bool) -> io::Result<File> {
            self.walk_directories(&self.route, create)
        }

        fn walk_directories(&self, steps: &[RouteStep], create: bool) -> io::Result<File> {
            let mut directory = self.base.try_clone()?;
            let mut prefix = Vec::new();
            for step in steps {
                prefix.push(step.name.clone());
                let name = cstring(&step.name)?;
                directory = match step.expected {
                    Some(expected) => {
                        open_expected_directory(directory.as_raw_fd(), &name, expected)?
                    }
                    None => {
                        let created = self
                            .created
                            .lock()
                            .map_err(|_| changed("copy destination creation state was poisoned"))?
                            .get(&prefix)
                            .copied();
                        if let Some(expected) = created {
                            open_expected_directory(directory.as_raw_fd(), &name, expected)?
                        } else if create {
                            mkdir(directory.as_raw_fd(), &name)?;
                            let child = open_directory(directory.as_raw_fd(), &name)?;
                            let identity = Identity::from_stat(&stat_fd(child.as_raw_fd())?);
                            self.created
                                .lock()
                                .map_err(|_| {
                                    changed("copy destination creation state was poisoned")
                                })?
                                .insert(prefix.clone(), identity);
                            child
                        } else {
                            return Err(changed(format!(
                                "destination directory {} was not created by this copy plan",
                                step.name.to_string_lossy()
                            )));
                        }
                    }
                };
            }
            Ok(directory)
        }
    }

    pub(super) fn open(path: &Path) -> io::Result<Box<dyn FsCopyDestination>> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy destination has no parent",
            )
        })?;
        let leaf = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy destination has no final component",
            )
        })?;
        let absolute = path.is_absolute();
        let mut directory = open_start(absolute)?;
        let components = path_names(parent)?;
        let mut missing = Vec::new();
        let mut found_missing = false;
        for name in components {
            if found_missing {
                missing.push(name);
                continue;
            }
            let c_name = cstring(&name)?;
            match open_directory(directory.as_raw_fd(), &c_name) {
                Ok(child) => directory = child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    found_missing = true;
                    missing.push(name);
                }
                Err(error) => return Err(error),
            }
        }
        let base_path = descriptor_path(directory.as_raw_fd())?;
        missing.push(leaf.to_owned());
        Ok(Box::new(UnixCopyDestination {
            base: Arc::new(directory),
            base_path,
            root: missing,
            created: Arc::new(Mutex::new(HashMap::new())),
        }))
    }

    fn admit_route(base: &File, names: &[OsString]) -> io::Result<(Vec<RouteStep>, Option<File>)> {
        let mut directory = base.try_clone()?;
        let mut route = Vec::with_capacity(names.len());
        let mut missing = false;
        for (index, name) in names.iter().enumerate() {
            if missing {
                route.push(RouteStep {
                    name: name.clone(),
                    expected: None,
                });
                continue;
            }
            let name_c = cstring(name)?;
            match stat_at(directory.as_raw_fd(), &name_c) {
                Ok(stat) => {
                    let identity = Identity::from_stat(&stat);
                    route.push(RouteStep {
                        name: name.clone(),
                        expected: Some(identity),
                    });
                    let file = open_entry(directory.as_raw_fd(), &name_c)?;
                    let opened = Identity::from_stat(&stat_fd(file.as_raw_fd())?);
                    if opened != identity {
                        return Err(changed("copy destination changed while it was admitted"));
                    }
                    if index + 1 == names.len() {
                        return Ok((route, Some(file)));
                    }
                    if !identity.is_directory() {
                        return Err(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            "copy destination ancestor is not a directory",
                        ));
                    }
                    directory = file;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    missing = true;
                    route.push(RouteStep {
                        name: name.clone(),
                        expected: None,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok((route, None))
    }

    fn open_start(absolute: bool) -> io::Result<File> {
        let path = if absolute {
            OsStr::new("/")
        } else {
            OsStr::new(".")
        };
        let path = cstring(path)?;
        // SAFETY: path is NUL-terminated and flags require no mode.
        from_fd(unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        })
    }

    fn path_names(path: &Path) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::ParentDir => names.push(OsString::from("..")),
                Component::Normal(name) => names.push(name.to_owned()),
                Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "copy destination prefixes are unsupported on this platform",
                    ));
                }
            }
        }
        Ok(names)
    }

    fn relative_names(path: &Path) -> io::Result<Vec<OsString>> {
        if path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy target path must be relative to its destination root",
            ));
        }
        let names = path_names(path)?;
        if names.iter().any(|name| name == "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy target cannot escape its destination root",
            ));
        }
        Ok(names)
    }

    fn open_expected_directory(dir: RawFd, name: &CString, expected: Identity) -> io::Result<File> {
        if !expected.is_directory() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "admitted copy destination component is not a directory",
            ));
        }
        let child = open_directory(dir, name)?;
        if Identity::from_stat(&stat_fd(child.as_raw_fd())?) != expected {
            return Err(changed(
                "copy destination directory changed after preflight",
            ));
        }
        Ok(child)
    }

    fn open_directory(dir: RawFd, name: &CString) -> io::Result<File> {
        // SAFETY: name is NUL-terminated and dir remains live.
        from_fd(unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        })
    }

    fn open_entry(dir: RawFd, name: &CString) -> io::Result<File> {
        // O_NONBLOCK prevents a raced FIFO from blocking admission.
        // SAFETY: name is NUL-terminated and dir remains live.
        from_fd(unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        })
    }

    fn create_temporary(dir: RawFd) -> io::Result<(File, CString)> {
        create_temporary_with(dir, random_temporary_name)
    }

    fn create_temporary_with(
        dir: RawFd,
        mut next_name: impl FnMut() -> io::Result<CString>,
    ) -> io::Result<(File, CString)> {
        let mut collision = None;
        for _ in 0..128 {
            let name = next_name()?;
            // SAFETY: name is NUL-terminated, dir remains live, and mode is
            // supplied because O_CREAT is present.
            let fd = unsafe {
                libc::openat(
                    dir,
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if fd >= 0 {
                return Ok((from_fd(fd)?, name));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
            collision = Some(error);
        }
        Err(collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate copy publication temporary file",
            )
        }))
    }

    fn random_temporary_name() -> io::Result<CString> {
        let mut random = [0_u8; 16];
        fill_random(&mut random)?;
        let mut name = Vec::with_capacity(12 + random.len() * 2);
        name.extend_from_slice(b".shoal-copy-");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in random {
            name.push(HEX[usize::from(byte >> 4)]);
            name.push(HEX[usize::from(byte & 0x0f)]);
        }
        Ok(CString::new(name).expect("hex temporary name is a valid C string"))
    }

    #[cfg(target_os = "linux")]
    fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
        let mut filled = 0;
        while filled < bytes.len() {
            // SAFETY: the remaining slice is writable for its supplied length.
            let result = unsafe {
                libc::getrandom(bytes[filled..].as_mut_ptr().cast(), bytes.len() - filled, 0)
            };
            if result > 0 {
                filled += usize::try_from(result)
                    .map_err(|_| io::Error::other("getrandom returned an invalid length"))?;
            } else if result == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "getrandom returned no bytes",
                ));
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    #[cfg(target_vendor = "apple")]
    fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
        // SAFETY: arc4random_buf initializes the writable byte slice.
        unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
        Ok(())
    }

    fn publish(
        dir: RawFd,
        temporary: &CString,
        destination: &CString,
        expected: Option<Identity>,
    ) -> io::Result<()> {
        let Some(expected) = expected else {
            return rename_noreplace(dir, temporary, destination);
        };
        rename_exchange(dir, temporary, destination)?;
        let displaced = stat_at(dir, temporary);
        let valid = displaced
            .as_ref()
            .is_ok_and(|stat| Identity::from_stat(stat) == expected && stat.st_nlink == 1);
        if valid {
            return unlink(dir, temporary);
        }

        let detail = match displaced {
            Ok(_) => "destination identity or link count changed after preflight".to_string(),
            Err(error) => format!("cannot verify displaced destination: {error}"),
        };
        match rename_exchange(dir, temporary, destination) {
            Ok(()) => Err(changed(detail)),
            Err(rollback) => Err(changed(format!(
                "{detail}; atomic publication rollback failed: {rollback}"
            ))),
        }
    }

    #[cfg(target_os = "linux")]
    fn rename_noreplace(dir: RawFd, from: &CString, to: &CString) -> io::Result<()> {
        rename_linux(dir, from, to, libc::RENAME_NOREPLACE)
    }

    #[cfg(target_os = "linux")]
    fn rename_exchange(dir: RawFd, from: &CString, to: &CString) -> io::Result<()> {
        rename_linux(dir, from, to, libc::RENAME_EXCHANGE)
    }

    #[cfg(target_os = "linux")]
    fn rename_linux(dir: RawFd, from: &CString, to: &CString, flags: u32) -> io::Result<()> {
        // SAFETY: both names are NUL-terminated and dir remains live.
        let status = unsafe { libc::renameat2(dir, from.as_ptr(), dir, to.as_ptr(), flags) };
        status_result(status)
    }

    #[cfg(target_vendor = "apple")]
    fn rename_noreplace(dir: RawFd, from: &CString, to: &CString) -> io::Result<()> {
        rename_apple(dir, from, to, libc::RENAME_EXCL)
    }

    #[cfg(target_vendor = "apple")]
    fn rename_exchange(dir: RawFd, from: &CString, to: &CString) -> io::Result<()> {
        rename_apple(dir, from, to, libc::RENAME_SWAP)
    }

    #[cfg(target_vendor = "apple")]
    fn rename_apple(dir: RawFd, from: &CString, to: &CString, flags: u32) -> io::Result<()> {
        // SAFETY: both names are NUL-terminated and dir remains live.
        let status = unsafe { libc::renameatx_np(dir, from.as_ptr(), dir, to.as_ptr(), flags) };
        status_result(status)
    }

    fn mkdir(dir: RawFd, name: &CString) -> io::Result<()> {
        // SAFETY: name is NUL-terminated and dir remains live.
        status_result(unsafe { libc::mkdirat(dir, name.as_ptr(), 0o700) })
    }

    fn unlink(dir: RawFd, name: &CString) -> io::Result<()> {
        // SAFETY: name is NUL-terminated and dir remains live.
        status_result(unsafe { libc::unlinkat(dir, name.as_ptr(), 0) })
    }

    fn set_mode(fd: RawFd, mode: u32) -> io::Result<()> {
        // SAFETY: fd remains live and mode contains portable permission bits.
        status_result(unsafe { libc::fchmod(fd, mode as libc::mode_t) })
    }

    fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: stat points to writable storage and fd remains live.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0 {
            // SAFETY: successful fstat initialized stat.
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn stat_at(dir: RawFd, name: &CString) -> io::Result<libc::stat> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: name is NUL-terminated, stat writable, and dir live.
        if unsafe {
            libc::fstatat(
                dir,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            // SAFETY: successful fstatat initialized stat.
            Ok(unsafe { stat.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn from_fd(fd: RawFd) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a successful open/openat returned a new owned fd.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn cstring(value: &OsStr) -> io::Result<CString> {
        CString::new(value.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem path contains an interior NUL",
            )
        })
    }

    fn status_result(status: i32) -> io::Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn changed(message: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message.into())
    }

    #[cfg(target_os = "linux")]
    fn descriptor_path(fd: RawFd) -> io::Result<PathBuf> {
        fs::read_link(format!("/proc/self/fd/{fd}"))
    }

    #[cfg(target_vendor = "apple")]
    fn descriptor_path(fd: RawFd) -> io::Result<PathBuf> {
        let mut bytes = vec![0_u8; libc::PATH_MAX as usize];
        // SAFETY: bytes is writable for PATH_MAX bytes and fd is live.
        let status = unsafe { libc::fcntl(fd, libc::F_GETPATH, bytes.as_mut_ptr()) };
        if status < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        bytes.truncate(length);
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }

    #[cfg(target_os = "linux")]
    fn has_extended_attributes(fd: RawFd) -> io::Result<bool> {
        // SAFETY: null/zero asks only for the required byte count.
        let count = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
        if count < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOTSUP) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        if count == 0 {
            return Ok(false);
        }
        let mut names = vec![0_u8; count as usize];
        // SAFETY: names is writable for its length and fd remains live.
        let read = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len()) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        names.truncate(read as usize);
        Ok(names
            .split(|byte| *byte == 0)
            .any(|name| !name.is_empty() && name != b"security.selinux"))
    }

    #[cfg(target_vendor = "apple")]
    fn has_extended_attributes(fd: RawFd) -> io::Result<bool> {
        // SAFETY: null/zero asks only for the required byte count.
        let count = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::cell::Cell;
        use std::collections::HashSet;

        #[test]
        fn publication_temporary_names_are_randomized() {
            let names = (0..64)
                .map(|_| random_temporary_name().unwrap().into_bytes())
                .collect::<HashSet<_>>();
            assert_eq!(names.len(), 64);
            assert!(names.iter().all(|name| {
                name.starts_with(b".shoal-copy-") && name.len() == b".shoal-copy-".len() + 32
            }));
        }

        #[test]
        fn temporary_allocation_has_a_bounded_collision_retry_wall() {
            let unique = random_temporary_name().unwrap();
            let root = std::env::temp_dir().join(OsStr::from_bytes(unique.as_bytes()));
            fs::create_dir(&root).unwrap();
            let directory = File::open(&root).unwrap();
            let collision = CString::new(".shoal-copy-collision").unwrap();
            fs::write(root.join(".shoal-copy-collision"), b"occupied").unwrap();
            let attempts = Cell::new(0usize);

            let error = create_temporary_with(directory.as_raw_fd(), || {
                attempts.set(attempts.get() + 1);
                Ok(collision.clone())
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(attempts.get(), 128);
            drop(directory);
            fs::remove_file(root.join(".shoal-copy-collision")).unwrap();
            fs::remove_dir(root).unwrap();
        }
    }
}

pub(crate) fn open(path: &Path) -> io::Result<Box<dyn FsCopyDestination>> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        unix::open(path)
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "pinned recursive-copy destinations require openat and conditional rename support",
        ))
    }
}
