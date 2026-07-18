//! CMD-argument coercion/expansion: turning AST `CmdArg` nodes into runtime
//! `Value`s (and back), and expanding globs into argv words.

use super::*;

pub(crate) const MAX_GLOB_MATCHES: usize = 16_384;
pub(crate) const MAX_GLOB_PATH_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_PROCESS_ARGV_VALUES: usize = 16_384;
pub(crate) const MAX_PROCESS_ARGV_BYTES: usize = 16 * 1024 * 1024;

pub(crate) struct ArgvBuilder {
    values: Vec<OsString>,
    bytes: usize,
    max_values: usize,
    max_bytes: usize,
}

impl ArgvBuilder {
    pub(crate) fn new(head: OsString) -> VResult<Self> {
        Self::with_limits(head, MAX_PROCESS_ARGV_VALUES, MAX_PROCESS_ARGV_BYTES)
    }

    fn with_limits(head: OsString, max_values: usize, max_bytes: usize) -> VResult<Self> {
        let mut builder = Self {
            values: Vec::new(),
            bytes: 0,
            max_values,
            max_bytes,
        };
        builder.push(head)?;
        Ok(builder)
    }

    pub(crate) fn push(&mut self, value: OsString) -> VResult<()> {
        if self.values.len() >= self.max_values {
            return Err(argv_limit(format!(
                "process argv reached its {}-value limit",
                self.max_values
            )));
        }
        self.bytes = self
            .bytes
            .checked_add(value.as_os_str().as_encoded_bytes().len())
            .ok_or_else(|| argv_limit("process argv byte accounting overflowed"))?;
        if self.bytes > self.max_bytes {
            return Err(argv_limit(format!(
                "process argv exceeds its {}-byte limit",
                self.max_bytes
            )));
        }
        self.values.push(value);
        Ok(())
    }

    pub(crate) fn extend(&mut self, values: impl IntoIterator<Item = OsString>) -> VResult<()> {
        for value in values {
            self.push(value)?;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Vec<OsString> {
        self.values
    }
}

pub(crate) fn validate_argv(values: &[OsString]) -> VResult<()> {
    if values.len() > MAX_PROCESS_ARGV_VALUES {
        return Err(argv_limit(format!(
            "process argv has {} values; the limit is {MAX_PROCESS_ARGV_VALUES}",
            values.len()
        )));
    }
    let mut bytes = 0usize;
    for value in values {
        bytes = bytes
            .checked_add(value.as_os_str().as_encoded_bytes().len())
            .ok_or_else(|| argv_limit("process argv byte accounting overflowed"))?;
        if bytes > MAX_PROCESS_ARGV_BYTES {
            return Err(argv_limit(format!(
                "process argv exceeds its {MAX_PROCESS_ARGV_BYTES}-byte limit"
            )));
        }
    }
    Ok(())
}

fn argv_limit(message: impl Into<String>) -> ErrorVal {
    ErrorVal::new("argv_limit", message)
        .with_hint("reduce arguments, narrow glob expansions, or feed data through stdin")
}

impl Evaluator {
    pub(crate) fn cmd_arg_value(&mut self, a: &CmdArg) -> VResult<Value> {
        match a {
            CmdArg::Word { text, .. } => Ok(Value::Str(text.clone())),
            CmdArg::Path { text, .. } => Ok(Value::Path(self.resolve_path(text))),
            CmdArg::Glob { pattern, .. } => Ok(Value::Glob(shoal_value::GlobVal {
                pattern: pattern.clone(),
                cwd: self.exec.shell.cwd.clone(),
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
        let mut paths: Vec<_> = expand_glob_paths(&g.cwd, &g.pattern, g.hidden)?
            .into_iter()
            .map(Value::Path)
            .collect();
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
                .unwrap_or_else(|| self.exec.shell.cwd.clone())
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
                self.exec.shell.cwd.join(p)
            }),
            Value::Str(s) => {
                let p = PathBuf::from(s);
                Ok(if p.is_absolute() {
                    p
                } else {
                    self.exec.shell.cwd.join(p)
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

/// Expand a filesystem glob behind one count/byte admission boundary shared
/// by runtime argv/list expansion and static plan path derivation.
pub(crate) fn expand_glob_paths(cwd: &Path, pattern: &str, hidden: bool) -> VResult<Vec<PathBuf>> {
    expand_glob_paths_with_limits(cwd, pattern, hidden, MAX_GLOB_MATCHES, MAX_GLOB_PATH_BYTES)
}

fn expand_glob_paths_with_limits(
    cwd: &Path,
    pattern: &str,
    hidden: bool,
    max_matches: usize,
    max_path_bytes: usize,
) -> VResult<Vec<PathBuf>> {
    let pat = cwd.join(pattern).to_string_lossy().into_owned();
    // Dotfile exclusion (site/content/internals/language-conformance-contract.md): a plain `*.txt` skips `.hidden.txt`;
    // dotfiles are only matched when the pattern's own last component starts
    // with `.`, or the glob was built `hidden: true`.
    let options = glob::MatchOptions {
        require_literal_leading_dot: !hidden && !pattern_matches_dotfiles(pattern),
        ..glob::MatchOptions::default()
    };
    let matches = glob::glob_with(&pat, options)
        .map_err(|error| ErrorVal::new("arg_error", error.to_string()))?;
    let mut paths = Vec::new();
    let mut path_bytes = 0usize;
    for path in matches.filter_map(Result::ok) {
        if paths.len() >= max_matches {
            return Err(glob_expansion_limit(format!(
                "glob matched more than {max_matches} paths"
            )));
        }
        path_bytes = path_bytes
            .checked_add(path.as_os_str().as_encoded_bytes().len())
            .ok_or_else(|| glob_expansion_limit("glob path-byte accounting overflowed"))?;
        if path_bytes > max_path_bytes {
            return Err(glob_expansion_limit(format!(
                "glob matches exceed the {max_path_bytes}-byte path limit"
            )));
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

fn glob_expansion_limit(message: impl Into<String>) -> ErrorVal {
    ErrorVal::new("glob_expansion_limit", message)
        .with_hint("narrow the glob pattern or walk the directory incrementally")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_expansion_fails_before_retaining_matches_past_either_wall() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert_eq!(
            expand_glob_paths_with_limits(dir.path(), "*", false, 2, 1024)
                .unwrap_err()
                .code,
            "glob_expansion_limit"
        );
        assert_eq!(
            expand_glob_paths_with_limits(dir.path(), "*", false, 8, 1)
                .unwrap_err()
                .code,
            "glob_expansion_limit"
        );
    }

    #[test]
    fn argv_builder_checks_count_and_bytes_before_retaining_the_next_value() {
        let mut count = ArgvBuilder::with_limits("cmd".into(), 2, 1024).unwrap();
        count.push("one".into()).unwrap();
        assert_eq!(count.push("two".into()).unwrap_err().code, "argv_limit");

        let mut bytes = ArgvBuilder::with_limits("c".into(), 8, 3).unwrap();
        assert_eq!(bytes.push("abc".into()).unwrap_err().code, "argv_limit");
    }
}
