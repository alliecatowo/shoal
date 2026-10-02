use super::*;

#[test]
fn typed_builtins_dispatch_before_path() {
    let dir = tempfile::tempdir().unwrap();
    let program = shoal_syntax::parse("touch a\nls").unwrap();
    let value = out_of(&eval(&program, dir.path()).unwrap());
    assert!(
        matches!(value, Value::Table(rows) if rows.len() == 1 && rows[0]["name"] == Value::Path("a".into()))
    );

    let rm = shoal_syntax::parse("rm a").unwrap();
    let value = out_of(&eval(&rm, dir.path()).unwrap());
    assert!(
        matches!(value, Value::List(rows) if matches!(&rows[0], Value::Record(r) if matches!(r.get("trash"), Some(Value::Path(_)))))
    );
    assert!(!dir.path().join("a").exists());
}

fn adapter_eval(toml: &str, src: &str) -> VResult<Value> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixture.toml"), toml).unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut evaluator = Evaluator::new(dir.path().into());
    evaluator.set_adapters(catalog);
    evaluator.eval_program(&shoal_syntax::parse(src).unwrap())
}

#[test]
fn adapters_rewrite_parse_and_honor_ok_codes() {
    let lines = adapter_eval(
        r#"[cmd.fixture]
bin="/usr/bin/printf"
invoke=["one\ntwo\n"]
output={parse="lines",type="list<str>"}
"#,
        "fixture",
    )
    .unwrap();
    assert!(
        matches!(lines, Value::Outcome(o) if o.out_value() == Value::List(vec![Value::Str("one".into()), Value::Str("two".into())]))
    );

    let accepted = adapter_eval(
        r#"[cmd.accept]
bin="/bin/sh"
ok_codes=[0,1]
invoke=["-c","exit 1"]
"#,
        "accept",
    )
    .unwrap();
    assert!(matches!(accepted, Value::Outcome(o) if o.ok && o.status == Some(1)));
}

#[test]
fn adapter_typed_flags_fail_before_spawn() {
    let error = adapter_eval(
        r#"[cmd.typed]
bin="/usr/bin/printf"
params={jobs="int"}
"#,
        "typed --jobs=nope",
    )
    .unwrap_err();
    assert_eq!(error.code, "arg_error");
    assert!(error.msg.contains("expected int"));
}

#[test]
fn adapter_consumed_flag_never_reaches_argv() {
    // Regression for the git-status porcelain corruption (shoal-adapters'
    // `consumed` rule, defect fix): `--short`/`-s` must stay a
    // recognized, validated flag but never be appended to argv, since
    // git's `--porcelain=v2` parser assumes an exact byte layout and
    // `--short` (last-wins) silently switches git to a different,
    // incompatible output format.
    let toml = r#"[cmd.fixture]
bin="/bin/echo"

[cmd.fixture.sub.status]
params = { short = "bool", branch = "bool" }
flags = { short = { s = "short", b = "branch" } }
invoke = ["status", "--porcelain=v2"]
consumed = ["short", "branch"]
"#;

    let long = adapter_eval(toml, "fixture status --short").unwrap();
    let Value::Outcome(o) = long else {
        panic!("expected outcome, got {long:?}")
    };
    assert_eq!(
        String::from_utf8(o.stdout.to_vec()).unwrap().trim(),
        "status --porcelain=v2",
        "--short must be accepted but dropped from argv"
    );

    let short = adapter_eval(toml, "fixture status -s").unwrap();
    let Value::Outcome(o) = short else {
        panic!("expected outcome, got {short:?}")
    };
    assert_eq!(
        String::from_utf8(o.stdout.to_vec()).unwrap().trim(),
        "status --porcelain=v2",
        "-s must be accepted but dropped from argv"
    );
}

#[test]
fn forced_head_bypasses_adapter() {
    // `^name` reaches the real command (language card): a forced head
    // must skip the adapter's flag/signature gate entirely. The corpus
    // runner carries no adapters, so this lives here.
    let toml = r#"[cmd.zzzfixture]
bin="zzzfixture-no-such-binary"

[cmd.zzzfixture.sub.log]
params = { follow = "bool" }
"#;
    // Unforced: the adapter gate rejects the unknown flag before spawn.
    let err = adapter_eval(toml, "zzzfixture log --oneline").unwrap_err();
    assert_eq!(err.code, "arg_error");
    assert!(err.msg.contains("unknown flag --oneline"));
    // Forced: dispatch bypasses the adapter and reaches PATH resolution
    // (`not_found` here — the bin doesn't exist — proving the adapter's
    // arg_error gate never ran).
    let err = adapter_eval(toml, "^zzzfixture log --oneline").unwrap_err();
    assert_eq!(err.code, "not_found");
}

