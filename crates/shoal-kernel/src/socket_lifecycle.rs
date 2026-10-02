//! Race-resistant publication and cleanup for the public Unix socket.
//!
//! Pathname Unix sockets have no `bindat(2)`, so binding directly at the
//! public pathname creates two hazards: the socket is born with a
//! process-umask-derived mode, and later pathname chmod/unlink operations can
//! act on a replacement.  We instead bind below a private directory pinned by
//! a file descriptor, set and verify the mode while the entry is unpublished,
//! and publish the exact inode with a no-overwrite hard link.

use std::ffi::{CString, OsStr};
#[cfg(test)]
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    #[cfg(test)]
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn from_entry(entry: EntryMetadata) -> Self {
        Self {
            device: entry.device,
            inode: entry.inode,
        }
    }
}

#[derive(Clone, Copy)]
struct EntryMetadata {
    device: u64,
    inode: u64,
    mode: libc::mode_t,
    uid: libc::uid_t,
}

impl EntryMetadata {
    fn is_socket(self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFSOCK
    }

    fn permissions(self) -> libc::mode_t {
        self.mode & 0o777
    }
}

struct Directory {
    fd: OwnedFd,
}

impl Directory {
    #[cfg(test)]
    fn open_socket_parent(path: &Path) -> io::Result<(Self, CString)> {
        Self::open_socket_parent_with_hook(path, &mut |_, _| {})
    }

