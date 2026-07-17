use super::*;

#[test]
fn overlay_replaces_duplicates_but_keeps_disjoint_commands() {
    let mut earlier = AdapterCatalog::empty();
    earlier.cmds.insert(
        "same".into(),
        CmdAdapter {
            name: "same".into(),
            bin: "old".into(),
            class: AdapterClass::Cli,
            ok_codes: vec![0],
            invoke_payload: InvokePayload::Arg,
            top: SubSpec::default(),
            subs: HashMap::new(),
        },
    );
    earlier.cmds.insert(
        "kept".into(),
        CmdAdapter {
            name: "kept".into(),
            bin: "kept".into(),
            class: AdapterClass::Cli,
            ok_codes: vec![0],
            invoke_payload: InvokePayload::Arg,
            top: SubSpec::default(),
            subs: HashMap::new(),
        },
    );
    let mut later = AdapterCatalog::empty();
    later.cmds.insert(
        "same".into(),
        CmdAdapter {
            name: "same".into(),
            bin: "new".into(),
            class: AdapterClass::Cli,
            ok_codes: vec![0],
            invoke_payload: InvokePayload::Arg,
            top: SubSpec::default(),
            subs: HashMap::new(),
        },
    );

    assert_eq!(earlier.overlay(&later), vec!["same"]);
    assert_eq!(earlier.lookup("same").unwrap().bin, "new");
    assert!(earlier.lookup("kept").is_some());
}

#[test]
fn loads_catalog_and_survives_bad_file() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("git.toml"),
        r#"[cmd.git]
bin="git"
class="cli"
ok_codes=[0,1]
[cmd.git.sub.status]
params={short="bool", n="int?"}
positional=["n"]
flags={short={s="short"}}
effects=["fs.read(cwd)"]
output={parse="porcelain-v2", type="table<{status: str, path: path}>"}
"#,
    )
    .unwrap();
    fs::write(d.path().join("bad.toml"), "[[[").unwrap();
    let (c, warnings) = AdapterCatalog::load_dir(d.path());
    assert_eq!(warnings.len(), 1);
    let git = c.lookup("git").unwrap();
    assert_eq!(git.ok_codes, [0, 1]);
    assert_eq!(git.subs["status"].short_flags["s"], "short");
}

#[test]
fn hostile_catalog_files_warn_without_hiding_valid_siblings() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("good.toml"),
        "[cmd.good]\nbin='true'\nunknown_future_field='preserved'\n",
    )
    .unwrap();
    let large = directory.path().join("large.toml");
    let file = fs::File::create(&large).unwrap();
    file.set_len((MAX_ADAPTER_MANIFEST_BYTES + 1) as u64)
        .unwrap();
    fs::write(directory.path().join("binary.toml"), [0xff]).unwrap();
    fs::write(
        directory.path().join("deep.toml"),
        format!(
            "x={}0{}\n",
            "[".repeat(MAX_ADAPTER_TOML_NESTING + 1),
            "]".repeat(MAX_ADAPTER_TOML_NESTING + 1)
        ),
    )
    .unwrap();
    fs::write(
        directory.path().join("duplicate.toml"),
        "[cmd.duplicate]\nbin='one'\nbin='two'\n",
    )
    .unwrap();

    let (catalog, warnings) = AdapterCatalog::load_dir(directory.path());
    assert!(catalog.lookup("good").is_some());
    assert_eq!(catalog.len(), 1);
    for needle in ["byte limit", "UTF-8", "nesting limit", "duplicate key"] {
        assert!(
            warnings.iter().any(|warning| warning.contains(needle)),
            "missing {needle:?} warning: {warnings:#?}"
        );
    }
}

#[test]
fn catalog_command_identities_are_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let source = (0..=MAX_ADAPTER_CATALOG_COMMANDS)
        .map(|index| format!("[cmd.c{index:04}]\nbin='true'\n"))
        .collect::<String>();
    fs::write(directory.path().join("wide.toml"), source).unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(directory.path());
    assert_eq!(catalog.len(), MAX_ADAPTER_CATALOG_COMMANDS);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("catalog command limit"))
    );
}

