//! Tree-walk evaluator for shoal's canonical AST.

mod args;
mod builtins;
mod call;
mod coerce;
mod command;
mod expr;
mod helpers;
mod host;
mod pattern;
mod plan;
mod reef;
mod script;
mod stmt;

pub(crate) use coerce::coerce_word;
pub use reef::{PromptReefBinding, PromptReefSnapshot};

use shoal_adapters::{AdapterCatalog, AdapterClass, SubSpec};
use shoal_ast::*;
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
use shoal_leash::{Effect, Estimates, Plan, Reversibility};
use shoal_value::{
    CallArgs, CallCtx, ClosureVal, Env, ErrorVal, OutcomeVal, Record, VResult, Value,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Statement,
    Value,
}

/// A count/summary of the live task table, for the prompt's `jobs` segment
/// (docs/AGENT-SURFACE.md §12.1). Zero I/O: reads the in-memory task registry
/// only, never a subprocess or the filesystem.
///
/// `suspended` is always `0` today — the task registry has no suspended state
/// yet (only `Running`/`Done`); the field exists so this is additive, not a
/// breaking change, the day a suspend state lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobsSnapshot {
    pub running: usize,
    pub suspended: usize,
    pub total: usize,
}

/// Host renderer for statement-position outcomes (defect #1).
pub type StatementSink = Box<dyn FnMut(&Value) + Send>;

pub struct Evaluator {
    pub env: Env,
    cwd: PathBuf,
    process_env: Vec<(OsString, OsString)>,
    pub interactive: bool,
    pub it: Value,
    cancel: CancelToken,
    adapters: AdapterCatalog,
    /// Host renderer for statement-position outcomes (TDD §4.5, defect #1).
    sink: Option<StatementSink>,
    /// Runtime call-stack depth guard (defect #9).
    call_depth: usize,
    /// Nesting depth inside `fn` bodies — gates `cd`/env writes (defect #10).
    in_fn_body: usize,
    /// Live task registry backing the `jobs` builtin (defect #14).
    jobs: Vec<shoal_value::TaskVal>,
    /// reef (docs/REEF.md): cached scope chain, keyed on the cwd it was
    /// discovered for. Rebuilt only when the cwd changes (cd / `with cwd:`).
    /// `None` until the first spawn/`which`/`reef` touches it; cheap when no
    /// manifest is in scope (a pure filesystem walk with an empty result).
    reef_chain: Option<(PathBuf, shoal_reef::ScopeChain)>,
    /// reef: the provider stack, built lazily on the first *constrained*
    /// resolution — never touched on the hot path when no manifest is in scope.
    reef_resolver: Option<Arc<shoal_reef::Resolver>>,
    /// reef: the in-memory lock, loaded from (and persisted next to) the nearest
    /// manifest. Empty and inert when no manifest is in scope.
    reef_lock: shoal_reef::Lockfile,
    /// reef: filesystem path the current lock loads from / persists to.
    reef_lock_path: Option<PathBuf>,
    /// reef: optional user-scope `shoal.toml` whose `[reef]` table forms the
    /// user scope. `None` (the default) means no user scope — the zero-config,
    /// zero-regression path. Hosts wire a real path via
    /// [`Evaluator::set_reef_user_manifest`]; tests never point at real config.
    reef_user_manifest: Option<PathBuf>,
    /// reef: `with reef: {tool: constraint, …} { }` override layers (REEF.md
    /// §6), nearest-first (innermost `with reef:` block wins). Empty and inert
    /// when no `with reef:` is on the dynamic stack — zero-regression.
    reef_overrides: Vec<shoal_reef::ScopeEntry>,
}

enum Flow {
    Value(Value),
    Return(Value),
    Break,
    Continue,
}

#[derive(Clone)]
struct ExecMeta {
    ok_codes: Vec<i32>,
    class: AdapterClass,
    parse: String,
    output_type: Option<String>,
}

