use super::*;

#[test]
fn append_completed_persists_a_finished_row_atomically() {
    let journal = Journal::in_memory().unwrap();
    let id = journal
        .append_completed(
            &rec("audit", "supervisor", 7, "# approval p"),
            Some(0),
            true,
            0,
        )
        .unwrap();
    let rows = journal.entries_by_id(&[id]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, Some(0));
    assert_eq!(rows[0].ok, Some(true));
    assert_eq!(rows[0].dur_ns, Some(0));
}

#[test]
fn append_finish_query_roundtrip() {
    let j = Journal::in_memory().unwrap();
    let e = rec("s1", "human", 1_000, "git push origin main");
    let id = j.append(&e).unwrap();
    assert_eq!(id, 1);

    // Before finish: NULL status/ok/dur.
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.id, id);
    assert_eq!(r.session, "s1");
    assert_eq!(r.principal, "human");
    assert_eq!(r.ts_ns, 1_000);
    assert_eq!(r.cwd, b"/home/user/proj".to_vec());
    assert_eq!(r.src, "git push origin main");
    assert_eq!(r.ast_json, r#"{"kind":"call","cmd":"x"}"#);
    assert_eq!(r.effects_json, r#"["opaque"]"#);
    assert!(r.opaque);
    assert_eq!(r.status, None);
    assert_eq!(r.ok, None);
    assert_eq!(r.dur_ns, None);
    assert!(r.outputs.is_empty());

    j.finish(id, Some(0), true, 42_000_000).unwrap();
    let rows = j.query(&JournalQuery::default()).unwrap();
    let r = &rows[0];
    assert_eq!(r.status, Some(0));
    assert_eq!(r.ok, Some(true));
    assert_eq!(r.dur_ns, Some(42_000_000));
}

#[test]
fn finish_unknown_id_errors() {
    let j = Journal::in_memory().unwrap();
    let err = j.finish(999, Some(0), true, 1).unwrap_err();
    assert!(matches!(err, rusqlite::Error::StatementChangedRows(0)));
}

#[test]
fn completion_outputs_transcript_and_marker_commit_together() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "return 42")).unwrap();
    let payload = r#"{"n":0,"summary":{"type":"int"}}"#;

    j.complete_with_outputs(
        id,
        &[("value", b"42"), ("render", b"42\n")],
        Some((2, payload)),
        Some(0),
        true,
        17,
    )
    .unwrap();

    let rows = j.entries_by_id(&[id]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, Some(0));
    assert_eq!(rows[0].ok, Some(true));
    assert_eq!(rows[0].dur_ns, Some(17));
    assert_eq!(
        rows[0]
            .outputs
            .iter()
            .map(|output| output.kind.as_str())
            .collect::<Vec<_>>(),
        ["value", "render"]
    );
    let events = j.transcript_events_by_entry(&[id]).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].ts_ns, 2);
    assert_eq!(events[0].payload_json, payload);
}