    fn open_socket_parent_with_hook(
        path: &Path,
        after_create: &mut impl FnMut(&Directory, &OsStr),
    ) -> io::Result<(Self, CString)> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path needs a file name")
        })?;
        let name = cstring(name)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path needs a parent")
        })?;
        let (start, components): (&Path, Vec<_>) = if parent.is_absolute() {
            (Path::new("/"), parent.components().skip(1).collect())
        } else {
            (Path::new("."), parent.components().collect())
        };
        let mut directory = Self::open_path(start)?;
        for component in components {
            match component {
                Component::CurDir | Component::RootDir => {}
                Component::Normal(name) => {
                    directory = directory.open_or_create_child_with_hook(name, after_create)?;
                }
                Component::ParentDir => directory = directory.open_child(OsStr::new(".."))?,
                Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unsupported socket path prefix",
                    ));
                }
            }
        }
        Ok((directory, name))
    }

    fn open_path(path: &Path) -> io::Result<Self> {
        let path = cstring(path.as_os_str())?;
        // SAFETY: `path` is NUL-terminated and flags request an owned directory
        // descriptor without following a final symbolic link.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        owned_fd(fd)
    }

    fn open_child(&self, name: &OsStr) -> io::Result<Self> {
        let name = cstring(name)?;
        // SAFETY: the directory fd and C string are valid for this call.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        owned_fd(fd)
    }

    fn open_or_create_child_with_hook(
        &self,
        name: &OsStr,
        after_create: &mut impl FnMut(&Directory, &OsStr),
    ) -> io::Result<Self> {
        match self.open_child(name) {
            Ok(directory) => Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name_c = cstring(name)?;
                // SAFETY: the parent descriptor and component are valid.
                let created = unsafe { libc::mkdirat(self.fd.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if created == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error);
                    }
                }
                let created_identity = if created == 0 {
                    let identity = FileIdentity::from_entry(self.metadata(&name_c)?);
                    after_create(self, name);
                    Some(identity)
                } else {
                    None
                };
                if let Some(created_identity) = created_identity {
                    self.pin_and_secure_created_child(name, created_identity)
                } else {
                    self.open_child(name)
                }
            }
            Err(error) => Err(error),
        }
    }

    fn pin_and_secure_created_child(
        &self,
        name: &OsStr,
        created_identity: FileIdentity,
    ) -> io::Result<Self> {
        let directory = self.open_child(name)?;
        let opened = directory.descriptor_metadata()?;
        if FileIdentity::from_entry(opened) != created_identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "new socket directory was replaced before it could be pinned",
            ));
        }
        // Use the retained descriptor, never a re-resolved path. The identity
        // comparison above proves this descriptor is ours, not a substitute.
        // SAFETY: the descriptor is live and owned by `directory`.
        if unsafe { libc::fchmod(directory.fd.as_raw_fd(), 0o700) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let secured = directory.descriptor_metadata()?;
        if FileIdentity::from_entry(secured) != created_identity || secured.permissions() != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "new socket directory identity or mode changed while securing it",
            ));
        }
        Ok(directory)
    }

    fn create_private_child(&self) -> io::Result<(CString, Self)> {
        for _ in 0..128 {
            let sequence = UNIQUE.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(
                ".shoal-socket-{}-{sequence:016x}",
                std::process::id()
            ))
            .expect("generated name has no NUL");
            // SAFETY: the parent descriptor and generated name are valid.
            if unsafe { libc::mkdirat(self.fd.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
                let identity = FileIdentity::from_entry(self.metadata(&name)?);
                let directory = self
                    .pin_and_secure_created_child(OsStr::from_bytes(name.as_bytes()), identity)?;
                return Ok((name, directory));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate private socket staging directory",
        ))
    }

    fn metadata(&self, name: &CString) -> io::Result<EntryMetadata> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: all pointers are valid and `stat` is writable.
        if unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized the complete structure.
        let stat = unsafe { stat.assume_init() };
        #[cfg(target_os = "linux")]
        let device = stat.st_dev;
        #[cfg(target_os = "macos")]
        let device = stat.st_dev as u64;
        Ok(EntryMetadata {
            device,
            inode: stat.st_ino,
            mode: stat.st_mode,
            uid: stat.st_uid,
        })
    }

    fn descriptor_metadata(&self) -> io::Result<EntryMetadata> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: the descriptor is live and `stat` points to writable storage.
        if unsafe { libc::fstat(self.fd.as_raw_fd(), stat.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstat initialized the complete structure.
        let stat = unsafe { stat.assume_init() };
        #[cfg(target_os = "linux")]
        let device = stat.st_dev;
        #[cfg(target_os = "macos")]
        let device = stat.st_dev as u64;
        Ok(EntryMetadata {
            device,
            inode: stat.st_ino,
            mode: stat.st_mode,
            uid: stat.st_uid,
        })
    }

    fn access_path(&self, name: &CString) -> PathBuf {
        #[cfg(target_os = "linux")]
        let root = "/proc/self/fd";
        #[cfg(target_os = "macos")]
        let root = "/dev/fd";
        PathBuf::from(root)
            .join(self.fd.as_raw_fd().to_string())
            .join(OsStr::from_bytes(name.as_bytes()))
    }

    fn rename_to(
        &self,
        source: &CString,
        destination: &Directory,
        target: &CString,
    ) -> io::Result<()> {
        // SAFETY: both directory descriptors and names are valid.
        if unsafe {
            libc::renameat(
                self.fd.as_raw_fd(),
                source.as_ptr(),
                destination.fd.as_raw_fd(),
                target.as_ptr(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn link_to(
        &self,
        source: &CString,
        destination: &Directory,
        target: &CString,
    ) -> io::Result<()> {
        // `linkat` never overwrites `target`, giving publication its atomic
        // no-replacement guarantee on both Linux and macOS.
        // SAFETY: both directory descriptors and names are valid.
        if unsafe {
            libc::linkat(
                self.fd.as_raw_fd(),
                source.as_ptr(),
                destination.fd.as_raw_fd(),
                target.as_ptr(),
                0,
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn unlink(&self, name: &CString, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: the directory descriptor and name are valid.
        if unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), flags) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn try_clone(&self) -> io::Result<Self> {
        // SAFETY: fcntl duplicates the live descriptor with close-on-exec.
        let fd = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        owned_fd(fd)
    }
}

struct StagingDirectory {
    parent: Directory,
    parent_name: CString,
    directory: Directory,
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        // `bound` is always our newly-created unpublished inode. Candidate
        // and cleanup entries may instead be a concurrently swapped public
        // entry that could not be restored because another replacement won
        // the public name. Never delete those on an error path: rmdir then
        // deliberately fails and leaves the quarantined entry recoverable.
        let _ = self.directory.unlink(&name("bound"), 0);
        let _ = self.parent.unlink(&self.parent_name, libc::AT_REMOVEDIR);
    }
}

/// An already-bound, privately published listener whose cleanup is pinned to
/// the exact filesystem inode created during binding.
pub struct BoundSocket {
    listener: UnixListener,
    parent: Directory,
    public_name: CString,
    identity: FileIdentity,
    staging: StagingDirectory,
}

impl BoundSocket {
    pub fn bind(path: &Path) -> io::Result<Self> {
        Self::bind_with_hooks(path, &mut |_, _| {}, |_, _| {})
    }

    #[cfg(test)]
    fn bind_with_hook(path: &Path, hook: impl FnMut(BindPhase, &Path)) -> io::Result<Self> {
        Self::bind_with_hooks(path, &mut |_, _| {}, hook)
    }

    #[cfg(test)]
    fn bind_with_parent_hook(
        path: &Path,
        mut after_parent_create: impl FnMut(&Directory, &OsStr),
    ) -> io::Result<Self> {
        Self::bind_with_hooks(path, &mut after_parent_create, |_, _| {})
    }

    fn bind_with_hooks(
        path: &Path,
        after_parent_create: &mut impl FnMut(&Directory, &OsStr),
        mut hook: impl FnMut(BindPhase, &Path),
    ) -> io::Result<Self> {
        validate_socket_path(path)?;
        let (parent, public_name) =
            Directory::open_socket_parent_with_hook(path, after_parent_create).map_err(
                |error| {
                    contextual(
                        path,
                        "cannot open socket parent without following symlinks",
                        error,
                    )
                },
            )?;
        let (staging_parent, staging) = parent.create_private_child().map_err(|error| {
            contextual(
                path,
                "cannot create private socket staging directory",
                error,
            )
        })?;
        let staging = StagingDirectory {
            parent: parent.try_clone()?,
            parent_name: staging_parent,
            directory: staging,
        };
        let candidate = name("candidate");
        quarantine_existing(&parent, &public_name, &staging.directory, &candidate, path)?;

        let unpublished = name("bound");
        let bind_path = staging.directory.access_path(&unpublished);
        let listener = UnixListener::bind(&bind_path)
            .map_err(|error| contextual(path, "cannot bind private socket", error))?;
        let before = staging.directory.metadata(&unpublished)?;
        let identity = FileIdentity::from_entry(before);
        hook(BindPhase::BeforeModeLockdown, &bind_path);

        // The entry is still unreachable through the public path and lives in
        // a 0700 directory. Set its exact eventual public mode descriptor-
        // relative, then prove the same inode survived the operation.
        // SAFETY: fd/name are valid; AT_SYMLINK_NOFOLLOW rejects a symlink swap.
        if unsafe {
            libc::fchmodat(
                staging.directory.fd.as_raw_fd(),
                unpublished.as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == -1
        {
            return Err(contextual(
                path,
                "cannot set unpublished socket mode",
                io::Error::last_os_error(),
            ));
        }
        let secured = staging.directory.metadata(&unpublished)?;
        if FileIdentity::from_entry(secured) != identity || secured.permissions() != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unpublished socket identity or mode changed before publication",
            ));
        }

        hook(BindPhase::BeforePublication, path);
        staging
            .directory
            .link_to(&unpublished, &parent, &public_name)
            .map_err(|error| {
                contextual(path, "cannot publish socket without replacement", error)
            })?;
        let published = parent.metadata(&public_name)?;
        if FileIdentity::from_entry(published) != identity || published.permissions() != 0o600 {
            let _ = cleanup_if_identity(&parent, &public_name, &staging.directory, identity);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "published socket identity or mode did not match bound socket",
            ));
        }
        staging.directory.unlink(&unpublished, 0)?;
        Ok(Self {
            listener,
            parent,
            public_name,
            identity,
            staging,
        })
    }

    pub(crate) fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = cleanup_if_identity(
            &self.parent,
            &self.public_name,
            &self.staging.directory,
            self.identity,
        );
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum BindPhase {
    BeforeModeLockdown,
    BeforePublication,
}

fn quarantine_existing(
    parent: &Directory,
    public_name: &CString,
    staging: &Directory,
    candidate: &CString,
    display_path: &Path,
) -> io::Result<()> {
    match parent.rename_to(public_name, staging, candidate) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(contextual(
                display_path,
                "cannot quarantine socket path",
                error,
            ));
        }
    }
    let metadata = staging.metadata(candidate)?;
    let ours = metadata.uid == effective_uid();
    if !metadata.is_socket() || !ours {
        restore_quarantined(parent, public_name, staging, candidate)?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to replace a non-socket or differently owned socket path",
        ));
    }
    if UnixStream::connect(staging.access_path(candidate)).is_ok() {
        restore_quarantined(parent, public_name, staging, candidate)?;
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "kernel already listening",
        ));
    }
    staging.unlink(candidate, 0)
}

