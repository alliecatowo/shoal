pub(super) fn open(
    path: &Path,
    expected: &FsEntryIdentity,
) -> io::Result<Box<dyn FsRemovalTree>> {
    let fd = open_directory_path(path)?;
    let stat = stat_fd(fd.as_raw_fd())?;
    if !expected.matches_stat(&stat) {
        return Err(changed("recursive-removal root changed after preflight"));
    }
    let mut admission = Admission::default();
    let root = inventory(&fd, 1, &mut admission)?;
    Ok(Box::new(UnixRemovalTree { root_fd: fd, root }))
}

pub(super) fn remove_leaf(path: &Path, expected: &FsEntryIdentity) -> io::Result<()> {
    remove_leaf_with(path, expected, |_, _, _, _| {})
}

fn remove_leaf_with(
    path: &Path,
    expected: &FsEntryIdentity,
    hook: impl FnMut(CommitStage, RawFd, &CString, Option<&CString>),
) -> io::Result<()> {
    let (parent_path, name) = split_entry(path)?;
    let parent = open_directory_path(parent_path)?;
    let name = cstring(name)?;
    let stat = stat_at(parent.as_raw_fd(), &name).map_err(|error| {
        changed(format!(
            "cannot verify permanent-removal leaf before commit: {error}"
        ))
    })?;
    if !expected.matches_stat(&stat) {
        return Err(changed(
            "permanent-removal leaf changed before deletion commit",
        ));
    }
    let identity = Identity::from_stat(&stat);
    if identity.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "leaf removal capability cannot remove a directory",
        ));
    }
    let child = Node {
        name: OsString::from(OsStr::from_bytes(name.to_bytes())),
        identity,
        mount_key: mount_key_at(parent.as_raw_fd(), &name)?,
        directory: None,
    };
    remove_child_with(&parent, &child, hook)
}
