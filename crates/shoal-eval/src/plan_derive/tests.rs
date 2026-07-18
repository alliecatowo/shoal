use super::*;
use shoal_value::{Fs, ReadSeek};
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct DenyProbeFs {
    probes: Mutex<Vec<String>>,
}

impl DenyProbeFs {
    fn deny<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "filesystem adapter denied operation",
        ))
    }

    fn probes(&self) -> Vec<String> {
        self.probes.lock().unwrap().clone()
    }
}

impl Fs for DenyProbeFs {
    fn read(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Self::deny()
    }
    fn read_to_string(&self, _path: &Path) -> io::Result<String> {
        Self::deny()
    }
    fn open_read(&self, _path: &Path) -> io::Result<Box<dyn ReadSeek + Send>> {
        Self::deny()
    }
    fn write(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
        Self::deny()
    }
    fn append(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
        Self::deny()
    }
    fn touch(&self, _path: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
        Self::deny()
    }
    fn symlink_metadata(&self, _path: &Path) -> io::Result<std::fs::Metadata> {
        Self::deny()
    }
    fn is_file(&self, path: &Path) -> bool {
        self.probes
            .lock()
            .unwrap()
            .push(format!("is_file:{}", path.display()));
        false
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.probes
            .lock()
            .unwrap()
            .push(format!("canonicalize:{}", path.display()));
        Self::deny()
    }
    fn read_dir(&self, _path: &Path) -> io::Result<Vec<PathBuf>> {
        Self::deny()
    }
    fn create_dir(&self, _path: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn create_dir_all(&self, _path: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn remove_file(&self, _path: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn remove_dir_all(&self, _path: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn rename(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn copy(&self, _from: &Path, _to: &Path) -> io::Result<u64> {
        Self::deny()
    }
    fn hard_link(&self, _src: &Path, _dst: &Path) -> io::Result<()> {
        Self::deny()
    }
    fn symlink(&self, _target: &Path, _link: &Path) -> io::Result<()> {
        Self::deny()
    }
}

#[test]
fn module_resolution_probes_only_the_injected_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("module.shl");
    std::fs::write(&module, "export let x = 1").unwrap();
    let fs = Arc::new(DenyProbeFs::default());
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_fs(fs.clone());

    let effects = ev
        .plan_program(&shoal_syntax::parse("use ./module").unwrap())
        .unwrap()
        .effects;
    let base = dir.path().join("./module");
    assert!(
        effects.contains(&Effect::FsRead {
            paths: vec![base.clone()]
        }),
        "a denied adapter must not discover the ambient .shl file: {effects:?}"
    );
    assert_eq!(
        fs.probes(),
        vec![
            format!("is_file:{}", base.with_extension("shl").display()),
            format!("canonicalize:{}", base.display()),
        ]
    );
}

#[test]
fn cd_canonicalization_cannot_escape_a_denying_adapter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("child")).unwrap();
    let fs = Arc::new(DenyProbeFs::default());
    let mut ev = Evaluator::new(dir.path().to_path_buf());
    ev.set_fs(fs.clone());

    let err = ev
        .eval_program(&shoal_syntax::parse("cd child").unwrap())
        .unwrap_err();
    assert_eq!(err.code, "arg_error");
    assert_eq!(ev.cwd(), dir.path());
    assert_eq!(
        fs.probes(),
        vec![format!(
            "canonicalize:{}",
            dir.path().join("child").display()
        )]
    );
}

/// Production guard for the evaluator paths audited in HR-C. It is
/// intentionally scoped to path-probe spellings rather than metadata
/// classification (`Metadata::is_file/is_dir`), which is already obtained
/// through `Fs::metadata`/`Fs::symlink_metadata` and must remain available
/// for symlink-safe trash handling.
#[test]
fn audited_evaluator_paths_have_no_ambient_path_probes() {
    fn production(source: &'static str) -> &'static str {
        source
            .rsplit_once("\n#[cfg(test)]\nmod tests")
            .map_or(source, |(production, _)| production)
    }

    let audited = [
        ("builtins.rs", production(include_str!("../builtins.rs"))),
        (
            "command/navigation.rs",
            production(include_str!("../command/navigation.rs")),
        ),
        ("journal.rs", production(include_str!("../journal.rs"))),
        (
            "plan_derive.rs",
            production(include_str!("../plan_derive.rs")),
        ),
        (
            "reef_builtins.rs",
            production(include_str!("../reef_builtins.rs")),
        ),
    ];
    let forbidden_everywhere = [
        ".exists()",
        ".canonicalize()",
        "std::fs::metadata(",
        "std::fs::symlink_metadata(",
        "FileFingerprint::capture(",
    ];
    for &(name, source) in &audited {
        for forbidden in forbidden_everywhere {
            assert!(
                !source.contains(forbidden),
                "ambient filesystem probe `{forbidden}` reappeared in {name}"
            );
        }
    }

    for (name, forbidden) in [
        ("builtins.rs", "root.is_dir()"),
        ("builtins.rs", "dest.is_dir()"),
        ("journal.rs", "dest.is_dir()"),
        ("journal.rs", "target.is_file()"),
        ("plan_derive.rs", "with_shl.is_file()"),
    ] {
        let source = audited
            .iter()
            .find_map(|(candidate, source)| (*candidate == name).then_some(*source))
            .unwrap();
        assert!(
            !source.contains(forbidden),
            "ambient filesystem probe `{forbidden}` reappeared in {name}"
        );
    }
}

/// Keep effect attribution split by responsibility. The planner is an
/// exhaustive security boundary, so allowing its command dispatcher or AST
/// walk to grow back into a single review-hostile unit is a regression even
/// when the immediate behavior still passes.
#[test]
fn planner_responsibilities_stay_bounded() {
    let root = include_str!("../plan_derive.rs");
    let production = root
        .split_once("\n#[cfg(test)]\nmod tests")
        .expect("planner keeps production and tests separated")
        .0;
    let commands = include_str!("commands.rs");
    let attribution = include_str!("attribution.rs");
    let statements = include_str!("statements.rs");
    let inputs = include_str!("inputs.rs");
    let value_effects = include_str!("value_effects.rs");

    for (name, source, ceiling) in [
        ("plan_derive.rs production", production, 320),
        ("commands.rs", commands, 350),
        ("attribution.rs", attribution, 140),
        ("statements.rs", statements, 100),
        ("inputs.rs", inputs, 100),
        ("value_effects.rs", value_effects, 140),
    ] {
        let lines = source.lines().count();
        assert!(
            lines <= ceiling,
            "{name} grew to {lines} lines (ceiling {ceiling}); split the new responsibility"
        );
    }

    let plan_call = commands
        .split_once("pub(super) fn plan_call")
        .expect("command coordinator exists")
        .1
        .split_once("\n    /// Heads intercepted")
        .expect("command helpers remain separate")
        .0;
    assert!(
        plan_call.lines().count() <= 80,
        "plan_call became a dispatcher god function; extract a command concern"
    );

    for forbidden in [
        "parse_declared_effect",
        "resolve_command(call)",
        "RedirectKind::",
        "hash_resolved_bin",
        "canonicalize(&candidate)",
    ] {
        assert!(
            !production.contains(forbidden),
            "planner root reclaimed delegated responsibility `{forbidden}`"
        );
    }
    assert!(
        !commands.contains("Stmt::"),
        "command planning must not absorb statement traversal"
    );
    assert!(
        !attribution.contains("push_effect"),
        "literal attribution must remain effect-free"
    );
}

/// A7: an adapter that declares a now-recognized `proc.spawn(...)` plus an
/// unrecognized effect kind. The spawn must be derived (it was silently
/// dropped before), and the unknown kind must plan as opaque — never
/// silently ignored (fail-closed).
#[test]
fn adapter_effect_vocabulary_is_exhaustive_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.toml"),
        r#"[cmd.deployer]
bin="/bin/true"
effects=["proc.spawn(container)", "net.connect(registry:443)", "quantum.entangle(qubit)"]
"#,
    )
    .unwrap();
    let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut ev = Evaluator::new(dir.path().into());
    ev.set_adapters(catalog);
    let effects = ev
        .plan_program(&shoal_syntax::parse("deployer").unwrap())
        .unwrap()
        .effects;
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "container")),
        "declared proc.spawn was not derived: {effects:?}"
    );
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::NetConnect { host, port } if host == "registry" && *port == 443)
        ),
        "declared net.connect was not derived: {effects:?}"
    );
    assert!(
        effects.contains(&Effect::Opaque),
        "unknown adapter effect kind was silently dropped: {effects:?}"
    );
}

