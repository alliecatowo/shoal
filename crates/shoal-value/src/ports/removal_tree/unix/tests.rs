use super::*;
use std::path::PathBuf;

#[derive(Debug)]
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn create() -> Self {
        for _ in 0..QUARANTINE_ATTEMPTS {
            let name = random_quarantine_name().expect("random test directory name");
            let path = std::env::temp_dir().join(OsStr::from_bytes(name.to_bytes()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", path.display()),
            }
        }
        panic!("could not reserve a unique test directory")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn one_child(parent: &Path, child_name: &str) -> (File, Directory) {
    let fd = open_directory_path(parent).expect("open test parent");
    let mut admission = Admission::default();
    let directory = inventory(&fd, 1, &mut admission).expect("inventory test parent");
    assert_eq!(directory.children.len(), 1);
    assert_eq!(directory.children[0].name, OsStr::new(child_name));
    (fd, directory)
}

fn quarantine_entries(parent: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(parent)
        .expect("read test parent")
        .map(|entry| entry.expect("read test entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.as_bytes().starts_with(QUARANTINE_PREFIX))
        })
        .collect()
}

#[test]
fn replacement_at_commit_is_restored_without_deletion_or_quarantine_leak() {
    let root = TestDirectory::create();
    let child = root.0.join("child");
    let admitted = root.0.join("admitted-moved-by-racer");
    std::fs::write(&child, b"admitted").unwrap();
    let (fd, directory) = one_child(&root.0, "child");

    let error = remove_child_with(&fd, &directory.children[0], |stage, _, _, _| {
        if stage == CommitStage::BeforeRename {
            std::fs::rename(&child, &admitted).unwrap();
            std::fs::write(&child, b"replacement").unwrap();
        }
    })
    .expect_err("commit-point replacement must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("restored without overwrite"));
    assert_eq!(std::fs::read(&child).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&admitted).unwrap(), b"admitted");
    assert!(quarantine_entries(&root.0).is_empty());
}

#[test]
fn occupied_original_contains_raced_replacement_without_overwrite() {
    let root = TestDirectory::create();
    let child = root.0.join("child");
    let admitted = root.0.join("admitted-moved-by-racer");
    std::fs::write(&child, b"admitted").unwrap();
    let (fd, directory) = one_child(&root.0, "child");

    let error = remove_child_with(&fd, &directory.children[0], |stage, _, _, _| match stage {
        CommitStage::BeforeRename => {
            std::fs::rename(&child, &admitted).unwrap();
            std::fs::write(&child, b"replacement").unwrap();
        }
        CommitStage::AfterRename => std::fs::write(&child, b"blocker").unwrap(),
    })
    .expect_err("an occupied original name must prevent restoration");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("remains contained as"));
    assert_eq!(std::fs::read(&child).unwrap(), b"blocker");
    assert_eq!(std::fs::read(&admitted).unwrap(), b"admitted");
    let contained = quarantine_entries(&root.0);
    assert_eq!(contained.len(), 1);
    assert_eq!(std::fs::read(&contained[0]).unwrap(), b"replacement");
}

#[test]
fn healthy_nested_removal_cleans_every_private_commit_name() {
    let root = TestDirectory::create();
    let victim = root.0.join("victim");
    std::fs::create_dir_all(victim.join("one/two")).unwrap();
    std::fs::write(victim.join("root-file"), b"root").unwrap();
    std::fs::write(victim.join("one/two/leaf"), b"leaf").unwrap();
    let metadata = std::fs::symlink_metadata(&victim).unwrap();
    let expected = FsEntryIdentity::from_metadata(&metadata);

    let tree = open(&victim, &expected).expect("admit healthy tree");
    tree.remove(&victim).expect("remove healthy tree");

    assert!(!victim.exists());
    assert!(quarantine_entries(&root.0).is_empty());
}

#[test]
fn top_level_quarantined_leaf_replacement_at_commit_is_preserved() {
    let root = TestDirectory::create();
    let target = root.0.join("already-quarantined-leaf");
    let admitted = root.0.join("admitted-moved-by-racer");
    std::fs::write(&target, b"admitted").unwrap();
    let expected = FsEntryIdentity::from_metadata(
        &std::fs::symlink_metadata(&target).expect("admit quarantined leaf"),
    );

    let error = remove_leaf_with(&target, &expected, |stage, _, _, _| {
        if stage == CommitStage::BeforeRename {
            std::fs::rename(&target, &admitted).unwrap();
            std::fs::write(&target, b"replacement").unwrap();
        }
    })
    .expect_err("a replacement after outer quarantine must not be deleted");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&admitted).unwrap(), b"admitted");
    assert!(quarantine_entries(&root.0).is_empty());
}

#[test]
fn final_directory_replacement_at_commit_is_restored_without_deletion() {
    let root = TestDirectory::create();
    let target = root.0.join("already-quarantined-directory");
    let admitted = root.0.join("admitted-directory-moved-by-racer");
    std::fs::create_dir(&target).unwrap();
    let parent = open_directory_path(&root.0).unwrap();
    let name = cstring(OsStr::new("already-quarantined-directory")).unwrap();
    let stat = stat_at(parent.as_raw_fd(), &name).unwrap();
    let identity = Identity::from_stat(&stat);
    let mount_key = mount_key_at(parent.as_raw_fd(), &name).unwrap();

    let error =
        remove_empty_directory_with(&parent, &name, identity, mount_key, |stage, _, _, _| {
            if stage == CommitStage::BeforeRename {
                std::fs::rename(&target, &admitted).unwrap();
                std::fs::create_dir(&target).unwrap();
            }
        })
        .expect_err("a replacement at final root commit must not be deleted");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(target.is_dir());
    assert!(admitted.is_dir());
    assert!(quarantine_entries(&root.0).is_empty());
}

#[test]
fn new_descendant_during_final_commit_is_preserved_and_root_is_restored() {
    let root = TestDirectory::create();
    let target = root.0.join("already-quarantined-directory");
    std::fs::create_dir(&target).unwrap();
    let parent = open_directory_path(&root.0).unwrap();
    let name = cstring(OsStr::new("already-quarantined-directory")).unwrap();
    let stat = stat_at(parent.as_raw_fd(), &name).unwrap();
    let identity = Identity::from_stat(&stat);
    let mount_key = mount_key_at(parent.as_raw_fd(), &name).unwrap();

    let error = remove_empty_directory_with(
        &parent,
        &name,
        identity,
        mount_key,
        |stage, _, _, quarantine| {
            if stage == CommitStage::AfterRename {
                let quarantine = quarantine.expect("commit name exists after rename");
                let path = root
                    .0
                    .join(OsStr::from_bytes(quarantine.to_bytes()))
                    .join("new-descendant");
                std::fs::write(path, b"preserve").unwrap();
            }
        },
    )
    .expect_err("a concurrent new descendant must prevent directory unlink");

    assert!(error.to_string().contains("restored without overwrite"));
    assert_eq!(
        std::fs::read(target.join("new-descendant")).unwrap(),
        b"preserve"
    );
    assert!(quarantine_entries(&root.0).is_empty());
}
