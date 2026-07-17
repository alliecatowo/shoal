use super::*;
use crate::cas::TRUNCATION_MARKER;
use crate::schema::CURRENT_SCHEMA_VERSION;
use std::io::Read as _;

fn rec(session: &str, principal: &str, ts_ns: i64, src: &str) -> EntryRecord {
    EntryRecord {
        session: session.to_string(),
        principal: principal.to_string(),
        ts_ns,
        cwd: b"/home/user/proj".to_vec(),
        src: src.to_string(),
        ast_json: r#"{"kind":"call","cmd":"x"}"#.to_string(),
        effects_json: r#"["opaque"]"#.to_string(),
        opaque: true,
    }
}

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
fn cas_verified_reader_streams_large_content_without_materializing_api() {
    let journal = Journal::in_memory().unwrap();
    let id = journal
        .append(&rec("cas", "human", 1, "large output"))
        .unwrap();
    let payload = (0..(2 * 1024 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let hash = journal.record_output(id, "stdout", &payload).unwrap();
    let mut reader = journal.cas().open_verified(&hash).unwrap();
    let mut observed = Vec::new();
    let mut chunk = [0u8; 31 * 1024];
    loop {
        let n = reader.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        observed.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(observed, payload);
}

/// Count regular files under `dir`, recursively.
fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            n += count_files(&entry.path());
        } else {
            n += 1;
        }
    }
    n
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
fn cas_roundtrip_and_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    let id = j.append(&rec("s", "human", 1, "cat big.log")).unwrap();

    let payload = b"hello CAS world\nline two\n".repeat(100);
    let h1 = j.record_output(id, "stdout", &payload).unwrap();
    let h2 = j.record_output(id, "stderr", &payload).unwrap();
    assert_eq!(h1, h2, "identical bytes must hash identically");
    assert_eq!(h1, blake3::hash(&payload).to_hex().to_string());

    // Same bytes twice -> exactly one file in the CAS.
    assert_eq!(count_files(&dir.path().join("cas")), 1);
    // Sharded layout: cas/<hex[0..2]>/<hex[2..4]>/<hex>.zst
    let blob = dir
        .path()
        .join("cas")
        .join(&h1[0..2])
        .join(&h1[2..4])
        .join(format!("{h1}.zst"));
    assert!(blob.is_file());
    // Stored compressed, not raw.
    let on_disk = fs::read(&blob).unwrap();
    assert_ne!(on_disk, payload);
    assert!(on_disk.len() < payload.len());

    // Roundtrip through read_blob.
    assert_eq!(j.read_blob(&h1).unwrap().unwrap(), payload);

    // Both output rows are linked and joined by query.
    let rows = j.query(&JournalQuery::default()).unwrap();
    let outs = &rows[0].outputs;
    assert_eq!(outs.len(), 2);
    assert_eq!(outs[0].kind, "stdout");
    assert_eq!(outs[1].kind, "stderr");
    assert!(
        outs.iter()
            .all(|o| o.hash == h1 && o.len == payload.len() as i64)
    );
}