#[test]
fn single_char_adapter_param_emits_posix_single_dash() {
    // A single-character param (git log's `n`) must reach the child as
    // `-n`, not `--n` — the adapter used to validate `--n` and then
    // forward it verbatim, which the real tool rejects, leaving the
    // adapter's own advertised flag unusable. printf echoes its argv
    // back so the emitted spelling is directly observable.
    let toml = r#"[cmd.fixture]
bin="/usr/bin/printf"
invoke=["%s %s"]
params={ n = "int?" }
output={parse="lines",type="list<str>"}
"#;
    for src in ["fixture --n=2", "fixture --n 2"] {
        let out = adapter_eval(toml, src).unwrap();
        let Value::Outcome(o) = out else {
            panic!("expected outcome");
        };
        assert_eq!(
            o.out_value(),
            Value::List(vec![Value::Str("-n 2".into())]),
            "{src} argv spelling"
        );
    }
}

#[test]
fn planning_derives_exact_builtin_paths_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a"), b"a").unwrap();
    let mut evaluator = Evaluator::new(dir.path().into());
    let program = shoal_syntax::parse("cp a b\nrm a").unwrap();
    let plan = evaluator.plan_program(&program).unwrap();
    assert!(plan.effects.contains(&Effect::FsRead {
        paths: vec![dir.path().join("a")]
    }));
    assert!(plan.effects.contains(&Effect::FsWrite {
        paths: vec![dir.path().join("b")]
    }));
    assert!(plan.effects.contains(&Effect::FsDelete {
        paths: vec![dir.path().join("a")],
        permanent: false,
    }));
    assert!(dir.path().join("a").exists());
    assert!(!dir.path().join("b").exists());
}

#[test]
fn planning_substitutes_adapter_effects() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().into());
    assert!(evaluator.load_bundled_adapters().is_empty());
    let plan = evaluator
        .plan_program(&shoal_syntax::parse("git push origin main").unwrap())
        .unwrap();
    assert!(plan.effects.contains(&Effect::FsRead {
        paths: vec![dir.path().into()]
    }));
    assert!(plan.effects.contains(&Effect::NetConnect {
        host: "origin".into(),
        port: 443
    }));
    assert!(
        plan.effects
            .iter()
            .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "git"))
    );
}

#[test]
fn planning_unknown_and_sh_are_opaque_and_spawn_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let src = format!("unknown-command\nsh {{ touch {} }}", marker.display());
    let mut evaluator = Evaluator::new(dir.path().into());
    let plan = evaluator
        .plan_program(&shoal_syntax::parse(&src).unwrap())
        .unwrap();
    assert!(plan.effects.contains(&Effect::Opaque));
    assert!(!marker.exists());
}

// ---- site/content/internals/language-conformance-contract.md binary-content-hash spawn pinning ------------------------

/// `hash_resolved_bin` must produce reef/leash's exact blake3-hex so a pin
/// an author copies from `reef`/`which` output compares equal to what the
/// spawn gate computes. Cross-check against all three producers.
#[test]
fn hash_resolved_bin_matches_reef_and_leash_encoding() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("toolbin");
    std::fs::write(&bin, b"#!/bin/sh\necho hi\n").unwrap();
    let ev = Evaluator::new(dir.path().into());
    let got = ev
        .hash_resolved_bin(OsStr::new(bin.as_os_str()))
        .expect("absolute path is hashable");
    assert!(!got.is_empty());
    // Same as hashing the bytes directly (reef's `hash_bytes`)…
    assert_eq!(
        got,
        shoal_reef::hashcache::hash_bytes(b"#!/bin/sh\necho hi\n")
    );
    // …and as reef's file-hash cache…
    assert_eq!(
        got,
        shoal_reef::hashcache::HashCache::new()
            .hash_file(&bin)
            .unwrap()
    );
    // …and as leash's own preflight hasher (the exec-time verifier).
    assert_eq!(got, shoal_leash::preflight_spawn(&bin, &[]).unwrap().hash);
}

#[test]
fn hash_resolved_bin_streams_multi_chunk_files_and_rejects_directories() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("large-toolbin");
    let bytes = vec![b'z'; 3 * 64 * 1024 + 19];
    std::fs::write(&bin, &bytes).unwrap();
    let evaluator = Evaluator::new(dir.path().into());
    assert_eq!(
        evaluator
            .hash_resolved_bin(OsStr::new(bin.as_os_str()))
            .unwrap(),
        blake3::hash(&bytes).to_hex().to_string()
    );
    assert!(
        evaluator
            .hash_resolved_bin(OsStr::new(dir.path().as_os_str()))
            .is_none()
    );
}

#[test]
fn production_spawn_gate_hashing_does_not_use_whole_file_fs_read() {
    let source = include_str!("../command/external.rs");
    let production = source.split("#[cfg(test)]").next().unwrap();
    assert!(!production.contains("self.host.fs.read(&resolved)"));
    assert!(production.contains("[0u8; 64 * 1024]"));
}

