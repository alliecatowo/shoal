use super::*;

/// End-to-end proof that a real (on-disk) kernel session's own
/// `Evaluator` gets a journal installed automatically, with no test-side
/// probe injection (unlike `handlers_exec::tests::
/// exec_calls_set_source_so_stmt_journal_entries_carry_src`, which
/// installs a journal by hand because it drives an ephemeral
/// `Kernel::new()` — deliberately the ONE case that must stay journal-
/// less, since it has no on-disk state dir at all). `Kernel::open` gives
/// this kernel a real `state_dir`, so `session()` should now open a
/// second journal handle on it and hand it to the session's evaluator
/// (`crates/shoal-kernel/src/session.rs`). Attach, run a marker
/// statement, then run the in-language `history` builtin — all over the
/// same real Unix-socket wire `session.attach`/`exec` use in production —
/// and confirm the marker's exact source text comes back in `history`'s
/// `src` column. Before the fix, `session()` never called
/// `Evaluator::set_journal` at all, so `history` inside a kernel session
/// always came back empty regardless of what the kernel's own separate,
/// coarser exec-level journal (`self.persistence.journal`, `journal.query`) recorded.
#[test]
fn kernel_open_installs_a_session_journal_so_history_builtin_sees_real_data() {
    let dir = tempfile::tempdir().unwrap();
    let human_token = create_local_human_token(dir.path());
    let kernel = Kernel::open(dir.path()).unwrap();
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let server_kernel = kernel.clone();
    let thread = std::thread::spawn(move || server_kernel.handle_stream(server).unwrap());
    attach_bearer(&mut client, &mut reader, &human_token);
    let marker_src = "let kernel_journal_probe_4471 = 4471";
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src": marker_src}),
    );
    assert!(
        exec.error.is_none(),
        "marker exec must succeed: {:?}",
        exec.error
    );

    let hist = call(
        &mut client,
        &mut reader,
        3,
        "exec",
        json!({"src": "history"}),
    );
    let hist_result = hist.result.expect("exec of `history` must succeed");
    let cols = hist_result["value"]["cols"]["src"]
        .as_array()
        .unwrap_or_else(|| panic!("history's table has no src column: {hist_result:?}"));
    assert!(
        cols.iter().any(|v| v["v"] == marker_src),
        "no journal entry with src={marker_src:?} found among {cols:?} — the session \
             evaluator has no journal installed, so the in-language `history` builtin is inert \
             even though a real on-disk Kernel::open must install one automatically"
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// site/content/internals/kernel-protocol.md (`pty.list` / `shoal://pty`): open PTY sessions are
/// first-class and session-scoped. A pty opened by session A is enumerated
/// by A's `pty.list`, is invisible to a DIFFERENT session B (both the list
/// and a direct `pty.read` of A's ref — an opaque `UNKNOWN_PTY`), and
/// leaves A's list once closed. Drives a real `cat` on a PTY like the live
/// MCP test, over the same Unix-socket wire production uses.
#[test]
fn pty_list_is_session_scoped() {
    let kernel = Kernel::new();
    // Session A on connection one.
    let (mut a, server) = UnixStream::pair().unwrap();
    let mut a_reader = BufReader::new(a.try_clone().unwrap());
    let ka = kernel.clone();
    let ta = std::thread::spawn(move || serve_embedded_test_stream(ka, server).unwrap());
    call(
        &mut a,
        &mut a_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"A","client":{"kind":"agent","tty":false}}),
    );
    let opened = call(&mut a, &mut a_reader, 2, "pty.open", json!({"cmd":"cat"}));
    let pty_id = opened.result.unwrap()["pty_id"]
        .as_str()
        .expect("pty.open returns a pty_id")
        .to_owned();

    // A sees exactly its one pty, with the documented shape.
    let list_a = call(&mut a, &mut a_reader, 3, "pty.list", json!({}));
    let ptys_a = list_a.result.unwrap()["ptys"].as_array().unwrap().clone();
    assert_eq!(ptys_a.len(), 1, "session A sees its one pty: {ptys_a:?}");
    assert_eq!(ptys_a[0]["pty_id"], json!(pty_id));
    assert_eq!(ptys_a[0]["cmd"], "cat");
    assert_eq!(ptys_a[0]["alive"], true);
    assert!(ptys_a[0]["pid"].as_u64().unwrap() > 0);
    assert!(ptys_a[0]["cols"].as_u64().unwrap() > 0);
    assert!(ptys_a[0]["rows"].as_u64().unwrap() > 0);

    // Session B on a second connection: a different session must NOT see
    // A's ptys, and cannot read A's pty by ref (opaque not-found).
    let (mut b, server) = UnixStream::pair().unwrap();
    let mut b_reader = BufReader::new(b.try_clone().unwrap());
    let kb = kernel.clone();
    let tb = std::thread::spawn(move || serve_embedded_test_stream(kb, server).unwrap());
    call(
        &mut b,
        &mut b_reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"B","client":{"kind":"agent","tty":false}}),
    );
    let list_b = call(&mut b, &mut b_reader, 2, "pty.list", json!({}));
    assert!(
        list_b.result.unwrap()["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "session B must not see session A's ptys"
    );
    let read_b = call(
        &mut b,
        &mut b_reader,
        3,
        "pty.read",
        json!({"pty_id": pty_id}),
    );
    assert_eq!(
        read_b.error.expect("B cannot read A's pty").code,
        UNKNOWN_PTY,
        "another session's pty ref is an opaque UNKNOWN_PTY"
    );

    // Closing from A drops it out of A's list.
    call(
        &mut a,
        &mut a_reader,
        4,
        "pty.close",
        json!({"pty_id": pty_id}),
    );
    let list_a2 = call(&mut a, &mut a_reader, 5, "pty.list", json!({}));
    assert!(
        list_a2.result.unwrap()["ptys"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a closed pty must leave pty.list"
    );

    drop(a);
    drop(a_reader);
    ta.join().unwrap();
    drop(b);
    drop(b_reader);
    tb.join().unwrap();
}

// -----------------------------------------------------------------------
// The elision rule (site/content/internals/kernel-protocol.md).
// -----------------------------------------------------------------------

/// A >100-row table (real `ls` over a directory with 150 files, not a
/// synthetic stand-in) must come back elided: shape + schema + a 5-row
/// preview, never the 150-row payload. Then drill into a single row by
/// field-path and confirm that small result is NOT elided.
#[test]
fn big_table_exec_elides_then_drills_by_path() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..150 {
        std::fs::write(dir.path().join(format!("f{i:04}.txt")), b"x").unwrap();
    }
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src": format!("ls {}", dir.path().display())}),
    );
    let result = exec.result.expect("ls must succeed");
    let value_ref = result["ref"].as_str().unwrap().to_owned();
    // `ls` is a command: its wire shape is `outcome` with a structured
    // `.out`. Elision unwraps to `.out` for the decision (mirroring
    // render_block's outcome-unification) — the 150-row *table* elides,
    // the outer outcome envelope (status/ok/cmd/…) still travels.
    let value = &result["value"];
    assert_eq!(value["$"], "outcome");
    let out = &value["out"];
    assert_eq!(out["$"], "ref", "a 150-row table must elide, got {out}");
    assert_eq!(out["of"], "table");
    assert_eq!(out["n"], 150);
    assert_eq!(
        out["cols"]["name"], "str",
        "shape (schema) travels even when the payload does not"
    );
    assert_eq!(out["preview"]["$"], "table");
    assert_eq!(
        out["preview"]["n"], 5,
        "preview is a small head, not the full 150 rows"
    );
    assert!(out["render_head"].as_str().unwrap().contains("name"));
    let wire_len = serde_json::to_string(value).unwrap().len();
    assert!(
        wire_len < 4 * 1024,
        "the elided form itself must stay tiny, was {wire_len} bytes"
    );

    // Drill in: value.get with a field-path returns one small row —
    // NOT elided, because it never hits any threshold.
    let get = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref, "path": "out[3]"}),
    );
    let drilled = get.result.unwrap()["value"].clone();
    assert_ne!(
        drilled["$"], "ref",
        "a single drilled row must not be elided: {drilled}"
    );
    assert_eq!(drilled["$"], "record");
    assert!(
        drilled["v"]["name"].is_object(),
        "drilled row keeps its fields: {drilled}"
    );

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn small_value_is_not_elided() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"[1,2,3]"}),
    );
    let value = exec.result.unwrap()["value"].clone();
    assert_eq!(
        value["$"], "list",
        "a 3-item list is nowhere near any threshold"
    );
    assert_eq!(
        value["v"],
        json!([{"$":"int","v":1},{"$":"int","v":2},{"$":"int","v":3}])
    );
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// A caller may loosen the byte budget, but never past the 64 KiB hard
/// cap — a misbehaving agent cannot flood its own context by asking
/// nicely.
#[test]
fn elision_hard_cap_cannot_be_disabled() {
    let huge = Value::Str("x".repeat(100_000));
    let loosened = ElideSpec {
        max_bytes: Some(5_000_000),
        max_rows: None,
        max_items: None,
    };
    let budget = ElideBudget::from_spec(Some(&loosened));
    assert_eq!(
        budget.max_bytes, ELIDE_HARD_CAP,
        "a requested budget above the hard cap must clamp down to it"
    );
    match elide_wire_value(&huge, "shoal://out/1", &budget) {
        WireValue::Ref { of, n, .. } => {
            assert_eq!(of, "str");
            assert_eq!(n, 100_000);
        }
        other => panic!(
            "a 100 KB string must still elide despite a 5 MB requested budget, got {other:?}"
        ),
    }
}

