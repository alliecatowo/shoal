use super::filesystem::{snapshot_changed_error, snapshot_limit_error};
use super::{
    Fs, FsCopyDestination, FsCopySource, FsEntryIdentity, FsFileSnapshot, FsRemovalTree, ReadSeek,
    copy_destination, copy_source, fs_metadata, removal_tree,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static ATOMIC_REPLACE_SEQ: AtomicU64 = AtomicU64::new(1);

/// The default [`Fs`] adapter: each method forwards to the identical `std::fs`
/// call the evaluator made inline before the port existed.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdFs;

impl Fs for StdFs {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
        Ok(Box::new(fs::File::open(path)?))
    }
    fn read_bounded_stable(&self, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
        use io::Read as _;

        let mut file = fs::File::open(path)?;
        let before = FsFileSnapshot::from_metadata(&file.metadata()?);
        if before.len() > limit as u64 {
            return Err(snapshot_limit_error(limit));
        }
        let mut bytes = Vec::with_capacity((before.len() as usize).min(limit));
        (&mut file)
            .take(limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        let after = FsFileSnapshot::from_metadata(&file.metadata()?);
        let path_after = self.file_snapshot(path)?;
        if before != after || !after.same_file(&path_after) || after != path_after {
            return Err(snapshot_changed_error());
        }
        if bytes.len() > limit {
            return Err(snapshot_limit_error(limit));
        }
        Ok(bytes)
    }
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        fs::write(path, data)
    }
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(data))
    }
    fn open_append(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Ok(Box::new(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?,
        ))
    }
    fn open_write(&self, path: &Path) -> io::Result<Box<dyn io::Write + Send>> {
        Ok(Box::new(
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?,
        ))
    }
    fn touch(&self, path: &Path) -> io::Result<()> {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map(|_| ())
    }
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
    fn atomic_replace(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        use io::Write as _;

        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
        let mut last_collision = None;
        for _ in 0..128 {
            let seq = ATOMIC_REPLACE_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = parent.join(format!(
                ".shoal-atomic-replace-{}-{seq}",
                std::process::id()
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = match options.open(&tmp) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let result = (|| {
                file.write_all(data)?;
                file.sync_all()?;
                drop(file);
                fs::rename(&tmp, path)?;
                // Persist the directory entry as well as the file contents.
                // Unix permits opening a directory for fsync; other platforms
                // do not expose one uniform directory-sync primitive.
                #[cfg(unix)]
                fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            return result;
        }
        Err(last_collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate atomic replacement temporary file",
            )
        }))
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(path)? {
            out.push(entry?.path());
        }
        Ok(out)
    }
    fn read_dir_limited(
        &self,
        path: &Path,
        max_entries: usize,
        max_path_bytes: usize,
    ) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut path_bytes = 0usize;
        for entry in fs::read_dir(path)? {
            if out.len() >= max_entries {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("directory contains more than {max_entries} admitted entries"),
                ));
            }
            let entry = entry?.path();
            path_bytes = path_bytes
                .checked_add(entry.as_os_str().as_encoded_bytes().len())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory path accounting overflowed",
                    )
                })?;
            if path_bytes > max_path_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("directory paths exceed {max_path_bytes} admitted bytes"),
                ));
            }
            out.push(entry);
        }
        Ok(out)
    }
    fn read_dir_prefix(&self, path: &Path, max_entries: usize) -> io::Result<Vec<PathBuf>> {
        fs::read_dir(path)?
            .take(max_entries)
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }
    #[cfg(unix)]
    fn create_private_dir_all(&self, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fn create_private_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
    fn remove_entry_if_unchanged(&self, path: &Path, expected: &FsEntryIdentity) -> io::Result<()> {
        removal_tree::remove_leaf(path, expected)
    }
    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
    #[cfg(unix)]
    fn rename_if_unchanged(
        &self,
        from: &Path,
        to: &Path,
        expected: &FsEntryIdentity,
    ) -> io::Result<()> {
        crate::fs_mutation::rename_if_unchanged(from, to, expected)
    }
    fn open_removal_tree(
        &self,
        path: &Path,
        expected: &FsEntryIdentity,
    ) -> io::Result<Box<dyn FsRemovalTree>> {
        removal_tree::open(path, expected)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        let mut source = fs::File::open(from)?;
        let mut destination = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(to)?;
        io::copy(&mut source, &mut destination)
    }
    fn open_copy_source(&self, path: &Path) -> io::Result<Box<dyn FsCopySource>> {
        copy_source::open(path)
    }
    fn open_copy_destination(&self, path: &Path) -> io::Result<Box<dyn FsCopyDestination>> {
        copy_destination::open(path)
    }
    fn copy_capability_budget(&self) -> io::Result<Option<usize>> {
        copy_source::retained_handle_budget()
    }
    fn set_permissions(&self, path: &Path, permissions: fs::Permissions) -> io::Result<()> {
        fs::set_permissions(path, permissions)
    }
    fn has_extended_attributes(&self, path: &Path) -> io::Result<bool> {
        fs_metadata::has_extended_attributes(path)
    }
    fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()> {
        fs::hard_link(src, dst)
    }
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }
}