#[test]
fn ingest_spill_adopts_file_and_cas_reader_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();

    // Write a "spill file" as shoal-exec would, in the journal's spill dir.
    let spill_dir = j.spill_dir().unwrap();
    let payload = b"spilled capture bytes\n".repeat(4096);
    let hash = blake3::hash(&payload).to_hex().to_string();
    let src = spill_dir.join("capture-spill-xyz");
    fs::write(&src, &payload).unwrap();

    j.ingest_spill(&src, &hash, payload.len() as u64, true)
        .unwrap();

    // Source file consumed; blob present under its real blake3, pinned.
    assert!(!src.exists(), "the spill source is removed after adoption");
    let blob = dir
        .path()
        .join("cas")
        .join(&hash[0..2])
        .join(&hash[2..4])
        .join(format!("{hash}.zst"));
    assert!(blob.is_file(), "the adopted blob exists in the CAS");
    assert!(
        fs::read(&blob).unwrap().len() < payload.len(),
        "stored compressed"
    );
    assert_eq!(
        j.pins().unwrap(),
        vec![hash.clone()],
        "spill blob is pinned"
    );

    // The DB-independent Cas reader materializes the exact full bytes.
    let cas = j.cas();
    assert_eq!(cas.read(&hash).unwrap(), payload);
    // ...as does read_blob, and its stored_len is the true (uncompressed) len.
    assert_eq!(j.read_blob(&hash).unwrap().unwrap(), payload);
    let rows = j.query(&JournalQuery::default()).unwrap();
    let _ = rows; // no entry linkage for a spill blob; it lives by its pin.

    // Idempotent: re-adopting identical bytes is a no-op that still succeeds.
    let src2 = spill_dir.join("capture-spill-again");
    fs::write(&src2, &payload).unwrap();
    j.ingest_spill(&src2, &hash, payload.len() as u64, false)
        .unwrap();
    assert_eq!(count_files(&dir.path().join("cas")), 1, "dedup: one blob");

    // A missing blob is a NotFound error, not wrong bytes.
    let absent = blake3::hash(b"nope").to_hex().to_string();
    assert_eq!(
        cas.read(&absent).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn distinct_bytes_get_distinct_files() {
    let dir = tempfile::tempdir().unwrap();
    let j = Journal::open(dir.path()).unwrap();
    let id = j.append(&rec("s", "human", 1, "x")).unwrap();
    let h1 = j.record_output(id, "stdout", b"alpha").unwrap();
    let h2 = j.record_output(id, "stdout", b"beta").unwrap();
    assert_ne!(h1, h2);
    assert_eq!(count_files(&dir.path().join("cas")), 2);
    assert_eq!(j.read_blob(&h1).unwrap().unwrap(), b"alpha");
    assert_eq!(j.read_blob(&h2).unwrap().unwrap(), b"beta");
}

#[test]
fn record_output_empty_bytes() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "true")).unwrap();
    let h = j.record_output(id, "stdout", b"").unwrap();
    assert_eq!(j.read_blob(&h).unwrap().unwrap(), Vec::<u8>::new());
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows[0].outputs[0].len, 0);
}

#[test]
fn read_blob_missing_returns_none() {
    let j = Journal::in_memory().unwrap();
    // Well-formed hash that was never stored.
    let absent = blake3::hash(b"never stored").to_hex().to_string();
    assert_eq!(j.read_blob(&absent).unwrap(), None);
    // Malformed hashes cannot name blobs.
    assert_eq!(j.read_blob("").unwrap(), None);
    assert_eq!(j.read_blob("zz").unwrap(), None);
    assert_eq!(j.read_blob("../../etc/passwd").unwrap(), None);
}

#[test]
fn query_head_filter() {
    let j = Journal::in_memory().unwrap();
    j.append(&rec("s", "human", 1, "git push origin main"))
        .unwrap();
    j.append(&rec("s", "human", 2, "cargo build --release"))
        .unwrap();
    j.append(&rec("s", "human", 3, "gitk --all")).unwrap();
    j.append(&rec("s", "human", 4, "  git   status")).unwrap(); // leading whitespace ok

    let q = JournalQuery {
        head: Some("git".to_string()),
        ..JournalQuery::default()
    };
    let rows = j.query(&q).unwrap();
    assert_eq!(rows.len(), 2, "prefix match ('gitk') must not count");
    assert_eq!(rows[0].src, "  git   status"); // newest first
    assert_eq!(rows[1].src, "git push origin main");
}

#[test]
fn query_principal_filter() {
    let j = Journal::in_memory().unwrap();
    j.append(&rec("s", "human", 1, "ls")).unwrap();
    j.append(&rec("s", "agent:refactor", 2, "cargo test"))
        .unwrap();
    j.append(&rec("s", "human", 3, "pwd")).unwrap();

    let q = JournalQuery {
        principal: Some("agent:refactor".to_string()),
        ..JournalQuery::default()
    };
    let rows = j.query(&q).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].src, "cargo test");
}

#[test]
fn query_session_filter_is_exact() {
    let j = Journal::in_memory().unwrap();
    j.append(&rec("alpha", "human", 1, "first")).unwrap();
    j.append(&rec("beta", "human", 2, "second")).unwrap();
    j.append(&rec("alpha", "human", 3, "third")).unwrap();

    let rows = j
        .query(&JournalQuery {
            session: Some("alpha".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].src, "third");
    assert_eq!(rows[1].src, "first");
}

#[test]
fn query_ok_filter_excludes_unfinished() {
    let j = Journal::in_memory().unwrap();
    let ok_id = j.append(&rec("s", "human", 1, "true")).unwrap();
    let bad_id = j.append(&rec("s", "human", 2, "false")).unwrap();
    j.append(&rec("s", "human", 3, "sleep 999")).unwrap(); // never finished
    j.finish(ok_id, Some(0), true, 10).unwrap();
    j.finish(bad_id, Some(1), false, 20).unwrap();

    let q_ok = JournalQuery {
        ok: Some(true),
        ..JournalQuery::default()
    };
    let rows = j.query(&q_ok).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, ok_id);

    let q_bad = JournalQuery {
        ok: Some(false),
        ..JournalQuery::default()
    };
    let rows = j.query(&q_bad).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, bad_id);

    // No ok filter: unfinished entry included.
    assert_eq!(j.query(&JournalQuery::default()).unwrap().len(), 3);
}