/// A7 (unit): every effect kind in the vocabulary parses to its concrete
/// effect; a bare unknown token plans as opaque, never dropped.
#[test]
fn declared_effect_covers_full_vocabulary() {
    use crate::plan_effects::parse_declared_effect;
    let cwd = Path::new("/tmp");
    let b = std::collections::HashMap::new();
    assert_eq!(
        parse_declared_effect("net.listen(8080)", &b, cwd),
        vec![Effect::NetListen { port: 8080 }]
    );
    assert_eq!(
        parse_declared_effect("env.write(TOKEN)", &b, cwd),
        vec![Effect::EnvWrite {
            names: vec!["TOKEN".into()]
        }]
    );
    assert_eq!(
        parse_declared_effect("secret.use(github)", &b, cwd),
        vec![Effect::SecretUse {
            names: vec!["github".into()]
        }]
    );
    assert_eq!(
        parse_declared_effect("session.write", &b, cwd),
        vec![Effect::SessionWrite]
    );
    assert_eq!(parse_declared_effect("time", &b, cwd), vec![Effect::Time]);
    assert_eq!(
        parse_declared_effect("bogus.kind(x)", &b, cwd),
        vec![Effect::Opaque]
    );
    assert_eq!(
        parse_declared_effect("bare-nonsense", &b, cwd),
        vec![Effect::Opaque]
    );
}

