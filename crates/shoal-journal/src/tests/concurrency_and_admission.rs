use super::*;

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

#[test]
fn tiny_database_budget_refuses_begin_without_creating_an_audit_row() {
    let journal = Journal::in_memory_with_options(JournalOptions {
        database_max_bytes: 1,
        ..Default::default()
    })
    .unwrap();
    let error = journal
        .append(&rec("session", "human", 1, "must-not-run"))
        .unwrap_err();
    assert_eq!(
        storage_admission(&error).map(|error| error.domain),
        Some(StorageDomain::Database)
    );
    assert!(journal.query(&JournalQuery::default()).unwrap().is_empty());
}

#[test]
fn sqlite_full_is_typed_and_rolls_back_the_begin_row() {
    let journal = Journal::in_memory().unwrap();
    let pages: i64 = journal
        .conn
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    journal
        .conn
        .pragma_update(None, "max_page_count", pages)
        .unwrap();
    let mut record = rec("session", "human", 1, "full");
    record.src = "x".repeat(1024 * 1024);
    let error = journal.append(&record).unwrap_err();
    assert!(
        matches!(
            error,
            rusqlite::Error::SqliteFailure(code, _)
                if code.code == rusqlite::ErrorCode::DiskFull
        ),
        "unexpected SQLite-full error: {error}"
    );
    assert!(journal.query(&JournalQuery::default()).unwrap().is_empty());
}

#[test]
fn tiny_cas_budget_rejects_output_before_file_or_metadata_commit() {
    let journal = Journal::in_memory_with_options(JournalOptions {
        cas_max_bytes: 8,
        ..Default::default()
    })
    .unwrap();
    let id = journal.append(&rec("s", "human", 1, "capture")).unwrap();
    let error = journal
        .record_output(id, "stdout", b"sixteen bytes!!!!")
        .unwrap_err();
    assert_eq!(
        storage_admission(&error).map(|error| error.domain),
        Some(StorageDomain::Cas)
    );
    let rows = journal.query(&JournalQuery::default()).unwrap();
    assert!(rows[0].outputs.is_empty());
    let status = journal.storage_status().unwrap();
    assert_eq!(status.cas_logical_bytes, 0);
    assert_eq!(status.cas_physical_bytes, 0);
}

#[test]
fn concurrent_cas_writers_cannot_both_spend_the_same_remaining_budget() {
    let dir = tempfile::tempdir().unwrap();
    let options = JournalOptions {
        cas_max_bytes: 100,
        ..Default::default()
    };
    let journal = Journal::open_with_options(dir.path(), options).unwrap();
    let ids = [
        journal.append(&rec("s", "human", 1, "one")).unwrap(),
        journal.append(&rec("s", "human", 2, "two")).unwrap(),
    ];
    drop(journal);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers = ids.map(|id| {
        let root = dir.path().to_path_buf();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            let journal = Journal::open_with_options(&root, options).unwrap();
            barrier.wait();
            journal.record_output(id, "stdout", &[id as u8; 80])
        })
    });
    barrier.wait();
    let results = workers.map(|worker| worker.join().unwrap());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .filter_map(storage_admission)
            .filter(|error| error.domain == StorageDomain::Cas)
            .count(),
        1
    );
    let status = Journal::open_with_options(dir.path(), options)
        .unwrap()
        .storage_status()
        .unwrap();
    assert_eq!(status.cas_logical_bytes, 80);
}

