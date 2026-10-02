fn inventory(fd: &File, depth: usize, admission: &mut Admission) -> io::Result<Directory> {
    if depth > MAX_DEPTH {
        return Err(limit_error(format!(
            "recursive removal exceeds its {MAX_DEPTH}-directory depth limit"
        )));
    }
    let root_stat = stat_fd(fd.as_raw_fd())?;
    let identity = Identity::from_stat(&root_stat);
    let mount_key = mount_key_fd(fd.as_raw_fd())?;
    if !identity.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recursive-removal capability root is not a directory",
        ));
    }
    let names = directory_names(
        fd.as_raw_fd(),
        MAX_ENTRIES.saturating_sub(admission.entries),
        MAX_NAME_BYTES.saturating_sub(admission.name_bytes),
    )?;
    let mut children = Vec::with_capacity(names.len());
    for name in names {
        admission.admit(&name, depth)?;
        let c_name = cstring(&name)?;
        let stat = stat_at(fd.as_raw_fd(), &c_name)?;
        let child_identity = Identity::from_stat(&stat);
        let child_mount_key = mount_key_at(fd.as_raw_fd(), &c_name)?;
        if child_mount_key != mount_key {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "recursive removal refuses mounted descendant {}",
                    name.to_string_lossy()
                ),
            ));
        }
        let directory = if child_identity.is_dir() {
            let child = open_directory_at(fd.as_raw_fd(), &c_name)?;
            let opened = Identity::from_stat(&stat_fd(child.as_raw_fd())?);
            if opened != child_identity || mount_key_fd(child.as_raw_fd())? != mount_key {
                return Err(changed(format!(
                    "descendant {} changed while it was admitted",
                    name.to_string_lossy()
                )));
            }
            Some(inventory(&child, depth + 1, admission)?)
        } else {
            None
        };
        children.push(Node {
            name,
            identity: child_identity,
            mount_key: child_mount_key,
            directory,
        });
    }
    Ok(Directory {
        identity,
        mount_key,
        children,
    })
}

fn validate_directory(fd: &File, directory: &Directory) -> io::Result<()> {
    let current = Identity::from_stat(&stat_fd(fd.as_raw_fd())?);
    if current != directory.identity || mount_key_fd(fd.as_raw_fd())? != directory.mount_key {
        return Err(changed("an admitted directory descriptor changed identity"));
    }
    let admitted_name_bytes = directory.children.iter().try_fold(0usize, |total, child| {
        total
            .checked_add(child.name.as_bytes().len())
            .ok_or_else(|| changed("admitted descendant name accounting overflowed"))
    })?;
    let names = directory_names(
        fd.as_raw_fd(),
        directory.children.len(),
        admitted_name_bytes,
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::FileTooLarge {
            changed("recursive-removal descendants changed after preflight")
        } else {
            error
        }
    })?;
    let admitted = directory
        .children
        .iter()
        .map(|child| child.name.as_os_str())
        .collect::<HashSet<_>>();
    if names.len() != admitted.len()
        || names
            .iter()
            .any(|name| !admitted.contains(name.as_os_str()))
    {
        return Err(changed(
            "recursive-removal descendants changed after preflight",
        ));
    }
    for child in &directory.children {
        let name = cstring(&child.name)?;
        let found = Identity::from_stat(&stat_at(fd.as_raw_fd(), &name)?);
        if found != child.identity || mount_key_at(fd.as_raw_fd(), &name)? != child.mount_key {
            return Err(changed(format!(
                "descendant {} changed after preflight",
                child.name.to_string_lossy()
            )));
        }
        if let Some(child_directory) = &child.directory {
            let child_fd = open_directory_at(fd.as_raw_fd(), &name)?;
            let opened = Identity::from_stat(&stat_fd(child_fd.as_raw_fd())?);
            if opened != child.identity {
                return Err(changed(format!(
                    "directory {} changed while validation opened it",
                    child.name.to_string_lossy()
                )));
            }
            validate_directory(&child_fd, child_directory)?;
        }
    }
    Ok(())
}