#[cfg(test)]
mod fs_tests {
    use super::super::filesystem::collect_bounded_stable;
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    struct GrowingReader {
        inner: io::Cursor<Vec<u8>>,
        grew: Arc<AtomicBool>,
    }

    impl io::Read for GrowingReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.grew.store(true, Ordering::SeqCst);
            self.inner.read(bytes)
        }
    }

    struct EndlessReader;

    impl io::Read for EndlessReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            bytes.fill(b'x');
            Ok(bytes.len())
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let seq = ATOMIC_REPLACE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "shoal-value-atomic-replace-test-{}-{seq}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create atomic-replace test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn replacement_temps(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .expect("read test directory")
            .map(|entry| entry.expect("read directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    name.as_encoded_bytes()
                        .starts_with(b".shoal-atomic-replace-")
                })
            })
            .collect()
    }

    #[test]
    fn atomic_replace_succeeds_and_failed_rename_cleans_its_temp() {
        let root = TestDir::new();
        let target = root.0.join("target");
        fs::write(&target, b"old").expect("write original");
        StdFs
            .atomic_replace(&target, b"new")
            .expect("replace and sync");
        assert_eq!(fs::read(&target).expect("read replacement"), b"new");
        assert!(replacement_temps(&root.0).is_empty());

        let directory_target = root.0.join("directory-target");
        fs::create_dir(&directory_target).expect("create rename-error target");
        StdFs
            .atomic_replace(&directory_target, b"must-not-land")
            .expect_err("a file cannot atomically replace a directory");
        assert!(directory_target.is_dir());
        assert!(
            replacement_temps(&root.0).is_empty(),
            "failed atomic replacement leaked a temporary file"
        );
    }

    #[test]
    fn stable_bounded_read_accepts_exact_boundary_and_rejects_sparse_length() {
        let root = TestDir::new();
        let exact = root.0.join("exact");
        fs::write(&exact, b"12345678").unwrap();
        assert_eq!(StdFs.read_bounded_stable(&exact, 8).unwrap(), b"12345678");

        let sparse = root.0.join("sparse");
        let file = fs::File::create(&sparse).unwrap();
        file.set_len(9).unwrap();
        let error = StdFs.read_bounded_stable(&sparse, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("8-byte"));
    }

    #[test]
    fn stable_read_bounds_a_hostile_reader_and_rejects_growth_or_stat_read_swap() {
        let initial = FsFileSnapshot {
            len: 3,
            modified: None,
            identity: Some((7, 11)),
        };
        let error =
            collect_bounded_stable(EndlessReader, &initial, 8, || Ok(initial.clone())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("8-byte"));

        let grew = Arc::new(AtomicBool::new(false));
        let reader = GrowingReader {
            inner: io::Cursor::new(b"abc".to_vec()),
            grew: grew.clone(),
        };
        let error = collect_bounded_stable(reader, &initial, 8, || {
            assert!(grew.load(Ordering::SeqCst));
            Ok(FsFileSnapshot {
                len: 4,
                ..initial.clone()
            })
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed"));

        let error = collect_bounded_stable(io::Cursor::new(b"abc"), &initial, 8, || {
            Ok(FsFileSnapshot {
                identity: Some((7, 12)),
                ..initial.clone()
            })
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed"));
    }

    #[cfg(unix)]
    #[test]
    fn file_snapshot_identity_detects_longer_inode_replacement() {
        let root = TestDir::new();
        let target = root.0.join("target");
        let replacement = root.0.join("replacement");
        fs::write(&target, b"old").unwrap();
        fs::write(&replacement, b"a much longer replacement").unwrap();
        let before = StdFs.file_snapshot(&target).unwrap();
        fs::rename(&replacement, &target).unwrap();
        let after = StdFs.file_snapshot(&target).unwrap();
        assert!(!before.same_file(&after));
        assert!(after.len() > before.len());
    }

    #[test]
    fn production_directory_reads_enforce_the_limit_while_iterating() {
        let root = TestDir::new();
        for name in ["a", "b", "c"] {
            fs::write(root.0.join(name), name).expect("write directory fixture");
        }
        let error = StdFs
            .read_dir_bounded(&root.0, 2)
            .expect_err("a third entry must exceed the caller's wall");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            StdFs
                .read_dir_limited(&root.0, 3, 0)
                .expect_err("the first nonempty path must cross a zero-byte wall")
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            StdFs
                .read_dir_bounded(&root.0, 3)
                .expect("the exact wall is admitted")
                .len(),
            3
        );
        assert_eq!(
            StdFs
                .read_dir_prefix(&root.0, 2)
                .expect("a maintenance prefix is deliberately partial")
                .len(),
            2
        );
    }
}
