fn directory_names(
    fd: RawFd,
    max_entries: usize,
    max_name_bytes: usize,
) -> io::Result<Vec<OsString>> {
    // fdopendir takes ownership, so enumerate through a duplicate.
    // SAFETY: fcntl duplicates a live descriptor with close-on-exec.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: duplicate is an owned directory descriptor.
    let directory = unsafe { libc::fdopendir(duplicate) };
    if directory.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    // dup shares the directory stream offset with the retained fd. Reset
    // every fresh DIR stream so validation observes the whole directory,
    // not the EOF left by admission or an earlier validation pass.
    // SAFETY: directory is a successful fdopendir result.
    unsafe { libc::rewinddir(directory) };
    let result = collect_directory_names(directory, max_entries, max_name_bytes);
    // SAFETY: directory is a successful fdopendir result and uniquely owns
    // the duplicate descriptor.
    let close_status = unsafe { libc::closedir(directory) };
    if let Err(error) = result {
        return Err(error);
    }
    if close_status != 0 {
        return Err(io::Error::last_os_error());
    }
    result
}

fn collect_directory_names(
    directory: *mut libc::DIR,
    max_entries: usize,
    max_name_bytes: usize,
) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    let mut name_bytes = 0usize;
    loop {
        clear_errno();
        // SAFETY: directory remains live for this loop.
        let entry = unsafe { libc::readdir(directory) };
        if entry.is_null() {
            let errno = current_errno();
            if errno != 0 {
                return Err(io::Error::from_raw_os_error(errno));
            }
            return Ok(names);
        }
        // SAFETY: readdir returned a live NUL-terminated d_name.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() >= max_entries {
            return Err(limit_error(format!(
                "directory contains more than {max_entries} admitted entries"
            )));
        }
        name_bytes = name_bytes
            .checked_add(bytes.len())
            .ok_or_else(|| limit_error("directory name accounting overflowed"))?;
        if name_bytes > max_name_bytes {
            return Err(limit_error(format!(
                "directory names exceed {max_name_bytes} admitted bytes"
            )));
        }
        names.push(OsString::from_vec(bytes.to_vec()));
    }
}

fn open_directory_path(path: &Path) -> io::Result<File> {
    let path = cstring(path.as_os_str())?;
    // SAFETY: path is NUL-terminated and flags require no mode.
    owned_fd(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    })
}

fn open_directory_at(parent: RawFd, name: &CString) -> io::Result<File> {
    // SAFETY: name is NUL-terminated and parent remains live.
    owned_fd(unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    })
}

fn owned_fd(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a successful open/openat returned a new owned fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn stat_fd(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable storage and fd is live.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == 0 {
        // SAFETY: successful fstat initialized stat.
        Ok(unsafe { stat.assume_init() })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn stat_at(parent: RawFd, name: &CString) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: name is NUL-terminated, stat is writable, parent is live.
    if unsafe {
        libc::fstatat(
            parent,
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

#[cfg(target_os = "linux")]
fn mount_key_fd(fd: RawFd) -> io::Result<u64> {
    statx_mount_key(fd, std::ptr::null(), libc::AT_EMPTY_PATH)
}

#[cfg(target_os = "linux")]
fn mount_key_at(parent: RawFd, name: &CString) -> io::Result<u64> {
    statx_mount_key(parent, name.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
}

#[cfg(target_os = "linux")]
fn statx_mount_key(parent: RawFd, name: *const libc::c_char, flags: i32) -> io::Result<u64> {
    let empty = b"\0";
    let name = if name.is_null() {
        empty.as_ptr().cast()
    } else {
        name
    };
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: name is NUL-terminated, stat is writable, and parent is live.
    let status =
        unsafe { libc::statx(parent, name, flags, libc::STATX_MNT_ID, stat.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statx initialized the structure.
    let stat = unsafe { stat.assume_init() };
    if stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "recursive removal requires kernel mount-identity reporting",
        ));
    }
    Ok(stat.stx_mnt_id)
}

#[cfg(target_vendor = "apple")]
fn mount_key_fd(fd: RawFd) -> io::Result<u64> {
    #[allow(clippy::unnecessary_cast)]
    stat_fd(fd).map(|stat| stat.st_dev as u64)
}

#[cfg(target_vendor = "apple")]
fn mount_key_at(parent: RawFd, name: &CString) -> io::Result<u64> {
    #[allow(clippy::unnecessary_cast)]
    stat_at(parent, name).map(|stat| stat.st_dev as u64)
}

fn unlink_at(parent: RawFd, name: &CString, flags: i32) -> io::Result<()> {
    // SAFETY: name is NUL-terminated and parent remains live.
    if unsafe { libc::unlinkat(parent, name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn split_entry(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "entry has no parent"))?;
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "entry has no final component")
    })?;
    Ok((parent, name))
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem path contains an interior NUL",
        )
    })
}

fn changed(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn limit_error(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, message.into())
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