#[test]
fn catalog_manifest_identities_are_bounded_deterministically() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..=MAX_ADAPTER_MANIFEST_FILES {
        fs::write(
            directory.path().join(format!("m{index:04}.toml")),
            format!("[cmd.c{index:04}]\nbin='true'\n"),
        )
        .unwrap();
    }
    let (catalog, warnings) = AdapterCatalog::load_dir(directory.path());
    assert_eq!(catalog.len(), MAX_ADAPTER_MANIFEST_FILES);
    assert!(catalog.lookup("c0000").is_some());
    assert!(
        catalog
            .lookup(&format!("c{:04}", MAX_ADAPTER_MANIFEST_FILES))
            .is_none()
    );
    assert!(warnings.iter().any(|warning| warning.contains("omitted")));
}

#[test]
fn production_catalog_loader_has_no_whole_file_read() {
    let production = include_str!("../lib.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(!production.contains("fs::read_to_string"));
    let input = include_str!("../catalog_input.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(input.contains("MAX_ADAPTER_MANIFEST_BYTES + 1"));
}
#[test]
fn parses_json_ndjson_and_lines() {
    assert!(matches!(
        parse_output("json", br#"[{"a":1}]"#, None),
        Some(Value::Table(_))
    ));
    assert!(
        matches!(parse_output("ndjson", b"{\"a\":1}\n{\"a\":2}\n", None), Some(Value::Table(t)) if t.len()==2)
    );
    assert_eq!(
        parse_output("lines", b"a\r\nb\n", None),
        Some(Value::List(vec![
            Value::Str("a".into()),
            Value::Str("b".into())
        ]))
    );
}
#[test]
fn parses_csv_quotes_and_z_records() {
    let v = parse_output(
        "csv",
        b"name,note,n\nfoo,\"a,b\",42\n",
        Some("table<{name: str, note: str, n: int}>"),
    )
    .unwrap();
    assert!(
        matches!(v, Value::Table(t) if t[0]["note"] == Value::Str("a,b".into()) && t[0]["n"] == Value::Int(42))
    );
    let h = "table<{hash: str, author: str, path: path}>";
    assert!(
        matches!(parse_output("z-records", b"abc\0Allie\0a.rs\0def\0Bob\0b.rs\0", Some(h)), Some(Value::Table(t)) if t.len()==2 && matches!(t[0]["path"], Value::Path(_)))
    );
}
#[test]
fn parses_kv_and_porcelain() {
    assert!(
        matches!(parse_output("kv", b"a=1\nb: two\n", None), Some(Value::Record(r)) if r.len()==2)
    );
    let p = parse_output(
        "porcelain-v2",
        b"? new file.txt\n1 .M N... 100644 100644 100644 a b src/lib.rs\n",
        None,
    );
    assert!(matches!(p, Some(Value::Table(t)) if t.len()==2));
}
// `cols2` (added for `vmstat(8)`, which stacks a fixed two-line banner --
// a category header directly above the real per-column header, with no
// flag to suppress just one of the two -- unlike `ps`/`df`'s single
// cleanly-discardable header line) discards exactly 2 leading lines
// before treating every remaining line as a data row, reusing the same
// whitespace-run splitting + last-column-overflow-merge rules as `cols`.
#[test]
fn cols2_discards_two_leading_lines_then_parses_like_cols() {
    let h = "table<{r: int, b: int, swpd: size_kb, free: size_kb}>";
    let good = b"procs -----------memory----------\n\
                      r  b   swpd   free\n\
                      2  1 8388028 310712\n";
    let v = parse_output("cols2", good, Some(h)).expect("vmstat-shaped output must parse");
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["r"], Value::Int(2));
    assert_eq!(rows[0]["b"], Value::Int(1));
    assert_eq!(rows[0]["swpd"], Value::Size(8388028 * 1024));
    assert_eq!(rows[0]["free"], Value::Size(310712 * 1024));

    // A malformed data row (fewer fields than the hint, once the two
    // leading lines are discarded) must degrade the whole parse to
    // `None`, not silently misalign columns -- same mismatch rule as
    // plain `cols`.
    let malformed = b"procs -----------memory----------\n\
                           r  b   swpd   free\n\
                           2  1 8388028\n";
    assert_eq!(parse_output("cols2", malformed, Some(h)), None);
}

#[test]
fn malformed_structured_output_degrades() {
    assert!(parse_output("json", b"no", None).is_none());
    assert!(parse_output("csv", b"a,b\n1\n", None).is_none());
    assert!(parse_output("z-records", b"a\0", None).is_none());
}

// Regression for Real bug #2: `adapters/du.toml` used to declare
// `parse = "tsv"` for `du -k`-shaped output, but real `du` prints NO
// header line at all -- every line is a genuine `<size>\t<path>` data
// row. `tsv`'s "first row is the header" rule silently swallowed the
// very first real row as fake column names (keyed by whatever that
// row's literal text happened to be, not by the promised `size`/`path`
// hint names) and, for a single-directory invocation (exactly one output
// line), degraded a real one-row result to a phantom EMPTY table.
// `tsv-headerless` fixes this: every line is data, and column
// identity/order come from the hint alone. (`size_kb`-typed, not bare
// `int`: `du -k`'s numbers carry no unit suffix, so `shoal_value::
// parse_size` can't parse them directly -- but the adapter itself
// already knows, from its own pinned `-k` invoke flag, that the bare
// digits are 1024-byte blocks, so `size_kb` bridges that gap by scaling
// `* 1024` into a real `Value::Size` instead of leaving the caller with
// an untyped `int` -- see `parse_size_kb` and `du.toml`.)
#[test]
fn tsv_headerless_parses_du_shaped_output_with_no_header_row() {
    let h = "table<{size: size_kb, path: path}>";
    // Single-directory `du -k .` output: exactly one line, no header.
    // The old `tsv` strategy would have swallowed this as a "header"
    // and returned an empty table.
    let single = parse_output("tsv-headerless", b"335328176\t.\n", Some(h)).unwrap();
    assert_eq!(
        single,
        Value::Table(vec![{
            let mut r = Record::new();
            r.insert("size".into(), Value::Size(335328176 * 1024));
            r.insert("path".into(), Value::Path(".".into()));
            r
        }])
    );

    // Multi-line `du -k` output: every line (including the first) must
    // survive as a real row, not get eaten as a header.
    let multi = parse_output(
        "tsv-headerless",
        b"44\tcrates/shoal-adapters/src\n24\tcrates/shoal-adapters/tests\n",
        Some(h),
    )
    .unwrap();
    let Value::Table(rows) = multi else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 2, "no row should be swallowed as a header");
    assert_eq!(rows[0]["size"], Value::Size(44 * 1024));
    assert_eq!(
        rows[1]["path"],
        Value::Path("crates/shoal-adapters/tests".into())
    );
}

#[test]
fn tsv_headerless_preserves_embedded_spaces_via_explicit_tab_delimiter() {
    // Unlike `cols` (which splits on whitespace RUNS and must merge
    // overflow into the last column to survive embedded spaces),
    // `tsv-headerless` splits on a literal tab, so a path containing
    // ordinary spaces survives with no special-casing at all.
    let h = "table<{size: size_kb, path: path}>";
    let v = parse_output("tsv-headerless", b"512\tMy Documents\n", Some(h)).unwrap();
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows[0]["path"], Value::Path("My Documents".into()));
    assert_eq!(rows[0]["size"], Value::Size(512 * 1024));
}