/// Derive the effect list for `src` under a fresh evaluator rooted at `cwd`.
fn effects_at(cwd: &Path, src: &str) -> Vec<Effect> {
    let mut ev = Evaluator::new(cwd.to_path_buf());
    ev.plan_program(&shoal_syntax::parse(src).unwrap())
        .unwrap()
        .effects
}

#[test]
fn http_planning_uses_the_runtime_uri_authority_parser() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        effects_at(
            dir.path(),
            r#"http.get("HTTP://EXAMPLE.COM:8080/resource")"#
        ),
        vec![Effect::NetConnect {
            host: "example.com".into(),
            port: 8080,
        }]
    );
    assert_eq!(
        effects_at(dir.path(), r#"http.get("https://example.com/resource")"#),
        vec![Effect::NetConnect {
            host: "example.com".into(),
            port: 443,
        }]
    );
    assert_eq!(
        effects_at(dir.path(), r#"http.get("http://example.com:99999/")"#),
        vec![Effect::NetConnect {
            host: "*".into(),
            port: 443,
        }],
        "an invalid runtime authority must plan as wildcard, not a different concrete endpoint"
    );
}

/// Planner/runtime resolution lockstep: no name in the canonical builtin
/// registry may fall through to the generic external-spawn branch. This is
/// the regression that made the in-language `history` command look like an
/// external binary and caused a restricted kernel session to deny it.
#[test]
fn canonical_builtins_never_fall_through_as_external_spawns() {
    let dir = tempfile::tempdir().unwrap();
    for &name in builtins::builtin_names() {
        let src = match name {
            "cp" | "mv" | "ln" => format!("{name} from to"),
            "interact" => "interact echo".to_string(),
            "open" => "open file".to_string(),
            "save" => "save file value".to_string(),
            "run" => "run echo".to_string(),
            "source" => "source script.shl".to_string(),
            "assert" => "assert true".to_string(),
            "apply" => "apply 1".to_string(),
            "explain" => "explain \"echo hi\"".to_string(),
            _ => name.to_string(),
        };
        let effects = effects_at(dir.path(), &src);
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == name)),
            "builtin `{name}` fell through as an external spawn: {effects:?}"
        );
    }

    assert_eq!(
        effects_at(dir.path(), "history"),
        vec![Effect::JournalRead],
        "history must plan the journal read performed by runtime dispatch"
    );
    assert_eq!(
        effects_at(dir.path(), "journal"),
        vec![Effect::JournalRead],
        "journal must plan the journal read performed by runtime dispatch"
    );
    assert_eq!(
        effects_at(dir.path(), "plan { echo hi }"),
        vec![Effect::SessionWrite],
        "plan must declare the stored-program mutation used by apply"
    );
    assert!(
        effects_at(dir.path(), "interact echo")
            .iter()
            .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "echo")),
        "interact must plan its command argument as the spawned process"
    );
}