#[test]
fn query_since_ts_filter() {
    let j = Journal::in_memory().unwrap();
    j.append(&rec("s", "human", 100, "old")).unwrap();
    j.append(&rec("s", "human", 200, "mid")).unwrap();
    j.append(&rec("s", "human", 300, "new")).unwrap();

    let q = JournalQuery {
        since_ts_ns: Some(200),
        ..JournalQuery::default()
    };
    let rows = j.query(&q).unwrap();
    assert_eq!(rows.len(), 2, "since is inclusive");
    assert_eq!(rows[0].src, "new");
    assert_eq!(rows[1].src, "mid");
}

#[test]
fn query_limit_and_order() {
    let j = Journal::in_memory().unwrap();
    for i in 0..5 {
        j.append(&rec("s", "human", i, &format!("cmd{i}"))).unwrap();
    }
    let q = JournalQuery {
        limit: 2,
        ..JournalQuery::default()
    };
    let rows = j.query(&q).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].src, "cmd4"); // ORDER BY id DESC
    assert_eq!(rows[1].src, "cmd3");
    assert!(rows[0].id > rows[1].id);
}

#[test]
fn query_default_limit_is_100() {
    let j = Journal::in_memory().unwrap();
    for i in 0..105 {
        j.append(&rec("s", "human", i, "echo hi")).unwrap();
    }
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 100);
    assert_eq!(rows[0].ts_ns, 104); // newest first
}

#[test]
fn head_filter_respects_limit() {
    let j = Journal::in_memory().unwrap();
    for i in 0..4 {
        j.append(&rec("s", "human", i * 2, &format!("git commit -m {i}")))
            .unwrap();
        j.append(&rec("s", "human", i * 2 + 1, "ls -la")).unwrap();
    }
    let q = JournalQuery {
        head: Some("git".to_string()),
        limit: 2,
        ..JournalQuery::default()
    };
    let rows = j.query(&q).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].src, "git commit -m 3");
    assert_eq!(rows[1].src, "git commit -m 2");
}

#[test]
fn combined_filters() {
    let j = Journal::in_memory().unwrap();
    let a = j.append(&rec("s", "agent:x", 10, "git push")).unwrap();
    let b = j.append(&rec("s", "human", 20, "git push")).unwrap();
    let c = j.append(&rec("s", "agent:x", 30, "git pull")).unwrap();
    for id in [a, b, c] {
        j.finish(id, Some(0), true, 1).unwrap();
    }
    let q = JournalQuery {
        head: Some("git".to_string()),
        principal: Some("agent:x".to_string()),
        ok: Some(true),
        since_ts_ns: Some(15),
        ..JournalQuery::default()
    };
    let rows = j.query(&q).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, c);
}

#[test]
fn entries_by_id_returns_exactly_the_requested_rows_in_order() {
    let j = Journal::in_memory().unwrap();
    let a = j.append(&rec("s", "human", 1, "cmd-a")).unwrap();
    let b = j.append(&rec("s", "human", 2, "cmd-b")).unwrap();
    let c = j.append(&rec("s", "human", 3, "cmd-c")).unwrap();
    for id in [a, b, c] {
        j.finish(id, Some(0), true, 1).unwrap();
    }

    // Out-of-order, non-contiguous request: the result must come back in
    // THIS order, not database (id ascending/descending) order.
    let rows = j.entries_by_id(&[c, a]).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, c);
    assert_eq!(rows[0].src, "cmd-c");
    assert_eq!(rows[1].id, a);
    assert_eq!(rows[1].src, "cmd-a");

    // A missing id (never appended / GC'd) is simply absent, not an error,
    // and does not shift the position of ids either side of it.
    let missing = 9999;
    let rows = j.entries_by_id(&[a, missing, b]).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "the missing id must be skipped, not erred on"
    );
    assert_eq!(rows[0].id, a);
    assert_eq!(rows[1].id, b);

    // All missing: empty, not an error.
    assert!(j.entries_by_id(&[missing]).unwrap().is_empty());

    // Empty request: empty, no query issued.
    assert!(j.entries_by_id(&[]).unwrap().is_empty());
}

