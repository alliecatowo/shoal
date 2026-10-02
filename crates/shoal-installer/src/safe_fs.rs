use sha2::{Digest, Sha256};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::validate_relative;
use crate::model::{Identity, LOCK_NAME};

pub struct PrefixLock {
    _file: File,
}

pub struct SafeRoot {
    path: PathBuf,
    directory: File,
    device: u64,
    inode: u64,
}

impl SafeRoot {
    pub fn open(path: &Path) -> io::Result<Self> {
        fs::create_dir_all(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(other(format!(
                "install prefix is not a real directory: {}",
                path.display()
            )));
        }
        let directory = open_directory(path)?;
        let stat = fstat(directory.as_raw_fd())?;
        Ok(Self {
            path: path.to_path_buf(),
            directory,
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock(&self, timeout_ms: u64) -> io::Result<PrefixLock> {
        self.revalidate_root()?;
        let name = cstring(OsStr::new(LOCK_NAME))?;
        // SAFETY: the root descriptor and NUL-terminated constant name remain
        // valid for the call; ownership of a successful descriptor transfers
        // immediately to `File` below.
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh successful `openat` result owned here.
        let file = unsafe { File::from_raw_fd(fd) };
        let stat = fstat(file.as_raw_fd())?;
        // SAFETY: `geteuid` has no pointer preconditions.
        let current_uid = unsafe { libc::geteuid() };
        if stat.st_uid != current_uid || (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(other(
                "install lock is not an owner-controlled regular file",
            ));
        }
        // SAFETY: `file` owns a live descriptor for the duration of the call.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let start = Instant::now();
        loop {
            // SAFETY: `file` owns a live descriptor and the operation flags
            // are a valid nonblocking exclusive-lock combination.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(PrefixLock { _file: file });
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error);
            }
            if start.elapsed() >= Duration::from_millis(timeout_ms) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "another Shoal installer holds the prefix lock",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn ensure_dir(&self, relative: &Path, mode: u32) -> io::Result<()> {
        validate_relative(relative)?;
        self.revalidate_root()?;
        let mut current = duplicate(&self.directory)?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(other("unsafe directory component"));
            };
            let name = cstring(name)?;
            let next = openat_directory(current.as_raw_fd(), &name);
            current = match next {
                Ok(directory) => directory,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // SAFETY: the directory descriptor and component CString
                    // remain live and the mode is limited to permission bits.
                    if unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), mode) } != 0 {
                        let create_error = io::Error::last_os_error();
                        if create_error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(create_error);
                        }
                    }
                    openat_directory(current.as_raw_fd(), &name)?
                }
                Err(error) => return Err(error),
            };
        }
        Ok(())
    }

    pub fn identity(&self, relative: &Path) -> io::Result<Option<Identity>> {
        let (parent, name) = match self.open_parent(relative, false) {
            Ok(parts) => parts,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let file = match openat_file(
            parent.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let stat = fstat(file.as_raw_fd())?;
        if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(other(format!(
                "managed path is not a regular file: {}",
                relative.display()
            )));
        }
        identity_from_file(file)
    }

    pub fn copy_out(&self, relative: &Path, destination: &Path, mode: u32) -> io::Result<()> {
        let (parent, name) = self.open_parent(relative, false)?;
        let mut source = openat_file(
            parent.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )?;
        let stat = fstat(source.as_raw_fd())?;
        if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(other(format!(
                "managed path is not a regular file: {}",
                relative.display()
            )));
        }
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(destination)?;
        io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        Ok(())
    }

    pub fn read_file(&self, relative: &Path) -> io::Result<Vec<u8>> {
        let (parent, name) = self.open_parent(relative, false)?;
        let mut file = openat_file(
            parent.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )?;
        let stat = fstat(file.as_raw_fd())?;
        if (stat.st_mode & libc::S_IFMT) != libc::S_IFREG {
            return Err(other(format!(
                "managed path is not a regular file: {}",
                relative.display()
            )));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub fn install_file(
        &self,
        relative: &Path,
        source: &Path,
        mode: u32,
        nonce: &str,
    ) -> io::Result<()> {
        let parent_relative = relative
            .parent()
            .ok_or_else(|| other("managed path has no parent"))?;
        if !parent_relative.as_os_str().is_empty() {
            self.ensure_dir(parent_relative, 0o755)?;
        }
        let (parent, name) = self.open_parent(relative, true)?;
        self.assert_parent_pinned(parent_relative, &parent)?;
        let temporary = cstring(OsStr::new(&format!(".shoal-install-{nonce}")))?;
        let _ = unlinkat(parent.as_raw_fd(), &temporary);
        let result = (|| {
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(source)?;
            if input.metadata()?.file_type().is_symlink() || !input.metadata()?.is_file() {
                return Err(other(format!(
                    "install source is not a regular file: {}",
                    source.display()
                )));
            }
            let mut output = openat_file(
                parent.as_raw_fd(),
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                mode,
            )?;
            io::copy(&mut input, &mut output)?;
            // SAFETY: `output` owns a live descriptor and `mode` contains the
            // installer's fixed permission bits.
            if unsafe { libc::fchmod(output.as_raw_fd(), mode) } != 0 {
                return Err(io::Error::last_os_error());
            }
            output.sync_all()?;
            self.assert_parent_pinned(parent_relative, &parent)?;
            // SAFETY: both CStrings and the pinned directory descriptor remain
            // valid; source and destination are names in that same directory.
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            parent.sync_all()
        })();
        if result.is_err() {
            let _ = unlinkat(parent.as_raw_fd(), &temporary);
        }
        result
    }

    pub fn install_bytes(
        &self,
        relative: &Path,
        bytes: &[u8],
        mode: u32,
        nonce: &str,
    ) -> io::Result<()> {
        let parent_relative = relative
            .parent()
            .ok_or_else(|| other("managed path has no parent"))?;
        if !parent_relative.as_os_str().is_empty() {
            self.ensure_dir(parent_relative, 0o755)?;
        }
        let (parent, name) = self.open_parent(relative, true)?;
        self.assert_parent_pinned(parent_relative, &parent)?;
        let temporary = cstring(OsStr::new(&format!(".shoal-install-{nonce}")))?;
        let _ = unlinkat(parent.as_raw_fd(), &temporary);
        let result = (|| {
            let mut output = openat_file(
                parent.as_raw_fd(),
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                mode,
            )?;
            output.write_all(bytes)?;
            output.sync_all()?;
            self.assert_parent_pinned(parent_relative, &parent)?;
            // SAFETY: both CStrings and the pinned directory descriptor remain
            // valid; source and destination are names in that same directory.
            if unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            parent.sync_all()
        })();
        if result.is_err() {
            let _ = unlinkat(parent.as_raw_fd(), &temporary);
        }
        result
    }

    pub fn remove_file(&self, relative: &Path) -> io::Result<()> {
        let parent_relative = relative
            .parent()
            .ok_or_else(|| other("managed path has no parent"))?;
        let (parent, name) = match self.open_parent(relative, false) {
            Ok(parts) => parts,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        self.assert_parent_pinned(parent_relative, &parent)?;
        match unlinkat(parent.as_raw_fd(), &name) {
            Ok(()) => parent.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn sync(&self) -> io::Result<()> {
        self.directory.sync_all()
    }

    fn open_parent(&self, relative: &Path, create_parent: bool) -> io::Result<(File, CString)> {
        validate_relative(relative)?;
        self.revalidate_root()?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        if create_parent && !parent.as_os_str().is_empty() {
            self.ensure_dir(parent, 0o755)?;
        }
        let mut directory = duplicate(&self.directory)?;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                return Err(other("unsafe parent component"));
            };
            directory = openat_directory(directory.as_raw_fd(), &cstring(name)?)?;
        }
        let name = relative
            .file_name()
            .ok_or_else(|| other("managed path has no file name"))?;
        Ok((directory, cstring(name)?))
    }

    fn assert_parent_pinned(&self, relative: &Path, pinned: &File) -> io::Result<()> {
        self.revalidate_root()?;
        let current = if relative.as_os_str().is_empty() {
            duplicate(&self.directory)?
        } else {
            let mut directory = duplicate(&self.directory)?;
            for component in relative.components() {
                let Component::Normal(name) = component else {
                    return Err(other("unsafe parent component"));
                };
                directory = openat_directory(directory.as_raw_fd(), &cstring(name)?)?;
            }
            directory
        };
        let expected = fstat(pinned.as_raw_fd())?;
        let actual = fstat(current.as_raw_fd())?;
        if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
            return Err(other(format!(
                "managed destination parent changed during install: {}",
                relative.display()
            )));
        }
        Ok(())
    }

    fn revalidate_root(&self) -> io::Result<()> {
        let current = open_directory(&self.path)?;
        let stat = fstat(current.as_raw_fd())?;
        if stat.st_dev as u64 != self.device || stat.st_ino as u64 != self.inode {
            return Err(other("install prefix changed identity during transaction"));
        }
        Ok(())
    }
}

pub fn identity_for_path(path: &Path, mode: u32) -> io::Result<Identity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(other(format!(
            "source is not a regular file: {}",
            path.display()
        )));
    }
    let mut identity =
        identity_from_file(File::open(path)?)?.ok_or_else(|| other("source disappeared"))?;
    identity.mode = mode;
    Ok(identity)
}

