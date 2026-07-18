use super::*;

fn eval_parsed(ev: &mut Evaluator, src: &str) -> Value {
    let out = ev
        .eval_program(&shoal_syntax::parse(src).unwrap())
        .unwrap_or_else(|e| panic!("{src}: {e}"));
    let Value::Outcome(outcome) = out else {
        panic!("{src}: expected an outcome, got {out:?}")
    };
    outcome
        .parsed
        .as_ref()
        .cloned()
        .unwrap_or_else(|| panic!("{src}: outcome carried no parsed value"))
}

/// Run `src` in a fresh `Evaluator` rooted at `cwd`, returning the
/// resolution/health record `which`/`reef` carry as an outcome's
/// `.parsed` value (mirrors `crates/shoal-eval/tests/reef_integration.rs`'s
/// own unwrap pattern).
fn parsed(cwd: &Path, src: &str) -> Value {
    let mut ev = Evaluator::new(cwd.to_path_buf());
    eval_parsed(&mut ev, src)
}

#[test]
fn which_reports_the_same_winning_source_as_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().into());
    ev.eval_program(&shoal_syntax::parse("fn deploy() { null }").unwrap())
        .unwrap();
    ev.env_mut()
        .declare("answer", Value::Int(42), false)
        .unwrap();

    for (head, expected) in [
        ("deploy", "session_callable"),
        ("answer", "bound_value"),
        ("ls", "structured_builtin"),
        ("cd", "special_builtin"),
    ] {
        let Value::Record(record) = eval_parsed(&mut ev, &format!("which {head}")) else {
            panic!("which {head}: expected record")
        };
        assert_eq!(
            record.get("source"),
            Some(&Value::Str(expected.into())),
            "which {head} diverged from runtime precedence"
        );
        assert!(matches!(record.get("reason"), Some(Value::Str(reason)) if !reason.is_empty()));
    }
}

#[test]
fn which_adapter_trace_includes_schema_and_executable_resolution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("tool.toml"),
        r#"
[cmd.audittool]
bin = "sh"
params = { verbose = "bool" }
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
    assert!(warnings.is_empty(), "{warnings:?}");

    let mut ev = Evaluator::new(dir.path().into());
    ev.set_adapters(catalog);
    let Value::Record(record) = eval_parsed(&mut ev, "which audittool") else {
        panic!("expected adapter resolution record")
    };
    assert_eq!(record.get("source"), Some(&Value::Str("adapter".into())));
    assert!(matches!(record.get("adapter"), Some(Value::Record(schema))
        if schema.get("bin") == Some(&Value::Str("sh".into()))));
    assert!(
        matches!(record.get("executable"), Some(Value::Record(executable))
        if executable.get("path").is_some_and(|path| !matches!(path, Value::Null)))
    );
    assert!(matches!(record.get("hash"), Some(Value::Str(hash)) if !hash.is_empty()));
}

/// Fix 2: two scopes constraining `faketool` incompatibly is a pure
/// manifest-chain decision (site/content/internals/reef-resolution.md) — no real tool install needed, so
/// this doesn't need a fixture resolver at all. Before the fix, `which`'s
/// `Err(_)` arm swallowed this and reported a bare ambient/null guess.
#[test]
fn which_surfaces_conflict_instead_of_ambient_fallback() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join(".reef.toml"),
        "[tools]\nfaketool = \"18\"\n",
    )
    .unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".reef.toml"), "[tools]\nfaketool = \"22\"\n").unwrap();

    let Value::Record(r) = parsed(&sub, "which faketool") else {
        panic!("expected a record")
    };
    assert_eq!(
        r.get("scope"),
        Some(&Value::Str("unresolved: reef_conflict".into()))
    );
    assert!(
        matches!(r.get("note"), Some(Value::Str(s)) if s.contains("18") && s.contains("22")),
        "note should cite both conflicting constraints, got {:?}",
        r.get("note")
    );
}

/// Fix 2: a valid-but-drifted lock entry (hand-written, pointing at a
/// fixture file whose content doesn't match the recorded hash) is a pure
/// function of the lock + on-disk bytes — no real provider/tool needed,
/// since a valid lock entry short-circuits `resolve()` before any
/// provider is ever consulted.
#[test]
fn which_surfaces_drift_instead_of_ambient_fallback() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".reef.toml"), "[tools]\nfaketool = \"*\"\n").unwrap();
    let bin = dir.path().join("fakebin");
    std::fs::write(&bin, b"original-bytes").unwrap();
    std::fs::write(
        dir.path().join("reef.lock"),
        format!(
            "[tool.faketool]\nname = \"faketool\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"{}\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
            bin.display()
        ),
    )
    .unwrap();

    let Value::Record(r) = parsed(dir.path(), "which faketool") else {
        panic!("expected a record")
    };
    assert_eq!(
        r.get("scope"),
        Some(&Value::Str("unresolved: reef_drift".into()))
    );
}