/// The flip side: loosening below the hard cap is honored, so a caller
/// that wants a bit more headroom than the 8 KiB default legitimately
/// gets it.
#[test]
fn elision_budget_can_be_loosened_up_to_the_hard_cap() {
    let modest = Value::Str("y".repeat(20_000)); // > 8 KiB default, < 64 KiB cap
    let loosened = ElideSpec {
        max_bytes: Some(5_000_000),
        max_rows: None,
        max_items: None,
    };
    let budget = ElideBudget::from_spec(Some(&loosened));
    match elide_wire_value(&modest, "shoal://out/1", &budget) {
        WireValue::Str { .. } => {}
        other => panic!("a 20 KiB string fits under a loosened 64 KiB cap, got {other:?}"),
    }
}

#[test]
fn value_get_elide_param_tightens_default_row_threshold() {
    let (mut client, server) = UnixStream::pair().unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let kernel = Kernel::new();
    let server_kernel = kernel.clone();
    let thread =
        std::thread::spawn(move || serve_embedded_test_stream(server_kernel, server).unwrap());
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","client":{"kind":"agent","tty":false}}),
    );
    // 10 items: under every default threshold, so a plain exec would not elide.
    let exec = call(
        &mut client,
        &mut reader,
        2,
        "exec",
        json!({"src":"[0,1,2,3,4,5,6,7,8,9]"}),
    );
    assert_ne!(exec.result.as_ref().unwrap()["value"]["$"], "ref");
    let value_ref = exec.result.unwrap()["ref"].as_str().unwrap().to_owned();
    // A caller may tighten the budget per call — max_items:5 must elide
    // this same 10-item list on a follow-up `value.get`.
    let get = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref": value_ref, "path": null, "slice": null, "elide": {"max_items": 5}}),
    );
    let value = get.result.unwrap()["value"].clone();
    assert_eq!(
        value["$"], "ref",
        "a tightened per-call budget must elide: {value}"
    );
    assert_eq!(value["n"], 10);
    drop(client);
    drop(reader);
    thread.join().unwrap();
}