#[test]
fn entries_by_id_joins_outputs_like_query_does() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo hi")).unwrap();
    j.finish(id, Some(0), true, 1).unwrap();
    j.record_output(id, "stdout", b"hi\n").unwrap();

    let rows = j.entries_by_id(&[id]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outputs.len(), 1);
    assert_eq!(rows[0].outputs[0].kind, "stdout");
}

#[test]
fn transcript_event_record_and_fetch_by_entry_in_order() {
    let j = Journal::in_memory().unwrap();
    let a = j.append(&rec("s", "human", 1, "let x = 1")).unwrap();
    let b = j.append(&rec("s", "human", 2, "let y = 2")).unwrap();
    j.finish(a, Some(0), true, 1).unwrap();
    j.finish(b, Some(0), true, 1).unwrap();

    j.record_transcript_event(a, 1_000, r#"{"$":"record","v":{"n":{"$":"int","v":1}}}"#)
        .unwrap();
    j.record_transcript_event(b, 2_000, r#"{"$":"record","v":{"n":{"$":"int","v":2}}}"#)
        .unwrap();

    // Requested out of order: result mirrors the request order, not
    // insertion/entry_id order.
    let rows = j.transcript_events_by_entry(&[b, a]).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].entry_id, b);
    assert_eq!(rows[0].ts_ns, 2_000);
    assert_eq!(
        rows[0].payload_json,
        r#"{"$":"record","v":{"n":{"$":"int","v":2}}}"#
    );
    assert_eq!(rows[1].entry_id, a);
    assert_eq!(rows[1].ts_ns, 1_000);

    // An entry_id with no transcript row (e.g. a failed exec, which never
    // publishes a transcript event) is simply absent.
    let c = j.append(&rec("s", "human", 3, "1 / 0")).unwrap();
    j.finish(c, Some(1), false, 1).unwrap();
    let rows = j.transcript_events_by_entry(&[a, c, b]).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "entry with no transcript row must be skipped"
    );
    assert_eq!(rows[0].entry_id, a);
    assert_eq!(rows[1].entry_id, b);

    assert!(j.transcript_events_by_entry(&[]).unwrap().is_empty());
}

#[test]
fn opening_a_pre_transcript_event_journal_still_works_additive_migration() {
    // Simulate a journal.db written before `transcript_event` existed: build
    // the OLD schema by hand (entry/output/undo/pin/blob only — no
    // transcript_event table), append + finish an entry the old way, close
    // it, then reopen with today's `Journal::open`. The additive migration
    // contract (`CREATE TABLE IF NOT EXISTS` in `init_schema`, run on every
    // open) must both (a) let the pre-existing on-disk data open and read
    // back fine and (b) make the new table available from that point on —
    // without any explicit ALTER TABLE / versioned migration step.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("journal.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE entry(
                 id        INTEGER PRIMARY KEY,
                 session   TEXT    NOT NULL,
                 principal TEXT    NOT NULL,
                 ts        INTEGER NOT NULL,
                 dur_ns    INTEGER,
                 cwd       BLOB    NOT NULL,
                 env_hash  BLOB,
                 src       TEXT    NOT NULL,
                 ast       BLOB    NOT NULL,
                 effects   TEXT    NOT NULL,
                 status    INTEGER,
                 ok        BOOL,
                 opaque    BOOL    NOT NULL
             );
             CREATE TABLE output(
                 entry_id INTEGER NOT NULL,
                 kind     TEXT    NOT NULL,
                 hash     BLOB    NOT NULL,
                 len      INTEGER NOT NULL,
                 meta     TEXT
             );
             CREATE TABLE undo(
                 entry_id INTEGER NOT NULL,
                 op       TEXT    NOT NULL,
                 inverse  TEXT    NOT NULL
             );
             CREATE TABLE pin(hash BLOB PRIMARY KEY);
             CREATE TABLE blob(
                 hash BLOB PRIMARY KEY,
                 stored_len INTEGER NOT NULL,
                 created_ns INTEGER NOT NULL,
                 last_access_ns INTEGER NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entry (session, principal, ts, dur_ns, cwd, env_hash, src, ast, effects,
                                status, ok, opaque)
             VALUES ('s','human',1,10,X'2f','', 'echo old','{}','[\"opaque\"]',0,1,1)",
            [],
        )
        .unwrap();
    }

    // Reopening with the current code must not fail, must see the
    // old row, and must expose the new table.
    let j = Journal::open(dir.path()).expect("a pre-transcript-event journal.db must still open");
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1, "pre-existing data survives the migration");
    assert_eq!(rows[0].src, "echo old");

    let id = j.append(&rec("s", "human", 2, "echo new")).unwrap();
    j.finish(id, Some(0), true, 1).unwrap();
    j.record_transcript_event(id, 5_000, r#"{"$":"record","v":{}}"#)
        .expect("the new table must be usable immediately after an additive-migration open");
    let got = j.transcript_events_by_entry(&[id]).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].ts_ns, 5_000);
}

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
fn in_memory_cas_lives_with_journal() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo hi")).unwrap();
    let h = j.record_output(id, "stdout", b"hi\n").unwrap();
    // The tempdir CAS must still be readable as long as the Journal lives.
    assert_eq!(j.read_blob(&h).unwrap().unwrap(), b"hi\n");
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