/// Fix 4: `reef doctor`'s drift check, same fixture shape as the `which`
/// drift test above.
#[test]
fn reef_doctor_flags_drift() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".reef.toml"), "[tools]\nfaketool = \"*\"\n").unwrap();
    let bin = dir.path().join("fakebin");
    std::fs::write(&bin, b"original-bytes").unwrap();
    std::fs::write(
        dir.path().join("reef.lock"),
        format!(
            "[tool.faketool]\nname = \"faketool\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"{}\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
            bin.display()
        ),
    )
    .unwrap();

    let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
        panic!("expected a table")
    };
    let drift = rows
        .iter()
        .find(|r| r.get("check") == Some(&Value::Str("drift".into())))
        .expect("a drift row is present");
    assert_eq!(drift.get("name"), Some(&Value::Str("faketool".into())));
    assert_eq!(drift.get("status"), Some(&Value::Str("drift".into())));
}

/// Fix 4: an orphan lock entry — `reef.lock` remembers `ghosttool`, but no
/// manifest in scope mentions it anymore.
#[test]
fn reef_doctor_flags_orphan_lock() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".reef.toml"), "[tools]\nsh = \"*\"\n").unwrap();
    std::fs::write(
        dir.path().join("reef.lock"),
        "[tool.ghosttool]\nname = \"ghosttool\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"/nonexistent/ghosttool\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
    )
    .unwrap();

    let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
        panic!("expected a table")
    };
    let orphan = rows
        .iter()
        .find(|r| r.get("check") == Some(&Value::Str("orphan".into())))
        .expect("an orphan row is present");
    assert_eq!(orphan.get("name"), Some(&Value::Str("ghosttool".into())));
}

/// Fix 4: shadowed-ambient — `sh` is locked to a fixture path, but the
/// REAL ambient `sh` (guaranteed present on any POSIX host, same
/// assumption the rest of this corpus/test suite already makes) resolves
/// to a different binary.
#[test]
fn reef_doctor_flags_shadowed_ambient() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".reef.toml"), "[tools]\nsh = \"*\"\n").unwrap();
    let fake = dir.path().join("fake-sh");
    std::fs::write(&fake, b"not a real shell").unwrap();
    std::fs::write(
        dir.path().join("reef.lock"),
        format!(
            "[tool.sh]\nname = \"sh\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"{}\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
            fake.display()
        ),
    )
    .unwrap();

    let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
        panic!("expected a table")
    };
    let shadowed = rows
        .iter()
        .find(|r| r.get("check") == Some(&Value::Str("shadowed_ambient".into())))
        .expect("a shadowed_ambient row is present");
    assert_eq!(shadowed.get("name"), Some(&Value::Str("sh".into())));
}

/// The manifest filenames `ScopeChain::discover` (site/content/internals/reef-resolution.md) looks
/// for at every directory on its walk from `cwd` up to the filesystem
/// root.
const REEF_MANIFEST_NAMES: &[&str] = &[".reef.toml", "mise.toml", ".mise.toml", ".tool-versions"];

/// `reef_doctor_empty_scope_is_empty_table_not_error` asserts the
/// "genuinely nothing constrains anything anywhere" invariant — but
/// `ScopeChain::discover` walks from `dir` all the way to the real
/// filesystem root, including the shared OS temp dir every
/// `tempfile::tempdir()` nests under. That walk is only actually empty
/// when no ancestor directory happens to contain a
/// `.reef.toml`/`mise.toml`/`.mise.toml`/`.tool-versions` — true on a
/// clean host, but not something this test can force from Rust alone
/// (fully bounding the walk needs a root/boundary knob on
/// `ScopeChain::discover` itself, a `shoal-reef` source change). Rather
/// than let ambient contamination surface as a confusing generic
/// `assertion failed` a few lines down, fail loudly here with a precise
/// pointer at the offending file, so it reads as "environmental
/// contamination" (fix your host / clean the shared tempdir) rather
/// than "reef regressed".
fn panic_if_ancestor_reef_pollution(dir: &Path) {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        for name in REEF_MANIFEST_NAMES {
            let candidate = d.join(name);
            if candidate.exists() {
                panic!(
                    "ambient reef-manifest pollution detected above this test's own \
                     tempdir: {candidate:?} exists and was NOT created by this test. \
                     ScopeChain::discover (site/content/internals/reef-resolution.md) walks from cwd to the \
                     filesystem root, so this file makes the scope chain non-empty and \
                     breaks this test's \"nothing constrains anything\" premise. This is \
                     environmental contamination (e.g. a stray manifest left in a shared \
                     /tmp by an unrelated manual `reef`/`mise` repro), not a product \
                     regression — remove the file and re-run."
                );
            }
        }
        cur = d.parent();
    }
}

/// `reef doctor` with no manifest in scope is a clean, empty table — not
/// an error (unlike `reef lock`, a health check has nothing to say about
/// nothing).
#[test]
fn reef_doctor_empty_scope_is_empty_table_not_error() {
    let dir = tempfile::tempdir().unwrap();
    panic_if_ancestor_reef_pollution(dir.path());
    let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
        panic!("expected a table")
    };
    assert!(rows.is_empty());
}