fn restore_quarantined(
    parent: &Directory,
    public_name: &CString,
    staging: &Directory,
    candidate: &CString,
) -> io::Result<()> {
    staging.link_to(candidate, parent, public_name)?;
    staging.unlink(candidate, 0)
}

fn cleanup_if_identity(
    parent: &Directory,
    public_name: &CString,
    staging: &Directory,
    identity: FileIdentity,
) -> io::Result<()> {
    let cleanup = name("cleanup");
    match parent.rename_to(public_name, staging, &cleanup) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let found = FileIdentity::from_entry(staging.metadata(&cleanup)?);
    if found == identity {
        staging.unlink(&cleanup, 0)
    } else {
        restore_quarantined(parent, public_name, staging, &cleanup)
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn name(value: &str) -> CString {
    CString::new(value).expect("static name has no NUL")
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path contains a NUL byte",
        )
    })
}

fn owned_fd(fd: RawFd) -> io::Result<Directory> {
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: successful open/openat returned a new owned descriptor.
        Ok(Directory {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }
}

fn contextual(path: &Path, action: &str, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{action} {}: {error}", path.display()),
    )
}

fn validate_socket_path(path: &Path) -> io::Result<()> {
    // The public hard link still has to fit in a native sockaddr_un when a
    // client connects. Leave one byte for its terminating NUL.
    // SAFETY: an all-zero sockaddr_un is valid for inspecting array capacity.
    let address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    let capacity = address.sun_path.len();
    if path.as_os_str().as_bytes().len() >= capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unix socket path exceeds the {capacity}-byte platform limit"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kernel;
    use crate::connection_worker::{ConnectionJob, ConnectionSpawner};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    #[test]
    fn publishes_exact_socket_at_0600_and_removes_it_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run/kernel.sock");
        let bound = BoundSocket::bind(&path).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(UnixStream::connect(&path).is_ok());
        drop(bound);
        assert!(!path.exists());
    }

    #[test]
    fn fresh_parents_are_0700_but_an_explicit_shared_parent_is_not_chmoded() {
        let root = tempfile::tempdir().unwrap();
        let fresh_path = root.path().join("fresh/run/kernel.sock");
        let fresh = BoundSocket::bind(&fresh_path).unwrap();
        assert_eq!(
            fs::metadata(fresh_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(fresh);

        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        let explicit = BoundSocket::bind(&shared.join("kernel.sock")).unwrap();
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o777
        );
        drop(explicit);
    }

    #[test]
    fn fresh_parent_substitution_is_rejected_without_chmod_or_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run/kernel.sock");
        let displaced = root.path().join("created-by-shoal");
        let replacement = root.path().join("run");
        let mut swapped = false;

        let result = BoundSocket::bind_with_parent_hook(&path, |_, created_name| {
            assert_eq!(created_name, OsStr::new("run"));
            assert!(!swapped, "only the final parent should be newly created");
            fs::rename(&replacement, &displaced).unwrap();
            fs::create_dir(&replacement).unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755)).unwrap();
            swapped = true;
        });
        let error = match result {
            Ok(_) => panic!("a substituted fresh parent must not reach socket readiness"),
            Err(error) => error,
        };

        assert!(swapped);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            fs::metadata(&replacement).unwrap().permissions().mode() & 0o777,
            0o755,
            "the substituted directory must never be chmoded"
        );
        assert!(
            displaced.is_dir(),
            "Shoal's original directory is contained"
        );
        assert!(
            !path.exists(),
            "failure must not publish a socket beneath the replacement"
        );
    }

    #[test]
    fn stale_socket_is_removed_but_a_live_listener_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("stale.sock");
        drop(UnixListener::bind(&stale).unwrap());
        let replacement = BoundSocket::bind(&stale).unwrap();
        drop(replacement);

        let live_path = root.path().join("live.sock");
        let live = UnixListener::bind(&live_path).unwrap();
        let error = match BoundSocket::bind(&live_path) {
            Ok(_) => panic!("must not replace a live listener"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(UnixStream::connect(&live_path).is_ok());
        drop(live);
    }

    #[test]
    fn cleanup_preserves_a_post_bind_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("kernel.sock");
        let bound = BoundSocket::bind(&path).unwrap();
        let original = fs::symlink_metadata(&path).unwrap();
        let displaced = root.path().join("original.sock");
        fs::rename(&path, &displaced).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        let replacement_identity =
            FileIdentity::from_metadata(&fs::symlink_metadata(&path).unwrap());
        assert_ne!(replacement_identity, FileIdentity::from_metadata(&original));
        drop(bound);
        assert_eq!(
            FileIdentity::from_metadata(&fs::symlink_metadata(&path).unwrap()),
            replacement_identity
        );
        assert!(UnixStream::connect(&path).is_ok());
        drop(replacement);
    }

    #[test]
    fn pinned_parent_cleanup_ignores_an_ancestor_path_swap() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("run");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("kernel.sock");
        let bound = BoundSocket::bind(&path).unwrap();

        let moved_parent = root.path().join("moved-run");
        fs::rename(&parent, &moved_parent).unwrap();
        fs::create_dir(&parent).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        drop(bound);

        assert!(!moved_parent.join("kernel.sock").exists());
        assert!(UnixStream::connect(&path).is_ok());
        drop(replacement);
    }

    #[test]
    fn stale_cleanup_preserves_a_final_non_socket_swap() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("kernel.sock");
        drop(UnixListener::bind(&path).unwrap());
        let displaced = root.path().join("old.sock");
        fs::rename(&path, &displaced).unwrap();
        fs::write(&path, b"replacement").unwrap();

        let error = match BoundSocket::bind(&path) {
            Ok(_) => panic!("must not replace a non-socket final entry"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn restore_collision_keeps_both_the_quarantined_entry_and_new_final() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("kernel.sock");
        let (parent, public_name) = Directory::open_socket_parent(&path).unwrap();
        let (staging_name, staging_directory) = parent.create_private_child().unwrap();
        let staging_path = root.path().join(OsStr::from_bytes(staging_name.as_bytes()));
        let staging = StagingDirectory {
            parent: parent.try_clone().unwrap(),
            parent_name: staging_name,
            directory: staging_directory,
        };
        let original = UnixListener::bind(&path).unwrap();
        let candidate = name("candidate");
        parent
            .rename_to(&public_name, &staging.directory, &candidate)
            .unwrap();
        let newest = UnixListener::bind(&path).unwrap();

        let error =
            restore_quarantined(&parent, &public_name, &staging.directory, &candidate).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        drop(staging);
        assert!(staging_path.join("candidate").exists());
        assert!(UnixStream::connect(&path).is_ok());
        drop(original);
        drop(newest);
    }

    #[test]
    fn post_bind_identity_swap_is_rejected_before_publication() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("kernel.sock");
        let mut replacement = None;
        let error = match BoundSocket::bind_with_hook(&path, |phase, hidden| {
            if phase == BindPhase::BeforeModeLockdown {
                let displaced = hidden.with_file_name("displaced");
                fs::rename(hidden, &displaced).unwrap();
                replacement = Some(UnixListener::bind(hidden).unwrap());
                fs::remove_file(displaced).unwrap();
            }
        }) {
            Ok(_) => panic!("must reject a swapped unpublished entry"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!path.exists());
        drop(replacement);
    }

    #[test]
    fn publication_never_overwrites_a_last_moment_final_swap() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("kernel.sock");
        let mut replacement = None;
        let error = match BoundSocket::bind_with_hook(&path, |phase, public| {
            if phase == BindPhase::BeforePublication {
                replacement = Some(UnixListener::bind(public).unwrap());
            }
        }) {
            Ok(_) => panic!("atomic publication must not overwrite a replacement"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(UnixStream::connect(&path).is_ok());
        drop(replacement);
    }

    #[test]
    fn rejects_a_symlinked_parent_component_without_touching_its_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("swapped")).unwrap();
        let path = root.path().join("swapped/kernel.sock");
        assert!(BoundSocket::bind(&path).is_err());
        assert!(!target.join("kernel.sock").exists());
    }

    struct FailFirstSpawn {
        calls: Arc<AtomicUsize>,
    }

    impl ConnectionSpawner for FailFirstSpawn {
        fn spawn(&self, job: ConnectionJob) -> io::Result<()> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                drop(job);
                Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "injected worker exhaustion",
                ))
            } else {
                std::thread::spawn(job);
                Ok(())
            }
        }
    }

    #[test]
    fn worker_spawn_failure_releases_connection_and_keeps_accepting() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("kernel.sock");
        let bound = BoundSocket::bind(&path).unwrap();
        let kernel = Kernel::new();
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let server_kernel = kernel.clone();
        let server_stop = stop.clone();
        let server_calls = calls.clone();
        let server = std::thread::spawn(move || {
            server_kernel
                .serve_bound_until_with_spawner(
                    bound,
                    server_stop,
                    &FailFirstSpawn {
                        calls: server_calls,
                    },
                )
                .unwrap();
        });

        let first = UnixStream::connect(&path).unwrap();
        wait_for(|| calls.load(Ordering::SeqCst) >= 1);
        drop(first);
        let second = UnixStream::connect(&path).unwrap();
        wait_for(|| calls.load(Ordering::SeqCst) >= 2);
        stop.store(true, Ordering::SeqCst);
        drop(second);
        server.join().unwrap();
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    fn wait_for(mut predicate: impl FnMut() -> bool) {
        for _ in 0..200 {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition was not reached before timeout");
    }
}
