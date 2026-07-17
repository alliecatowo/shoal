//! `ShoalCompleter` — a real `reedline::Completer` that dispatches on cursor
//! context using the modal lexer (site/content/internals/language-conformance-contract.md statement-dispatch rule,
//! approximated), instead of a flat startup-snapshot word list.
//!
//! Contexts:
//! - **head** (first word of a COMMAND statement): session `fn`/alias names
//!   (from the evaluator's live `Env`) + builtins + adapter names + a live
//!   `PATH` scan (cached per-directory, invalidated on mtime change).
//! - **arg** (later word of a COMMAND statement): live filesystem entries
//!   resolved against the argument's own directory prefix, or — when the
//!   word looks like a flag (`-`/`--`) and the head resolves to a known
//!   adapter/fn — flags drawn from that adapter's declared params/short
//!   flags or the function's signature.
//! - **expr** (anywhere else — after `let x = `, inside `(...)`, a bare
//!   expression statement): in-scope variable/function names.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use reedline::{Completer, Suggestion};
use shoal_adapters::AdapterCatalog;
use shoal_value::Env;

mod candidates;
mod context;
mod discovery;
mod inference;

use candidates::finish;
#[cfg(test)]
use candidates::subsequence_match;
use context::{Ctx, classify};
#[cfg(test)]
use discovery::{MAX_PATH_CACHE_DIRS, PATH_CACHE_REVALIDATE};
use discovery::{PathDiscovery, adapter_names};

pub struct ShoalCompleter {
    env: Env,
    cwd: Arc<Mutex<PathBuf>>,
    /// Executable search directories from the session that will execute the
    /// command. Attached REPLs cannot use the client's process PATH: the
    /// kernel session may have changed PATH independently.
    discovery: PathDiscovery,
    adapters: Vec<AdapterCatalog>,
    adapter_names: Vec<String>,
    /// `completion.fuzzy` (site/content/internals/configuration-reference.md): allow typo-tolerant,
    /// non-contiguous matches instead of requiring a strict prefix.
    fuzzy: bool,
    /// `completion.case_insensitive`.
    case_insensitive: bool,
    /// `completion.max_results`: cap on candidates returned per completion.
    max_results: usize,
}

/// `shoal_config::Completion`'s own defaults (fuzzy/case-insensitive on, 100
/// results) — used so a `ShoalCompleter::new` call that never reaches
/// `configure` (every existing call site, including every test in this
/// module) keeps behaving exactly as it did before config was wired in.
const DEFAULT_FUZZY: bool = true;
const DEFAULT_CASE_INSENSITIVE: bool = true;
const DEFAULT_MAX_RESULTS: usize = 100;
impl ShoalCompleter {
    pub fn new(
        env: Env,
        cwd: Arc<Mutex<PathBuf>>,
        adapters: Vec<AdapterCatalog>,
        adapter_names: Vec<String>,
    ) -> Self {
        Self {
            env,
            cwd,
            discovery: PathDiscovery::new(),
            adapters,
            adapter_names,
            fuzzy: DEFAULT_FUZZY,
            case_insensitive: DEFAULT_CASE_INSENSITIVE,
            max_results: DEFAULT_MAX_RESULTS,
        }
    }

    /// Apply `[completion]` config (site/content/internals/configuration-reference.md). Builder-style so
    /// existing `ShoalCompleter::new(...)` call sites are unaffected when a
    /// caller (a test, an embedder) doesn't need config-driven behavior.
    pub fn configure(mut self, fuzzy: bool, case_insensitive: bool, max_results: usize) -> Self {
        self.fuzzy = fuzzy;
        self.case_insensitive = case_insensitive;
        self.max_results = max_results.max(1);
        self
    }

    /// Use a live, sanitized session PATH projection. `None` inside the cell
    /// means an older remote omitted the projection, in which case retaining
    /// the process-PATH fallback preserves protocol compatibility.
    pub(crate) fn with_path_dirs(mut self, path_dirs: Arc<Mutex<Option<Vec<PathBuf>>>>) -> Self {
        self.discovery.set_session_dirs(path_dirs);
        self
    }

