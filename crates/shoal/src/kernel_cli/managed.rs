//! Owner-only bearer used by the explicit `shoal kernel start/stop` pair.

use shoal_auth::TokenStore;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_CREDENTIAL_BYTES: u64 = 256;
const LIFECYCLE_LOCK_TIMEOUT: Duration = Duration::from_secs(6);

pub(super) struct Credential {
    token: String,
    id: String,
    pid: Option<u32>,
    path: PathBuf,
    store_path: PathBuf,
    // Serializes the complete provision -> PID bind -> readiness transaction
    // for this socket. Without retaining it, two starts could replace and
    // revoke each other's credential before either listener became ready.
    _lifecycle_lock: Option<fs::File>,
}

impl Credential {
    pub(super) fn provision(socket: &Path) -> Result<Option<Self>, String> {
        let paths = shoal_paths::ShoalPaths::discover();
        let store_path = paths.token_store(paths.state_dir());
        let path = credential_path(paths.state_dir(), socket);
        let lifecycle_lock = acquire_lifecycle_lock(&path)
            .map_err(|error| format!("cannot acquire managed kernel lifecycle lock: {error}"))?;
        // The first start retains the lock through readiness. A waiter must
        // re-probe after acquiring it rather than replacing the now-live
        // winner's credential based on its stale pre-lock socket probe.
        if UnixStream::connect(socket).is_ok() {
            return Ok(None);
        }
        let mut previous = match Self::read(&path, &store_path) {
            Ok(credential) => Some(credential),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "cannot inspect existing managed kernel credential: {error}"
                ));
            }
        };
        let mut store = TokenStore::open(&store_path).map_err(describe_authority)?;
        if let Some(existing) = &previous {
            match store.validate_checked(&existing.token) {
                Ok(Some(meta)) if meta.id == existing.id && meta.profile == "supervisor" => {}
                Ok(None) => {
                    remove_matching_credential(&path, &store_path, &existing.id).map_err(
                        |error| {
                            format!(
                                "cannot remove revoked managed kernel credential before replacement: {error}"
                            )
                        },
                    )?;
                    previous = None;
                }
                Ok(Some(_)) => {
                    return Err(
                        "existing managed kernel credential does not identify supervisor authority"
                            .into(),
                    );
                }
                Err(error) => {
                    return Err(format!(
                        "cannot validate existing managed kernel credential: {error}"
                    ));
                }
            }
        }
        let (token, meta) = store
            .create(
                format!("uid:{}:shoal-kernel-supervisor", effective_uid()),
                "supervisor".into(),
                Vec::new(),
                None,
            )
            .map_err(describe_authority)?;
        let credential = Self {
            token,
            id: meta.id,
            pid: None,
            path,
            store_path,
            _lifecycle_lock: Some(lifecycle_lock),
        };
        if let Err(error) = credential.persist() {
            return Err(combine_authority_cleanup_error(
                format!("cannot persist managed kernel credential: {error}"),
                credential.revoke_and_remove(),
            ));
        }
        if let Some(previous) = previous
            && let Err(error) = store.revoke(&previous.id)
        {
            return Err(combine_authority_cleanup_error(
                format!("cannot revoke replaced managed kernel credential: {error}"),
                credential.revoke_and_remove(),
            ));
        }
        Ok(Some(credential))
    }

    pub(super) fn load(socket: &Path) -> Result<Option<Self>, String> {
        let paths = shoal_paths::ShoalPaths::discover();
        let store_path = paths.token_store(paths.state_dir());
        let path = credential_path(paths.state_dir(), socket);
        match Self::read(&path, &store_path) {
            Ok(credential) => {
                let store = TokenStore::open(&store_path).map_err(describe_authority)?;
                match store.validate_checked(&credential.token) {
                    Ok(Some(meta)) if meta.id == credential.id && meta.profile == "supervisor" => {
                        Ok(Some(credential))
                    }
                    Ok(_) => Err("managed kernel credential is no longer authorized".into()),
                    Err(error) => Err(format!(
                        "cannot validate managed kernel credential: {error}"
                    )),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("cannot read managed kernel credential: {error}")),
        }
    }

    pub(super) fn token(&self) -> &str {
        &self.token
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub(super) fn bind_pid(&mut self, pid: u32) -> Result<(), String> {
        let current = Self::read(&self.path, &self.store_path)
            .map_err(|error| format!("cannot revalidate managed kernel credential: {error}"))?;
        if current.id != self.id || current.token != self.token {
            return Err("managed kernel credential changed before PID binding".into());
        }
        self.pid = Some(pid);
        self.persist()
            .map_err(|error| format!("cannot bind managed kernel PID: {error}"))
    }

    pub(super) fn revoke_and_remove(&self) -> Result<(), String> {
        let revoke = TokenStore::open(&self.store_path)
            .and_then(|mut store| store.revoke(&self.id).map(|_| ()))
            .map_err(describe_authority);
        let remove = remove_matching_credential(&self.path, &self.store_path, &self.id)
            .map_err(|error| format!("cannot remove managed kernel credential: {error}"));
        combine_cleanup_results(revoke, remove)
    }

    fn persist(&self) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "credential needs parent")
        })?;
        let directory = secure_credential_dir(parent, true)?;
        let name = credential_name(&self.path)?;
        self.persist_at(&directory, name)
    }

    fn persist_at(&self, directory: &fs::File, name: &OsStr) -> io::Result<()> {
        let (tmp, mut file) = create_random_temp(directory, name)?;
        let publish = (|| {
            writeln!(file, "{}", self.id)?;
            writeln!(file, "{}", self.token)?;
            writeln!(
                file,
                "{}",
                self.pid
                    .map_or_else(|| "pending".into(), |pid| pid.to_string())
            )?;
            file.sync_all()?;
            drop(file);
            rename_at(directory, &tmp, name)
        })();
        if let Err(error) = publish {
            let _ = unlink_at(directory, &tmp);
            return Err(error);
        }
        directory.sync_all()
    }

    fn read(path: &Path, store_path: &Path) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "credential needs parent")
        })?;
        let directory = secure_credential_dir(parent, false)?;
        let file = open_at(
            &directory,
            credential_name(path)?,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )?;
        Self::read_open(file, path, store_path)
    }

    fn read_open(file: fs::File, path: &Path, store_path: &Path) -> io::Result<Self> {
        // Validate the already-open object, rather than checking a pathname
        // and reopening it across a symlink/replacement race.
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != effective_uid()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > MAX_CREDENTIAL_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "credential is not an owner-only regular file",
            ));
        }
        let mut contents = String::new();
        file.take(MAX_CREDENTIAL_BYTES + 1)
            .read_to_string(&mut contents)?;
        if contents.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "credential exceeds its read wall",
            ));
        }
        let mut lines = contents.lines();
        let id = lines
            .next()
            .filter(|line| !line.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "credential id is missing")
            })?;
        let token = lines
            .next()
            .filter(|line| !line.is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "credential bearer is missing")
            })?;
        let pid = match lines.next() {
            Some("pending") => None,
            Some(pid) => Some(pid.parse::<u32>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "credential PID is invalid")
            })?),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "credential PID is missing",
                ));
            }
        };
        if lines.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "credential has trailing fields",
            ));
        }
        Ok(Self {
            token: token.into(),
            id: id.into(),
            pid,
            path: path.into(),
            store_path: store_path.into(),
            _lifecycle_lock: None,
        })
    }
}

