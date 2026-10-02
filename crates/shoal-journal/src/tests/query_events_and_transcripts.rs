use super::*;

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
fn entry_kind_and_parent_round_trip_and_filter_explicitly() {
    let j = Journal::in_memory().unwrap();
    let mut exec = rec("s", "human", 1, "whole program");
    exec.kind = EntryKind::Exec;
    let exec_id = j.append(&exec).unwrap();
    let mut statement = rec("s", "human", 2, "one statement");
    statement.parent_id = Some(exec_id);
    let statement_id = j.append(&statement).unwrap();

    let rows = j
        .query(&JournalQuery {
            kind: Some(EntryKind::Statement),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, statement_id);
    assert_eq!(rows[0].kind, EntryKind::Statement);
    assert_eq!(rows[0].parent_id, Some(exec_id));

    let exec_row = j.entries_by_id(&[exec_id]).unwrap().pop().unwrap();
    assert_eq!(exec_row.kind, EntryKind::Exec);
    assert_eq!(exec_row.parent_id, None);
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
fn durable_event_seed_and_ranges_are_owner_scoped_and_bounded() {
    let j = Journal::in_memory().unwrap();
    let mut coarse_ids = Vec::new();
    for n in 0..6 {
        let mut entry = rec("s", "human", n, "return");
        entry.kind = EntryKind::Exec;
        entry.ast_json = r#"{"stmts":[]}"#.into();
        let id = j.append(&entry).unwrap();
        j.finish(id, Some(0), true, 1).unwrap();
        if n % 2 == 0 {
            j.record_transcript_event(id, n, "{}").unwrap();
        }
        coarse_ids.push(id);
        // A valid evaluator-style statement row must never consume a journal
        // channel sequence.
        let fine = j.append(&rec("s", "human", n, "fine")).unwrap();
        j.finish(fine, Some(0), true, 1).unwrap();
    }
    let mut foreign = rec("s", "agent:other", 99, "return");
    foreign.kind = EntryKind::Exec;
    foreign.ast_json = r#"{"stmts":[]}"#.into();
    j.append(&foreign).unwrap();

    // Semantic type, not incidental JSON shape, defines channel membership.
    let mut program_shaped_statement = rec("s", "human", 100, "not an exec");
    program_shaped_statement.ast_json = r#"{"stmts":[]}"#.into();
    j.append(&program_shaped_statement).unwrap();
    let mut non_program_shaped_exec = rec("s", "human", 101, "still an exec");
    non_program_shaped_exec.kind = EntryKind::Exec;
    non_program_shaped_exec.ast_json = "null".into();
    let semantic_exec = j.append(&non_program_shaped_exec).unwrap();

    let seed = j.journal_event_seed("human", "s", 2).unwrap();
    assert_eq!(seed.published, 7);
    assert_eq!(seed.tail_entry_ids, vec![coarse_ids[5], semantic_exec]);
    assert_eq!(
        j.journal_event_entry_ids("human", "s", 1, 3).unwrap(),
        coarse_ids[1..4]
    );

    let transcript = j.transcript_event_seed("human", "s", 2).unwrap();
    assert_eq!(transcript.published, 3);
    assert_eq!(
        transcript.tail_entry_ids,
        vec![coarse_ids[2], coarse_ids[4]]
    );
    assert_eq!(
        j.transcript_event_entry_ids("human", "s", 1, 2).unwrap(),
        vec![coarse_ids[2], coarse_ids[4]]
    );
    assert!(
        j.journal_event_entry_ids("human", "s", u64::MAX, 1)
            .is_err(),
        "sequence offsets must never wrap through an unchecked SQL cast"
    );
    assert!(
        j.journal_event_seed("human", "s", usize::MAX).is_err(),
        "host-sized limits must be checked before binding to SQLite"
    );
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