/// A1: `use ./mod` reads the module file and covers its body conservatively.
#[test]
fn use_reads_module_and_covers_body() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mod.shl"), "export let x = 1").unwrap();
    let effects = effects_at(dir.path(), "use ./mod");
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::FsRead { paths } if paths.iter().any(|p| p.ends_with("mod.shl")))
        ),
        "use did not read the module: {effects:?}"
    );
    assert!(
        effects.contains(&Effect::Opaque),
        "module body not covered: {effects:?}"
    );
}

/// A2: persistent `env.NAME = …` is an EnvWrite naming NAME.
#[test]
fn env_assignment_is_env_write() {
    let dir = tempfile::tempdir().unwrap();
    let effects = effects_at(dir.path(), "env.AUDIT_ONLY = \"y\"");
    assert!(
        effects.contains(&Effect::EnvWrite {
            names: vec!["AUDIT_ONLY".into()]
        }),
        "env.NAME = … did not plan an EnvWrite: {effects:?}"
    );
}

/// A3: `>`/`>>` write the redirect target; `< file` reads it.
#[test]
fn redirects_derive_fs_effects() {
    let dir = tempfile::tempdir().unwrap();
    let trunc = effects_at(dir.path(), "echo hi > out.txt");
    assert!(
        trunc.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("out.txt")]
        }),
        "`> out.txt` did not plan a write: {trunc:?}"
    );
    let append = effects_at(dir.path(), "echo hi >> out.txt");
    assert!(
        append.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("out.txt")]
        }),
        "`>> out.txt` did not plan a write: {append:?}"
    );
    let read = effects_at(dir.path(), "cat < in.txt");
    assert!(
        read.contains(&Effect::FsRead {
            paths: vec![dir.path().join("in.txt")]
        }),
        "`< in.txt` did not plan a read: {read:?}"
    );
}

/// A4: `.save`/`.append` write the path argument; path reads read it.
#[test]
fn method_save_append_and_path_read() {
    let dir = tempfile::tempdir().unwrap();
    let save = effects_at(dir.path(), "\"x\".save(\"p\")");
    assert!(
        save.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("p")]
        }),
        ".save did not plan a write: {save:?}"
    );
    let append = effects_at(dir.path(), "\"x\".append(\"p\")");
    assert!(
        append.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("p")]
        }),
        ".append did not plan a write: {append:?}"
    );
    // Bare `.read` (Field form) and `.read()` (MethodCall form) both read.
    for src in ["path(\"f\").read", "path(\"f\").read()"] {
        let read = effects_at(dir.path(), src);
        assert!(
            read.contains(&Effect::FsRead {
                paths: vec![dir.path().join("f")]
            }),
            "`{src}` did not plan a read: {read:?}"
        );
    }
}

/// A9: `.feed(cat)` spawns cat externally (matching run_argv), not the
/// `cat` builtin — no FsRead, a concrete ProcSpawn.
#[test]
fn feed_resolves_command_as_external_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let effects = effects_at(dir.path(), "\"x\".feed(cat)");
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "cat")),
        ".feed(cat) did not spawn cat: {effects:?}"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::FsRead { .. })),
        ".feed(cat) mis-resolved cat as the builtin (FsRead): {effects:?}"
    );
}

