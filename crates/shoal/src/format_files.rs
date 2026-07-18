//! Filesystem transaction boundary for `shoal fmt`.
//!
//! Every input is parsed and admitted by the trivia-preservation policy before
//! the first file is changed. Rewrites refuse links and metadata that an
//! atomic replacement could silently discard, retain ordinary permissions,
//! sync file contents, and sync the containing directory entry.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use shoal_syntax::parse;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct FormatPlan {
    path: PathBuf,
    formatted: String,
    snapshot: FileSnapshot,
}

struct FileSnapshot {
    permissions: fs::Permissions,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    owner: u32,
}

pub(crate) fn run(check: bool, files: Vec<PathBuf>) -> Result<i32, String> {
    if files.is_empty() {
        let src = crate::args::read_source_stream(io::stdin().lock(), "stdin")?;
        let ast = parse(&src).map_err(|error| format!("stdin: {error}"))?;
        let formatted = format_source_safely(&src, &ast);
        if check {
            return Ok(i32::from(formatted != src));
        }
        io::stdout()
            .lock()
            .write_all(formatted.as_bytes())
            .map_err(|error| format!("cannot write stdout: {error}"))?;
        return Ok(0);
    }

    // Build the complete plan first. A malformed or unsafe later input must
    // never leave an earlier input rewritten as a side effect.
    let plans = files
        .iter()
        .map(|path| prepare(path, !check))
        .collect::<Result<Vec<_>, _>>()?;
    let changed = plans.iter().any(Option::is_some);
    if check {
        return Ok(i32::from(changed));
    }
    for plan in plans.into_iter().flatten() {
        commit(plan)?;
    }
    Ok(0)
}

fn prepare(path: &Path, will_replace: bool) -> Result<Option<FormatPlan>, String> {
    let (file, metadata) = open_regular_no_follow(path)?;
    let snapshot = FileSnapshot::new(&metadata);
    let src = crate::args::read_source_stream(&file, &path.display().to_string())?;
    let ast = parse(&src).map_err(|error| format!("{}: {error}", path.display()))?;
    let formatted = format_source_safely(&src, &ast);
    if formatted == src {
        return Ok(None);
    }
    if will_replace {
        validate_replace_metadata(path, &file, &metadata)?;
    }
    Ok(Some(FormatPlan {
        path: path.to_owned(),
        formatted,
        snapshot,
    }))
}

/// The AST intentionally omits free comments/trivia. The syntax crate owns
/// the common token-aware admission policy used by both CLI and LSP.
fn format_source_safely(src: &str, ast: &shoal_ast::Program) -> String {
    shoal_syntax::format_source_preserving_trivia(src, ast).unwrap_or_else(|_| src.to_owned())
}

fn open_regular_no_follow(path: &Path) -> Result<(fs::File, fs::Metadata), String> {
    let link_metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if link_metadata.file_type().is_symlink() {
        return Err(format!(
            "cannot format {}: symbolic links are refused",
            path.display()
        ));
    }
    if !link_metadata.is_file() {
        return Err(format!(
            "cannot format {}: source is not a regular file",
            path.display()
        ));
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || !same_file(&link_metadata, &metadata) {
        return Err(format!(
            "cannot format {}: source changed while it was opened",
            path.display()
        ));
    }
    Ok((file, metadata))
}

fn validate_replace_metadata(
    path: &Path,
    file: &fs::File,
    metadata: &fs::Metadata,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Atomic rename creates a new inode owned by this process. Refuse a
        // file whose ownership could therefore change silently.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "cannot format {}: file is not owned by the current user",
                path.display()
            ));
        }
        if has_extended_metadata(file)
            .map_err(|error| format!("cannot inspect {} metadata: {error}", path.display()))?
        {
            return Err(format!(
                "cannot format {}: extended attributes or ACLs would be lost",
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, file, metadata);
    Ok(())
}

fn commit(plan: FormatPlan) -> Result<(), String> {
    let parent = plan.path.parent().unwrap_or_else(|| Path::new("."));
    let name = plan
        .path
        .file_name()
        .ok_or_else(|| format!("invalid path {}", plan.path.display()))?
        .to_string_lossy();
    let (tmp_path, mut tmp_file) = create_temp(parent, &name).map_err(|error| {
        format!(
            "cannot create temporary file for {}: {error}",
            plan.path.display()
        )
    })?;

    let result = (|| -> io::Result<()> {
        tmp_file.write_all(plan.formatted.as_bytes())?;
        // Writing can clear set-id bits, so restore the complete mode after
        // contents have reached the temporary inode.
        tmp_file.set_permissions(plan.snapshot.permissions.clone())?;
        tmp_file.sync_all()?;

        let (current, metadata) = open_regular_no_follow(&plan.path).map_err(io::Error::other)?;
        if !plan.snapshot.matches(&metadata) {
            return Err(io::Error::other("source changed after formatter preflight"));
        }
        validate_replace_metadata(&plan.path, &current, &metadata).map_err(io::Error::other)?;
        if !managed_metadata_matches(&current, &tmp_file)? {
            return Err(io::Error::other(
                "replacement would change system-managed security metadata",
            ));
        }
        fs::rename(&tmp_path, &plan.path)?;
        // The rename is not durable until the directory entry is synced.
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result.map_err(|error| format!("cannot write {}: {error}", plan.path.display()))
}

fn create_temp(parent: &Path, name: &str) -> io::Result<(PathBuf, fs::File)> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{name}.shoal-fmt-{}-{sequence}",
            std::process::id()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary filename attempts exhausted",
    ))
}

