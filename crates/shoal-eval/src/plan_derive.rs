//! The AST walk that derives a conservative, concrete [`Plan`] (see
//! [`crate::plan`] for the split rationale). Never spawns or mutates;
//! per-builtin/adapter effect computation lives in [`crate::plan_effects`].

use super::*;

use crate::plan_effects::push_effect;

mod attribution;
mod commands;
mod statements;

use attribution::{
    cmd_arg_str_literal, is_path_read_method, str_literal, url_host_port, url_literal,
};

type Functions = std::collections::HashMap<String, Block>;
type Aliases = std::collections::HashMap<String, CmdCall>;

impl Evaluator {
    /// Derive a conservative, concrete plan without spawning or mutating.
    pub fn plan_program(&mut self, program: &Program) -> VResult<Plan> {
        let mut effects = Vec::new();
        let mut functions = Functions::new();
        let mut aliases = Aliases::new();
        for stmt in &program.stmts {
            if let Stmt::Fn { decl } = stmt {
                functions.insert(decl.name.clone(), decl.body.clone());
            }
            if let Stmt::Alias { name, target, .. } = stmt {
                aliases.insert(name.clone(), target.clone());
            }
        }
        for stmt in &program.stmts {
            self.plan_stmt(stmt, &functions, &aliases, &mut effects, 0)?;
        }
        let reversibility = if effects
            .iter()
            .any(|e| matches!(e, Effect::Opaque | Effect::FsDelete { .. }))
        {
            Reversibility::Unknown
        } else {
            Reversibility::Reversible
        };
        Ok(Plan::new(effects, reversibility, Estimates::default()))
    }