/// A5: a call to a previously-defined session function that cannot be
/// statically expanded derives an approval-requiring effect, never nothing.
#[test]
fn session_closure_call_is_opaque() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().into());
    // Define a session function with an effect, as a prior REPL line would.
    ev.eval_program(&shoal_syntax::parse("fn danger() { \"x\".save(\"p\") }").unwrap())
        .unwrap();
    // A later program that only *calls* it cannot expand the closure body.
    for src in ["danger()", "danger"] {
        let effects = ev
            .plan_program(&shoal_syntax::parse(src).unwrap())
            .unwrap()
            .effects;
        assert!(
            effects.contains(&Effect::Opaque),
            "session closure `{src}` was reported effect-free: {effects:?}"
        );
    }
}

#[test]
fn planner_and_runtime_agree_on_non_callable_binding_shadow() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().into());
    ev.env_mut().declare("ls", Value::Int(42), false).unwrap();

    let bare = shoal_syntax::parse("ls").unwrap();
    assert_eq!(ev.eval_program(&bare).unwrap(), Value::Int(42));
    assert!(
        ev.plan_program(&bare).unwrap().effects.is_empty(),
        "planning must not invent a process or filesystem effect for a bound value"
    );

    let with_arg = ev
        .plan_program(&shoal_syntax::parse("ls .").unwrap())
        .unwrap();
    assert!(
        with_arg
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::FsRead { .. })),
        "an ineligible value shadow must fall through to the builtin"
    );
}

/// A6: generic external commands derive a concrete ProcSpawn (like adapter
/// spawns), both via `run(...)` and as a bare external.
#[test]
fn generic_external_commands_spawn_concretely() {
    let dir = tempfile::tempdir().unwrap();
    let run = effects_at(dir.path(), "run(\"echo\", \"hi\")");
    assert!(
        run.iter()
            .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "echo")),
        "run(echo) did not spawn echo: {run:?}"
    );
    let bare = effects_at(dir.path(), "some_external_tool_xyz");
    assert!(
        bare.iter().any(
            |e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "some_external_tool_xyz")
        ),
        "bare external did not spawn: {bare:?}"
    );
}

#[test]
fn dynamic_run_bypasses_callable_and_builtin_layers() {
    let dir = tempfile::tempdir().unwrap();
    let mut ev = Evaluator::new(dir.path().into());
    ev.eval_program(&shoal_syntax::parse("fn ls() { null }").unwrap())
        .unwrap();
    let effects = ev
        .plan_program(&shoal_syntax::parse(r#"run("ls")"#).unwrap())
        .unwrap()
        .effects;
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ProcSpawn { argv0, .. } if argv0 == "ls")),
        "run must bypass the callable and builtin layers: {effects:?}"
    );
    assert!(!effects.contains(&Effect::Opaque));
}

/// A8: effectful builtins and `spawn`/`parallel` bodies derive their
/// bodies'/arguments' effects.
#[test]
fn effectful_builtins_and_task_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let spawned = effects_at(dir.path(), "spawn { \"x\".save(\"p\") }");
    assert!(
        spawned.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("p")]
        }),
        "spawn body write missing: {spawned:?}"
    );
    let par = effects_at(dir.path(), "parallel(() => \"x\".save(\"p\"))");
    assert!(
        par.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("p")]
        }),
        "parallel body write missing: {par:?}"
    );
    let save = effects_at(dir.path(), "save(\"x\", \"p\")");
    assert!(
        save.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("x")]
        }),
        "builtin save write missing: {save:?}"
    );
    let open = effects_at(dir.path(), "open(\"Cargo.toml\")");
    assert!(
        open.contains(&Effect::FsRead {
            paths: vec![dir.path().join("Cargo.toml")]
        }) && open.contains(&Effect::Opaque),
        "open effects missing: {open:?}"
    );
}