#[test]
fn tsv_headerless_degrades_on_column_count_mismatch() {
    let h = "table<{size: size_kb, path: path}>";
    // Three tab-separated fields where the hint promises exactly two.
    assert_eq!(
        parse_output("tsv-headerless", b"4\textra\tfile.txt\n", Some(h)),
        None
    );
}

// `size_kb` is the whole point: a `du`/`df`-shaped column must come out
// as a real, comparable `Value::Size`, not a bare `int` a caller has to
// remember is secretly kilobytes and manually multiply. This drives the
// parsed cell through `shoal_value::ops::compare` the same way
// `(du).where(.size > 1mb)` does at the language level. The language
// contract doesn't reach shoal-adapters directly,
// so this is the crate-local proof the byte scaling is right).
#[test]
fn size_kb_column_is_comparable_as_a_real_size() {
    let h = "table<{size: size_kb, path: path}>";
    // 500kb = 512_000b (under 1mb's 1_000_000b); 2048kb = 2_097_152b
    // (over it) -- one row on each side of the threshold, so the filter
    // below can't pass vacuously true or vacuously false either way.
    let bytes = b"500\tsmall\n2048\tbig\n";
    let v = parse_output("tsv-headerless", bytes, Some(h)).unwrap();
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 2);

    let one_mb = Value::Size(shoal_value::parse_size("1mb").unwrap());
    let over_1mb: Vec<_> = rows
        .iter()
        .filter(|r| {
            matches!(
                shoal_value::ops::compare(&r["size"], &one_mb),
                Ok(std::cmp::Ordering::Greater)
            )
        })
        .collect();
    assert_eq!(over_1mb.len(), 1, "only the 2048kb row exceeds 1mb");
    assert_eq!(over_1mb[0]["path"], Value::Path("big".into()));

    // Both rows exceed 1kb in bytes -- sanity-check the comparison isn't
    // vacuously true/false in both directions.
    let one_kb = Value::Size(shoal_value::parse_size("1kb").unwrap());
    let over_1kb: Vec<_> = rows
        .iter()
        .filter(|r| {
            matches!(
                shoal_value::ops::compare(&r["size"], &one_kb),
                Ok(std::cmp::Ordering::Greater)
            )
        })
        .collect();
    assert_eq!(over_1kb.len(), 2, "both rows exceed 1kb in bytes");
}

