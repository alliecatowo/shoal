use super::*;

#[test]
fn undo_record_and_list() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "rm -rf build")).unwrap();
    let other = j.append(&rec("s", "human", 2, "ls")).unwrap();

    let inv1 = serde_json::json!({"trash": "/home/user/.trash/build"}).to_string();
    let inv2 = serde_json::json!({"restore_bytes": {"path": "a.txt", "hash": "ab"}}).to_string();
    j.record_undo(id, "trash", &inv1).unwrap();
    j.record_undo(id, "restore_bytes", &inv2).unwrap();

    let undos = j.undos_for(id).unwrap();
    assert_eq!(undos.len(), 2);
    assert_eq!(undos[0], ("trash".to_string(), inv1.clone()));
    assert_eq!(undos[1], ("restore_bytes".to_string(), inv2.clone()));
    // Payload survives as valid JSON.
    let parsed: serde_json::Value = serde_json::from_str(&undos[0].1).unwrap();
    assert_eq!(parsed["trash"], "/home/user/.trash/build");

    assert!(j.undos_for(other).unwrap().is_empty());
    assert!(j.undos_for(9999).unwrap().is_empty());
}

#[test]
fn undo_trash_move_restores_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    // `undo_entry` resolves `root`'s leading symlink prefix before
    // checking that undo targets are contained within it (see
    // `checked_target`, `resolve_leading_symlink_prefix`). On macOS the
    // tempdir path is a symlink alias (e.g. `/var/folders/...` ->
    // `/private/var/folders/...`), so build `original` from the
    // canonicalized root here to mirror how a production `self.cwd`
    // (sourced from `getcwd`) would already be alias-free.
    let root_path = root.path().canonicalize().unwrap();
    let original = root_path.join("gone.txt");
    let trash_dir = tempfile::tempdir().unwrap();
    let trash = trash_dir.path().join("gone.txt");
    fs::write(&original, b"important").unwrap();
    fs::rename(&original, &trash).unwrap();
    let inverse = UndoInverse::TrashMove {
        original: original.clone(),
        trash: trash.clone(),
        trash_fingerprint: FileFingerprint::capture(&trash).unwrap(),
    };
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "rm gone.txt")).unwrap();
    j.record_undo_inverse(id, &inverse).unwrap();
    let report = j.undo_entry(id, root.path()).unwrap();
    assert_eq!(report.steps[0].status, UndoStatus::Applied);
    assert_eq!(fs::read(&original).unwrap(), b"important");
    assert_eq!(
        j.undo_entry(id, root.path()).unwrap().steps[0].status,
        UndoStatus::AlreadyApplied
    );
}

#[test]
fn undo_restore_bytes_refuses_stale_content() {
    let root = tempfile::tempdir().unwrap();
    // See undo_trash_move_restores_and_is_idempotent: canonicalize so
    // `path` shares the same prefix `undo_entry` compares against after
    // it resolves `root`'s leading symlink alias internally (macOS
    // tempdirs are symlink aliases into `/private/...`).
    let root_path = root.path().canonicalize().unwrap();
    let path = root_path.join("config");
    fs::write(&path, b"before").unwrap();
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "save config")).unwrap();
    let prior = j.record_output(id, "value", b"before").unwrap();
    fs::write(&path, b"after").unwrap();
    let inverse = UndoInverse::RestoreBytes {
        path: path.clone(),
        prior_hash: prior,
        expected_current: FileFingerprint::capture(&path).unwrap(),
    };
    j.record_undo_inverse(id, &inverse).unwrap();
    fs::write(&path, b"user edit").unwrap();
    assert!(matches!(j.undo_entry(id,root.path()),Err(UndoError::Stale(p)) if p==path));
    assert_eq!(fs::read(&path).unwrap(), b"user edit");
}

#[test]
fn undo_restore_bytes_uses_cas() {
    let root = tempfile::tempdir().unwrap();
    // See undo_trash_move_restores_and_is_idempotent: canonicalize so
    // `path` shares the same prefix `undo_entry` compares against after
    // it resolves `root`'s leading symlink alias internally (macOS
    // tempdirs are symlink aliases into `/private/...`).
    let root_path = root.path().canonicalize().unwrap();
    let path = root_path.join("config");
    fs::write(&path, b"before").unwrap();
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "save config")).unwrap();
    let prior = j.record_output(id, "value", b"before").unwrap();
    fs::write(&path, b"after").unwrap();
    j.record_undo_inverse(
        id,
        &UndoInverse::RestoreBytes {
            path: path.clone(),
            prior_hash: prior,
            expected_current: FileFingerprint::capture(&path).unwrap(),
        },
    )
    .unwrap();
    assert_eq!(
        j.undo_entry(id, root.path()).unwrap().steps[0].status,
        UndoStatus::Applied
    );
    assert_eq!(fs::read(&path).unwrap(), b"before");
    assert_eq!(
        j.undo_entry(id, root.path()).unwrap().steps[0].status,
        UndoStatus::AlreadyApplied
    );
}