impl Evaluator {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            env: Env::root(),
            cwd,
            process_env: std::env::vars_os().collect(),
            interactive: false,
            it: Value::Null,
            cancel: CancelToken::new(),
            adapters: AdapterCatalog::empty(),
            sink: None,
            call_depth: 0,
            in_fn_body: 0,
            jobs: Vec::new(),
            reef_chain: None,
            reef_resolver: None,
            reef_lock: shoal_reef::Lockfile::new(),
            reef_lock_path: None,
            reef_user_manifest: None,
            reef_overrides: Vec::new(),
        }
    }

    /// Point the user reef scope at a `shoal.toml` whose `[reef]` table becomes
    /// the user scope (REEF §1). Additive: without it, there is no user scope,
    /// which is the zero-regression default. Changing the cwd next re-discovers
    /// the chain with this path folded in.
    pub fn set_reef_user_manifest(&mut self, path: impl Into<PathBuf>) {
        self.reef_user_manifest = Some(path.into());
        self.reef_chain = None;
    }

    /// Inject the reef provider stack (resolver). Additive: without it the
    /// evaluator lazily builds [`shoal_reef::Resolver::with_defaults`] on the
    /// first constrained resolution. Hosts use this to pin providers; tests use
    /// it to point the resolver at fixture-rooted binaries instead of the real
    /// system.
    pub fn set_reef_resolver(&mut self, resolver: Arc<shoal_reef::Resolver>) {
        self.reef_resolver = Some(resolver);
    }

    /// Install the host's statement renderer (defect #1). Every statement-position
    /// command outcome (and every non-final top-level value) is routed here.
    /// When unset, a built-in default prints to real stdout so scripts behave
    /// without host wiring.
    pub fn set_statement_sink(&mut self, f: StatementSink) {
        self.sink = Some(f);
    }

    /// Bind `it` and append to the session `out` transcript list (REPL hook).
    /// `Var("it")` / `Var("out")` then resolve from the environment normally.
    pub fn record_transcript(&mut self, v: &Value) {
        self.env.declare("it", v.clone(), true);
        let mut out = match self.env.get("out") {
            Some(Value::List(xs)) => xs,
            _ => Vec::new(),
        };
        out.push(v.clone());
        self.env.declare("out", Value::List(out), true);
    }

    /// Route a value to the statement sink (or the default stdout renderer).
    pub(crate) fn emit(&mut self, v: &Value) {
        if let Some(sink) = self.sink.as_mut() {
            sink(v);
        } else {
            helpers::default_render(v);
        }
    }

    /// Route a statement value to the sink, skipping nulls and skipping
    /// interactive *external* outcomes (already streamed via PtyTee, defect #1).
    /// Builtin outcomes carry `pid == 0` and are never PtyTee-streamed, so they
    /// must still be rendered by the sink even interactively (outcome
    /// unification, REEF-cycle P1): only a real spawned child (`pid != 0`) was
    /// tee'd to the terminal and should be suppressed here.
    pub(crate) fn sink_value(&mut self, v: &Value) {
        if *v == Value::Null {
            return;
        }
        if self.interactive
            && let Value::Outcome(o) = v
            && o.pid != 0
        {
            return;
        }
        self.emit(v);
    }

    /// A count/summary of the live task table for the prompt's `jobs` segment
    /// (docs/AGENT-SURFACE.md §12.1). Cheap and I/O-free: call it once per
    /// command when building a `PromptContext`, never per keystroke.
    pub fn jobs_snapshot(&self) -> JobsSnapshot {
        let total = self.jobs.len();
        let running = self.jobs.iter().filter(|t| !t.is_done()).count();
        JobsSnapshot {
            running,
            suspended: 0,
            total,
        }
    }

    /// The task table backing the `jobs` builtin (defect #14).
    pub(crate) fn jobs_table(&self) -> Value {
        let rows = self
            .jobs
            .iter()
            .map(|t| {
                let mut r = Record::new();
                r.insert("id".into(), Value::Int(t.id as i64));
                r.insert("desc".into(), Value::Str(t.shared.desc.clone()));
                r.insert("done".into(), Value::Bool(t.is_done()));
                r
            })
            .collect();
        Value::Table(rows)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn set_adapters(&mut self, adapters: AdapterCatalog) {
        self.adapters = adapters;
    }

    pub fn load_bundled_adapters(&mut self) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
        let (catalog, warnings) = AdapterCatalog::load_dir(&root);
        self.adapters = catalog;
        warnings
    }

    /// Cancel the currently executing foreground process tree.
    pub fn cancel_current(&self) {
        self.cancel.cancel();
    }

    pub fn cancellation_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Install a fresh cancellation epoch before reading the next command.
    pub fn reset_cancel(&mut self) {
        self.cancel = CancelToken::new();
    }
}