impl FileSnapshot {
    fn new(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                permissions: metadata.permissions(),
                device: metadata.dev(),
                inode: metadata.ino(),
                owner: metadata.uid(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                permissions: metadata.permissions(),
            }
        }
    }

    fn matches(&self, metadata: &fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.device == metadata.dev()
                && self.inode == metadata.ino()
                && self.owner == metadata.uid()
        }
        #[cfg(not(unix))]
        {
            metadata.is_file()
        }
    }
}

#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, right: &fs::Metadata) -> bool {
    right.is_file()
}

#[cfg(target_os = "linux")]
fn has_extended_metadata(file: &fs::File) -> io::Result<bool> {
    Ok(list_xattrs(file)?
        .iter()
        .any(|name| name.as_slice() != b"security.selinux"))
}

#[cfg(target_os = "linux")]
fn list_xattrs(file: &fs::File) -> io::Result<Vec<Vec<u8>>> {
    use std::os::fd::AsRawFd;

    let fd = file.as_raw_fd();
    let count = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0_u8; count as usize];
    let written = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len()) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    names.truncate(written as usize);
    Ok(names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

#[cfg(target_os = "linux")]
fn xattr_value(file: &fs::File, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;

    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "xattr name contains NUL"))?;
    let fd = file.as_raw_fd();
    let count = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
    if count < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENODATA) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut value = vec![0_u8; count as usize];
    let written =
        unsafe { libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    value.truncate(written as usize);
    Ok(Some(value))
}

#[cfg(target_vendor = "apple")]
fn has_extended_metadata(file: &fs::File) -> io::Result<bool> {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;

    let fd = file.as_raw_fd();
    let count = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0, 0) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count != 0 {
        return Ok(true);
    }

    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> *mut c_void;
        fn acl_get_entry(
            acl: *mut c_void,
            entry_id: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_free(object: *mut c_void) -> libc::c_int;
    }
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(code) if [libc::ENOENT, libc::EINVAL, libc::ENOTSUP].contains(&code) => Ok(false),
            _ => Err(error),
        };
    }
    let mut entry = std::ptr::null_mut();
    let status = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
    let free_status = unsafe { acl_free(acl) };
    if status < 0 || free_status < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(status == 1)
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_vendor = "apple"))))]
fn has_extended_metadata(_: &fs::File) -> io::Result<bool> {
    // Shoal's published native targets are Linux and macOS. Refuse writes on
    // other Unix variants until their ACL/xattr APIs have an audited adapter.
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe ACL/extended-attribute detection is unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn managed_metadata_matches(source: &fs::File, replacement: &fs::File) -> io::Result<bool> {
    let name = b"security.selinux";
    Ok(xattr_value(source, name)? == xattr_value(replacement, name)?)
}

#[cfg(not(target_os = "linux"))]
fn managed_metadata_matches(_: &fs::File, _: &fs::File) -> io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_preflight_failure_leaves_earlier_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.shl");
        let malformed = dir.path().join("malformed.shl");
        fs::write(&first, "let x=1").unwrap();
        fs::write(&malformed, "let =").unwrap();

        assert!(run(false, vec![first.clone(), malformed]).is_err());
        assert_eq!(fs::read_to_string(first).unwrap(), "let x=1");
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.shl");
        fs::write(&path, "let x=1").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(run(false, vec![path.clone()]).unwrap(), 0);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn formatter_refuses_symlink_without_changing_link_or_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.shl");
        let link = dir.path().join("link.shl");
        fs::write(&target, "let x=1").unwrap();
        symlink(&target, &link).unwrap();

        let error = run(false, vec![link.clone()]).unwrap_err();
        assert!(error.contains("symbolic links are refused"));
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(target).unwrap(), "let x=1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn formatter_refuses_extended_attributes() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tagged.shl");
        fs::write(&path, "let x=1").unwrap();
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = CString::new("user.shoal-test").unwrap();
        let value = b"keep";
        let status = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if status != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSUP) {
            return;
        }
        assert_eq!(status, 0);

        let error = run(false, vec![path.clone()]).unwrap_err();
        assert!(error.contains("extended attributes or ACLs"));
        assert_eq!(fs::read_to_string(path).unwrap(), "let x=1");
    }

    #[cfg(unix)]
    #[test]
    fn stale_plan_refuses_replacement_and_cleans_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changed.shl");
        fs::write(&path, "let x=1").unwrap();
        let plan = prepare(&path, true).unwrap().unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "let replacement = 2\n").unwrap();

        let error = commit(plan).unwrap_err();
        assert!(error.contains("source changed after formatter preflight"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "let replacement = 2\n");
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("shoal-fmt"))
            .count();
        assert_eq!(leftovers, 0);
    }
}