#[test]
fn pins_are_idempotent_and_exempt_from_gc() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo")).unwrap();
    let hash = j.record_output(id, "stdout", b"pinned").unwrap();
    assert!(j.pin(&hash).unwrap());
    assert!(!j.pin(&hash).unwrap());
    assert_eq!(j.pins().unwrap(), vec![hash.clone()]);
    let report = j
        .gc(GcOptions {
            ttl: Some(std::time::Duration::ZERO),
            max_bytes: Some(0),
            dry_run: false,
        })
        .unwrap();
    assert!(report.deleted.is_empty());
    assert!(j.read_blob(&hash).unwrap().is_some());
    assert!(j.unpin(&hash).unwrap());
}

#[test]
fn gc_prefers_orphans_then_lru_and_dry_run_preserves() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "outputs")).unwrap();
    let old = j.record_output(id, "stdout", b"old").unwrap();
    let orphan = j.record_output(id, "stdout", b"orphan").unwrap();
    let recent = j.record_output(id, "stdout", b"recent").unwrap();
    let orphan_raw = hex_bytes(&orphan).unwrap();
    j.conn
        .execute("DELETE FROM output WHERE hash=?1", [orphan_raw])
        .unwrap();
    j.conn
        .execute(
            "UPDATE blob SET last_access_ns=1 WHERE hash=?1",
            [hex_bytes(&old).unwrap()],
        )
        .unwrap();
    j.conn
        .execute(
            "UPDATE blob SET last_access_ns=2 WHERE hash=?1",
            [hex_bytes(&recent).unwrap()],
        )
        .unwrap();
    let dry = j
        .gc(GcOptions {
            ttl: None,
            max_bytes: Some(10),
            dry_run: true,
        })
        .unwrap();
    assert_eq!(dry.candidates[0].hash, orphan);
    assert!(dry.deleted.is_empty());
    assert!(j.read_blob(&orphan).unwrap().is_some());
    let done = j
        .gc(GcOptions {
            ttl: None,
            max_bytes: Some(10),
            dry_run: false,
        })
        .unwrap();
    assert_eq!(done.deleted[0].hash, orphan);
    assert!(j.read_blob(&orphan).unwrap().is_none());
}

#[test]
fn ttl_collects_referenced_blob_but_metadata_survives() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo")).unwrap();
    let hash = j.record_output(id, "stdout", b"aged").unwrap();
    j.conn
        .execute("UPDATE blob SET last_access_ns=0", [])
        .unwrap();
    let report = j
        .gc(GcOptions {
            ttl: Some(std::time::Duration::from_secs(1)),
            max_bytes: None,
            dry_run: false,
        })
        .unwrap();
    assert!(report.deleted[0].referenced);
    assert!(j.read_blob(&hash).unwrap().is_none());
    let rows = j.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows[0].outputs[0].hash, hash);
}

#[test]
fn output_truncation_is_explicit_in_bytes_and_metadata() {
    let j = Journal::in_memory_with_options(JournalOptions {
        output_hard_cap: 128,
        ..Default::default()
    })
    .unwrap();
    let id = j.append(&rec("s", "human", 1, "loud")).unwrap();
    let original = vec![b'x'; 1000];
    let hash = j.record_output(id, "stdout", &original).unwrap();
    let stored = j.read_blob(&hash).unwrap().unwrap();
    assert_eq!(stored.len(), 128);
    assert!(stored.ends_with(TRUNCATION_MARKER));
    let row = &j.query(&JournalQuery::default()).unwrap()[0].outputs[0];
    assert_eq!(
        row.meta,
        Some(OutputMeta {
            truncated: true,
            original_len: 1000,
            stored_len: 128
        })
    );
    assert_eq!(row.len, 128);
}