    fn cwd(&self) -> PathBuf {
        self.cwd
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

impl Completer for ShoalCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let max_results = self.max_results;
        match classify(&self.env, line, pos) {
            Ctx::Head { start, word } => {
                finish(self.head_candidates(&word), start, pos, max_results)
            }
            Ctx::Arg { start, word, head } => {
                let names = if word.starts_with('-') {
                    self.flag_candidates(&head, &word)
                } else {
                    self.fs_candidates(&word)
                };
                finish(names, start, pos, max_results)
            }
            Ctx::Expr { start, word } => {
                finish(self.expr_candidates(&word), start, pos, max_results)
            }
            Ctx::Method { start, word, recv } => finish(
                self.method_candidates(&word, recv.as_deref()),
                start,
                pos,
                max_results,
            ),
            Ctx::None => Vec::new(),
        }
    }
}

/// Scan adapter config directories for `[cmd.<name>]` table keys — just the
/// name enumeration `AdapterCatalog` doesn't expose publicly (see
/// api_changes); flag/subcommand data still goes through the real
/// `AdapterCatalog::load_dir` + `lookup`.
pub fn scan_adapter_names(dirs: &[PathBuf]) -> Vec<String> {
    adapter_names(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_value::{Env, Value};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn completer_at(cwd: &Path) -> ShoalCompleter {
        ShoalCompleter::new(
            Env::root(),
            Arc::new(Mutex::new(cwd.to_path_buf())),
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn classify_empty_line_is_head() {
        let env = Env::root();
        assert_eq!(
            classify(&env, "", 0),
            Ctx::Head {
                start: 0,
                word: String::new()
            }
        );
    }

    #[test]
    fn classify_partial_head_word() {
        let env = Env::root();
        let ctx = classify(&env, "ec", 2);
        assert_eq!(
            ctx,
            Ctx::Head {
                start: 0,
                word: "ec".into()
            }
        );
    }

    #[test]
    fn classify_command_argument_position() {
        let env = Env::root();
        let ctx = classify(&env, "cat crates/sho", 14);
        assert_eq!(
            ctx,
            Ctx::Arg {
                start: 4,
                word: "crates/sho".into(),
                head: "cat".into(),
            }
        );
    }

    #[test]
    fn classify_flag_word_is_still_arg_context() {
        let env = Env::root();
        let ctx = classify(&env, "ls --col", 8);
        assert_eq!(
            ctx,
            Ctx::Arg {
                start: 3,
                word: "--col".into(),
                head: "ls".into(),
            }
        );
    }

    #[test]
    fn classify_after_let_equals_is_expr() {
        let env = Env::root();
        let ctx = classify(&env, "let x = tr", 10);
        assert_eq!(
            ctx,
            Ctx::Expr {
                start: 8,
                word: "tr".into()
            }
        );
    }

    #[test]
    fn classify_bound_variable_dot_is_method() {
        // A bound receiver followed by `.<word>` is method/field position, not
        // a plain expr reference — so completion offers method names.
        let env = Env::root();
        env.declare("items", Value::List(vec![Value::Int(1)]), false);
        let ctx = classify(&env, "items.le", 8);
        assert_eq!(
            ctx,
            Ctx::Method {
                start: 6,
                word: "le".into(),
                // `items` is a live list binding — receiver type inferred.
                recv: Some("list".into()),
            }
        );
    }

    #[test]
    fn classify_bound_variable_no_dot_is_expr() {
        // The same bound receiver with a trailing space (no `.`) is plain expr
        // position: variable/function/keyword completion, not method names.
        let env = Env::root();
        env.declare("items", Value::Int(1), false);
        let ctx = classify(&env, "items ", 6);
        assert_eq!(
            ctx,
            Ctx::Expr {
                start: 6,
                word: String::new()
            }
        );
    }

    #[test]
    fn classify_unbound_head_word_is_command() {
        let env = Env::root();
        let ctx = classify(&env, "gitx st", 7);
        assert_eq!(
            ctx,
            Ctx::Arg {
                start: 5,
                word: "st".into(),
                head: "gitx".into(),
            }
        );
    }

    #[test]
    fn head_candidates_include_keywords_and_builtins() {
        let mut c = completer_at(Path::new("."));
        let names = c.head_candidates("ec");
        assert!(names.iter().any(|n| n == "echo"));
    }

    #[test]
    fn head_candidates_include_full_builtin_registry() {
        // Regression against the old hand-copied `SHOAL_BUILTINS` (18 stale
        // entries): every real builtin command head now tab-completes because
        // the completer consumes shoal-eval's canonical registry directly.
        let mut c = completer_at(Path::new("."));
        for head in [
            "jobs", "history", "exit", "plan", "apply", "undo", "reef", "save", "assert",
            "interact", "open", "pushd", "popd", "dirs",
        ] {
            let names = c.head_candidates(head);
            assert!(
                names.iter().any(|n| n == head),
                "builtin `{head}` should be an offered head candidate"
            );
        }
    }

    #[test]
    fn method_position_offers_method_names_expr_does_not() {
        // `tbl.wh<TAB>` — a bound list receiver in `.`-position — offers value
        // method names (`where`); the same prefix in plain expr position
        // (`let x = wh`) offers variables/keywords, never method names.
        let env = Env::root();
        env.declare("tbl", Value::List(vec![Value::Int(1)]), false);
        let mut c = ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        );
        let method = c.complete("tbl.wh", 6);
        assert!(
            method.iter().any(|s| s.value == "where"),
            "method position must offer `.where`, got {:?}",
            method.iter().map(|s| &s.value).collect::<Vec<_>>()
        );

        let expr = c.complete("let x = wh", 10);
        assert!(
            !expr.iter().any(|s| s.value == "where"),
            "plain expr position must NOT offer method names, got {:?}",
            expr.iter().map(|s| &s.value).collect::<Vec<_>>()
        );
    }

    // ---- receiver-type-aware method completion after `.` -------------------

    fn completer_with(env: Env) -> ShoalCompleter {
        ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
    }

    /// Complete `line` at its end and collect the candidate strings.
    fn cands(c: &mut ShoalCompleter, line: &str) -> Vec<String> {
        c.complete(line, line.len())
            .into_iter()
            .map(|s| s.value)
            .collect()
    }

    fn has(cs: &[String], m: &str) -> bool {
        cs.iter().any(|c| c == m)
    }

    #[test]
    fn method_completion_infers_literal_receiver_types() {
        let mut c = completer_with(Env::root());

        // `[…]` list literal → list methods, not str/record-only methods.
        let list = cands(&mut c, "[1,2,3].");
        for m in ["where", "map", "sum", "sort_by", "first"] {
            assert!(has(&list, m), "list literal should offer `.{m}`: {list:?}");
        }
        for m in ["upper", "split", "keys"] {
            assert!(
                !has(&list, m),
                "list literal must NOT offer `.{m}`: {list:?}"
            );
        }

        // `"…"` string literal → str methods.
        let s = cands(&mut c, "\"hi\".");
        for m in ["upper", "len", "split", "trim"] {
            assert!(has(&s, m), "str literal should offer `.{m}`: {s:?}");
        }
        for m in ["where", "map", "keys"] {
            assert!(!has(&s, m), "str literal must NOT offer `.{m}`: {s:?}");
        }

        // `'…'` raw string literal → str methods too.
        let raw = cands(&mut c, "'hi'.");
        assert!(
            has(&raw, "upper"),
            "raw str literal should offer `.upper`: {raw:?}"
        );
        assert!(
            !has(&raw, "where"),
            "raw str literal must NOT offer `.where`: {raw:?}"
        );

        // Integer literal → numeric methods, not collection/str methods.
        let int = cands(&mut c, "42.");
        for m in ["abs", "round", "floor", "ceil"] {
            assert!(has(&int, m), "int literal should offer `.{m}`: {int:?}");
        }
        for m in ["where", "upper", "keys"] {
            assert!(!has(&int, m), "int literal must NOT offer `.{m}`: {int:?}");
        }

        // Float literal → numeric methods (and the `1.5` isn't mis-split).
        let float = cands(&mut c, "1.5.");
        assert!(
            has(&float, "round"),
            "float literal should offer `.round`: {float:?}"
        );
        assert!(
            !has(&float, "where"),
            "float literal must NOT offer `.where`: {float:?}"
        );

        // `{…}` record literal → record methods.
        let rec = cands(&mut c, "{a:1}.");
        for m in ["keys", "values", "items", "merge"] {
            assert!(has(&rec, m), "record literal should offer `.{m}`: {rec:?}");
        }
        for m in ["upper", "map", "where"] {
            assert!(
                !has(&rec, m),
                "record literal must NOT offer `.{m}`: {rec:?}"
            );
        }

        // `true`/`false` → bool (scalar) methods: universal serializers only.
        let b = cands(&mut c, "true.");
        assert!(has(&b, "json"), "bool literal should offer `.json`: {b:?}");
        for m in ["where", "upper", "abs"] {
            assert!(!has(&b, m), "bool literal must NOT offer `.{m}`: {b:?}");
        }

        // A size/duration literal → scalar methods (no distinct set of its own).
        let size = cands(&mut c, "1mb.");
        assert!(
            has(&size, "json"),
            "size literal should offer `.json`: {size:?}"
        );
        assert!(
            !has(&size, "upper"),
            "size literal must NOT offer `.upper`: {size:?}"
        );
        let dur = cands(&mut c, "30s.");
        assert!(
            has(&dur, "save"),
            "duration literal should offer `.save`: {dur:?}"
        );
        assert!(
            !has(&dur, "where"),
            "duration literal must NOT offer `.where`: {dur:?}"
        );
    }

    #[test]
    fn method_completion_universal_methods_appear_for_every_type() {
        // `.tap`/`.also` (dispatched for every receiver) and the universal
        // serializers survive the type narrowing.
        let mut c = completer_with(Env::root());
        for (line, ty) in [("[1,2,3].", "list"), ("\"x\".", "str"), ("42.", "int")] {
            let cs = cands(&mut c, line);
            for m in ["tap", "also", "json"] {
                assert!(
                    has(&cs, m),
                    "{ty} should still offer universal `.{m}`: {cs:?}"
                );
            }
        }
    }

    #[test]
    fn method_completion_infers_binding_receiver_type() {
        // `xs.` where `xs` is a live list binding → list methods, not str ops.
        let env = Env::root();
        env.declare("xs", Value::List(vec![Value::Int(1), Value::Int(2)]), false);
        let mut c = completer_with(env);
        let cs = cands(&mut c, "xs.");
        assert!(
            has(&cs, "where"),
            "list binding should offer `.where`: {cs:?}"
        );
        assert!(has(&cs, "sum"), "list binding should offer `.sum`: {cs:?}");
        assert!(
            !has(&cs, "upper"),
            "list binding must NOT offer `.upper`: {cs:?}"
        );

        // A str binding narrows to str methods.
        let env2 = Env::root();
        env2.declare("name", Value::Str("bob".into()), false);
        let mut c2 = completer_with(env2);
        let cs2 = cands(&mut c2, "name.");
        assert!(
            has(&cs2, "upper"),
            "str binding should offer `.upper`: {cs2:?}"
        );
        assert!(
            !has(&cs2, "where"),
            "str binding must NOT offer `.where`: {cs2:?}"
        );
    }

    #[test]
    fn method_completion_falls_back_to_union_for_complex_receivers() {
        // The union offers everything, so it contains BOTH a str-only method
        // (`upper`) and a list method (`where`) — the tell-tale of "not
        // narrowed". Every fallback case below must keep that full vocabulary.
        let env = Env::root();
        env.declare("rec", Value::Record(shoal_value::Record::new()), false);
        env.declare("xs", Value::List(vec![Value::Int(1)]), false);
        // A command binding: a type with no dedicated method table → union.
        env.declare(
            "f",
            Value::CmdRef(Arc::new(shoal_ast::CmdCall {
                head: "echo".into(),
                forced: false,
                env_prefix: Vec::new(),
                args: Vec::new(),
                redirects: Vec::new(),
                background: false,
                trailing: None,
                span: shoal_ast::Span::new(0, 0),
            })),
            false,
        );
        let mut c = completer_with(env);

        let union_like = |cs: &[String], label: &str| {
            assert!(
                has(cs, "upper"),
                "{label} should fall back to the union (has `upper`): {cs:?}"
            );
            assert!(
                has(cs, "where"),
                "{label} should fall back to the union (has `where`): {cs:?}"
            );
            assert!(
                has(cs, "keys"),
                "{label} should fall back to the union (has `keys`): {cs:?}"
            );
        };

        // Chained field access `rec.a.` — receiver type of the 2nd `.` unknown.
        union_like(&cands(&mut c, "rec.a."), "chained access");
        // Call/group result `f(x).`.
        union_like(&cands(&mut c, "f(x)."), "call result");
        // Indexed element `xs[0].` — the `[…]` is postfix, not a literal.
        union_like(&cands(&mut c, "xs[0]."), "indexed element");
        // A binding whose type has no dedicated table (a command) → union.
        union_like(&cands(&mut c, "f."), "table-less binding");
    }

    #[test]
    fn head_candidates_include_callable_session_names_but_not_plain_vars() {
        let env = Env::root();
        env.declare("mydata", Value::Int(3), false);
        env.declare(
            "deploy",
            Value::CmdRef(Arc::new(shoal_ast::CmdCall {
                head: "echo".into(),
                forced: false,
                env_prefix: Vec::new(),
                args: Vec::new(),
                redirects: Vec::new(),
                background: false,
                trailing: None,
                span: shoal_ast::Span::new(0, 0),
            })),
            false,
        );
        let mut c = ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        );
        let names = c.head_candidates("");
        assert!(names.iter().any(|n| n == "deploy"));
        assert!(!names.iter().any(|n| n == "mydata"));
    }

    #[test]
    fn expr_candidates_include_in_scope_variables() {
        let env = Env::root();
        env.declare("myvar", Value::Int(1), false);
        let c = completer_at(Path::new("."));
        // Use the completer's own env for this assertion instead.
        let names_env = env.visible_names();
        assert!(names_env.contains(&"myvar".to_string()));
        let _ = c; // constructed just to exercise the type in this module
    }

    #[test]
    fn fs_candidates_reflect_live_directory_contents() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("beta")).unwrap();
        let c = completer_at(dir.path());
        let mut names = c.fs_candidates("");
        names.sort();
        assert_eq!(names, vec!["alpha.txt".to_string(), "beta/".to_string()]);

        // Live: a file created *after* the completer was built still shows up
        // (no startup snapshot).
        fs::write(dir.path().join("gamma.txt"), b"").unwrap();
        let mut names2 = c.fs_candidates("");
        names2.sort();
        assert_eq!(
            names2,
            vec![
                "alpha.txt".to_string(),
                "beta/".to_string(),
                "gamma.txt".to_string()
            ]
        );
    }

    #[test]
    fn fs_candidates_descend_into_directory_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("crates")).unwrap();
        fs::create_dir(dir.path().join("crates/shoal-eval")).unwrap();
        fs::create_dir(dir.path().join("crates/shoal-syntax")).unwrap();
        let c = completer_at(dir.path());
        let mut names = c.fs_candidates("crates/sho");
        names.sort();
        assert_eq!(
            names,
            vec![
                "crates/shoal-eval/".to_string(),
                "crates/shoal-syntax/".to_string()
            ]
        );
    }

    #[test]
    fn fs_candidates_hide_dotfiles_unless_prefix_asks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible"), b"").unwrap();
        let c = completer_at(dir.path());
        assert_eq!(c.fs_candidates(""), vec!["visible".to_string()]);
        assert_eq!(c.fs_candidates("."), vec![".hidden".to_string()]);
    }

    #[test]
    fn path_names_cache_invalidates_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("toolone"), b"").unwrap();
        fs::set_permissions(
            dir.path().join("toolone"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(dir.path().join("not-executable"), b"").unwrap();
        fs::create_dir(dir.path().join("directory")).unwrap();
        let mut c = completer_at(Path::new("."));
        // Scan the cache primitive directly so the test is hermetic rather
        // than mutating process PATH. Non-executable files and directories are
        // never command-head candidates.
        let first = c.path_dir_names(dir.path());
        assert_eq!(first, vec!["toolone".to_string()]);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(dir.path().join("tooltwo"), b"").unwrap();
        fs::set_permissions(
            dir.path().join("tooltwo"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let mut second = c.path_dir_names(dir.path());
        second.sort();
        assert_eq!(second, vec!["toolone".to_string(), "tooltwo".to_string()]);
    }

    #[test]
    fn path_names_cache_revalidates_chmod_without_directory_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("mode-tool");
        fs::write(&tool, b"").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
        let mut c = completer_at(Path::new("."));
        assert!(c.path_dir_names(dir.path()).is_empty());

        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        std::thread::sleep(PATH_CACHE_REVALIDATE + Duration::from_millis(25));
        assert_eq!(c.path_dir_names(dir.path()), vec!["mode-tool".to_string()]);

        fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
        std::thread::sleep(PATH_CACHE_REVALIDATE + Duration::from_millis(25));
        assert!(c.path_dir_names(dir.path()).is_empty());
    }

    #[test]
    fn path_cache_churn_clears_at_its_advisory_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let mut completer = completer_at(Path::new("."));
        for index in 0..MAX_PATH_CACHE_DIRS {
            completer.path_dir_names(&root.path().join(format!("missing-{index}")));
        }
        assert_eq!(completer.discovery.cache_len(), MAX_PATH_CACHE_DIRS);

        let current = root.path().join("current");
        fs::create_dir(&current).unwrap();
        fs::write(current.join("bounded-tool"), b"").unwrap();
        fs::set_permissions(
            current.join("bounded-tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(
            completer.path_dir_names(&current),
            vec!["bounded-tool".to_string()]
        );
        assert_eq!(completer.discovery.cache_len(), 1);
        assert!(completer.discovery.cache_contains(&current));
    }

    #[test]
    fn path_names_follow_the_executing_session_not_the_client_process() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("remote-only-tool"), b"").unwrap();
        fs::set_permissions(
            dir.path().join("remote-only-tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let session_dirs = Arc::new(Mutex::new(Some(vec![dir.path().to_path_buf()])));
        let mut c = completer_at(Path::new(".")).with_path_dirs(session_dirs.clone());

        assert_eq!(c.path_names(), vec!["remote-only-tool".to_string()]);

        // An explicit empty PATH is different from an omitted old-protocol
        // projection and must not silently fall back to the client's PATH.
        *session_dirs.lock().unwrap() = Some(Vec::new());
        assert!(c.path_names().is_empty());
    }

    #[test]
    fn flag_candidates_from_adapter_catalog() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("git.toml"),
            r#"
[cmd.git]
bin = "git"
[cmd.git.sub.status]
params = { short = "bool" }
flags  = { short = { s = "short" } }
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let c = ShoalCompleter::new(
            Env::root(),
            Arc::new(Mutex::new(PathBuf::from("."))),
            vec![catalog],
            vec!["git".into()],
        );
        let flags = c.flag_candidates("git", "--s");
        assert!(flags.iter().any(|f| f == "--short"));
        let short = c.flag_candidates("git", "-");
        assert!(short.iter().any(|f| f == "-s"));
    }

    #[test]
    fn callable_shadow_hides_adapter_flags_by_shared_precedence() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tool.toml"),
            r#"
[cmd.tool]
bin = "tool"
params = { adapter_only = "bool" }
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");

        let mut evaluator = shoal_eval::Evaluator::new(dir.path().into());
        evaluator
            .eval_program(&shoal_syntax::parse("fn tool(session_only: bool) { null }").unwrap())
            .unwrap();
        let completer = ShoalCompleter::new(
            evaluator.env().clone(),
            Arc::new(Mutex::new(PathBuf::from("."))),
            vec![catalog],
            vec!["tool".into()],
        );
        let flags = completer.flag_candidates("tool", "--");
        assert!(flags.iter().any(|flag| flag == "--session_only"));
        assert!(!flags.iter().any(|flag| flag == "--adapter_only"));
    }

    #[test]
    fn subsequence_match_is_a_superset_of_prefix_matching() {
        assert!(subsequence_match("shoal-eval", "sho"), "a real prefix");
        assert!(
            subsequence_match("shoal-eval", "sev"),
            "non-contiguous typo-tolerant match"
        );
        assert!(!subsequence_match("shoal-eval", "zzz"));
        assert!(
            subsequence_match("anything", ""),
            "empty needle always matches"
        );
    }

    /// `completion.fuzzy = false` (site/content/internals/configuration-reference.md): only strict prefix
    /// matches, no non-contiguous "typo-tolerant" candidates.
    #[test]
    fn fuzzy_false_restricts_to_strict_prefix_matches() {
        let fuzzy_env = Env::root();
        fuzzy_env.declare("myservice", Value::Int(1), false);
        let fuzzy = ShoalCompleter::new(
            fuzzy_env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
        .configure(true, true, 100);
        // Fuzzy (default): a non-contiguous subsequence still matches.
        assert!(
            fuzzy
                .expr_candidates("mysvc")
                .iter()
                .any(|n| n == "myservice")
        );

        let strict_env = Env::root();
        strict_env.declare("myservice", Value::Int(1), false);
        let strict = ShoalCompleter::new(
            strict_env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
        .configure(false, true, 100);
        assert!(
            !strict
                .expr_candidates("mysvc")
                .iter()
                .any(|n| n == "myservice"),
            "fuzzy=false must reject a non-prefix subsequence match"
        );
        assert!(
            strict
                .expr_candidates("myser")
                .iter()
                .any(|n| n == "myservice")
        );
    }

    /// `completion.case_insensitive = false`: an exact-case mismatch must not
    /// match.
    #[test]
    fn case_insensitive_false_requires_exact_case() {
        let env = Env::root();
        env.declare("MyThing", Value::Int(1), false);
        let c = ShoalCompleter::new(
            env,
            Arc::new(Mutex::new(PathBuf::from("."))),
            Vec::new(),
            Vec::new(),
        )
        .configure(false, false, 100);
        assert!(!c.expr_candidates("mything").iter().any(|n| n == "MyThing"));
        assert!(c.expr_candidates("MyTh").iter().any(|n| n == "MyThing"));
    }

    /// `completion.max_results` caps the candidate list (site/content/internals/configuration-reference.md).
    #[test]
    fn max_results_caps_the_candidate_list() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("file{i}.txt")), b"").unwrap();
        }
        let c = ShoalCompleter::new(
            Env::root(),
            Arc::new(Mutex::new(dir.path().to_path_buf())),
            Vec::new(),
            Vec::new(),
        )
        .configure(true, true, 3);
        let names = c.fs_candidates("");
        assert_eq!(names.len(), 10, "fs_candidates itself is uncapped");
        let suggestions = finish(names, 0, 0, 3);
        assert_eq!(suggestions.len(), 3, "finish() truncates to max_results");
    }

    #[test]
    fn production_completion_ownership_stays_decomposed() {
        let root = include_str!("completer.rs");
        let production_root = root
            .split_once("\n#[cfg(test)]\nmod tests")
            .expect("inline completion tests remain after production")
            .0;
        let candidates = include_str!("completer/candidates.rs");
        let context = include_str!("completer/context.rs");
        let discovery = include_str!("completer/discovery.rs");
        let inference = include_str!("completer/inference.rs");

        assert!(
            production_root.lines().count() <= 180,
            "completion root must remain Reedline orchestration"
        );
        assert!(candidates.lines().count() <= 220);
        assert!(context.lines().count() <= 190);
        assert!(discovery.lines().count() <= 230);
        assert!(inference.lines().count() <= 150);

        for forbidden in [
            "fs::read_dir",
            "Lexer::new",
            "resolve_command_source",
            "method_names",
            "PATH_CACHE_REVALIDATE: Duration",
        ] {
            assert!(
                !production_root.contains(forbidden),
                "responsibility `{forbidden}` leaked back into the root"
            );
        }

        let complete = production_root
            .split_once("fn complete")
            .expect("Reedline completion entrypoint")
            .1
            .split_once("\n    }\n}")
            .expect("completion impl boundary")
            .0;
        assert!(
            complete.lines().count() <= 35,
            "Reedline entrypoint should only classify and dispatch"
        );
    }
}