/// site/content/internals/language-conformance-contract.md wire follow-up: `value.get` RESOLVES a CAS-backed bytes ref. A
/// top-level `CasBytes` (a value-position capture spilled to the CAS) elides
/// to a `ref` on the default `format=json` path — a huge blob never ships
/// whole — but an explicit `slice` or `format=raw` fetches the real content
/// from the CAS through the value's own loader (the same `BytesLoad`/`Cas`
/// seam), honoring the elision wall on what actually travels back.
#[test]
fn value_get_resolves_cas_backed_bytes_ref() {
    struct FixedLoader(Vec<u8>);
    impl shoal_value::BytesLoad for FixedLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            Ok(self.0.clone())
        }

        fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            Ok(Box::new(std::io::Cursor::new(self.0.clone())))
        }
    }
    struct FailLoader;
    impl shoal_value::BytesLoad for FailLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "blob gone",
            ))
        }

        fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "blob gone",
            ))
        }
    }
    let decode =
        |s: &str| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap();

    let kernel = Kernel::new();
    // Pre-populate a session transcript with a CAS-backed bytes value (5000
    // bytes — well over the raw budget, so the default fetch must elide) and
    // a second one whose loader fails (an unresolvable ref).
    let content: Vec<u8> = (0u32..5000).map(|i| (i % 251) as u8).collect();
    let session = kernel.session("casb", &principal()).unwrap();
    {
        let ok = std::sync::Arc::new(shoal_value::CasBytesVal {
            hash: "a".repeat(64),
            len: content.len() as u64,
            preview: std::sync::Arc::new(content[..64].to_vec()),
            truncated: false,
            loader: std::sync::Arc::new(FixedLoader(content.clone())),
        });
        let broken = std::sync::Arc::new(shoal_value::CasBytesVal {
            hash: "b".repeat(64),
            len: 123,
            preview: std::sync::Arc::new(Vec::new()),
            truncated: false,
            loader: std::sync::Arc::new(FailLoader),
        });
        let mut t = session
            .transcript
            .lock()
            .expect("test lock should not be poisoned");
        t.insert(Ref::new("out", 1u64), Value::CasBytes(ok));
        t.insert(Ref::new("out", 2u64), Value::CasBytes(broken));
    }

    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"casb","client":{"kind":"agent","tty":false}}),
    );

    // Default json: a CAS-backed value elides to an honest ref (no content),
    // carrying the TRUE length — a huge blob never ships whole.
    let def = call(
        &mut client,
        &mut reader,
        2,
        "value.get",
        json!({"ref":"out:1"}),
    );
    let v = def.result.unwrap()["value"].clone();
    assert_eq!(v["$"], "ref", "a CAS-backed value elides by default: {v}");
    assert_eq!(v["of"], "bytes");
    assert_eq!(
        v["n"],
        content.len(),
        "the elided ref carries the true length"
    );

    // A small slice RESOLVES to the exact CAS bytes, inline.
    let sl = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref":"out:1","slice":[0,10]}),
    );
    let v = sl.result.unwrap()["value"].clone();
    assert_eq!(v["$"], "bytes", "a small slice resolves inline: {v}");
    assert_eq!(decode(v["v"].as_str().unwrap()), content[0..10]);

    // This value fits under one raw page, so format=raw resolves it completely.
    let raw = call(
        &mut client,
        &mut reader,
        4,
        "value.get",
        json!({"ref":"out:1","format":"raw"}),
    );
    assert_eq!(
        decode(raw.result.unwrap()["raw_base64"].as_str().unwrap()),
        content
    );

    // slice + format=raw resolves exactly the requested sub-range.
    let rawslice = call(
        &mut client,
        &mut reader,
        5,
        "value.get",
        json!({"ref":"out:1","slice":[5,15],"format":"raw"}),
    );
    assert_eq!(
        decode(rawslice.result.unwrap()["raw_base64"].as_str().unwrap()),
        content[5..15]
    );

    // A slice that is itself still oversized re-elides at the wall.
    let big = call(
        &mut client,
        &mut reader,
        6,
        "value.get",
        json!({"ref":"out:1","slice":[0,5000]}),
    );
    assert_eq!(
        big.result.unwrap()["value"]["$"],
        "ref",
        "an oversized slice re-elides rather than shipping whole"
    );

    // An unresolvable ref (its CAS blob is gone) is a clear error, no panic.
    let bad = call(
        &mut client,
        &mut reader,
        7,
        "value.get",
        json!({"ref":"out:2","slice":[0,1]}),
    );
    assert!(
        bad.error.is_some(),
        "a failed CAS resolution surfaces an error, not a panic"
    );

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn raw_value_pages_are_bounded_pageable_and_stream_cas_content() {
    struct StreamingLoader {
        bytes: Vec<u8>,
        loads: Arc<AtomicUsize>,
        opens: Arc<AtomicUsize>,
    }
    impl shoal_value::BytesLoad for StreamingLoader {
        fn load(&self) -> std::io::Result<Vec<u8>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other(
                "raw paging must not materialize the complete CAS value",
            ))
        }

        fn open(&self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(std::io::Cursor::new(self.bytes.clone())))
        }
    }

    let decode =
        |s: &str| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s).unwrap();
    let content = (0..(ELIDE_HARD_CAP + RAW_PAGE_MAX_BYTES + 11))
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let loads = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let kernel = Kernel::new();
    let session = kernel.session("paged", &principal()).unwrap();
    {
        let mut transcript = session
            .transcript
            .lock()
            .expect("test lock should not be poisoned");
        transcript.insert(
            Ref::new("out", 1u64),
            Value::Bytes(Arc::new(content.clone())),
        );
        transcript.insert(
            Ref::new("out", 2u64),
            Value::Str("\u{0000}\u{0001}🦀".repeat(RAW_PAGE_MAX_BYTES)),
        );
        transcript.insert(
            Ref::new("out", 3u64),
            Value::CasBytes(Arc::new(shoal_value::CasBytesVal {
                hash: "c".repeat(64),
                len: content.len() as u64,
                preview: Arc::new(content[..64].to_vec()),
                truncated: false,
                loader: Arc::new(StreamingLoader {
                    bytes: content.clone(),
                    loads: loads.clone(),
                    opens: opens.clone(),
                }),
            })),
        );
    }
    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"paged","client":{"kind":"agent","tty":false}}),
    );

    let first = call(
        &mut client,
        &mut reader,
        2,
        "value.get",
        json!({"ref":"out:1","format":"raw"}),
    );
    let first_json = serde_json::to_vec(&first).unwrap();
    assert!(
        first_json.len() < 64 * 1024,
        "raw wire page is hard bounded"
    );
    let first = first.result.unwrap();
    assert_eq!(first["page"]["returned_len"], RAW_PAGE_MAX_BYTES);
    assert_eq!(first["page"]["next_offset"], RAW_PAGE_MAX_BYTES);
    assert_eq!(first["page"]["done"], false);
    assert_eq!(
        decode(first["raw_base64"].as_str().unwrap()),
        content[..RAW_PAGE_MAX_BYTES]
    );

    let huge_end = call(
        &mut client,
        &mut reader,
        3,
        "value.get",
        json!({"ref":"out:1","format":"raw","slice":[RAW_PAGE_MAX_BYTES, usize::MAX]}),
    )
    .result
    .unwrap();
    assert_eq!(huge_end["page"]["offset"], RAW_PAGE_MAX_BYTES);
    assert_eq!(huge_end["page"]["returned_len"], RAW_PAGE_MAX_BYTES);

    let string = call(
        &mut client,
        &mut reader,
        4,
        "value.get",
        json!({"ref":"out:2","format":"raw"}),
    );
    assert!(serde_json::to_vec(&string).unwrap().len() < 64 * 1024);
    let string = string.result.unwrap();
    assert_eq!(string["page"]["unit"], "unicode_scalar");
    assert!(string["raw"].as_str().unwrap().len() <= RAW_PAGE_MAX_BYTES);

    let oversized_json_slice = call(
        &mut client,
        &mut reader,
        5,
        "value.get",
        json!({"ref":"out:3","slice":[0, usize::MAX]}),
    );
    assert_eq!(oversized_json_slice.error.unwrap().code, BAD_PATH_OR_SLICE);
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert_eq!(opens.load(Ordering::SeqCst), 0);

    let cas = call(
        &mut client,
        &mut reader,
        6,
        "value.get",
        json!({"ref":"out:3","format":"raw","slice":[13, usize::MAX]}),
    )
    .result
    .unwrap();
    assert_eq!(
        decode(cas["raw_base64"].as_str().unwrap()),
        content[13..13 + RAW_PAGE_MAX_BYTES]
    );
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert_eq!(opens.load(Ordering::SeqCst), 1);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn blob_get_pages_large_content_and_handles_offset_overflow() {
    let kernel = Kernel::new();
    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"blob-pages","client":{"kind":"agent","tty":false}}),
    );
    let source = format!("\"{}\"", "z".repeat(RAW_PAGE_MAX_BYTES * 3));
    let exec = call(&mut client, &mut reader, 2, "exec", json!({"src": source}));
    assert!(exec.error.is_none(), "large string exec succeeds: {exec:?}");
    let hash = {
        let journal = kernel
            .persistence
            .journal
            .lock()
            .expect("test lock should not be poisoned");
        journal
            .query(&JournalQuery::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.session == "blob-pages")
            .unwrap()
            .outputs
            .into_iter()
            .find(|output| output.kind == "value")
            .unwrap()
            .hash
    };

    let first = call(
        &mut client,
        &mut reader,
        3,
        "blob.get",
        json!({"hash":hash,"offset":0,"length":u64::MAX}),
    );
    assert!(serde_json::to_vec(&first).unwrap().len() < 64 * 1024);
    let first = first.result.unwrap();
    assert_eq!(first["page"]["returned_len"], RAW_PAGE_MAX_BYTES);
    assert_eq!(first["page"]["request_truncated"], true);
    let total = first["page"]["total_len"].as_u64().unwrap();
    assert!(total > RAW_PAGE_MAX_BYTES as u64);

    let last_offset = total - RAW_PAGE_MAX_BYTES as u64;
    let last = call(
        &mut client,
        &mut reader,
        4,
        "blob.get",
        json!({"hash":hash,"offset":last_offset,"length":RAW_PAGE_MAX_BYTES}),
    )
    .result
    .unwrap();
    assert_eq!(last["page"]["offset"], last_offset);
    assert_eq!(last["page"]["returned_len"], RAW_PAGE_MAX_BYTES);
    assert_eq!(last["page"]["done"], true);
    assert_eq!(last["page"]["next_offset"], Json::Null);

    let overflow = call(
        &mut client,
        &mut reader,
        5,
        "blob.get",
        json!({"hash":hash,"offset":u64::MAX,"length":u64::MAX}),
    )
    .result
    .unwrap();
    assert_eq!(overflow["page"]["offset"], total);
    assert_eq!(overflow["page"]["returned_len"], 0);
    assert_eq!(overflow["page"]["done"], true);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn blob_page_cache_hits_are_free_but_random_misses_are_owner_rate_limited() {
    let kernel = Kernel::builder()
        .limits(Limits {
            max_blob_decompressions_per_window: 2,
            blob_decompression_window_ms: 60_000,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let (mut client, mut reader, thread) = spawn(&kernel);
    call(
        &mut client,
        &mut reader,
        1,
        "session.attach",
        json!({"local_auth":"local-human","session":"blob-rate","client":{"kind":"agent","tty":false}}),
    );
    let source = format!("\"{}\"", "r".repeat(RAW_PAGE_MAX_BYTES * 5));
    assert!(
        call(&mut client, &mut reader, 2, "exec", json!({"src":source}))
            .error
            .is_none()
    );
    let hash = {
        let journal = kernel
            .persistence
            .journal
            .lock()
            .expect("test lock should not be poisoned");
        journal
            .query(&JournalQuery::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.session == "blob-rate")
            .unwrap()
            .outputs
            .into_iter()
            .find(|output| output.kind == "value")
            .unwrap()
            .hash
    };

    for (id, offset) in [(3, 0), (4, 0), (5, RAW_PAGE_MAX_BYTES as u64)] {
        let page = call(
            &mut client,
            &mut reader,
            id,
            "blob.get",
            json!({"hash":hash,"offset":offset,"length":RAW_PAGE_MAX_BYTES}),
        );
        assert!(
            page.error.is_none(),
            "page {offset} should be admitted: {page:?}"
        );
    }
    let denied = call(
        &mut client,
        &mut reader,
        6,
        "blob.get",
        json!({"hash":hash,"offset":u64::MAX,"length":u64::MAX}),
    );
    let error = denied
        .error
        .expect("third distinct decompression is limited");
    assert_eq!(error.code, QUOTA_EXCEEDED);
    assert_eq!(error.data.as_ref().unwrap()["max"], 2);
    assert_eq!(
        error.data.as_ref().unwrap()["owner"]["session"],
        "blob-rate"
    );

    drop(client);
    drop(reader);
    thread.join().unwrap();
}

#[test]
fn blob_decompression_budget_is_private_to_exact_session_owner() {
    fn attach_and_exec(
        client: &mut UnixStream,
        reader: &mut BufReader<UnixStream>,
        id: i64,
        session: &str,
        source: &str,
    ) {
        call(
            client,
            reader,
            id,
            "session.attach",
            json!({"local_auth":"local-human","session":session,"client":{"kind":"agent","tty":false}}),
        );
        assert!(
            call(client, reader, id + 1, "exec", json!({"src":source}))
                .error
                .is_none()
        );
    }

    let kernel = Kernel::builder()
        .limits(Limits {
            max_blob_decompressions_per_window: 1,
            blob_decompression_window_ms: 60_000,
            ..Limits::default()
        })
        .build()
        .unwrap();
    let (mut client, mut reader, thread) = spawn(&kernel);
    let source = format!("\"{}\"", "o".repeat(RAW_PAGE_MAX_BYTES * 5));

    attach_and_exec(&mut client, &mut reader, 1, "owner-a", &source);
    let hash = {
        let journal = kernel
            .persistence
            .journal
            .lock()
            .expect("test lock should not be poisoned");
        journal
            .query(&JournalQuery::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.session == "owner-a")
            .unwrap()
            .outputs
            .into_iter()
            .find(|output| output.kind == "value")
            .unwrap()
            .hash
    };
    assert!(
        call(
            &mut client,
            &mut reader,
            3,
            "blob.get",
            json!({"hash":hash,"offset":0,"length":RAW_PAGE_MAX_BYTES}),
        )
        .error
        .is_none()
    );

    attach_and_exec(&mut client, &mut reader, 4, "owner-b", &source);
    assert!(
        call(
            &mut client,
            &mut reader,
            6,
            "blob.get",
            json!({"hash":hash,"offset":RAW_PAGE_MAX_BYTES,"length":RAW_PAGE_MAX_BYTES}),
        )
        .error
        .is_none(),
        "owner B has an independent decompression budget"
    );

    call(
        &mut client,
        &mut reader,
        7,
        "session.attach",
        json!({"local_auth":"local-human","session":"owner-a","client":{"kind":"agent","tty":false}}),
    );
    let denied = call(
        &mut client,
        &mut reader,
        8,
        "blob.get",
        json!({"hash":hash,"offset":RAW_PAGE_MAX_BYTES * 2,"length":RAW_PAGE_MAX_BYTES}),
    );
    assert_eq!(denied.error.unwrap().code, QUOTA_EXCEEDED);

    drop(client);
    drop(reader);
    thread.join().unwrap();
}
