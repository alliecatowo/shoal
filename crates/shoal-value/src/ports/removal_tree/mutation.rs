fn remove_children(fd: &File, directory: &Directory) -> io::Result<()> {
    let current = Identity::from_stat(&stat_fd(fd.as_raw_fd())?);
    if current != directory.identity || mount_key_fd(fd.as_raw_fd())? != directory.mount_key {
        return Err(changed(
            "an admitted directory changed before descendant deletion",
        ));
    }
    for child in &directory.children {
        remove_child_with(fd, child, |_, _, _, _| {})?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitStage {
    BeforeRename,
    AfterRename,
}

/// Commit one admitted child by first moving the directory entry to an
/// unpredictable, no-replace sibling name. A racer can replace the public
/// name, but it cannot make us unlink that replacement: identity and mount
/// validation happen only after the atomic move has contained the object.
fn remove_child_with(
    fd: &File,
    child: &Node,
    mut hook: impl FnMut(CommitStage, RawFd, &CString, Option<&CString>),
) -> io::Result<()> {
    let name = cstring(&child.name)?;
    validate_child_at(fd.as_raw_fd(), &name, child, "before deletion commit")?;
    hook(CommitStage::BeforeRename, fd.as_raw_fd(), &name, None);

    let quarantine = quarantine_at(fd.as_raw_fd(), &name)?;
    hook(
        CommitStage::AfterRename,
        fd.as_raw_fd(),
        &name,
        Some(&quarantine),
    );

    if let Err(error) =
        validate_child_at(fd.as_raw_fd(), &quarantine, child, "after deletion commit")
    {
        return Err(restore_or_contain(
            fd.as_raw_fd(),
            &name,
            &quarantine,
            error,
        ));
    }

    if let Some(child_directory) = &child.directory {
        let child_fd = match open_directory_at(fd.as_raw_fd(), &quarantine) {
            Ok(child_fd) => child_fd,
            Err(error) => {
                return Err(restore_or_contain(
                    fd.as_raw_fd(),
                    &name,
                    &quarantine,
                    changed(format!("cannot open committed directory: {error}")),
                ));
            }
        };
        if let Err(error) = validate_child_fd(&child_fd, child) {
            return Err(restore_or_contain(
                fd.as_raw_fd(),
                &name,
                &quarantine,
                error,
            ));
        }
        remove_children(&child_fd, child_directory).map_err(|error| {
            changed(format!(
                "committed directory {} remains contained after descendant failure: {error}",
                quarantine.to_string_lossy()
            ))
        })?;
        if let Err(error) = validate_child_at(
            fd.as_raw_fd(),
            &quarantine,
            child,
            "before committed directory unlink",
        ) {
            return Err(restore_or_contain(
                fd.as_raw_fd(),
                &name,
                &quarantine,
                error,
            ));
        }
        unlink_or_restore(fd.as_raw_fd(), &name, &quarantine, libc::AT_REMOVEDIR)
    } else {
        unlink_or_restore(fd.as_raw_fd(), &name, &quarantine, 0)
    }
}

fn remove_empty_directory_with(
    parent: &File,
    name: &CString,
    identity: Identity,
    mount_key: u64,
    mut hook: impl FnMut(CommitStage, RawFd, &CString, Option<&CString>),
) -> io::Result<()> {
    validate_identity_at(
        parent.as_raw_fd(),
        name,
        identity,
        mount_key,
        "quarantined removal root changed before final commit",
    )?;
    hook(CommitStage::BeforeRename, parent.as_raw_fd(), name, None);
    let quarantine = quarantine_at(parent.as_raw_fd(), name)?;
    hook(
        CommitStage::AfterRename,
        parent.as_raw_fd(),
        name,
        Some(&quarantine),
    );
    if let Err(error) = validate_identity_at(
        parent.as_raw_fd(),
        &quarantine,
        identity,
        mount_key,
        "quarantined removal root changed at final commit",
    ) {
        return Err(restore_or_contain(
            parent.as_raw_fd(),
            name,
            &quarantine,
            error,
        ));
    }
    let directory = match open_directory_at(parent.as_raw_fd(), &quarantine) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(restore_or_contain(
                parent.as_raw_fd(),
                name,
                &quarantine,
                changed(format!("cannot open committed removal root: {error}")),
            ));
        }
    };
    let opened_stat = match stat_fd(directory.as_raw_fd()) {
        Ok(stat) => stat,
        Err(error) => {
            return Err(restore_or_contain(
                parent.as_raw_fd(),
                name,
                &quarantine,
                changed(format!("cannot verify committed removal root: {error}")),
            ));
        }
    };
    let opened_mount = match mount_key_fd(directory.as_raw_fd()) {
        Ok(mount) => mount,
        Err(error) => {
            return Err(restore_or_contain(
                parent.as_raw_fd(),
                name,
                &quarantine,
                changed(format!(
                    "cannot verify committed removal-root mount: {error}"
                )),
            ));
        }
    };
    if Identity::from_stat(&opened_stat) != identity || opened_mount != mount_key {
        return Err(restore_or_contain(
            parent.as_raw_fd(),
            name,
            &quarantine,
            changed("committed removal-root descriptor changed identity or mount"),
        ));
    }
    unlink_or_restore(parent.as_raw_fd(), name, &quarantine, libc::AT_REMOVEDIR)
}