impl CallCtx for Evaluator {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
        self.call_value(
            f,
            CallArgs {
                pos: args,
                named: vec![],
            },
        )
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
}

pub fn eval(program: &Program, cwd: impl AsRef<Path>) -> VResult<Value> {
    Evaluator::new(cwd.as_ref().to_path_buf()).eval_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, std::env::current_dir().unwrap())
    }

    /// Evaluate `src` capturing everything routed to the statement sink.
    fn run_capturing(src: &str) -> (VResult<Value>, Vec<Value>) {
        use std::sync::{Arc, Mutex};
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink2 = sink.clone();
        ev.set_statement_sink(Box::new(move |v: &Value| {
            sink2.lock().unwrap().push(v.clone())
        }));
        let out = ev.eval_program(&program);
        drop(ev); // release the sink's Arc clone before unwrapping
        let captured = Arc::try_unwrap(sink).unwrap().into_inner().unwrap();
        (out, captured)
    }

    fn run_in(src: &str, cwd: &Path) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, cwd)
    }

    /// The structured `.out` of a captured command outcome.
    fn out_of(v: &Value) -> Value {
        match v {
            Value::Outcome(o) => o.out_value(),
            other => other.clone(),
        }
    }

    #[test]
    fn defect1_nonfinal_and_block_commands_reach_sink() {
        // Non-final top-level statement values pass through to the sink; the
        // final value is returned. Every command now yields an outcome whose
        // `.out` carries the joined echo text (outcome unification, P1a).
        let (out, captured) = run_capturing("echo hi\necho bye");
        assert_eq!(out_of(&out.unwrap()), Value::Str("bye".into()));
        assert_eq!(captured.len(), 1);
        assert_eq!(out_of(&captured[0]), Value::Str("hi".into()));

        // Every iteration of a loop body's bare command reaches the sink.
        let (_out, captured) = run_capturing("for x in [1,2,3] { echo (x) }");
        let texts: Vec<Value> = captured.iter().map(out_of).collect();
        assert_eq!(
            texts,
            vec![
                Value::Str("1".into()),
                Value::Str("2".into()),
                Value::Str("3".into()),
            ]
        );
    }

    #[test]
    fn outcome_unification_builtin_out_and_ok() {
        // A builtin is an outcome: `.out` is its structured result, `.ok` true.
        assert_eq!(run("(echo hi).out").unwrap(), Value::Str("hi".into()));
        assert_eq!(run("(echo hi).ok").unwrap(), Value::Bool(true));
        // Unknown fields forward to `.out` (stat record → `.size`).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"xyz").unwrap();
        assert_eq!(run_in("(stat a).size", dir.path()).unwrap(), Value::Size(3));
    }

    #[test]
    fn outcome_unification_and_or_compose_commands() {
        // `echo a && echo b` prints BOTH (P1d): `a` via the sink, `b` returned.
        let (out, captured) = run_capturing("echo a && echo b");
        assert_eq!(out_of(&out.unwrap()), Value::Str("b".into()));
        assert_eq!(
            captured.iter().map(out_of).collect::<Vec<_>>(),
            vec![Value::Str("a".into())]
        );
        // A three-stage chain prints every stage.
        let (out, captured) = run_capturing("echo a && echo b && echo c");
        assert_eq!(out_of(&out.unwrap()), Value::Str("c".into()));
        assert_eq!(
            captured.iter().map(out_of).collect::<Vec<_>>(),
            vec![Value::Str("a".into()), Value::Str("b".into())]
        );
        // `||` recovers from a failed command without raising.
        let out = run("sh { exit 1 } || echo x").unwrap();
        assert_eq!(out_of(&out), Value::Str("x".into()));
    }

    #[test]
    fn outcome_forwards_collection_methods() {
        // `ls` is an outcome; `.where`/`.sort`/`.first(n)`/`.map` forward to its
        // `.out` table (outcome unification P1b + first(n) arity fix).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big"), vec![0u8; 2048]).unwrap();
        std::fs::write(dir.path().join("small"), b"x").unwrap();
        let names = run_in("ls.where(.size > 1b).sort(.name).map(.name)", dir.path()).unwrap();
        assert_eq!(names, Value::List(vec![Value::Path("big".into())]));
        // `.first(2)` returns a LIST of two, chainable into `.map`.
        std::fs::write(dir.path().join("mid"), vec![0u8; 4]).unwrap();
        let first_two = run_in("ls.sort(.name).first(2).map(.name)", dir.path()).unwrap();
        assert!(matches!(first_two, Value::List(xs) if xs.len() == 2));
    }

    #[test]
    fn double_echo_fixed_and_bare_echo_blank_line() {
        // A fn whose last body statement is a bare command prints ONCE: the
        // trailing command is the block value, not also sunk (P1 dbl-echo).
        let (out, captured) = run_capturing("fn g(){ echo hi }\ng()");
        assert_eq!(out_of(&out.unwrap()), Value::Str("hi".into()));
        assert!(
            captured.is_empty(),
            "trailing command must not double-print: {captured:?}"
        );
        // Bare `echo` emits a blank line: its outcome stdout is "\n".
        let (_out, captured) = run_capturing("echo\n42");
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            Value::Outcome(o) => assert_eq!(&*o.stdout, b"\n"),
            other => panic!("expected outcome, got {other:?}"),
        }
    }

    #[test]
    fn top_level_ls_renders_as_table() {
        // An outcome with a structured `.out` renders as that structure (P1c).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only"), b"x").unwrap();
        let v = run_in("ls", dir.path()).unwrap();
        let rendered = shoal_value::render::render_block(&v, 80);
        assert!(
            rendered.contains("name"),
            "ls should render a table: {rendered:?}"
        );
        assert!(
            rendered.contains("only"),
            "ls table should list the file: {rendered:?}"
        );
    }

    #[test]
    fn defect3_forced_command_still_resolves_session_fn() {
        assert_eq!(
            run("fn greet(n:str){ (n) }\n^greet world").unwrap(),
            Value::Str("world".into())
        );
    }

    #[test]
    fn defect4_stat_modified_is_datetime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let v = run_in("stat a", dir.path()).unwrap();
        let Value::Record(r) = out_of(&v) else {
            panic!("stat should be a record")
        };
        assert!(
            matches!(r.get("modified"), Some(Value::DateTime(_))),
            "modified must be a DateTime, got {:?}",
            r.get("modified")
        );
    }

    #[test]
    fn defect5_command_resolves_in_value_position() {
        // `let r = ls` invokes the builtin zero-arg in value position.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let v = run_in("let r = ls\nr", dir.path()).unwrap();
        // `ls` now yields an outcome; its `.out` is the table (P1a).
        assert!(matches!(out_of(&v), Value::Table(rows) if rows.len() == 1));
    }

    #[test]
    fn defect5_env_field_read_via_command() {
        // `env.PATH` reads by invoking the `env` builtin then projecting.
        unsafe { std::env::set_var("SHOAL_TEST_VAR", "hello") };
        let v = run("env.SHOAL_TEST_VAR").unwrap();
        assert_eq!(v, Value::Str("hello".into()));
    }

    #[test]
    fn defect8_redirect_applies_to_builtin() {
        let dir = tempfile::tempdir().unwrap();
        run_in("echo hi > b.txt", dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join("b.txt")).unwrap();
        assert_eq!(body, "hi\n");
    }

    #[test]
    fn defect9_recursion_guard_returns_error() {
        // Run on a large stack so the depth guard fires before the native stack
        // overflows (the real binary evaluates on a big main-thread stack).
        let code = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(|| run("fn rec(n:int){ rec(n) }\nrec(1)").unwrap_err().code)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(code, "recursion_limit");
    }

    #[test]
    fn defect10_cd_inside_fn_body_is_rejected() {
        let err = run("fn f(){ cd / }\nf()").unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(err.msg.contains("with cwd:"), "{}", err.msg);
    }

    #[test]
    fn defect11_env_assignment_writes_session_env() {
        use shoal_ast::*;
        let s = Span::default();
        let target = Expr::Field {
            recv: Box::new(Expr::Var {
                name: "env".into(),
                span: s,
            }),
            name: "SHOAL_ASSIGNED".into(),
            optional: false,
            span: s,
        };
        let program = Program {
            stmts: vec![
                Stmt::Assign {
                    target,
                    op: AssignOp::Set,
                    value: Expr::Str {
                        value: "bar".into(),
                        span: s,
                    },
                    span: s,
                },
                Stmt::Expr {
                    expr: Expr::Field {
                        recv: Box::new(Expr::Var {
                            name: "env".into(),
                            span: s,
                        }),
                        name: "SHOAL_ASSIGNED".into(),
                        optional: false,
                        span: s,
                    },
                    span: s,
                },
            ],
        };
        let v = eval(&program, std::env::current_dir().unwrap()).unwrap();
        assert_eq!(v, Value::Str("bar".into()));
    }

    #[test]
    fn defect11_env_assignment_rejected_in_fn_body() {
        use shoal_ast::*;
        let s = Span::default();
        let assign = Stmt::Assign {
            target: Expr::Field {
                recv: Box::new(Expr::Var {
                    name: "env".into(),
                    span: s,
                }),
                name: "X".into(),
                optional: false,
                span: s,
            },
            op: AssignOp::Set,
            value: Expr::Str {
                value: "1".into(),
                span: s,
            },
            span: s,
        };
        // fn f() { env.X = "1" }  then  f()
        let decl = FnDecl {
            name: "f".into(),
            params: vec![],
            rest: None,
            ret: None,
            body: Block {
                stmts: vec![assign],
                span: s,
            },
            doc: None,
            exported: false,
            span: s,
        };
        let program = Program {
            stmts: vec![
                Stmt::Fn { decl },
                Stmt::Expr {
                    expr: Expr::FnCall {
                        name: "f".into(),
                        args: Args::empty(),
                        span: s,
                    },
                    span: s,
                },
            ],
        };
        let err = eval(&program, std::env::current_dir().unwrap()).unwrap_err();
        assert!(err.msg.contains("with env:"), "{}", err.msg);
    }

    #[test]
    fn defect12_builtin_word_coercion() {
        // `sleep 0ms` binds the word to a duration; `sleep 0` to seconds. The
        // builtin now yields an outcome whose `.out` is null (P1a).
        assert_eq!(out_of(&run("sleep 0ms").unwrap()), Value::Null);
        assert_eq!(out_of(&run("sleep 0").unwrap()), Value::Null);
    }

    #[test]
    fn defect12_fn_param_word_coercion() {
        // A bare CMD word binds to a typed fn param.
        let v = run("fn add1(n: int) { n + 1 }\nadd1 41").unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn defect12_help_synthesis_returns_null() {
        let (out, captured) = run_capturing("fn deploy(env: str) { (env) }\ndeploy --help");
        assert_eq!(out.unwrap(), Value::Null);
        assert!(
            matches!(captured.last(), Some(Value::Str(s)) if s.contains("deploy") && s.contains("env")),
            "{captured:?}"
        );
    }

    #[test]
    fn defect14_task_methods_and_jobs() {
        assert_eq!(
            run("let t = spawn { 2 + 3 }\nt.await()").unwrap(),
            Value::Int(5)
        );
        let is_done = run("let t = spawn { 7 }\nt.await()\nt.is_done()").unwrap();
        assert_eq!(is_done, Value::Bool(true));
        // `jobs` returns the registry table.
        let jobs = run("spawn { 1 }\njobs").unwrap();
        assert!(matches!(jobs, Value::Table(rows) if !rows.is_empty()));
    }

    #[test]
    fn jobs_snapshot_counts_running_and_total() {
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        // Nothing spawned yet: a sane, zero-I/O empty snapshot.
        let empty = ev.jobs_snapshot();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.running, 0);
        assert_eq!(empty.suspended, 0);

        // Awaiting every spawned task deterministically drives them to done,
        // so the post-await snapshot is a stable total/zero-running count.
        let prog = shoal_syntax::parse(
            "let a = spawn { 1 + 1 }\nlet b = spawn { 2 + 2 }\na.await()\nb.await()",
        )
        .unwrap();
        ev.eval_program(&prog).unwrap();
        let snap = ev.jobs_snapshot();
        assert_eq!(snap.total, 2, "both spawned tasks are registered");
        assert_eq!(snap.running, 0, "both were awaited to completion");
    }

    #[test]
    fn echo_renders_non_scalar_values() {
        let v = run("let items = [1,2,3]\necho (items)").unwrap();
        assert_eq!(out_of(&v), Value::Str("[1, 2, 3]".into()));
    }

    #[test]
    fn record_transcript_binds_it_and_out() {
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        ev.record_transcript(&Value::Int(7));
        ev.record_transcript(&Value::Str("hi".into()));
        let it = ev
            .eval_program(&shoal_syntax::parse("it").unwrap())
            .unwrap();
        assert_eq!(it, Value::Str("hi".into()));
        let out = ev
            .eval_program(&shoal_syntax::parse("out").unwrap())
            .unwrap();
        assert_eq!(
            out,
            Value::List(vec![Value::Int(7), Value::Str("hi".into())])
        );
    }

    #[test]
    fn builtin_retry_and_parallel_and_save() {
        assert_eq!(run("retry(3, () => 42)").unwrap(), Value::Int(42));
        assert_eq!(
            run("parallel(() => 1, () => 2)").unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
        let dir = tempfile::tempdir().unwrap();
        run_in("save(\"out.txt\", \"payload\")", dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "payload"
        );
    }

    #[test]
    fn builtin_retry_eventually_surfaces_error() {
        let err = run("retry(2, () => missing_command_xyz)").unwrap_err();
        assert!(
            err.code == "undefined_var" || err.code == "not_found",
            "{}",
            err.code
        );
    }

    #[test]
    fn arithmetic_and_binding() {
        assert_eq!(run("let x = 2 + 3\nx * 4").unwrap(), Value::Int(20));
    }

    #[test]
    fn strict_conditions_and_short_circuit() {
        assert_eq!(
            run("false && missing\ntrue || missing").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(run("if true { 7 } else { 9 }").unwrap(), Value::Int(7));
        assert_eq!(run("if [1] { 2 }").unwrap_err().code, "type_error");
    }

    #[test]
    fn functions_are_callable() {
        assert_eq!(
            run("fn twice(x: int) { x * 2 }\ntwice(21)").unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn captured_external_outcome_is_structured() {
        let value = run("let r = sh { printf hello }\nr.out").unwrap();
        assert_eq!(value, Value::Str("hello".into()));
    }

    #[test]
    fn failed_statement_preserves_process_diagnostics() {
        let err = run("sh { printf boom >&2; exit 7 }").unwrap_err();
        assert_eq!(err.code, "cmd_failed");
        assert_eq!(err.status, Some(7));
        assert_eq!(err.stderr.as_deref(), Some("boom"));
    }

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
            paths: vec![dir.path().join("a")]
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

    #[test]
    fn planning_unions_conditional_and_static_function_effects() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn cleanup() { rm old }\nif true { cleanup() } else { touch new }";
        let mut evaluator = Evaluator::new(dir.path().into());
        let parsed = shoal_syntax::parse(src).unwrap();
        let plan = evaluator.plan_program(&parsed).unwrap();
        assert!(plan.effects.contains(&Effect::FsDelete {
            paths: vec![dir.path().join("old")]
        }));
        assert!(plan.effects.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("new")]
        }));
    }

    // --- match: type / record / list patterns (TDD §3.2) -----------------

    #[test]
    fn match_type_pattern_binds_and_falls_through() {
        assert_eq!(
            run(r#"match 5 { int n => "int:{n}"; _ => "other" }"#).unwrap(),
            Value::Str("int:5".into())
        );
        assert_eq!(
            run(r#"match "hi" { str s => "str:{s}"; _ => "other" }"#).unwrap(),
            Value::Str("str:hi".into())
        );
        // A type mismatch falls through to the next arm.
        assert_eq!(
            run(r#"match "hi" { int n => "int:{n}"; str s => "str:{s}" }"#).unwrap(),
            Value::Str("str:hi".into())
        );
        // A bare type name with no binder is a plain bind (matches anything).
        assert_eq!(
            run(r#"match 5 { int => 1; _ => 0 }"#).unwrap(),
            Value::Int(1)
        );
    }

    #[test]
    fn match_record_pattern_shorthand_sub_and_open() {
        assert_eq!(
            run(r#"match {name: "ada", age: 30} { {name, age} => "{name} is {age}"; _ => "no" }"#)
                .unwrap(),
            Value::Str("ada is 30".into())
        );
        // Nested record sub-pattern.
        assert_eq!(
            run("match {point: {x: 1, y: 2}} { {point: {x, y}} => x + y; _ => 0 }").unwrap(),
            Value::Int(3)
        );
        // Missing field falls through (open matching only ignores *extra*).
        assert_eq!(
            run(r#"match {name: "ada"} { {name, age} => "has age"; _ => "no age" }"#).unwrap(),
            Value::Str("no age".into())
        );
        // Record + nested list sub-pattern.
        assert_eq!(
            run("match {items: [1, 2, 3]} { {items: [a, b, c]} => a + b + c; _ => 0 }").unwrap(),
            Value::Int(6)
        );
    }

    #[test]
    fn match_record_pattern_guard_composes() {
        assert_eq!(
            run(r#"match {status: 200} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("ok".into())
        );
        assert_eq!(
            run(r#"match {status: 404} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("other:404".into())
        );
    }

    #[test]
    fn match_list_pattern_arity_rest_and_empty() {
        assert_eq!(
            run("match [1, 2, 3] { [a, b, c] => a + b + c; _ => 0 }").unwrap(),
            Value::Int(6)
        );
        // `...rest` binds the tail as a list.
        assert_eq!(
            run("match [1, 2, 3, 4] { [first, ...rest] => rest.len(); _ => 0 }").unwrap(),
            Value::Int(3)
        );
        // Fixed arity: a length mismatch falls through.
        assert_eq!(
            run(r#"match [1, 2] { [a, b, c] => "three"; [a, b] => "two"; _ => "other" }"#).unwrap(),
            Value::Str("two".into())
        );
        assert_eq!(
            run(r#"match [] { [] => "empty"; _ => "nonempty" }"#).unwrap(),
            Value::Str("empty".into())
        );
        assert_eq!(
            run(r#"match [1] { [] => "empty"; [a] => "one:{a}"; _ => "other" }"#).unwrap(),
            Value::Str("one:1".into())
        );
    }

    #[test]
    fn match_comma_separated_arms_parse() {
        assert_eq!(
            run(r#"match 2 { 1 => "a", 2 => "b", _ => "c" }"#).unwrap(),
            Value::Str("b".into())
        );
    }
}