#[test]
fn late_completion_failure_rolls_back_outputs_and_marker() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "return 42")).unwrap();
    j.record_transcript_event(id, 1, r#"{"existing":true}"#)
        .unwrap();

    let error = j
        .complete_with_outputs(
            id,
            &[("value", b"must not become visible")],
            Some((2, r#"{"duplicate":true}"#)),
            Some(0),
            true,
            17,
        )
        .unwrap_err();
    assert!(matches!(error, rusqlite::Error::SqliteFailure(_, _)));

    let rows = j.entries_by_id(&[id]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, None);
    assert_eq!(rows[0].ok, None);
    assert_eq!(rows[0].dur_ns, None);
    assert!(rows[0].outputs.is_empty());
    let events = j.transcript_events_by_entry(&[id]).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].ts_ns, 1);
    assert_eq!(events[0].payload_json, r#"{"existing":true}"#);
}

#[test]
fn aggregate_completion_preparation_is_bounded_before_writing() {
    let j = Journal::in_memory_with_options(JournalOptions {
        output_hard_cap: 8,
        ..Default::default()
    })
    .unwrap();
    let id = j.append(&rec("s", "human", 1, "return 42")).unwrap();

    assert!(
        j.complete_with_outputs(
            id,
            &[("value", b"12345"), ("render", b"67890")],
            None,
            Some(0),
            true,
            17,
        )
        .is_err()
    );
    let too_many = vec![("value", b"x".as_slice()); 17];
    assert!(
        j.complete_with_outputs(id, &too_many, None, Some(0), true, 17)
            .is_err()
    );

    let rows = j.entries_by_id(&[id]).unwrap();
    assert_eq!(rows[0].ok, None);
    assert!(rows[0].outputs.is_empty());
}

#[test]
fn unfinished_entry_survives_reopen_with_null_status() {
    // WAL crash-tolerance smoke: append, drop without finish, reopen.
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let j = Journal::open(dir.path()).unwrap();
        id = j.append(&rec("s1", "human", 5, "sleep 100")).unwrap();
        // Dropped without finish — simulates a crash mid-execution.
    }
    let j = Journal::open(dir.path()).unwrap();
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].src, "sleep 100");
    assert_eq!(rows[0].status, None);
    assert_eq!(rows[0].ok, None);
    assert_eq!(rows[0].dur_ns, None);
}

#[test]
fn open_creates_tree_and_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("deep").join("state");
    {
        let j = Journal::open(&state).unwrap();
        j.append(&rec("s", "human", 1, "ls")).unwrap();
    }
    assert!(state.join("journal.db").is_file());
    assert!(state.join("cas").is_dir());
    assert!(state.join("leases").is_dir());
    // WAL mode is persisted in the database header.
    let conn = Connection::open(state.join("journal.db")).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

#[test]
fn fresh_on_disk_db_is_stamped_to_current_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    {
        let j = Journal::open(dir.path()).unwrap();
        j.append(&rec("s", "human", 1, "echo hi")).unwrap();
    } // drop the handle so a fresh connection reads the persisted header cleanly
    let conn = Connection::open(dir.path().join("journal.db")).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn reopening_preserves_schema_version_and_data() {
    let dir = tempfile::tempdir().unwrap();
    {
        let j = Journal::open(dir.path()).unwrap();
        j.append(&rec("s", "human", 1, "echo persists")).unwrap();
    }
    let j = Journal::open(dir.path()).unwrap();
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1, "data must survive a reopen");
    assert_eq!(rows[0].src, "echo persists");
    let version: i64 = j
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn a_schema_version_from_a_newer_shoal_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    // Open once (stamps CURRENT_SCHEMA_VERSION), then hand-stamp a version this build has
    // never heard of — simulating a `journal.db` last written by a newer shoal.
    {
        let j = Journal::open(dir.path()).unwrap();
        drop(j);
        let conn = Connection::open(dir.path().join("journal.db")).unwrap();
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .unwrap();
    }
    // `Journal` has no `Debug` impl, so `unwrap_err` (which needs `T: Debug` for its panic
    // message) can't be called directly on `Result<Journal, _>`; discard the `Ok` payload first.
    let err = Journal::open(dir.path()).map(|_| ()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("newer") && msg.contains("schema version"),
        "error should clearly name a too-new schema version, got: {msg}"
    );
}

#[test]
fn legacy_zero_version_db_is_adopted_without_losing_rows() {
    // Simulate a "legacy" database: tables already exist (created by a real `Journal::open`,
    // so their shape is exactly today's), but `user_version` is 0 — either because this row
    // predates the versioning scaffold entirely, or (as done here) because we force it back to
    // 0 by hand after the fact. Either way `migrate` must adopt it to CURRENT without touching
    // the data.
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let j = Journal::open(dir.path()).unwrap();
        id = j.append(&rec("s", "human", 1, "echo legacy")).unwrap();
    }
    {
        // Force the just-stamped version back to 0, as if this were pre-versioning.
        let conn = Connection::open(dir.path().join("journal.db")).unwrap();
        conn.pragma_update(None, "user_version", 0i64).unwrap();
    }

    let j = Journal::open(dir.path()).expect("a legacy user_version=0 db must still open");
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1, "the pre-existing row must survive adoption");
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].src, "echo legacy");
    let version: i64 = j
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, CURRENT_SCHEMA_VERSION,
        "adoption must stamp the current version"
    );
}