fn acquire_lifecycle_lock(credential_path: &Path) -> io::Result<fs::File> {
    acquire_lifecycle_lock_with_timeout(credential_path, LIFECYCLE_LOCK_TIMEOUT)
}

fn acquire_lifecycle_lock_with_timeout(
    credential_path: &Path,
    timeout: Duration,
) -> io::Result<fs::File> {
    let parent = credential_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "credential needs parent"))?;
    let directory = secure_credential_dir(parent, true)?;
    let mut name = credential_name(credential_path)?.to_os_string();
    name.push(".lock");
    let lock = open_at(
        &directory,
        &name,
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0o600,
    )?;
    let metadata = lock.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed lifecycle lock is not an owner-only regular file",
        ));
    }
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: lock is a live owned descriptor; flock is supported on the
        // two platforms admitted by ensure_supported_credential_platform.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if !error
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "another managed kernel lifecycle did not finish within {}ms",
                    timeout.as_millis()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(lock)
}

fn secure_credential_dir(path: &Path, create: bool) -> io::Result<fs::File> {
    ensure_supported_credential_platform()?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.uid() != effective_uid() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "managed credential parent is not an owned directory",
                ));
            }
            // Opening with O_NOFOLLOW revalidates the path after metadata and
            // rejects a symlink substitution. The returned descriptor remains
            // the authority for every child operation, so a later path swap
            // cannot redirect credential I/O.
            let directory = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)?;
            let opened = directory.metadata()?;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "managed credential parent changed during validation",
                ));
            }
            if opened.permissions().mode() & 0o077 != 0 {
                directory.set_permissions(fs::Permissions::from_mode(0o700))?;
            }
            Ok(directory)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            if let Some(ancestor) = path.parent() {
                fs::create_dir_all(ancestor)?;
            }
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700).create(path)?;
            secure_credential_dir(path, false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_supported_credential_platform() -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ensure_supported_credential_platform() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "managed kernel credentials require Linux or macOS directory capabilities",
    ))
}

