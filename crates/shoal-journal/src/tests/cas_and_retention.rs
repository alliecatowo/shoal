use super::*;

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

fn typed_cas_read_error(error: &io::Error) -> &CasReadError {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<CasReadError>())
        .expect("CAS safety failure must retain its typed source")
}

fn typed_sql_cas_read_error(error: &rusqlite::Error) -> &CasReadError {
    let rusqlite::Error::ToSqlConversionFailure(source) = error else {
        panic!("CAS I/O failure used an unexpected SQLite carrier: {error:?}")
    };
    let io_error = source
        .downcast_ref::<io::Error>()
        .expect("CAS SQL carrier must retain the I/O error");
    typed_cas_read_error(io_error)
}

#[test]
fn cas_rejects_compressed_size_before_zstd_parsing() {
    let journal = Journal::in_memory().unwrap();
    let id = journal
        .append(&rec("cas", "human", 1, "compressed wall"))
        .unwrap();
    let hash = journal.record_output(id, "stdout", b"tiny").unwrap();
    fs::write(
        journal.blob_path(&hash),
        vec![0u8; CAS_COMPRESSED_OVERHEAD_BYTES as usize + 5],
    )
    .unwrap();

    let error = journal
        .cas()
        .open_verified_exact(&hash, 4)
        .err()
        .expect("oversized compressed blob must fail before decoding");
    assert!(matches!(
        typed_cas_read_error(&error),
        CasReadError::CompressedLimit { actual, limit }
            if actual > limit
    ));
}

#[test]
fn cas_stops_decompression_at_the_authoritative_length() {
    let journal = Journal::in_memory().unwrap();
    let id = journal
        .append(&rec("cas", "human", 1, "decode wall"))
        .unwrap();
    let payload = vec![b'x'; 4096];
    let hash = journal.record_output(id, "stdout", &payload).unwrap();

    let error = journal
        .cas()
        .open_verified_exact(&hash, 32)
        .err()
        .expect("decompression beyond the declared length must fail");
    assert!(matches!(
        typed_cas_read_error(&error),
        CasReadError::DecompressedLimit { actual, limit: 32 }
            if *actual > 32
    ));

    let error = journal
        .cas()
        .open_verified_exact(&hash, payload.len() as u64 + 1)
        .err()
        .expect("a short decompressed blob must fail its exact-length check");
    assert_eq!(
        typed_cas_read_error(&error),
        &CasReadError::LengthMismatch {
            expected: payload.len() as u64 + 1,
            actual: payload.len() as u64,
        }
    );
}

#[test]
fn cas_refuses_oversized_materialization_before_allocation() {
    let journal = Journal::in_memory().unwrap();
    let error = journal
        .cas()
        .read_exact(
            &"0".repeat(blake3::OUT_LEN * 2),
            CAS_MATERIALIZE_MAX_BYTES + 1,
        )
        .unwrap_err();
    assert_eq!(
        typed_cas_read_error(&error),
        &CasReadError::MaterializationLimit {
            declared: CAS_MATERIALIZE_MAX_BYTES + 1,
            limit: CAS_MATERIALIZE_MAX_BYTES,
        }
    );
}

#[test]
fn read_blob_range_is_exact_bounded_and_overflow_safe() {
    let journal = Journal::in_memory().unwrap();
    let id = journal
        .append(&rec("cas", "human", 1, "paged output"))
        .unwrap();
    let payload = (0..(256 * 1024 + 31))
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let hash = journal.record_output(id, "stdout", &payload).unwrap();

    let (total, middle) = journal
        .read_blob_range(&hash, 12_345, 8_192)
        .unwrap()
        .unwrap();
    assert_eq!(total, payload.len() as u64);
    assert_eq!(middle, payload[12_345..12_345 + 8_192]);

    let (_, boundary) = journal
        .read_blob_range(&hash, payload.len() as u64 - 17, 17)
        .unwrap()
        .unwrap();
    assert_eq!(boundary, payload[payload.len() - 17..]);

    let (_, past_end) = journal
        .read_blob_range(&hash, u64::MAX, usize::MAX)
        .unwrap()
        .unwrap();
    assert!(past_end.is_empty());
}