#[test]
fn undo_replays_moves_newest_first() {
    let root = tempfile::tempdir().unwrap();
    // See undo_trash_move_restores_and_is_idempotent: canonicalize so
    // `a`/`b`/`c` share the same prefix `undo_entry` compares against
    // after it resolves `root`'s leading symlink alias internally
    // (macOS tempdirs are symlink aliases into `/private/...`).
    let root_path = root.path().canonicalize().unwrap();
    let a = root_path.join("a");
    let b = root_path.join("b");
    let c = root_path.join("c");
    fs::write(&a, b"x").unwrap();
    fs::rename(&a, &b).unwrap();
    let fp = FileFingerprint::capture(&b).unwrap();
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "mv a b; mv b c")).unwrap();
    j.record_undo_inverse(
        id,
        &UndoInverse::MoveBack {
            from: b.clone(),
            to: a.clone(),
            expected_from: fp.clone(),
        },
    )
    .unwrap();
    fs::rename(&b, &c).unwrap();
    j.record_undo_inverse(
        id,
        &UndoInverse::MoveBack {
            from: c,
            to: b,
            expected_from: fp,
        },
    )
    .unwrap();
    let report = j.undo_entry(id, root.path()).unwrap();
    assert_eq!(report.steps.len(), 2);
    assert!(a.exists());
}

#[cfg(unix)]
#[test]
fn undo_rejects_traversal_and_symlink_parent() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "undo hostile")).unwrap();
    let escaped = root.path().join("..").join("escape");
    j.record_undo_inverse(
        id,
        &UndoInverse::MoveBack {
            from: escaped.clone(),
            to: root.path().join("safe"),
            expected_from: FileFingerprint {
                size: 0,
                modified_ns: None,
                hash: None,
            },
        },
    )
    .unwrap();
    assert!(matches!(
        j.undo_entry(id, root.path()),
        Err(UndoError::Escaped(_))
    ));
    let id2 = j.append(&rec("s", "human", 2, "undo symlink")).unwrap();
    symlink(outside.path(), root.path().join("link")).unwrap();
    let target = root.path().join("link/file");
    fs::write(outside.path().join("file"), b"after").unwrap();
    let prior = j.record_output(id2, "value", b"before").unwrap();
    j.record_undo_inverse(
        id2,
        &UndoInverse::RestoreBytes {
            path: target.clone(),
            prior_hash: prior,
            expected_current: FileFingerprint::capture(&target).unwrap(),
        },
    )
    .unwrap();
    assert!(matches!(
        j.undo_entry(id2, root.path()),
        Err(UndoError::Escaped(_))
    ));
    assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"after");
}

#[test]
fn undo_restores_scoped_target_when_root_is_passed_as_a_raw_symlink_alias() {
    // Regression test: `undo` must not refuse a
    // legitimate target just because the caller's `root` argument still
    // carries a raw OS-level symlink alias in its leading prefix (e.g.
    // macOS's `/tmp` -> `/private/tmp`, `/var` -> `/private/var`) while
    // the recorded target was built from the already-resolved form (as
    // `std::env::current_dir()`/`getcwd` would give). `undo_entry` must
    // resolve *that* leading prefix on `root` -- see
    // `resolve_leading_symlink_prefix` -- without requiring the caller
    // to pre-canonicalize. On Linux (no leading alias on a plain
    // tempdir) this is a harmless no-op, so the same test is valid on
    // both platforms.
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    fs::create_dir(root_path.join("nested")).unwrap();
    let path = root_path.join("nested").join("config");
    fs::write(&path, b"before").unwrap();
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "save config")).unwrap();
    let prior = j.record_output(id, "value", b"before").unwrap();
    fs::write(&path, b"after").unwrap();
    j.record_undo_inverse(
        id,
        &UndoInverse::RestoreBytes {
            path: path.clone(),
            prior_hash: prior,
            expected_current: FileFingerprint::capture(&path).unwrap(),
        },
    )
    .unwrap();
    // Pass the *raw*, un-pre-canonicalized tempdir path -- the form a
    // caller gets from a `TempDir`/session config without going through
    // `getcwd`, and exactly the form that used to make `checked_target`
    // (wrongly) refuse the target as escaped once `undo_entry` switched
    // from a no-op to a blanket `root.canonicalize()`.
    let report = j.undo_entry(id, root.path()).unwrap();
    assert_eq!(report.steps[0].status, UndoStatus::Applied);
    assert_eq!(fs::read(&path).unwrap(), b"before");
}

#[cfg(unix)]
#[test]
fn undo_still_refuses_intra_scope_symlink_when_root_alias_is_resolved() {
    // The leading-prefix fix must not weaken `ensure_no_symlink_parents`:
    // resolving a raw OS-level alias in `root` (see
    // `resolve_leading_symlink_prefix`) must never bleed into resolving
    // a symlink planted *inside* the tracked scope -- that's the TOCTOU
    // swap this check exists to catch, and it must still be refused
    // even when `root` itself needed the leading-alias treatment to
    // line up with the (already-resolved) recorded target.
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().canonicalize().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root_path.join("link")).unwrap();
    let target = root_path.join("link/file");
    fs::write(outside.path().join("file"), b"after").unwrap();
    let j = Journal::in_memory().unwrap();
    let id = j
        .append(&rec("s", "human", 1, "undo through symlink"))
        .unwrap();
    let prior = j.record_output(id, "value", b"before").unwrap();
    j.record_undo_inverse(
        id,
        &UndoInverse::RestoreBytes {
            path: target.clone(),
            prior_hash: prior,
            expected_current: FileFingerprint::capture(&target).unwrap(),
        },
    )
    .unwrap();
    // `root.path()` is the raw, un-canonicalized tempdir path -- the
    // same leading-alias resolution as the test above is in play --
    // while `target` was built from the canonical form and reaches
    // through an intra-scope symlink planted after the entry was
    // recorded.
    assert!(matches!(
        j.undo_entry(id, root.path()),
        Err(UndoError::Escaped(_))
    ));
    assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"after");
}