#[test]
fn version_one_entry_metadata_migration_preserves_and_classifies_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("journal.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE entry(
                 id INTEGER PRIMARY KEY, session TEXT NOT NULL, principal TEXT NOT NULL,
                 ts INTEGER NOT NULL, dur_ns INTEGER, cwd BLOB NOT NULL, env_hash BLOB,
                 src TEXT NOT NULL, ast BLOB NOT NULL, effects TEXT NOT NULL,
                 status INTEGER, ok BOOL, opaque BOOL NOT NULL
             );
             INSERT INTO entry(session,principal,ts,cwd,src,ast,effects,opaque)
                 VALUES ('s','human',1,X'2f','statement','{}','[]',0);
             INSERT INTO entry(session,principal,ts,cwd,src,ast,effects,opaque)
                 VALUES ('s','human',2,X'2f','exec','{\"stmts\":[]}','[]',0);
             INSERT INTO entry(session,principal,ts,cwd,src,ast,effects,opaque)
                 VALUES ('s','human',3,X'2f','approval','null','[{\"kind\":\"approval\"}]',0);
             INSERT INTO entry(session,principal,ts,cwd,src,ast,effects,opaque)
                 VALUES ('s','human',4,X'2f','malformed','not json','also not json',0);
             PRAGMA user_version=1;",
        )
        .unwrap();
    }

    let journal = Journal::open(dir.path()).expect("v1 journal must migrate without data loss");
    let rows = journal.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 4);
    let kind_for = |src: &str| rows.iter().find(|row| row.src == src).unwrap().kind;
    assert_eq!(kind_for("statement"), EntryKind::Statement);
    assert_eq!(kind_for("exec"), EntryKind::Exec);
    assert_eq!(kind_for("approval"), EntryKind::Approval);
    assert_eq!(kind_for("malformed"), EntryKind::Statement);
    assert!(rows.iter().all(|row| row.parent_id.is_none()));
    let version: i64 = journal
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn version_two_migration_adds_leases_without_touching_permanent_pins() {
    let dir = tempfile::tempdir().unwrap();
    let hash;
    {
        let journal = Journal::open(dir.path()).unwrap();
        let id = journal.append(&rec("s", "human", 1, "pinned")).unwrap();
        hash = journal.record_output(id, "stdout", b"keep me").unwrap();
        journal.pin(&hash).unwrap();
    }
    {
        let conn = Connection::open(dir.path().join("journal.db")).unwrap();
        conn.execute_batch("DROP TABLE pin_lease; PRAGMA user_version=2;")
            .unwrap();
    }

    let journal = Journal::open(dir.path()).expect("v2 journal must gain lease storage");
    assert_eq!(journal.pins().unwrap(), vec![hash]);
    let version: i64 = journal
        .conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);
    let lease_table: i64 = journal
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='pin_lease'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lease_table, 1);
}

#[test]
fn in_memory_journal_is_stamped_to_current_schema_version_and_stays_usable() {
    // Ephemeral/in-memory journals must be no-op-safe through `migrate`: a fresh in-memory
    // database starts at user_version 0 (same as a fresh on-disk one), so this exercises the
    // exact same "fresh database" arm, just without any file on disk.
    let j = Journal::in_memory().unwrap();
    let version: i64 = j
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);

    // And it must still be fully usable afterward.
    let id = j.append(&rec("s", "human", 1, "echo hi")).unwrap();
    j.finish(id, Some(0), true, 1).unwrap();
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
}