#[test]
fn size_kb_degrades_on_non_numeric_or_negative_cells() {
    let h = "table<{size: size_kb, path: path}>";
    assert_eq!(
        parse_output("tsv-headerless", b"notanumber\tfile\n", Some(h)),
        None
    );
    assert_eq!(parse_output("tsv-headerless", b"-5\tfile\n", Some(h)), None);
}

// Regression for Real bug #1: `git status --porcelain=v2 --short` used
// to have its `?`/`!` lines parsed as if they were still porcelain v2,
// baking a leading space into `path` (short format has a second marker
// byte where porcelain v2 has a separating space). The parser must now
// refuse to slice a shape it hasn't validated and degrade instead.
#[test]
fn porcelain_v2_short_format_corruption_degrades_instead_of_lying() {
    // `git status --porcelain=v2 --short` for an untracked file emits
    // short-format `"?? scratch/"`, not true porcelain v2's `"? scratch/"`.
    let short_format_bytes = b"?? scratch/\n";
    let out = parse_output("porcelain-v2", short_format_bytes, None);
    assert_eq!(out, None, "must degrade, not bake a corrupted path");

    // Sanity: genuine porcelain v2 for the same file still parses cleanly
    // with no leading-space corruption.
    let real_porcelain_bytes = b"? scratch/\n";
    let out = parse_output("porcelain-v2", real_porcelain_bytes, None).unwrap();
    assert!(matches!(&out, Value::Table(t) if t.len() == 1));
    if let Value::Table(t) = out {
        assert_eq!(t[0]["path"], Value::Path("scratch/".into()));
    }
}

#[test]
fn porcelain_v2_rejects_malformed_and_unknown_records() {
    // '?'/'!' line with no separating space at all.
    assert_eq!(parse_output("porcelain-v2", b"?nofile\n", None), None);
    // '1' ordinary-change line missing fields.
    assert_eq!(
        parse_output("porcelain-v2", b"1 .M N... 100644 100644 a b\n", None),
        None
    );
    // A path containing spaces is legitimate (git allows unquoted
    // filenames with embedded spaces in porcelain v2) and must parse
    // cleanly rather than being mistaken for a shape violation -- the
    // metadata fields are bounded and the final field absorbs the rest.
    assert_eq!(
        parse_output(
            "porcelain-v2",
            b"1 .M N... 100644 100644 100644 a b my file.txt\n",
            None
        )
        .map(|v| matches!(v, Value::Table(t) if t[0]["path"] == Value::Path("my file.txt".into()))),
        Some(true)
    );
    // Unmerged 'u' records and other unrecognized markers are not
    // modeled by this adapter and must not be silently dropped from an
    // otherwise "successful" table.
    assert_eq!(
        parse_output(
            "porcelain-v2",
            b"u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs\n",
            None
        ),
        None
    );
}

#[test]
fn porcelain_v2_renamed_entry_populates_orig() {
    let bytes =
        b"2 R100 N... 100644 100644 100644 aaaa1111 bbbb2222 R100 new_name.rs\told_name.rs\n";
    let v = parse_output("porcelain-v2", bytes, None).unwrap();
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], Value::Str("R100".into()));
    assert_eq!(rows[0]["path"], Value::Path("new_name.rs".into()));
    assert_eq!(rows[0]["orig"], Value::Path("old_name.rs".into()));
}