fn validate_identity_at(
    parent: RawFd,
    name: &CString,
    identity: Identity,
    mount_key: u64,
    message: &str,
) -> io::Result<()> {
    let found =
        stat_at(parent, name).map_err(|error| changed(format!("{message}: {error}")))?;
    let found_mount =
        mount_key_at(parent, name).map_err(|error| changed(format!("{message}: {error}")))?;
    if Identity::from_stat(&found) == identity && found_mount == mount_key {
        Ok(())
    } else {
        Err(changed(message))
    }
}

fn validate_child_fd(fd: &File, child: &Node) -> io::Result<()> {
    let opened = Identity::from_stat(&stat_fd(fd.as_raw_fd()).map_err(|error| {
        changed(format!(
            "cannot verify committed directory descriptor: {error}"
        ))
    })?);
    let opened_mount = mount_key_fd(fd.as_raw_fd()).map_err(|error| {
        changed(format!("cannot verify committed directory mount: {error}"))
    })?;
    if opened == child.identity && opened_mount == child.mount_key {
        Ok(())
    } else {
        Err(changed(
            "committed directory descriptor changed identity or mount",
        ))
    }
}

fn unlink_or_restore(
    parent: RawFd,
    original: &CString,
    quarantine: &CString,
    flags: i32,
) -> io::Result<()> {
    let error = match unlink_at(parent, quarantine, flags) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    match rename_noreplace(parent, quarantine, parent, original) {
        Ok(()) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot unlink committed entry; it was restored without overwrite: {error}"
            ),
        )),
        Err(restore) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot unlink committed entry; restoration failed ({restore}); it remains contained as {}: {error}",
                quarantine.to_string_lossy()
            ),
        )),
    }
}

fn validate_child_at(
    parent: RawFd,
    name: &CString,
    child: &Node,
    stage: &str,
) -> io::Result<()> {
    let found = Identity::from_stat(&stat_at(parent, name).map_err(|error| {
        changed(format!(
            "cannot verify descendant {} {stage}: {error}",
            child.name.to_string_lossy()
        ))
    })?);
    let found_mount = mount_key_at(parent, name).map_err(|error| {
        changed(format!(
            "cannot verify descendant {} mount {stage}: {error}",
            child.name.to_string_lossy()
        ))
    })?;
    if found == child.identity && found_mount == child.mount_key {
        Ok(())
    } else {
        Err(changed(format!(
            "descendant {} changed {stage}",
            child.name.to_string_lossy()
        )))
    }
}

fn quarantine_at(parent: RawFd, source: &CString) -> io::Result<CString> {
    for _ in 0..QUARANTINE_ATTEMPTS {
        let target = random_quarantine_name()?;
        match rename_noreplace(parent, source, parent, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => continue,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Err(changed(format!(
                    "descendant {} disappeared before deletion commit",
                    OsStr::from_bytes(source.to_bytes()).to_string_lossy()
                )));
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not reserve a private removal name after {QUARANTINE_ATTEMPTS} attempts"
        ),
    ))
}

fn restore_or_contain(
    parent: RawFd,
    original: &CString,
    quarantine: &CString,
    error: io::Error,
) -> io::Error {
    match rename_noreplace(parent, quarantine, parent, original) {
        Ok(()) => changed(format!(
            "{error}; raced replacement was restored without overwrite"
        )),
        Err(restore) => changed(format!(
            "{error}; restoration failed ({restore}); raced replacement remains contained as {}",
            quarantine.to_string_lossy()
        )),
    }
}

fn random_quarantine_name() -> io::Result<CString> {
    let mut random = [0_u8; 16];
    fill_random(&mut random)?;
    let mut name = Vec::with_capacity(QUARANTINE_PREFIX.len() + random.len() * 2);
    name.extend_from_slice(QUARANTINE_PREFIX);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in random {
        name.push(HEX[usize::from(byte >> 4)]);
        name.push(HEX[usize::from(byte & 0x0f)]);
    }
    // The generated bytes contain neither NUL nor a path separator.
    Ok(CString::new(name).expect("hex quarantine name is a valid C string"))
}

#[cfg(target_os = "linux")]
fn fill_random(bytes: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < bytes.len() {
        // SAFETY: the remaining slice is writable for the supplied length.
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
    // SAFETY: arc4random_buf initializes the supplied writable byte slice.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    Ok(())
}

#[cfg(target_os = "linux")]
fn rename_noreplace(
    from_parent: RawFd,
    from: &CString,
    to_parent: RawFd,
    to: &CString,
) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and both directory fds live.
    let status = unsafe {
        libc::renameat2(
            from_parent,
            from.as_ptr(),
            to_parent,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn rename_noreplace(
    from_parent: RawFd,
    from: &CString,
    to_parent: RawFd,
    to: &CString,
) -> io::Result<()> {
    // SAFETY: both names are NUL-terminated and both directory fds live.
    let status = unsafe {
        libc::renameatx_np(
            from_parent,
            from.as_ptr(),
            to_parent,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

