const MAX_ENTRIES: usize = 100_000;
const MAX_NAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const QUARANTINE_ATTEMPTS: usize = 32;
const QUARANTINE_PREFIX: &[u8] = b".shoal-rm-q-";

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

    #[allow(clippy::unnecessary_cast)]
    fn is_dir(self) -> bool {
        self.file_type == libc::S_IFDIR as u32
    }
}

#[derive(Debug)]
struct Node {
    name: OsString,
    identity: Identity,
    mount_key: u64,
    directory: Option<Directory>,
}

#[derive(Debug)]
struct Directory {
    identity: Identity,
    mount_key: u64,
    children: Vec<Node>,
}

#[derive(Debug)]
struct UnixRemovalTree {
    root_fd: File,
    root: Directory,
}

#[derive(Debug, Default)]
struct Admission {
    entries: usize,
    name_bytes: usize,
}

impl Admission {
    fn admit(&mut self, name: &OsStr, depth: usize) -> io::Result<()> {
        if depth > MAX_DEPTH {
            return Err(limit_error(format!(
                "recursive removal exceeds its {MAX_DEPTH}-directory depth limit"
            )));
        }
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| limit_error("recursive removal entry accounting overflowed"))?;
        if self.entries > MAX_ENTRIES {
            return Err(limit_error(format!(
                "recursive removal contains more than {MAX_ENTRIES} admitted entries"
            )));
        }
        self.name_bytes = self
            .name_bytes
            .checked_add(name.as_bytes().len())
            .ok_or_else(|| limit_error("recursive removal name accounting overflowed"))?;
        if self.name_bytes > MAX_NAME_BYTES {
            return Err(limit_error(format!(
                "recursive removal names exceed {MAX_NAME_BYTES} admitted bytes"
            )));
        }
        Ok(())
    }
}

impl FsRemovalTree for UnixRemovalTree {
    fn remove(&self, quarantined: &Path) -> io::Result<()> {
        let (parent, name) = split_entry(quarantined)?;
        let parent = open_directory_path(parent)?;
        let name = cstring(name)?;
        let found = stat_at(parent.as_raw_fd(), &name)?;
        if Identity::from_stat(&found) != self.root.identity
            || mount_key_at(parent.as_raw_fd(), &name)? != self.root.mount_key
        {
            return Err(changed("quarantined removal root changed identity"));
        }

        // Complete validation happens before the first unlink. This makes
        // a preflight-to-commit descendant replacement fail without a
        // partial deletion, while retained fds make ancestor renames
        // irrelevant to every descendant lookup.
        validate_directory(&self.root_fd, &self.root)?;
        remove_children(&self.root_fd, &self.root)?;

        remove_empty_directory_with(
            &parent,
            &name,
            self.root.identity,
            self.root.mount_key,
            |_, _, _, _| {},
        )
    }
}