pub fn sha256_reader(mut reader: impl Read) -> io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

pub fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn identity_from_file(mut file: File) -> io::Result<Option<Identity>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let (bytes, sha256) = sha256_reader(&mut file)?;
    Ok(Some(Identity {
        bytes,
        sha256,
        mode: metadata.mode() & 0o7777,
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

fn duplicate(file: &File) -> io::Result<File> {
    file.try_clone()
}

fn open_directory(path: &Path) -> io::Result<File> {
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| other("path contains NUL"))?;
    // SAFETY: `name` is NUL terminated and lives through `open`; a successful
    // descriptor transfers immediately into `File`.
    let fd = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `fd` is a fresh successful `open` result owned here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn openat_directory(parent: RawFd, name: &CString) -> io::Result<File> {
    // SAFETY: `parent` is live and `name` is a valid CString for this call.
    let fd = unsafe {
        libc::openat(
            parent,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `fd` is a fresh successful `openat` result owned here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn openat_file(parent: RawFd, name: &CString, flags: i32, mode: u32) -> io::Result<File> {
    // SAFETY: `parent` is live and `name` is a valid CString; callers provide
    // an `openat` flag/mode combination and take ownership through `File`.
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `fd` is a fresh successful `openat` result owned here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn unlinkat(parent: RawFd, name: &CString) -> io::Result<()> {
    // SAFETY: `parent` is live and `name` is a valid CString; flags request a
    // file unlink and therefore never follow a final symlink.
    if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `fd` is live by contract and `stat` points to sufficient writable
    // storage which is read only after a successful return.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: successful `fstat` initialized every field of `stat`.
        Ok(unsafe { stat.assume_init() })
    }
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| other("path contains NUL"))
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn fd_relative_publication_round_trips_content_and_mode() {
        let temporary = tempfile::tempdir().unwrap();
        let root = SafeRoot::open(temporary.path()).unwrap();
        let relative = Path::new("bin/tool");
        root.install_bytes(relative, b"payload", 0o700, "test")
            .unwrap();
        let identity = root.identity(relative).unwrap().unwrap();
        assert_eq!(identity.bytes, 7);
        assert_eq!(identity.mode, 0o700);
        assert_eq!(root.read_file(relative).unwrap(), b"payload");
        root.remove_file(relative).unwrap();
        assert!(root.identity(relative).unwrap().is_none());
    }

    #[test]
    fn fd_relative_traversal_rejects_a_symlinked_parent() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), temporary.path().join("bin")).unwrap();
        let root = SafeRoot::open(temporary.path()).unwrap();
        let error = root
            .install_bytes(Path::new("bin/tool"), b"payload", 0o700, "test")
            .unwrap_err();
        assert!(
            matches!(
                error.raw_os_error(),
                Some(libc::ELOOP) | Some(libc::ENOTDIR)
            ),
            "unexpected error: {error}"
        );
        assert!(!outside.path().join("tool").exists());
    }

    #[test]
    fn prefix_lock_contention_is_bounded() {
        let temporary = tempfile::tempdir().unwrap();
        let first_root = SafeRoot::open(temporary.path()).unwrap();
        let _held = first_root.lock(100).unwrap();
        let second_root = SafeRoot::open(temporary.path()).unwrap();
        let error = match second_root.lock(30) {
            Ok(_) => panic!("contending lock unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
