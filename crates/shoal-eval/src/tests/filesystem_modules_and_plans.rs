use super::*;

// --- structured builtins head / ln ----------------------------------------

#[test]
fn head_returns_first_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), b"a\nb\nc\nd\n").unwrap();
    assert_eq!(
        out_of(&run_in("head f 2", dir.path()).unwrap()),
        Value::List(vec![Value::Str("a".into()), Value::Str("b".into())])
    );
    // Default n = 10 returns all four.
    assert!(
        matches!(out_of(&run_in("head f", dir.path()).unwrap()), Value::List(xs) if xs.len() == 4)
    );
}

#[test]
fn ln_creates_symlink_and_hardlink() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("orig"), b"data").unwrap();
    run_in("ln --symbolic orig slink", dir.path()).unwrap();
    assert!(
        dir.path()
            .join("slink")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    run_in("ln orig hard", dir.path()).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("hard")).unwrap(),
        b"data".to_vec()
    );
}

// --- modules --------------------------------------------------------------

#[test]
fn use_binds_module_exports_and_runs_fns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("greet.shl"),
        "export fn hello(who: str) { \"hi {who}\" }\nexport let version = 3\nfn private() { 1 }",
    )
    .unwrap();
    // A module fn runs as a namespaced command.
    assert_eq!(
        run_in("use ./greet\ngreet.hello(\"ada\")", dir.path()).unwrap(),
        Value::Str("hi ada".into())
    );
    // A value export is a field.
    assert_eq!(
        run_in("use ./greet\ngreet.version", dir.path()).unwrap(),
        Value::Int(3)
    );
    // A non-exported decl is not visible.
    assert_eq!(
        run_in("use ./greet\ngreet.private", dir.path())
            .unwrap_err()
            .code,
        "field_missing"
    );
}

#[test]
fn circular_use_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.shl"), "use ./b\nexport let x = 1").unwrap();
    std::fs::write(dir.path().join("b.shl"), "use ./a\nexport let y = 2").unwrap();
    let err = run_in("use ./a", dir.path()).unwrap_err();
    assert_eq!(err.code, "custom");
    assert!(err.msg.contains("circular"), "{}", err.msg);
}

#[test]
fn missing_module_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        run_in("use ./nope", dir.path()).unwrap_err().code,
        "not_found"
    );
}

// --- plan / apply / explain -----------------------------------------------

#[test]
fn plan_renders_effects_without_running() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    let v = run_in("plan { rm x }", dir.path()).unwrap();
    let Value::Record(r) = &v else {
        panic!("plan should be a record, got {v:?}")
    };
    // The file is untouched — plan spawns/mutates nothing.
    assert!(dir.path().join("x").exists());
    let Some(Value::List(effects)) = r.get("effects") else {
        panic!("plan record needs effects")
    };
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Value::Str(s) if s.starts_with("trash"))),
        "{effects:?}"
    );
    assert!(matches!(r.get("id"), Some(Value::Int(_))));
}

#[test]
fn apply_runs_a_derived_plan() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x"), b"x").unwrap();
    // `plan { … }` derives (id 1) without mutating; `apply 1` runs it.
    let out = run_in("plan { rm x }\napply 1", dir.path()).unwrap();
    // Now the rm actually ran.
    assert!(!dir.path().join("x").exists());
    assert!(matches!(out_of(&out), Value::List(_)));
}

#[test]
fn explain_reports_effects_of_source() {
    let dir = tempfile::tempdir().unwrap();
    let v = run_in(r#"explain("rm x")"#, dir.path()).unwrap();
    let Value::Record(r) = v else { panic!() };
    assert_eq!(r.get("source"), Some(&Value::Str("rm x".into())));
    assert!(matches!(r.get("effects"), Some(Value::List(_))));
}

#[test]
fn plan_and_explain_report_permanent_delete_as_irreversible() {
    let dir = tempfile::tempdir().unwrap();
    for source in [
        "plan { rm --permanent x }",
        r#"explain("rm --permanent x")"#,
    ] {
        let Value::Record(record) = run_in(source, dir.path()).unwrap() else {
            panic!("{source} should return a plan record");
        };
        assert_eq!(
            record.get("reversible"),
            Some(&Value::Bool(false)),
            "{source}"
        );
        let Some(Value::List(effects)) = record.get("effects") else {
            panic!("{source} should return effects");
        };
        assert_eq!(
            effects,
            &vec![Value::Str(format!(
                "permanently delete {}",
                dir.path().join("x").display()
            ))],
            "{source}"
        );
    }
}

#[test]
fn rm_option_terminator_makes_permanent_spelling_a_recoverable_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("--permanent"), b"keep recoverable").unwrap();
    let value = run_in("rm -- --permanent", dir.path()).unwrap();
    let Value::Outcome(outcome) = value else {
        panic!("rm should return an outcome");
    };
    let Value::List(rows) = outcome.out_value() else {
        panic!("rm should return removal rows");
    };
    let Value::Record(row) = &rows[0] else {
        panic!("rm should return a removal record");
    };
    assert!(
        row.contains_key("trash"),
        "-- must select recoverable removal"
    );
    assert!(!dir.path().join("--permanent").exists());
}