#[test]
fn spill_adoption_verifies_metadata_and_rolls_back_quota_failure() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open_with_options(
        dir.path(),
        JournalOptions {
            cas_max_bytes: 8,
            ..Default::default()
        },
    )
    .unwrap();
    let spill_dir = journal.spill_dir().unwrap();
    let source = spill_dir.join("capture-spill-test");
    let payload = b"larger than budget";
    fs::write(&source, payload).unwrap();
    let hash = blake3::hash(payload).to_hex().to_string();

    assert!(
        journal
            .ingest_spill(&source, &hash, payload.len() as u64 - 1, true)
            .is_err()
    );
    assert!(source.exists(), "metadata mismatch does not consume source");
    let wrong_hash = blake3::hash(b"different").to_hex().to_string();
    assert!(
        journal
            .ingest_spill(&source, &wrong_hash, payload.len() as u64, true)
            .is_err()
    );
    assert!(source.exists(), "hash mismatch does not consume source");
    let error = journal
        .ingest_spill(&source, &hash, payload.len() as u64, true)
        .unwrap_err();
    assert_eq!(
        storage_admission(&error).map(|error| error.domain),
        Some(StorageDomain::Cas)
    );
    assert!(
        source.exists(),
        "quota refusal leaves RAII owner in control"
    );
    assert!(journal.pins().unwrap().is_empty());
    assert_eq!(journal.storage_status().unwrap().cas_logical_bytes, 0);
}

#[test]
fn pinned_content_blocks_recovery_until_explicit_unpin_and_gc() {
    let journal = Journal::in_memory_with_options(JournalOptions {
        cas_max_bytes: 100,
        ..Default::default()
    })
    .unwrap();
    let first = journal.append(&rec("s", "human", 1, "one")).unwrap();
    let hash = journal.record_output(first, "stdout", &[1; 80]).unwrap();
    journal.pin(&hash).unwrap();
    let report = journal
        .gc(GcOptions {
            max_bytes: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert!(report.deleted.is_empty(), "pins are never auto-evicted");
    let second = journal.append(&rec("s", "human", 2, "two")).unwrap();
    assert!(journal.record_output(second, "stdout", &[2; 80]).is_err());

    journal.unpin(&hash).unwrap();
    let report = journal
        .gc(GcOptions {
            max_bytes: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(report.reclaimed_bytes, 80);
    journal.record_output(second, "stdout", &[2; 80]).unwrap();
}

#[test]
fn storage_status_reconciles_database_wal_and_cas_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let id = journal.append(&rec("s", "human", 1, "persist")).unwrap();
    journal
        .record_output(id, "stdout", b"persistent bytes")
        .unwrap();
    journal.finish(id, Some(0), true, 1).unwrap();
    let before = journal.storage_status().unwrap();
    assert!(before.database_admission_bytes() > 0);
    assert_eq!(before.cas_logical_bytes, 16);
    drop(journal);

    let reopened = Journal::open(dir.path()).unwrap();
    let after = reopened.storage_status().unwrap();
    assert_eq!(after.cas_logical_bytes, before.cas_logical_bytes);
    assert_eq!(after.cas_physical_bytes, before.cas_physical_bytes);
    assert!(after.database_admission_bytes() > 0);
    assert!(after.database_admission_bytes() <= after.database_max_bytes);
}

#[test]
fn long_reader_wal_growth_eventually_refuses_new_begins_with_headroom() {
    let dir = tempfile::tempdir().unwrap();
    let options = JournalOptions {
        database_max_bytes: 512 * 1024,
        ..Default::default()
    };
    let journal = Journal::open_with_options(dir.path(), options).unwrap();
    let reader = rusqlite::Connection::open(dir.path().join("journal.db")).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM entry", [], |row| row.get(0))
        .unwrap();

    let mut accepted = 0;
    let refusal = loop {
        match journal.append(&rec("s", "human", accepted, "bounded WAL growth")) {
            Ok(id) => {
                accepted += 1;
                journal.finish(id, Some(0), true, 1).unwrap();
                assert!(accepted < 10_000, "WAL admission never converged");
            }
            Err(error) => break error,
        }
    };
    assert!(accepted > 0);
    assert_eq!(
        storage_admission(&refusal).map(|error| error.domain),
        Some(StorageDomain::Database)
    );
    let status = journal.storage_status().unwrap();
    assert!(status.wal_bytes > 0);
    assert!(status.database_admission_bytes() <= status.database_max_bytes);
    reader.execute_batch("ROLLBACK").unwrap();
}