/// The security-critical gate, exercised directly (a full external spawn is
/// awkward in-harness): no policy and no-`proc_spawn` policy both allow every
/// spawn (the no-regression guarantee); a pinned allowlist admits only the
/// matching binary and denies an unlisted one.
#[test]
fn spawn_gate_no_regression_then_enforces_when_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("toolbin");
    std::fs::write(&bin, b"real binary bytes").unwrap();
    let bin_os = OsStr::new(bin.as_os_str());
    let hash = shoal_reef::hashcache::hash_bytes(b"real binary bytes");

    // 1. No leash policy installed at all ⇒ allow (today's behavior).
    let ev = Evaluator::new(dir.path().into());
    assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

    // 2. Permissive policy (no `proc_spawn` grants) ⇒ allow. This is the
    //    default a human principal gets; a regression here would break the
    //    shell for everyone.
    let mut ev = Evaluator::new(dir.path().into());
    ev.set_leash_policy(LeashPolicy::permissive("human"), "human");
    assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

    // 3. Scoped fs policy but still no `proc_spawn` grants ⇒ allow.
    let mut ev = Evaluator::new(dir.path().into());
    ev.set_leash_policy(
        LeashPolicy::from_toml("[principal.agent]\n\n[principal.agent.fs]\nread=[\"/work/**\"]\n")
            .unwrap(),
        "agent",
    );
    assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

    // 4. Pinned to this binary's exact hash ⇒ allow it (hashed here, since
    //    reef didn't resolve it — reef_hash is None).
    let mut ev = Evaluator::new(dir.path().into());
    ev.set_leash_policy(
        LeashPolicy::from_toml(&format!("[principal.agent]\nproc_spawn = [\"{hash}\"]\n")).unwrap(),
        "agent",
    );
    assert!(ev.spawn_gate(bin_os, None, Span::default()).is_ok());

    // 5. Pinned to a DIFFERENT hash (and the name is not listed) ⇒ deny.
    let mut ev = Evaluator::new(dir.path().into());
    ev.set_leash_policy(
        LeashPolicy::from_toml(&format!(
            "[principal.agent]\nproc_spawn = [\"{}\"]\n",
            "00".repeat(32)
        ))
        .unwrap(),
        "agent",
    );
    let err = ev
        .spawn_gate(bin_os, None, Span::default())
        .expect_err("unlisted binary must be denied under an active pin");
    assert_eq!(err.code, "spawn_denied");

    // 6. Reusing reef's already-computed hash takes the same allow path
    //    without touching the file (pass a bogus path but the real hash).
    let mut ev = Evaluator::new(dir.path().into());
    ev.set_leash_policy(
        LeashPolicy::from_toml(&format!("[principal.agent]\nproc_spawn = [\"{hash}\"]\n")).unwrap(),
        "agent",
    );
    assert!(
        ev.spawn_gate(
            OsStr::new("/nonexistent/tool"),
            Some(&hash),
            Span::default()
        )
        .is_ok()
    );
}

/// `plan_derive` now emits a real, non-empty `bin_hash` for an adapter whose
/// bin resolves to a real file — the content hash a `proc_spawn` pin checks.
#[test]
fn planning_emits_real_bin_hash_for_resolved_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("toolbin");
    let body = b"fixture tool bytes for planning";
    std::fs::write(&bin, body).unwrap();
    // An adapter whose `bin` is the absolute fixture path (host-independent).
    std::fs::write(
        dir.path().join("mytool.toml"),
        format!("[cmd.mytool]\nbin=\"{}\"\n", bin.display()),
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut evaluator = Evaluator::new(dir.path().into());
    evaluator.set_adapters(catalog);
    let plan = evaluator
        .plan_program(&shoal_syntax::parse("mytool").unwrap())
        .unwrap();
    let spawn = plan
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::ProcSpawn { bin_hash, .. } => Some(bin_hash.clone()),
            _ => None,
        })
        .expect("adapter spawn effect present");
    assert!(!spawn.is_empty(), "bin_hash must no longer be empty");
    assert_eq!(spawn, shoal_reef::hashcache::hash_bytes(body));
}

#[test]
fn planning_unions_conditional_and_static_function_effects() {
    let dir = tempfile::tempdir().unwrap();
    let src = "fn cleanup() { rm old }\nif true { cleanup() } else { touch new }";
    let mut evaluator = Evaluator::new(dir.path().into());
    let parsed = shoal_syntax::parse(src).unwrap();
    let plan = evaluator.plan_program(&parsed).unwrap();
    assert!(plan.effects.contains(&Effect::FsDelete {
        paths: vec![dir.path().join("old")],
        permanent: false,
    }));
    assert!(plan.effects.contains(&Effect::FsWrite {
        paths: vec![dir.path().join("new")]
    }));
}