/// A10: no effectful form silently derives an empty effect set, and pure
/// forms derive nothing. (The compile-time exhaustive match — no wildcard —
/// is the structural guarantee; this pins the behavior.)
#[test]
fn effectful_forms_are_never_silently_empty() {
    let dir = tempfile::tempdir().unwrap();
    for src in [
        "echo hi > out",
        "cat < in",
        "rm x",
        "\"x\".save(\"p\")",
        "\"x\".append(\"p\")",
        "path(\"f\").read",
        "env.X = \"y\"",
        "use ./m",
        "open(\"f\")",
        "save(\"x\", \"p\")",
        "run(\"echo\", \"hi\")",
        "unknown_external_tool_xyz",
        "\"x\".feed(cat)",
        "spawn { \"x\".save(\"p\") }",
        "parallel(() => \"x\".save(\"p\"))",
        "http.get(\"https://example.com\")",
        "sh { echo hi }",
    ] {
        let effects = effects_at(dir.path(), src);
        assert!(
            !effects.is_empty(),
            "effectful form `{src}` derived no effects"
        );
    }
    for src in [
        "1 + 2",
        "let x = [1, 2, 3]",
        "\"a\".upper()",
        "{a: 1, b: 2}",
        "path(\"f\")",
    ] {
        let effects = effects_at(dir.path(), src);
        assert!(
            effects.is_empty(),
            "pure form `{src}` derived spurious effects: {effects:?}"
        );
    }
}

/// HR-A11: pin every effect-planning probe that originally demonstrated a
/// fail-open route. These assertions check the meaningful effect and target,
/// not merely that the list happens to be non-empty.
#[test]
fn original_audit_probes_have_meaningful_effects() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("module.shl"), "export let x = 1").unwrap();

    let has_write = |src: &str, path: &str| {
        effects_at(&root, src)
            .iter()
            .any(|e| matches!(e, Effect::FsWrite { paths } if paths.contains(&root.join(path))))
    };
    let has_read = |src: &str, path: &str| {
        effects_at(&root, src)
            .iter()
            .any(|e| matches!(e, Effect::FsRead { paths } if paths.contains(&root.join(path))))
    };
    let spawns = |src: &str, head: &str| {
        effects_at(&root, src)
            .iter()
            .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == head))
    };

    assert!(has_write("\"x\".save(\"p\")", "p"), "method save");
    assert!(has_write("echo hi > p", "p"), "redirect write");
    assert!(
        effects_at(&root, "env.AUDIT_ONLY = \"y\"").contains(&Effect::EnvWrite {
            names: vec!["AUDIT_ONLY".into()]
        }),
        "persistent env assignment"
    );
    assert!(
        has_read("path(\"Cargo.toml\").read", "Cargo.toml"),
        "path read"
    );
    let module = effects_at(&root, "use ./module");
    assert!(
        module.iter().any(
            |e| matches!(e, Effect::FsRead { paths } if paths.contains(&root.join("module.shl")))
        ) && module.contains(&Effect::Opaque),
        "module read/body coverage: {module:?}"
    );
    assert!(has_write("spawn { \"x\".save(\"p\") }", "p"), "spawn body");
    assert!(
        has_write("parallel(() => \"x\".save(\"p\"))", "p"),
        "parallel body"
    );
    assert!(spawns("run(\"echo\", \"hi\")", "echo"), "run builtin");

    let open = effects_at(&root, "open(\"Cargo.toml\")");
    assert!(
        open.iter().any(
            |e| matches!(e, Effect::FsRead { paths } if paths.contains(&root.join("Cargo.toml")))
        ) && open.contains(&Effect::Opaque),
        "open read/handler coverage: {open:?}"
    );
    assert!(spawns("\"x\".feed(cat)", "cat"), "feed external spawn");

    // The original function-form save probe also stays pinned: the runtime
    // signature is save(path, value), so the first argument is the target.
    assert!(has_write("save(\"p\", \"x\")", "p"), "save builtin");
}

fn parsed_expr(source: &str) -> Expr {
    let program = shoal_syntax::parse(source).unwrap();
    let [Stmt::Expr { expr, .. }] = program.stmts.as_slice() else {
        panic!("expected one expression in `{source}`")
    };
    expr.clone()
}