// The whole point of the `state` field: `(git status).where(.status ==
// "modified")` can never match anything because `status` is always the
// raw two-char `XY` porcelain code (`.M`, `M.`, `MM`, ...), never an
// English word -- a footgun for a shell whose pitch is "filter
// structured output like data." `state` is the derived semantic word
// that makes `.where(.state == "modified")` actually work, without
// disturbing `status` (asserted alongside `state` on every row here so
// a regression to either field is caught).
#[test]
fn porcelain_v2_derives_semantic_state_field() {
    let bytes = b"1 .M N... 100644 100644 100644 aaaa1111 bbbb2222 mod_worktree.rs\n\
1 M. N... 100644 100644 100644 aaaa1111 bbbb2222 mod_staged.rs\n\
1 A. N... 100644 100644 100644 aaaa1111 bbbb2222 added_staged.rs\n\
1 MM N... 100644 100644 100644 aaaa1111 bbbb2222 mod_both.rs\n\
2 R. N... 100644 100644 100644 aaaa1111 bbbb2222 R100 new_name.rs\told_name.rs\n\
? untracked_file.txt\n\
! ignored_file.log\n";
    let v = parse_output("porcelain-v2", bytes, None).unwrap();
    let Value::Table(rows) = v else {
        panic!("expected table")
    };
    assert_eq!(rows.len(), 7);

    // `.M`: unmodified in the index, modified in the worktree -- Y
    // wins, and Y='M' -> "modified". This is the exact case the task
    // description calls out: a worktree-modified file must read as
    // "modified", not something derived from the (unchanged) index half.
    assert_eq!(rows[0]["status"], Value::Str(".M".into()));
    assert_eq!(rows[0]["state"], Value::Str("modified".into()));

    // `M.`: modified in the index, unmodified in the worktree -- Y is
    // '.' (no worktree change to prefer), so falls back to X='M' ->
    // "modified".
    assert_eq!(rows[1]["status"], Value::Str("M.".into()));
    assert_eq!(rows[1]["state"], Value::Str("modified".into()));

    // `A.`: staged-new (added to the index) with no further worktree
    // change -- Y='.' falls back to X='A' -> "added", per the task's
    // required outcome for a purely staged-new file.
    assert_eq!(rows[2]["status"], Value::Str("A.".into()));
    assert_eq!(rows[2]["state"], Value::Str("added".into()));

    // `MM`: modified in both the index and the worktree -- Y='M' wins
    // (same result as `.M`, since worktree state takes priority
    // regardless of what's also staged).
    assert_eq!(rows[3]["status"], Value::Str("MM".into()));
    assert_eq!(rows[3]["state"], Value::Str("modified".into()));

    // `2 R.`: a rename entry with an unmodified worktree since the
    // rename -- Y='.' falls back to X='R' -> "renamed"; `orig` still
    // carries the pre-rename path exactly as before.
    assert_eq!(rows[4]["status"], Value::Str("R.".into()));
    assert_eq!(rows[4]["state"], Value::Str("renamed".into()));
    assert_eq!(rows[4]["path"], Value::Path("new_name.rs".into()));
    assert_eq!(rows[4]["orig"], Value::Path("old_name.rs".into()));

    // `?`: untracked has no `XY` pair at all -- fixed "untracked" state.
    assert_eq!(rows[5]["status"], Value::Str("?".into()));
    assert_eq!(rows[5]["state"], Value::Str("untracked".into()));

    // `!`: ignored, likewise fixed -- "ignored".
    assert_eq!(rows[6]["status"], Value::Str("!".into()));
    assert_eq!(rows[6]["state"], Value::Str("ignored".into()));
}

#[test]
fn z_records_empty_output_is_empty_table() {
    let h = "table<{hash: str, author: str, path: path}>";
    assert_eq!(
        parse_output("z-records", b"", Some(h)),
        Some(Value::Table(vec![]))
    );
}

#[test]
fn z_records_single_terminator_and_noise() {
    let h = "table<{hash: str, author: str, path: path}>";
    // A single trailing NUL (the normal `-z`-terminated shape): `N*fields+1`
    // cells, one trailing terminator empty -> pop exactly one -> N rows.
    let single = parse_output("z-records", b"abc\0Allie\0a.rs\0", Some(h)).unwrap();
    assert!(matches!(&single, Value::Table(t) if t.len() == 1));
    // Pure separator noise with no field data at all is an empty table.
    assert_eq!(
        parse_output("z-records", b"\0\0", Some(h)),
        Some(Value::Table(vec![]))
    );
    // Regression: a genuinely-empty FINAL field must survive.
    // `abc\0Allie\0\0` (path is empty) + `-z` terminator `\0` splits to
    // [abc, Allie, "", ""] = 4 cells, `4 % 3 == 1`; popping the single
    // terminator keeps the empty `path` field instead of over-trimming it
    // and degrading the whole table to bytes.
    let empty_last =
        parse_output("z-records", b"abc\0Allie\0\0", Some(h)).expect("empty last field parses");
    match &empty_last {
        Value::Table(t) => {
            assert_eq!(t.len(), 1);
            assert_eq!(t[0]["path"], Value::Path("".into()));
        }
        other => panic!("expected a 1-row table, got {other:?}"),
    }
    // A stream carrying MORE than the single record terminator (a genuine
    // stray extra NUL) is malformed and now degrades to bytes rather than
    // being silently absorbed -- the deliberate trade: absorbing
    // arbitrary trailing NULs is indistinguishable from an empty final
    // field, and preserving the field wins (site/content/internals/language-conformance-contract.md).
    assert!(parse_output("z-records", b"abc\0Allie\0a.rs\0\0", Some(h)).is_none());
}