#[test]
fn verified_page_cache_serves_exact_hits_without_redecompression() {
    let journal = Journal::in_memory().unwrap();
    let id = journal
        .append(&rec("cas", "human", 1, "cached output"))
        .unwrap();
    let payload = (0..(256 * 1024 + 31))
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let hash = journal.record_output(id, "stdout", &payload).unwrap();
    let offset = 192 * 1024;
    let expected = payload[offset..offset + 8192].to_vec();
    assert_eq!(
        journal
            .read_blob_range(&hash, offset as u64, 8192)
            .unwrap()
            .unwrap()
            .1,
        expected
    );

    // Damage the legacy single-stream backing file after the verified page is
    // cached. The exact hit remains trusted and needs no decompression, while
    // a distinct distant page must reopen, reverify, and reject corruption.
    fs::write(journal.blob_path(&hash), b"not a zstd stream").unwrap();
    assert_eq!(
        journal
            .cached_blob_range(&hash, offset as u64, 8192)
            .unwrap()
            .unwrap()
            .1,
        expected
    );
    assert!(journal.read_blob_range(&hash, 0, 8192).is_err());
}

#[test]
fn page_cache_enforces_byte_and_entry_bounds() {
    let mut cache = BlobPageCache::default();
    for index in 0..(BLOB_PAGE_CACHE_MAX_ENTRIES + 20) {
        cache.insert(BlobPageCacheEntry {
            hash: format!("{index:064x}"),
            offset: 0,
            length: 8192,
            total: 8192,
            bytes: vec![index as u8; 8192],
        });
    }
    assert!(cache.entries.len() <= BLOB_PAGE_CACHE_MAX_ENTRIES);
    assert!(cache.bytes <= BLOB_PAGE_CACHE_MAX_BYTES);
    assert!(cache.get(&format!("{:064x}", 0), 0, 8192).is_none());
    assert!(
        cache
            .get(
                &format!("{:064x}", BLOB_PAGE_CACHE_MAX_ENTRIES + 19),
                0,
                8192,
            )
            .is_some()
    );
}

/// Count regular files under `dir`, recursively.
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
fn bounded_read_preserves_pre_metadata_crash_orphan_recovery() {
    let journal = Journal::in_memory().unwrap();
    let payload = b"blob reached disk before its metadata transaction";
    let hash = blake3::hash(payload).to_hex().to_string();
    let path = journal.blob_path(&hash);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, zstd::encode_all(&payload[..], 3).unwrap()).unwrap();

    assert_eq!(journal.blob_len(&hash).unwrap(), None);
    assert_eq!(
        journal.read_blob(&hash).unwrap().as_deref(),
        Some(&payload[..])
    );
}