fn command_expr(call: CmdCall) -> Program {
    Program {
        stmts: vec![Stmt::Expr {
            span: call.span,
            expr: Expr::Cmd {
                span: call.span,
                call: Box::new(call),
            },
        }],
    }
}

fn expression_arg(source: &str) -> CmdArg {
    CmdArg::Expr {
        expr: parsed_expr(source),
        span: Span::default(),
    }
}

#[test]
fn command_inputs_are_recursively_planned_before_external_or_plugin_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let span = Span::default();
    let call = CmdCall {
        head: "audit-external".into(),
        forced: false,
        args: vec![
            expression_arg(r#"http.get("https://input.example:8443/x")"#),
            CmdArg::FlagLong {
                name: "token".into(),
                value: Some(Box::new(expression_arg(r#"secret.get("ARG_TOKEN")"#))),
                span,
            },
            expression_arg(r#"path("nested.txt").read()"#),
            expression_arg(r#""response={http.get("https://interp.example/x")}""#),
            expression_arg(r#"() => secret.get("CLOSURE_TOKEN")"#),
            expression_arg("dynamic_path.lines()"),
        ],
        redirects: vec![],
        env_prefix: vec![EnvPrefix {
            name: "AUTH".into(),
            value: expression_arg(r#"secret.get("ENV_TOKEN")"#),
            span,
        }],
        background: false,
        trailing: None,
        span,
    };
    let mut evaluator = Evaluator::new(dir.path().into());
    let effects = evaluator.plan_program(&command_expr(call)).unwrap().effects;

    for expected in [
        Effect::NetConnect {
            host: "input.example".into(),
            port: 8443,
        },
        Effect::SecretUse {
            names: vec!["ARG_TOKEN".into()],
        },
        Effect::SecretUse {
            names: vec!["ENV_TOKEN".into()],
        },
        Effect::FsRead {
            paths: vec![dir.path().join("nested.txt")],
        },
        Effect::NetConnect {
            host: "interp.example".into(),
            port: 443,
        },
        Effect::SecretUse {
            names: vec!["CLOSURE_TOKEN".into()],
        },
    ] {
        assert!(
            effects.contains(&expected),
            "missing {expected:?}: {effects:?}"
        );
    }
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::ProcSpawn { argv0, .. } if argv0 == "audit-external")
    ));
    assert!(
        effects.contains(&Effect::Opaque),
        "a nested dynamic path read must fail closed: {effects:?}"
    );
    // Plugin and external calls share `plan_command_inputs` before their
    // source-specific dispatch; this test deliberately exercises every input
    // shape without requiring a compiled component fixture.
}

#[test]
fn dynamic_redirects_and_path_receivers_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let span = Span::default();
    let dynamic_redirect = CmdCall {
        head: "audit-external".into(),
        forced: false,
        args: vec![],
        redirects: vec![Redirect {
            kind: RedirectKind::In,
            target: expression_arg("target"),
            span,
        }],
        env_prefix: vec![],
        background: false,
        trailing: None,
        span,
    };
    let mut evaluator = Evaluator::new(dir.path().into());
    let effects = evaluator
        .plan_program(&command_expr(dynamic_redirect))
        .unwrap()
        .effects;
    assert!(effects.contains(&Effect::Opaque), "{effects:?}");

    for method in ["read", "read_bytes", "lines"] {
        let effects = effects_at(dir.path(), &format!("let p = path(\"x\")\np.{method}()"));
        assert!(
            effects.contains(&Effect::Opaque),
            "dynamic p.{method} failed open: {effects:?}"
        );
    }
    let precise = effects_at(dir.path(), r#"path("x").read_bytes()"#);
    assert!(precise.contains(&Effect::FsRead {
        paths: vec![dir.path().join("x")]
    }));
}

#[test]
fn secret_and_environment_reads_are_attributed_through_headers_and_bindings() {
    let dir = tempfile::tempdir().unwrap();
    let mut evaluator = Evaluator::new(dir.path().into());
    evaluator
        .env_mut()
        .declare(
            "bound_token",
            Value::Secret(shoal_value::SecretVal {
                name: "BOUND_TOKEN".into(),
                value: Arc::from("material"),
            }),
            false,
        )
        .unwrap();
    let effects = evaluator
        .plan_program(
            &shoal_syntax::parse(
                r#"http.get("https://example.test", headers: {Authorization: bound_token})
env.PATH
os.env()"#,
            )
            .unwrap(),
        )
        .unwrap()
        .effects;
    for expected in [
        Effect::SecretUse {
            names: vec!["BOUND_TOKEN".into()],
        },
        Effect::EnvRead {
            names: vec!["PATH".into()],
        },
        Effect::EnvRead {
            names: vec!["*".into()],
        },
    ] {
        assert!(
            effects.contains(&expected),
            "missing {expected:?}: {effects:?}"
        );
    }

    let policy = LeashPolicy::from_toml(
        "[principal.agent]\nauto_apply='in-grant'\nnet_connect=['example.test:443']\nenv_read=['PATH', '*']\nsecret_use=[]\n",
    )
    .unwrap();
    let plan = Plan::new(effects, Reversibility::Reversible, Estimates::default());
    assert_eq!(
        policy.evaluate_plan("agent", &plan),
        shoal_leash::Verdict::Deny,
        "the newly surfaced secret effect must be enforceable"
    );

    let dynamic = effects_at(
        dir.path(),
        "let requested = \"DYNAMIC_TOKEN\"\nsecret.get(requested)",
    );
    assert!(dynamic.contains(&Effect::SecretUse {
        names: vec!["*".into()]
    }));
}

#[test]
fn tilde_paths_use_the_evaluator_environment_in_plans_and_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let injected_home = dir.path().join("injected-home");
    let mut evaluator = Evaluator::new(dir.path().into());
    evaluator
        .exec
        .shell
        .process_env
        .retain(|(name, _)| name != "HOME");
    evaluator.exec.shell.process_env.push((
        OsString::from("HOME"),
        injected_home.clone().into_os_string(),
    ));

    let effects = evaluator
        .plan_program(&shoal_syntax::parse("cat ~/notes.txt").unwrap())
        .unwrap()
        .effects;
    assert!(effects.contains(&Effect::FsRead {
        paths: vec![injected_home.join("notes.txt")]
    }));
    assert_eq!(
        evaluator.resolve_path("~/notes.txt"),
        injected_home.join("notes.txt")
    );

    let quoted = CmdArg::Str {
        expr: Expr::Str {
            value: "~/quoted.txt".into(),
            span: Span::default(),
        },
        span: Span::default(),
    };
    let quoted_runtime = evaluator.arg_path(&quoted).unwrap();
    let quoted_plan = evaluator.cmd_arg_path_literal(&quoted).unwrap();
    assert_eq!(quoted_runtime, dir.path().join("~/quoted.txt"));
    assert_eq!(quoted_plan, quoted_runtime);
}

#[test]
fn stream_sources_are_canonical_in_process_constructors_in_runtime_and_plans() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("events.log");
    std::fs::write(&file, "").unwrap();
    let source = format!(
        "every(1h)\nwatch(path({path:?}))\ntail(path({path:?}))",
        path = file.to_string_lossy()
    );
    let mut evaluator = Evaluator::new(dir.path().into());
    let effects = evaluator
        .plan_program(&shoal_syntax::parse(&source).unwrap())
        .unwrap()
        .effects;
    assert!(effects.contains(&Effect::Time), "{effects:?}");
    assert!(effects.contains(&Effect::FsRead {
        paths: vec![file.clone()]
    }));
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ProcSpawn { argv0, .. }
            if matches!(argv0.as_str(), "every" | "watch" | "tail"))),
        "stream constructors escaped to process resolution: {effects:?}"
    );

    let runtime = evaluator
        .eval_program(&shoal_syntax::parse("every(1h)").unwrap())
        .unwrap();
    assert!(matches!(runtime, Value::Stream(_)));
}