    pub(crate) fn plan_expr(
        &mut self,
        expr: &Expr,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match expr {
            Expr::Cmd { call, .. } => self.plan_call(call, functions, aliases, out, depth),
            Expr::LangBlock { .. } => {
                push_effect(out, Effect::Opaque);
                Ok(())
            }
            Expr::Block { block, .. } | Expr::Spawn { body: block, .. } => {
                self.plan_block(block, functions, aliases, out, depth)
            }
            Expr::If {
                cond, then, r#else, ..
            } => {
                self.plan_expr(cond, functions, aliases, out, depth)?;
                self.plan_block(then, functions, aliases, out, depth)?;
                if let Some(other) = r#else {
                    self.plan_expr(other, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Try { body, handler, .. } => {
                self.plan_block(body, functions, aliases, out, depth)?;
                self.plan_block(handler, functions, aliases, out, depth)
            }
            Expr::Catch { expr, handler, .. } => {
                self.plan_expr(expr, functions, aliases, out, depth)?;
                self.plan_expr(handler, functions, aliases, out, depth)
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.plan_expr(lhs, functions, aliases, out, depth)?;
                self.plan_expr(rhs, functions, aliases, out, depth)
            }
            Expr::Unary { expr, .. } => self.plan_expr(expr, functions, aliases, out, depth),
            Expr::Field { recv, name, .. } => {
                // A bare `path("f").read`/`.lines`/… (no parens) is a filesystem
                // read of the receiver path (A4).
                if is_path_read_method(name)
                    && let Some(p) = self.path_literal(recv)
                {
                    push_effect(out, Effect::FsRead { paths: vec![p] });
                }
                self.plan_expr(recv, functions, aliases, out, depth)
            }
            Expr::Index { recv, index, .. } => {
                self.plan_expr(recv, functions, aliases, out, depth)?;
                self.plan_expr(index, functions, aliases, out, depth)
            }
            Expr::MethodCall {
                recv, name, args, ..
            } => {
                // `.feed(cmd)` bypasses builtin/adapter dispatch and spawns the
                // command via run_argv (A9): resolve the command operand as an
                // external spawn, exactly like the runtime — handled before the
                // generic traversal so it is not mis-resolved as a builtin.
                if name == "feed" && args.pos.len() == 1 && args.named.is_empty() {
                    return self.plan_feed(recv, &args.pos[0], functions, aliases, out, depth);
                }
                // `http.get/post/put/delete(url, …)` declares a `net.connect`
                // effect for leash + plan (site/content/internals/roadmap-and-priorities.md). The host is parsed from a
                // literal URL argument; a non-literal URL declares an
                // unknown-host connect (`*`).
                if let Expr::Var { name: ns, .. } = &**recv
                    && ns == "http"
                    && matches!(name.as_str(), "get" | "post" | "put" | "delete")
                {
                    let (host, port) = args
                        .pos
                        .first()
                        .and_then(url_literal)
                        .map(|u| url_host_port(&u))
                        .unwrap_or_else(|| ("*".into(), 443));
                    push_effect(out, Effect::NetConnect { host, port });
                }
                // `.save`/`.append` write the path argument (A4). A dynamic path
                // cannot be bounded, so it plans as `Opaque` (approval).
                if matches!(name.as_str(), "save" | "append") {
                    let path = args.pos.first().and_then(|a| self.path_literal(a));
                    self.plan_save(path, out);
                }
                // Filesystem-backed path reads (`.read`/`.lines`/…) of a path
                // literal receiver (A4).
                if is_path_read_method(name)
                    && let Some(p) = self.path_literal(recv)
                {
                    push_effect(out, Effect::FsRead { paths: vec![p] });
                }
                self.plan_expr(recv, functions, aliases, out, depth)?;
                for e in &args.pos {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                for n in &args.named {
                    self.plan_expr(&n.value, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::FnCall { name, args, span } => {
                // Arguments always contribute their own effects — including the
                // bodies of lambda arguments to `parallel`/`retry`/`on`/`map`
                // (A8), via the `Expr::Lambda` arm.
                for e in &args.pos {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                for n in &args.named {
                    self.plan_expr(&n.value, functions, aliases, out, depth)?;
                }
                match name.as_str() {
                    // Effectful builtins invoked as functions (A8).
                    "run" => self.plan_run_target(args.pos.first().and_then(str_literal), out),
                    // `save(path, value)` writes the first argument.
                    "save" => {
                        let path = args.pos.first().and_then(|a| self.path_literal(a));
                        self.plan_save(path, out);
                    }
                    "open" => {
                        let path = args.pos.first().and_then(|a| self.path_literal(a));
                        self.plan_open(path, out);
                    }
                    // Clock reads.
                    "now" | "today" => push_effect(out, Effect::Time),
                    // Higher-order builtins: their closure bodies are already
                    // planned above via the Lambda arm; `assert` is pure.
                    "parallel" | "retry" | "on" | "assert" => {}
                    // Provably pure value constructors (no IO at construction).
                    "path" | "glob" | "regex" | "channel" => {}
                    other => {
                        if let Some(body) = functions.get(other) {
                            // A function declared in this program: expand it.
                            self.plan_block(body, functions, aliases, out, depth + 1)?;
                        } else if self
                            .exec
                            .shell
                            .env
                            .get(other)
                            .is_some_and(|v| v.is_callable())
                        {
                            // A session-stored closure/function that cannot be
                            // statically expanded (A5): require approval, never
                            // report nothing.
                            push_effect(out, Effect::Opaque);
                        } else if self.is_command_name(other) {
                            // A bare name that resolves as a command runs as one
                            // (defect #5); plan it with command resolution.
                            self.plan_command_ref(other, *span, functions, aliases, out, depth)?;
                        } else {
                            // Not a known pure form, an expandable function, a
                            // session closure, or a command — cannot be proven
                            // effect-free (A5/A10).
                            push_effect(out, Effect::Opaque);
                        }
                    }
                }
                Ok(())
            }
            Expr::List { items, .. } => {
                for e in items {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Record { fields, .. } => {
                for f in fields {
                    self.plan_expr(&f.value, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Range { start, end, .. } => {
                self.plan_expr(start, functions, aliases, out, depth)?;
                self.plan_expr(end, functions, aliases, out, depth)
            }
            Expr::With {
                cwd,
                env,
                reef,
                body,
                ..
            } => {
                if let Some(e) = cwd {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = env {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = reef {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                self.plan_block(body, functions, aliases, out, depth)
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.plan_expr(scrutinee, functions, aliases, out, depth)?;
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.plan_expr(g, functions, aliases, out, depth)?
                    }
                    self.plan_expr(&arm.body, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            // A lambda's body effects surface wherever the lambda is used — the
            // higher-order builtins (`parallel`, `map`, …) will invoke it (A8).
            Expr::Lambda { body, .. } => self.plan_expr(body, functions, aliases, out, depth),
            Expr::StrInterp { parts, .. } => {
                for part in parts {
                    if let StrPart::Expr { expr } = part {
                        self.plan_expr(expr, functions, aliases, out, depth)?;
                    }
                }
                Ok(())
            }
            // Provably effect-free atoms: an empty effect set is correct here.
            // No wildcard arm — a new `Expr` variant must be classified here,
            // so an effectful form can never silently derive no effects (A10).
            Expr::Null { .. }
            | Expr::Bool { .. }
            | Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Size { .. }
            | Expr::Duration { .. }
            | Expr::Time { .. }
            | Expr::DateTime { .. }
            | Expr::Regex { .. }
            | Expr::Var { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
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
            ("builtins.rs", production(include_str!("builtins.rs"))),
            (
                "command/navigation.rs",
                production(include_str!("command/navigation.rs")),
            ),
            ("journal.rs", production(include_str!("journal.rs"))),
            ("plan_derive.rs", production(include_str!("plan_derive.rs"))),
            (
                "reef_builtins.rs",
                production(include_str!("reef_builtins.rs")),
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
        let root = include_str!("plan_derive.rs");
        let production = root
            .split_once("\n#[cfg(test)]\nmod tests")
            .expect("planner keeps production and tests separated")
            .0;
        let commands = include_str!("plan_derive/commands.rs");
        let attribution = include_str!("plan_derive/attribution.rs");
        let statements = include_str!("plan_derive/statements.rs");

        for (name, source, ceiling) in [
            ("plan_derive.rs production", production, 320),
            ("commands.rs", commands, 350),
            ("attribution.rs", attribution, 140),
            ("statements.rs", statements, 100),
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
            bare.iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "some_external_tool_xyz")),
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
        std::fs::write(dir.path().join("module.shl"), "export let x = 1").unwrap();

        let has_write = |src: &str, path: &str| {
            effects_at(dir.path(), src).iter().any(
                |e| matches!(e, Effect::FsWrite { paths } if paths.contains(&dir.path().join(path))),
            )
        };
        let has_read = |src: &str, path: &str| {
            effects_at(dir.path(), src).iter().any(
                |e| matches!(e, Effect::FsRead { paths } if paths.contains(&dir.path().join(path))),
            )
        };
        let spawns = |src: &str, head: &str| {
            effects_at(dir.path(), src)
                .iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == head))
        };

        assert!(has_write("\"x\".save(\"p\")", "p"), "method save");
        assert!(has_write("echo hi > p", "p"), "redirect write");
        assert!(
            effects_at(dir.path(), "env.AUDIT_ONLY = \"y\"").contains(&Effect::EnvWrite {
                names: vec!["AUDIT_ONLY".into()]
            }),
            "persistent env assignment"
        );
        assert!(
            has_read("path(\"Cargo.toml\").read", "Cargo.toml"),
            "path read"
        );
        let module = effects_at(dir.path(), "use ./module");
        assert!(
            module.iter().any(
                |e| matches!(e, Effect::FsRead { paths } if paths.contains(&dir.path().join("module.shl")))
            ) && module.contains(&Effect::Opaque),
            "module read/body coverage: {module:?}"
        );
        assert!(has_write("spawn { \"x\".save(\"p\") }", "p"), "spawn body");
        assert!(
            has_write("parallel(() => \"x\".save(\"p\"))", "p"),
            "parallel body"
        );
        assert!(spawns("run(\"echo\", \"hi\")", "echo"), "run builtin");

        let open = effects_at(dir.path(), "open(\"Cargo.toml\")");
        assert!(
            open.iter().any(
                |e| matches!(e, Effect::FsRead { paths } if paths.contains(&dir.path().join("Cargo.toml")))
            ) && open.contains(&Effect::Opaque),
            "open read/handler coverage: {open:?}"
        );
        assert!(spawns("\"x\".feed(cat)", "cat"), "feed external spawn");

        // The original function-form save probe also stays pinned: the runtime
        // signature is save(path, value), so the first argument is the target.
        assert!(has_write("save(\"p\", \"x\")", "p"), "save builtin");
    }
}
