//! CMD-argument coercion/expansion: turning AST `CmdArg` nodes into runtime
//! `Value`s (and back), and expanding globs into argv words.

use super::*;

impl Evaluator {
    pub(crate) fn cmd_arg_value(&mut self, a: &CmdArg) -> VResult<Value> {
        match a {
            CmdArg::Word { text, .. } => Ok(Value::Str(text.clone())),
            CmdArg::Path { text, .. } => Ok(Value::Path(self.resolve_path(text))),
            CmdArg::Glob { pattern, .. } => Ok(Value::Glob(shoal_value::GlobVal {
                pattern: pattern.clone(),
                cwd: self.exec.cwd.clone(),
                hidden: false,
            })),
            CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => {
                self.eval_expr(expr, Position::Value)
            }
            CmdArg::FlagLong { name, value, .. } => Ok(Value::Str(match value {
                Some(v) => format!(
                    "--{name}={}",
                    shoal_value::render::render_inline(&self.cmd_arg_value(v)?)
                ),
                None => format!("--{name}"),
            })),
            CmdArg::FlagShort { chars, .. } => Ok(Value::Str(format!("-{chars}"))),
            CmdArg::DashDash { .. } => Ok(Value::Str("--".into())),
            CmdArg::Dash { .. } => Ok(Value::Str("-".into())),
        }
    }
    pub(crate) fn expand_arg(&mut self, a: &CmdArg) -> VResult<Vec<Value>> {
        let v = self.cmd_arg_value(a)?;
        if let Value::Glob(g) = v {
            let paths = self.expand_glob(&g)?;
            // Zero-match glob lint (defect #16, site/content/internals/language-conformance-contract.md): nullglob still yields zero
            // argv, but a statement-level miss is worth a diagnostic.
            if paths.is_empty() {
                eprintln!("shoal: no matches for {}", g.pattern);
            }
            Ok(paths)
        } else {
            Ok(vec![v])
        }
    }
    /// Expand a glob value into its sorted `list<path>` matches against the
    /// glob's origin cwd, honoring the dotfile-exclusion rule (site/content/internals/language-conformance-contract.md). This
    /// is the shared core behind command-argument expansion, `for x in <glob>`,
    /// and the glob-value collection methods; it emits no nullglob lint — the
    /// command-argument path adds that itself.
    pub(crate) fn expand_glob(&self, g: &shoal_value::GlobVal) -> VResult<Vec<Value>> {
        let pat = g.cwd.join(&g.pattern).to_string_lossy().into_owned();
        // Dotfile exclusion (site/content/internals/language-conformance-contract.md): a plain `*.txt` skips `.hidden.txt`;
        // dotfiles are only matched when the pattern's own last component
        // starts with `.`, or the glob was built `hidden: true`.
        let options = glob::MatchOptions {
            require_literal_leading_dot: !g.hidden && !pattern_matches_dotfiles(&g.pattern),
            ..glob::MatchOptions::default()
        };
        let mut paths = glob::glob_with(&pat, options)
            .map_err(|e| ErrorVal::new("arg_error", e.to_string()))?
            .filter_map(Result::ok)
            .map(Value::Path)
            .collect::<Vec<_>>();
        paths.sort_by_key(shoal_value::render::render_inline);
        Ok(paths)
    }
    pub(crate) fn argv_value(&self, v: Value) -> VResult<OsString> {
        match v {
            Value::Str(s) => Ok(s.into()),
            Value::Path(p) => Ok(p.into_os_string()),
            Value::Int(i) => Ok(i.to_string().into()),
            Value::Float(f) => Ok(f.to_string().into()),
            Value::Size(n) => Ok(n.to_string().into()),
            Value::Duration(n) => Ok(n.to_string().into()),
            Value::Bool(b) => Ok(b.to_string().into()),
            Value::Secret(_) => Err(ErrorVal::new(
                "type_error",
                "secret cannot be placed in argv",
            )),
            other => Err(ErrorVal::new(
                "type_error",
                format!("{} cannot be passed as argv", other.type_name()),
            )),
        }
    }
    pub(crate) fn resolve_path(&self, text: &str) -> PathBuf {
        if let Some(rest) = text.strip_prefix("~/") {
            std::env::home_dir()
                .unwrap_or_else(|| self.exec.cwd.clone())
                .join(rest)
        } else {
            PathBuf::from(text)
        }
    }
    pub(crate) fn arg_path(&mut self, a: &CmdArg) -> VResult<PathBuf> {
        match self.cmd_arg_value(a)? {
            Value::Path(p) => Ok(if p.is_absolute() {
                p
            } else {
                self.exec.cwd.join(p)
            }),
            Value::Str(s) => {
                let p = PathBuf::from(s);
                Ok(if p.is_absolute() {
                    p
                } else {
                    self.exec.cwd.join(p)
                })
            }
            _ => Err(ErrorVal::new("arg_error", "redirect target must be a path")),
        }
    }
    pub(crate) fn value_cmd_arg(&self, v: Value, span: Span) -> VResult<CmdArg> {
        Ok(match v {
            Value::Path(p) => CmdArg::Path {
                text: p.to_string_lossy().into_owned(),
                span,
            },
            Value::Str(s) => CmdArg::Word { text: s, span },
            _ => {
                return Err(ErrorVal::new(
                    "type_error",
                    "alias arguments must be strings or paths",
                ));
            }
        })
    }
}

/// Whether a glob pattern intends to match dotfiles: true when its final path
/// component begins with a literal `.` (site/content/internals/language-conformance-contract.md "unless pattern starts with
/// `.`"). `**/.env` → true, `*.txt` / `**/*.txt` → false.
fn pattern_matches_dotfiles(pattern: &str) -> bool {
    pattern
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|last| last.starts_with('.'))
}