#[test]
fn z_records_git_log_empty_subject_stays_a_table() {
    // The real trigger: `adapters/git.toml`'s `git log ... -z` emits 4
    // fields (`%H\0%an\0%aI\0%s`) per commit. A repo whose most-recent
    // commit has an EMPTY subject produces a trailing empty `subject` cell
    // immediately before the `-z` record terminator. The old loop popped
    // both, corrupting `len % fields` and degrading the log to raw bytes;
    // The parser pops only the single terminator so `subject: ""` survives.
    let h = "table<{hash: str, author: str, date: datetime, subject: str}>";
    // Two commits: the second (most-recent-first from git) has an empty
    // subject. Bytes: rec1 fields + \0, rec2 fields (empty subject) + \0.
    // `\x00` (not `\0`) before the year digits so the NUL doesn't read as
    // an octal escape.
    let bytes =
        b"h2\0Bob\x002024-01-02T00:00:00Z\0\0h1\0Alice\x002024-01-01T00:00:00Z\0first commit\0";
    let v = parse_output("z-records", bytes, Some(h)).expect("git-log z-records parses");
    match &v {
        Value::Table(t) => {
            assert_eq!(t.len(), 2, "both commits present");
            assert_eq!(t[0]["subject"], Value::Str("".into()));
            assert_eq!(t[1]["subject"], Value::Str("first commit".into()));
        }
        other => panic!("expected a 2-row table, got {other:?}"),
    }
}