#[test]
fn blob_access_refreshes_lru_timestamp() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo")).unwrap();
    let hash = j.record_output(id, "stdout", b"hot").unwrap();
    let raw = hex_bytes(&hash).unwrap();
    j.conn
        .execute(
            "UPDATE blob SET last_access_ns=1 WHERE hash=?1",
            [raw.clone()],
        )
        .unwrap();
    assert_eq!(j.read_blob(&hash).unwrap().unwrap(), b"hot");
    let access: i64 = j
        .conn
        .query_row(
            "SELECT last_access_ns FROM blob WHERE hash=?1",
            [raw],
            |r| r.get(0),
        )
        .unwrap();
    assert!(access > 1);
}

#[test]
fn read_blob_rejects_corrupted_content() {
    // Reads are integrity-verified. Store a genuine blob, then overwrite
    // its on-disk .zst with a *valid* zstd stream of DIFFERENT bytes (a swap /
    // bit-rot that still decompresses cleanly). read_blob must refuse it rather
    // than hand back the wrong content to `undo`/`blob.get`.
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo")).unwrap();
    let hash = j.record_output(id, "stdout", b"genuine payload").unwrap();
    assert_eq!(j.read_blob(&hash).unwrap().unwrap(), b"genuine payload");

    let forged = zstd::encode_all(&b"tampered payload"[..], 3).unwrap();
    fs::write(j.blob_path(&hash), forged).unwrap();

    let err = j.read_blob(&hash);
    assert!(
        err.is_err(),
        "a content-hash mismatch must be an integrity error, got {err:?}"
    );
    assert!(
        format!("{:?}", err.unwrap_err()).contains("integrity"),
        "error should name the integrity failure"
    );
    let stream_err = j.cas().open_verified(&hash);
    assert!(
        stream_err.is_err(),
        "streaming reads must verify before exposing corrupt bytes"
    );
}

#[test]
fn concurrent_first_open_waits_through_wal_and_schema_initialization() {
    let dir = tempfile::tempdir().unwrap();
    for round in 0..10 {
        let state = dir.path().join(format!("concurrent-open-{round}"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Journal::open(&state)
                })
            })
            .collect::<Vec<_>>();
        let opened = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Result<Vec<_>, _>>();
        assert!(
            opened.is_ok(),
            "concurrent first-open round {round} failed: {:?}",
            opened.err()
        );
    }
}

#[test]
fn busy_timeout_zero_fails_fast_on_contended_write() {
    // Baseline: with busy_timeout = 0 (rusqlite's
    // default), a write that meets a held writer lock fails immediately with
    // SQLITE_BUSY — which the journaling call sites swallow, silently dropping
    // the entry. This proves the failure mode the default timeout guards.
    let dir = tempfile::tempdir().unwrap();
    let writer = Journal::open_with_options(
        dir.path(),
        JournalOptions {
            busy_timeout: std::time::Duration::ZERO,
            ..Default::default()
        },
    )
    .unwrap();
    // A second connection grabs the write lock and holds it for the whole test.
    let blocker = rusqlite::Connection::open(dir.path().join("journal.db")).unwrap();
    blocker.busy_timeout(std::time::Duration::ZERO).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let res = writer.append(&rec("s", "human", 1, "echo dropped"));
    assert!(
        res.is_err(),
        "a zero busy_timeout must fail fast against a held write lock"
    );
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn busy_timeout_lets_a_blocked_writer_wait_instead_of_dropping() {
    // A non-zero busy_timeout makes a contended writer WAIT for the lock
    // rather than drop its entry. A held lock is released after a short delay;
    // the write must then land (with timeout = 0 it would have errored, per the
    // test above).
    let dir = tempfile::tempdir().unwrap();
    let writer = Journal::open_with_options(
        dir.path(),
        JournalOptions {
            busy_timeout: std::time::Duration::from_secs(10),
            ..Default::default()
        },
    )
    .unwrap();
    let blocker = rusqlite::Connection::open(dir.path().join("journal.db")).unwrap();
    blocker
        .busy_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    // Release the lock after a delay; the append below is already waiting on it.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        blocker.execute_batch("COMMIT").unwrap();
    });

    let id = writer
        .append(&rec("s", "human", 1, "echo survived"))
        .expect("busy_timeout should let the write wait for the lock, not drop it");
    releaser.join().unwrap();

    let rows = writer.query(&JournalQuery::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].src, "echo survived");
}