#[test]
fn every_public_cas_entry_rejects_non_blake3_keys_without_panicking() {
    let j = Journal::in_memory().unwrap();
    let spill = tempfile::NamedTempFile::new().unwrap();
    for malformed in ["", "00", "abcd", "zzzz", "../00"] {
        assert!(j.read_blob(malformed).unwrap().is_none());
        assert!(j.blob_len(malformed).unwrap().is_none());
        assert!(j.pin(malformed).is_err());
        assert!(j.unpin(malformed).is_err());
        assert!(j.ingest_spill(spill.path(), malformed, 0, false).is_err());
        assert_eq!(
            j.cas().read(malformed).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
}

#[test]
fn corrupted_output_hash_is_a_typed_query_error() {
    let j = Journal::in_memory().unwrap();
    let id = j.append(&rec("s", "human", 1, "echo")).unwrap();
    j.conn
        .execute(
            "INSERT INTO output(entry_id,kind,hash,len) VALUES(?1,'stdout',?2,0)",
            rusqlite::params![id, vec![0u8]],
        )
        .unwrap();
    assert!(j.query(&JournalQuery::default()).is_err());
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
fn live_spill_leases_are_counted_and_release_on_last_value_drop() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path()).unwrap();
    let payload = b"one deduplicated live capture".repeat(1024);
    let hash = blake3::hash(&payload).to_hex().to_string();
    let spill = journal.spill_dir().unwrap();

    let first_path = spill.join("first");
    fs::write(&first_path, &payload).unwrap();
    let first = journal
        .ingest_spill_leased(&first_path, &hash, payload.len() as u64)
        .unwrap();
    let second_path = spill.join("second");
    fs::write(&second_path, &payload).unwrap();
    let second = journal
        .ingest_spill_leased(&second_path, &hash, payload.len() as u64)
        .unwrap();

    assert_eq!(journal.protected_hashes().unwrap(), vec![hash.clone()]);
    let count: i64 = journal
        .conn
        .query_row(
            "SELECT ref_count FROM pin_lease WHERE hash=?1",
            [hex_bytes(&hash).unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    assert!(
        journal
            .gc(GcOptions {
                max_bytes: Some(0),
                ..Default::default()
            })
            .unwrap()
            .deleted
            .is_empty()
    );

    drop(first);
    assert_eq!(journal.protected_hashes().unwrap(), vec![hash.clone()]);
    drop(second);
    assert!(journal.protected_hashes().unwrap().is_empty());
    let report = journal
        .gc(GcOptions {
            max_bytes: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0].hash, hash);
}

#[test]
fn gc_reaps_a_crashed_owner_lease_before_selecting_blobs() {
    let journal = Journal::in_memory().unwrap();
    let id = journal.append(&rec("s", "human", 1, "spill")).unwrap();
    let hash = journal
        .record_output(id, "stdout", b"orphaned lease")
        .unwrap();
    journal
        .conn
        .execute(
            "DELETE FROM output WHERE hash=?1",
            [hex_bytes(&hash).unwrap()],
        )
        .unwrap();
    journal
        .conn
        .execute(
            "INSERT INTO pin_lease(hash,owner,ref_count) VALUES(?1,'dead-beef',1)",
            [hex_bytes(&hash).unwrap()],
        )
        .unwrap();

    let report = journal
        .gc(GcOptions {
            max_bytes: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0].hash, hash);
    let leases: i64 = journal
        .conn
        .query_row("SELECT COUNT(*) FROM pin_lease", [], |row| row.get(0))
        .unwrap();
    assert_eq!(leases, 0);
}

#[test]
fn one_journal_owner_cannot_release_another_owners_live_blob() {
    let dir = tempfile::tempdir().unwrap();
    let first_journal = Journal::open(dir.path()).unwrap();
    let second_journal = Journal::open(dir.path()).unwrap();
    let payload = b"shared across two live sessions".repeat(1024);
    let hash = blake3::hash(&payload).to_hex().to_string();

    let first_path = first_journal.spill_dir().unwrap().join("owner-one");
    fs::write(&first_path, &payload).unwrap();
    let first = first_journal
        .ingest_spill_leased(&first_path, &hash, payload.len() as u64)
        .unwrap();
    let second_path = second_journal.spill_dir().unwrap().join("owner-two");
    fs::write(&second_path, &payload).unwrap();
    let second = second_journal
        .ingest_spill_leased(&second_path, &hash, payload.len() as u64)
        .unwrap();

    drop(first);
    drop(first_journal);
    assert_eq!(
        second_journal.protected_hashes().unwrap(),
        vec![hash.clone()]
    );
    assert!(
        second_journal
            .gc(GcOptions {
                max_bytes: Some(0),
                ..Default::default()
            })
            .unwrap()
            .deleted
            .is_empty(),
        "the second owner's OS lock must keep its lease live"
    );

    drop(second);
    assert!(second_journal.protected_hashes().unwrap().is_empty());
    assert_eq!(
        second_journal
            .gc(GcOptions {
                max_bytes: Some(0),
                ..Default::default()
            })
            .unwrap()
            .deleted
            .len(),
        1
    );
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
fn corrupted_blob_hash_is_a_typed_gc_error() {
    let j = Journal::in_memory().unwrap();
    j.conn
        .execute(
            "INSERT INTO blob(hash,stored_len,created_ns,last_access_ns) VALUES(?1,0,0,0)",
            [vec![0u8]],
        )
        .unwrap();
    let result = j.gc(GcOptions {
        ttl: Some(std::time::Duration::ZERO),
        max_bytes: Some(0),
        dry_run: false,
    });
    assert!(result.is_err());
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

    // Keep the forged body the same length so this fixture specifically
    // reaches the content-address integrity check rather than the earlier,
    // independently tested exact-length wall.
    let forged = zstd::encode_all(&b"forged! payload"[..], 3).unwrap();
    fs::write(j.blob_path(&hash), forged).unwrap();

    let error = j
        .read_blob(&hash)
        .expect_err("a content-hash mismatch must be an integrity error");
    assert!(matches!(
        typed_sql_cas_read_error(&error),
        CasReadError::HashMismatch { hash: actual } if actual == &hash
    ));
    let stream_err = j.cas().open_verified(&hash);
    assert!(
        stream_err.is_err(),
        "streaming reads must verify before exposing corrupt bytes"
    );
}