#[test]
fn bundled_adapter_pack_loads_without_warnings() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
    let (catalog, warnings) = AdapterCatalog::load_dir(&root);
    assert!(
        warnings.is_empty(),
        "bundled adapter warnings: {warnings:#?}"
    );
    let required = [
        "git",
        "cargo",
        "rg",
        "docker",
        "kubectl",
        "jq",
        "curl",
        "tar",
        "fd",
        "du",
        "ps",
        "df",
        "systemctl",
        "brew",
        "npm",
        "pnpm",
        "gh",
        "go",
        "pip",
        "sqlite3",
        "terraform",
        "helm",
        "ip",
        "python",
        "node",
        "ruby",
        "deno",
        "bash",
        "ss",
        "systemd-analyze",
        "jj",
        "rustup",
        "bun",
        "aws",
        "gcloud",
        "yq",
        "stat",
        "zip",
        "unzip",
        "yarn",
        "uv",
        "podman",
        "lsblk",
        "findmnt",
        "journalctl",
        "who",
        "env",
        "lscpu",
        "vmstat",
    ];
    assert_eq!(catalog.len(), required.len());
    for name in required {
        assert!(catalog.lookup(name).is_some(), "missing adapter {name}");
    }
    // site/content/internals/values-streams-execution.md: the interpreter-class set the shipped pack declares,
    // wired end to end through the same loader path as every other
    // class value.
    for interp in ["python", "node", "ruby", "deno", "jq", "bash", "yq"] {
        assert_eq!(
            catalog.lookup(interp).unwrap().class,
            AdapterClass::Interpreter,
            "{interp} should be class = \"interpreter\""
        );
        // No adapter declares invoke_payload explicitly yet, so every
        // interpreter-class adapter falls back to the documented
        // default.
        assert_eq!(
            catalog.lookup(interp).unwrap().invoke_payload,
            InvokePayload::Arg
        );
    }
    // python/node/ruby/deno/bash each declare the flag template that
    // precedes their raw block's payload argv word (see
    // `site/content/internals/values-streams-execution.md`);
    // jq takes its filter as a bare positional, so it declares none.
    assert_eq!(
        catalog.lookup("python").unwrap().top.invoke,
        Some(vec!["-c".to_string()])
    );
    assert_eq!(
        catalog.lookup("node").unwrap().top.invoke,
        Some(vec!["-e".to_string()])
    );
    assert_eq!(
        catalog.lookup("deno").unwrap().top.invoke,
        Some(vec!["eval".to_string()])
    );
    assert_eq!(catalog.lookup("jq").unwrap().top.invoke, None);
    // `yq` (mikefarah, YAML-native jq-alike) pins `-o=json` since its
    // default output is YAML, unlike jq's already-JSON default.
    assert_eq!(
        catalog.lookup("yq").unwrap().top.invoke,
        Some(vec!["-o=json".to_string()])
    );
    // The `cols` strategy (added for `ps`/`df`) is wired end to end
    // through the same loader path as every other parser.
    assert_eq!(catalog.lookup("ps").unwrap().top.parse, "cols");
    assert_eq!(catalog.lookup("df").unwrap().top.parse, "cols");
    // The `tsv-headerless` strategy (added for `du`/`stat`, Real bug #2)
    // is wired end to end through the same loader path as every other
    // parser, and `du`'s `human_readable` flag is consumed to protect
    // the pinned `-k` numeric format (see `du.toml`).
    assert_eq!(catalog.lookup("du").unwrap().top.parse, "tsv-headerless");
    assert_eq!(catalog.lookup("stat").unwrap().top.parse, "tsv-headerless");
    assert_eq!(
        catalog.lookup("du").unwrap().top.consumed,
        vec!["human_readable".to_string()]
    );
    // gh's two-word real subcommands are flattened into single
    // shoal-side sub names whose `invoke` template supplies both words.
    assert_eq!(
        catalog.lookup("gh").unwrap().subs["pr_list"].invoke,
        Some(vec![
            "pr".to_string(),
            "list".to_string(),
            "--json".to_string(),
            "number,title,state,author,url,createdAt".to_string()
        ])
    );
    assert_eq!(
        catalog.lookup("cargo").unwrap().subs["metadata"].invoke,
        Some(vec![
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string()
        ])
    );
    assert_eq!(
        catalog.lookup("git").unwrap().subs["diff"].ok_codes,
        Some(vec![0, 1])
    );
    assert_eq!(catalog.lookup("rg").unwrap().top.parse, "ndjson");
    assert_eq!(
        catalog.lookup("docker").unwrap().class,
        AdapterClass::Daemon
    );
    // The porcelain-corruption fix: `short`/`branch` stay valid,
    // declared flags but must never reach git's argv alongside the
    // pinned `--porcelain=v2` invoke template.
    let git_status = &catalog.lookup("git").unwrap().subs["status"];
    assert!(git_status.params.iter().any(|p| p.name == "short"));
    assert!(git_status.short_flags.contains_key("s"));
    assert_eq!(
        git_status.consumed,
        vec!["short".to_string(), "branch".to_string()]
    );
    // Same class of fix, swept into docker's format-pinned subs.
    let docker = catalog.lookup("docker").unwrap();
    assert_eq!(docker.subs["ps"].consumed, vec!["quiet".to_string()]);
    assert_eq!(docker.subs["images"].consumed, vec!["quiet".to_string()]);
    // kubectl's `get` and rg's top-level command pin an output format
    // too, but declare no forwardable param that could override it, so
    // there is nothing to consume there.
    assert!(
        catalog.lookup("kubectl").unwrap().subs["get"]
            .consumed
            .is_empty()
    );
    assert!(catalog.lookup("rg").unwrap().top.consumed.is_empty());
    // git's flattened `stash_list`/`stash_push`/`stash_pop` subs (no
    // single real verb, same trick as `gh`'s `pr_list`/`run_list`).
    let git = catalog.lookup("git").unwrap();
    assert_eq!(
        git.subs["stash_list"].invoke,
        Some(vec!["stash".to_string(), "list".to_string()])
    );
    assert_eq!(
        git.subs["stash_push"].invoke,
        Some(vec!["stash".to_string(), "push".to_string()])
    );
    assert_eq!(
        git.subs["stash_pop"].invoke,
        Some(vec!["stash".to_string(), "pop".to_string()])
    );
    assert!(git.subs.contains_key("show"));
    assert!(git.subs.contains_key("remote"));
    // podman's own formatter recognizes bare `--format json` directly
    // (no `docker.toml`-style go-template/tsv workaround needed).
    assert_eq!(
        catalog.lookup("podman").unwrap().subs["ps"].invoke,
        Some(vec![
            "ps".to_string(),
            "--format".to_string(),
            "json".to_string()
        ])
    );
    assert_eq!(
        catalog.lookup("podman").unwrap().subs["ps"].consumed,
        vec!["quiet".to_string()]
    );
    // `vmstat`'s two-line banner needs the `cols2` strategy (plain
    // `cols` only ever discards one line); wired end to end through the
    // same loader path as every other parser.
    assert_eq!(catalog.lookup("vmstat").unwrap().top.parse, "cols2");
    // `env`'s `KEY=VALUE` lines are the exact shape `kv` was built for.
    assert_eq!(catalog.lookup("env").unwrap().top.parse, "kv");
    // `who` has no header line of its own; `-H` is pinned via `invoke`
    // so `cols`'s "discard exactly one line" assumption holds.
    assert_eq!(catalog.lookup("who").unwrap().top.parse, "cols");
    assert_eq!(
        catalog.lookup("who").unwrap().top.invoke,
        Some(vec!["-H".to_string()])
    );
    // `lsblk`/`findmnt`/`lscpu` are JSON-native (`-J`), same family as
    // `ip`/`systemctl`.
    assert_eq!(catalog.lookup("lsblk").unwrap().top.parse, "json");
    assert_eq!(catalog.lookup("findmnt").unwrap().top.parse, "json");
    assert_eq!(catalog.lookup("lscpu").unwrap().top.parse, "json");
    // `journalctl -o json` is one JSON object per line, i.e. `ndjson`,
    // not a single top-level JSON document.
    assert_eq!(catalog.lookup("journalctl").unwrap().top.parse, "ndjson");
    // `ip`'s pre-existing `addr`/`route` subs gain a `link` sibling
    // (interface list), same JSON-array shape.
    assert_eq!(
        catalog.lookup("ip").unwrap().subs["link"].invoke,
        Some(vec![
            "-j".to_string(),
            "link".to_string(),
            "show".to_string()
        ])
    );
}