fn create_random_temp(directory: &fs::File, name: &OsStr) -> io::Result<(OsString, fs::File)> {
    for _ in 0..8 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let suffix = blake3::hash(&random).to_hex();
        let mut candidate = name.to_os_string();
        candidate.push(format!(".tmp.{suffix}"));
        match open_at(
            directory,
            &candidate,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique credential temporary file",
    ))
}

fn credential_name(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "credential needs a file name"))
}

fn relative_name(name: &OsStr) -> io::Result<CString> {
    if name.as_bytes().contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory-relative credential name contains a separator",
        ));
    }
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "credential name contains NUL"))
}

fn open_at(
    directory: &fs::File,
    name: &OsStr,
    flags: libc::c_int,
    mode: u32,
) -> io::Result<fs::File> {
    let name = relative_name(name)?;
    // SAFETY: the directory fd and C string are valid for the duration of the
    // call. Successful ownership of the returned descriptor transfers to File.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: openat returned a fresh owned descriptor.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

fn rename_at(directory: &fs::File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let from = relative_name(from)?;
    let to = relative_name(to)?;
    // SAFETY: both names are valid C strings and both descriptors remain open.
    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlink_at(directory: &fs::File, name: &OsStr) -> io::Result<()> {
    let name = relative_name(name)?;
    // SAFETY: the name and retained directory descriptor are valid.
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn link_at(directory: &fs::File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let from = relative_name(from)?;
    let to = relative_name(to)?;
    // SAFETY: both names are valid C strings and both descriptors remain open.
    let result = unsafe {
        libc::linkat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn remove_matching_credential(path: &Path, store_path: &Path, expected_id: &str) -> io::Result<()> {
    remove_matching_credential_with(path, store_path, expected_id, || {})
}

fn remove_matching_credential_with(
    path: &Path,
    store_path: &Path,
    expected_id: &str,
    after_containment: impl FnOnce(),
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "credential needs parent"))?;
    let directory = secure_credential_dir(parent, false)?;
    let name = credential_name(path)?;

    // Contain the currently named object before inspecting it. All subsequent
    // checks and deletion address the unpredictable quarantine entry through
    // the retained directory capability, never the replaceable public name.
    let (quarantine, reservation) = create_random_temp(&directory, name)?;
    drop(reservation);
    match rename_at(&directory, name, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            unlink_at(&directory, &quarantine)?;
            return Ok(());
        }
        Err(error) => {
            let _ = unlink_at(&directory, &quarantine);
            return Err(error);
        }
    }
    after_containment();

    let captured = open_at(
        &directory,
        &quarantine,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
    .and_then(|file| Credential::read_open(file, path, store_path));
    if captured.is_ok_and(|credential| credential.id == expected_id) {
        unlink_at(&directory, &quarantine)?;
        directory.sync_all()?;
        return Ok(());
    }

    // This was a concurrent replacement, not the revoked credential. Restore
    // it without overwriting any still-newer publisher. linkat is atomic and
    // fails with EEXIST instead of replacing the public name.
    if let Err(error) = link_at(&directory, &quarantine, name) {
        return Err(io::Error::new(
            error.kind(),
            format!(
                "captured credential did not match revoked id and could not be restored: {error}"
            ),
        ));
    }
    unlink_at(&directory, &quarantine)?;
    directory.sync_all()
}

fn credential_path(state_dir: &Path, socket: &Path) -> PathBuf {
    let digest = blake3::hash(socket.as_os_str().as_bytes());
    state_dir
        .join("managed-kernels")
        .join(format!("{}.credential", digest.to_hex()))
}

fn describe_authority(error: io::Error) -> String {
    format!("cannot update managed kernel authority: {error}")
}

fn combine_authority_cleanup_error(primary: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => format!("{primary}; managed credential cleanup also failed: {cleanup}"),
    }
}

fn combine_cleanup_results(
    revoke: Result<(), String>,
    remove: Result<(), String>,
) -> Result<(), String> {
    match (revoke, remove) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(revoke), Err(remove)) => Err(format!("{revoke}; removal also failed: {remove}")),
    }
}

fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn test_credential(path: PathBuf, id: &str) -> Credential {
        Credential {
            token: format!("shoal_test_{id}"),
            id: id.into(),
            pid: Some(42),
            store_path: path.with_extension("tokens.json"),
            path,
            _lifecycle_lock: None,
        }
    }

    #[test]
    fn retained_directory_capability_survives_parent_path_swap() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("managed-kernels");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let credential = test_credential(parent.join("kernel.credential"), "expected");
        let directory = secure_credential_dir(&parent, false).unwrap();

        let retained = root.path().join("retained-parent");
        fs::rename(&parent, &retained).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();

        credential
            .persist_at(&directory, credential_name(&credential.path).unwrap())
            .unwrap();
        assert!(!credential.path.exists(), "replacement parent was modified");
        let retained_path = retained.join("kernel.credential");
        assert!(retained_path.exists());
        let opened = open_at(
            &directory,
            OsStr::new("kernel.credential"),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .unwrap();
        assert_eq!(
            Credential::read_open(opened, &retained_path, &credential.store_path)
                .unwrap()
                .id,
            "expected"
        );
    }

    #[test]
    fn conditional_removal_preserves_replacements() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("managed-kernels");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join("kernel.credential");
        let expected = test_credential(path.clone(), "expected");
        expected.persist().unwrap();

        let replacement_contents = "replacement\nshoal_test_replacement\n77\n";
        remove_matching_credential_with(&path, &expected.store_path, &expected.id, || {
            fs::write(&path, replacement_contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), replacement_contents);

        // If replacement won before containment, its identity fails the
        // expected-id check and it is restored without overwrite.
        remove_matching_credential(&path, &expected.store_path, &expected.id).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), replacement_contents);
        let entries = fs::read_dir(&parent)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1, "quarantine entry leaked");
    }

    #[test]
    fn credential_is_owner_only_validated_and_revoked() {
        let _environment = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let old_state = std::env::var_os("SHOAL_STATE_DIR");
        // SAFETY: the crate-wide environment test lock serializes this mutation.
        unsafe { std::env::set_var("SHOAL_STATE_DIR", dir.path()) };
        let socket = dir.path().join("run/kernel.sock");
        let credential = Credential::provision(&socket).unwrap().unwrap();
        let loaded = Credential::load(&socket).unwrap().unwrap();
        assert_eq!(loaded.id, credential.id);
        assert_eq!(loaded.pid(), None);
        let mut credential = credential;
        credential.bind_pid(4242).unwrap();
        assert_eq!(
            Credential::load(&socket).unwrap().unwrap().pid(),
            Some(4242)
        );
        assert_eq!(
            fs::metadata(&credential.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        credential.revoke_and_remove().unwrap();
        assert!(Credential::load(&socket).unwrap().is_none());

        let target = dir.path().join("attacker-target");
        fs::write(&target, "id\ntoken\n42\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &credential.path).unwrap();
        assert!(
            Credential::load(&socket)
                .err()
                .unwrap()
                .contains("cannot read managed kernel credential")
        );
        fs::remove_file(&credential.path).unwrap();

        let parent = credential.path.parent().unwrap();
        let mut lock_name = credential.path.file_name().unwrap().to_os_string();
        lock_name.push(".lock");
        drop(credential._lifecycle_lock.take());
        fs::remove_file(parent.join(lock_name)).unwrap();
        fs::remove_dir(parent).unwrap();
        let redirected = dir.path().join("redirected");
        fs::create_dir(&redirected).unwrap();
        symlink(&redirected, parent).unwrap();
        assert!(
            Credential::provision(&socket)
                .err()
                .unwrap()
                .contains("managed kernel lifecycle lock")
        );
        if let Some(value) = old_state {
            // SAFETY: paired restoration for the test-only mutation above.
            unsafe { std::env::set_var("SHOAL_STATE_DIR", value) };
        } else {
            // SAFETY: paired restoration for the test-only mutation above.
            unsafe { std::env::remove_var("SHOAL_STATE_DIR") };
        }
    }

    #[test]
    fn serialized_start_waiter_preserves_the_live_winners_credential() {
        let _environment = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let old_state = std::env::var_os("SHOAL_STATE_DIR");
        // SAFETY: the crate-wide environment test lock serializes this mutation.
        unsafe { std::env::set_var("SHOAL_STATE_DIR", dir.path()) };
        let socket = dir.path().join("run/kernel.sock");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let mut winner = Credential::provision(&socket).unwrap().unwrap();
        winner.bind_pid(4242).unwrap();
        let winner_id = winner.id.clone();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(winner._lifecycle_lock.take());

        assert!(
            Credential::provision(&socket).unwrap().is_none(),
            "a waiter must adopt the now-live socket without replacing authority"
        );
        let retained = Credential::load(&socket).unwrap().unwrap();
        assert_eq!(retained.id, winner_id);
        retained.revoke_and_remove().unwrap();
        drop(listener);

        if let Some(value) = old_state {
            // SAFETY: paired restoration for the test-only mutation above.
            unsafe { std::env::set_var("SHOAL_STATE_DIR", value) };
        } else {
            // SAFETY: paired restoration for the test-only mutation above.
            unsafe { std::env::remove_var("SHOAL_STATE_DIR") };
        }
    }

    #[test]
    fn lifecycle_lock_contention_has_a_bounded_failure() {
        let root = tempfile::tempdir().unwrap();
        let credential = root.path().join("managed-kernels/kernel.credential");
        let _held = acquire_lifecycle_lock(&credential).unwrap();
        let start = Instant::now();
        let error = acquire_lifecycle_lock_with_timeout(&credential, Duration::from_millis(20))
            .expect_err("a contended lifecycle lock must not wait forever");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("20ms"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn revoked_stale_credential_does_not_block_reprovisioning() {
        let _environment = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let old_state = std::env::var_os("SHOAL_STATE_DIR");
        // SAFETY: the crate-wide environment test lock serializes this mutation.
        unsafe { std::env::set_var("SHOAL_STATE_DIR", dir.path()) };
        let socket = dir.path().join("run/kernel.sock");
        let stale = Credential::provision(&socket).unwrap().unwrap();
        let stale_id = stale.id.clone();
        let store_path = stale.store_path.clone();
        let mut store = TokenStore::open(&store_path).unwrap();
        assert!(store.revoke(&stale_id).unwrap());
        drop(stale);

        let replacement = Credential::provision(&socket).unwrap().unwrap();
        assert_ne!(replacement.id, stale_id);
        assert_eq!(
            Credential::load(&socket).unwrap().unwrap().id,
            replacement.id
        );
        replacement.revoke_and_remove().unwrap();

        if let Some(value) = old_state {
            // SAFETY: paired restoration for the test-only mutation above.
            unsafe { std::env::set_var("SHOAL_STATE_DIR", value) };
        } else {
            // SAFETY: paired restoration for the test-only mutation above.
            unsafe { std::env::remove_var("SHOAL_STATE_DIR") };
        }
    }
}