/// Item 1 — the footgun scenario: a MALFORMED `cwd/.reef.toml` under a VALID
/// ancestor `.reef.toml`. `reef add` must surface the LOCAL parse error and
/// leave the ancestor's manifest byte-for-byte untouched. Before the fix,
/// `ScopeChain::discover` silently skipped the broken local file, so the
/// chain's nearest parsed `Reef` scope was the ANCESTOR and `reef add`
/// silently mutated it — hiding the local parse error entirely.
#[test]
fn reef_add_surfaces_local_parse_error_not_ancestor_write() {
    let root = tempfile::tempdir().unwrap();
    let ancestor = root.path().join(".reef.toml");
    let ancestor_text = "[tools]\nnode = \"18\"\n";
    std::fs::write(&ancestor, ancestor_text).unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let local = sub.join(".reef.toml");
    let local_text = "[tools\nfaketool = "; // malformed TOML
    std::fs::write(&local, local_text).unwrap();

    let mut ev = Evaluator::new(sub.clone());
    let err = ev
        .eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
        .expect_err("a malformed local manifest must surface a parse error");
    assert_eq!(err.code, "reef_provider");
    assert!(
        err.msg.contains(&local.display().to_string()),
        "the parse error must name the LOCAL manifest, got: {}",
        err.msg
    );
    // The ancestor manifest is untouched — no silent write one dir up.
    assert_eq!(std::fs::read_to_string(&ancestor).unwrap(), ancestor_text);
    // The malformed local file is left exactly as-is (we never wrote it).
    assert_eq!(std::fs::read_to_string(&local).unwrap(), local_text);
}

/// Item 1 — the ordinary local case: a VALID `cwd/.reef.toml` under a valid
/// ancestor. `reef add` edits the LOCAL manifest; the ancestor is untouched.
#[test]
fn reef_add_edits_local_manifest_not_ancestor() {
    let root = tempfile::tempdir().unwrap();
    let ancestor = root.path().join(".reef.toml");
    let ancestor_text = "[tools]\nnode = \"18\"\n";
    std::fs::write(&ancestor, ancestor_text).unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let local = sub.join(".reef.toml");
    std::fs::write(&local, "[tools]\nrg = \"*\"\n").unwrap();

    let mut ev = Evaluator::new(sub.clone());
    // faketool never resolves, so the lock step no-ops — but the manifest
    // EDIT still lands (the tool constraint is written before the lock).
    ev.eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
        .expect("reef add on a valid local manifest succeeds");

    let written = std::fs::read_to_string(&local).unwrap();
    let tbl: toml::Table = written.parse().unwrap();
    assert_eq!(tbl["tools"]["faketool"].as_str(), Some("1"));
    assert_eq!(tbl["tools"]["rg"].as_str(), Some("*"), "existing pin kept");
    // The ancestor never sees the new pin.
    assert_eq!(std::fs::read_to_string(&ancestor).unwrap(), ancestor_text);
}

/// Item 1 — no local manifest: `reef add` falls back to the chain's nearest
/// ancestor `.reef.toml` ("writes nearest manifest", site/content/internals/reef-resolution.md), since the
/// subdir has none of its own.
#[test]
fn reef_add_falls_back_to_nearest_ancestor_when_no_local() {
    let root = tempfile::tempdir().unwrap();
    let ancestor = root.path().join(".reef.toml");
    std::fs::write(&ancestor, "[tools]\nnode = \"18\"\n").unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir_all(&sub).unwrap();

    let mut ev = Evaluator::new(sub.clone());
    ev.eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
        .expect("reef add falls back to the ancestor manifest");

    // The ancestor gained the pin; no local manifest was created.
    let tbl: toml::Table = std::fs::read_to_string(&ancestor).unwrap().parse().unwrap();
    assert_eq!(tbl["tools"]["faketool"].as_str(), Some("1"));
    assert_eq!(tbl["tools"]["node"].as_str(), Some("18"));
    assert!(
        !sub.join(".reef.toml").exists(),
        "no local manifest should be created when an ancestor exists"
    );
}

/// Item 1 — greenfield: no manifest anywhere in the chain → create a fresh
/// `cwd/.reef.toml`. (Guarded against ambient ancestor pollution above the
/// shared tempdir, which would otherwise steal the write as a "nearest
/// ancestor".)
#[test]
fn reef_add_creates_local_manifest_when_none_in_scope() {
    let dir = tempfile::tempdir().unwrap();
    panic_if_ancestor_reef_pollution(dir.path());
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
        .expect("reef add creates a manifest when none exists");
    let local = dir.path().join(".reef.toml");
    assert!(local.exists(), "a fresh cwd/.reef.toml must be created");
    let tbl: toml::Table = std::fs::read_to_string(&local).unwrap().parse().unwrap();
    assert_eq!(tbl["tools"]["faketool"].as_str(), Some("1"));
}