#[test]
fn invalid_schema_warns_without_poisoning_siblings() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("pack.toml"),
        r#"
[cmd.good]
params={path="path"}
[cmd.bad_type]
params={x="quantum"}
[cmd.bad_parser]
output={parse="wishful"}
[cmd.bad_binding]
params={x="str"}
positional=["missing"]
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
    assert!(catalog.lookup("good").is_some());
    assert!(catalog.lookup("bad_type").is_none());
    assert!(catalog.lookup("bad_parser").is_none());
    assert!(catalog.lookup("bad_binding").is_none());
    assert_eq!(warnings.len(), 3);
}

#[test]
fn consumed_targeting_undeclared_param_warns_without_poisoning_siblings() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("pack.toml"),
        r#"
[cmd.good]
params={path="path"}
[cmd.bad_consumed]
params={x="str"}
consumed=["missing"]
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
    assert!(catalog.lookup("good").is_some());
    assert!(catalog.lookup("bad_consumed").is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("consumed"), "{warnings:?}");
}

// site/content/internals/values-streams-execution.md: `class = "interpreter"` is a schema value alongside
// cli|tui|daemon, and `invoke_payload` is only meaningful there.
#[test]
fn interpreter_class_loads_and_defaults_invoke_payload_to_arg() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("pack.toml"),
        r#"
[cmd.py]
bin="python3"
class="interpreter"
invoke=["-c"]
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    let py = catalog.lookup("py").unwrap();
    assert_eq!(py.class, AdapterClass::Interpreter);
    assert_eq!(py.invoke_payload, InvokePayload::Arg);
    assert_eq!(py.top.invoke, Some(vec!["-c".to_string()]));
}

#[test]
fn interpreter_class_accepts_explicit_stdin_payload_mode() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("pack.toml"),
        r#"
[cmd.example]
bin="example"
class="interpreter"
invoke_payload="stdin"
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        catalog.lookup("example").unwrap().invoke_payload,
        InvokePayload::Stdin
    );
}

#[test]
fn invoke_payload_on_non_interpreter_class_warns_without_poisoning_siblings() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("pack.toml"),
        r#"
[cmd.good]
params={path="path"}
[cmd.bad_class]
class="cli"
invoke_payload="stdin"
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
    assert!(catalog.lookup("good").is_some());
    assert!(catalog.lookup("bad_class").is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("invoke_payload"), "{warnings:?}");
}

#[test]
fn unknown_invoke_payload_value_warns_without_poisoning_siblings() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("pack.toml"),
        r#"
[cmd.good]
params={path="path"}
[cmd.bad_payload]
class="interpreter"
invoke_payload="socket"
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
    assert!(catalog.lookup("good").is_some());
    assert!(catalog.lookup("bad_payload").is_none());
    assert_eq!(warnings.len(), 1);
}
