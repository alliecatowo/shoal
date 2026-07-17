//! Session directory navigation and directory-stack mutation.

use super::*;

impl Evaluator {
    /// The single choke point for a real session cwd change (`cd`, `cd -`,
    /// `pushd`, `popd`, `j`): stash the prior cwd as OLDPWD (so `cd -` returns
    /// to the *exact* directory left, byte-identical), move to `new`, and feed
    /// the destination to the `j`/`jump` frecency store (best-effort — a store
    /// write failure never fails the navigation). `with cwd:` and module loads
    /// deliberately do NOT flow through here: those are scoped save/restore cwd
    /// swaps, not navigation the user asked the shell to remember.
    pub(crate) fn change_cwd(&mut self, new: PathBuf) {
        let prev = std::mem::replace(&mut self.exec.shell.cwd, new);
        self.exec.shell.oldpwd = Some(prev);
        let cwd = self.exec.shell.cwd.clone();
        self.record_cd(&cwd);
    }

    /// Reject a session-cwd mutation (`cd`/`pushd`/`popd`) inside a `fn` body
    /// (site/content/internals/language-conformance-contract.md): a fn must not move the ambient session cwd — `with cwd:` is
    /// the scoped alternative. A pure guard shared by all three verbs.
    fn ensure_cwd_mutable(&self, verb: &str, span: Span) -> VResult<()> {
        if self.exec.control.in_fn_body > 0 {
            return Err(ErrorVal::new(
                "custom",
                format!(
                    "{verb} is only allowed at session top level; use `with cwd:` inside a fn body"
                ),
            )
            .with_span(span));
        }
        Ok(())
    }

    /// `cd [dir]` / `cd -` (site/content/internals/language-conformance-contract.md). Bare `cd` goes to `$HOME`; `cd -` returns
    /// to the previous directory (OLDPWD) and echoes it (bash parity, achieved
    /// by returning the `Path`, which the statement sink renders); otherwise cd
    /// to the resolved, canonicalized path. Every form records into the frecency
    /// store and updates OLDPWD via [`Evaluator::change_cwd`].
    pub(super) fn eval_cd(&mut self, call: &CmdCall) -> VResult<Value> {
        self.ensure_cwd_mutable("cd", call.span)?;
        // `cd -`: jump back to the previous directory (bash's `$OLDPWD`).
        if matches!(call.args.first(), Some(CmdArg::Dash { .. })) {
            let Some(prev) = self.exec.shell.oldpwd.clone() else {
                return Err(ErrorVal::new("custom", "cd: OLDPWD not set").with_span(call.span));
            };
            self.change_cwd(prev);
            return Ok(Value::Path(self.exec.shell.cwd.clone()));
        }
        let target = self.cd_target(call)?;
        self.change_cwd(target);
        Ok(Value::Path(self.exec.shell.cwd.clone()))
    }

    /// Resolve a `cd`/`pushd` path argument to an absolute, canonicalized
    /// directory. A missing argument means `$HOME` (the bare-`cd` case; `pushd`
    /// never calls this with no argument — that is its swap form). A non-path
    /// value is an `arg_error`; a path that does not resolve is one too.
    fn cd_target(&mut self, call: &CmdCall) -> VResult<PathBuf> {
        let p = call
            .args
            .first()
            .map(|a| self.cmd_arg_value(a))
            .transpose()?
            .unwrap_or_else(|| {
                Value::Path(std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")))
            });
        let p = match p {
            Value::Path(p) => p,
            Value::Str(s) => PathBuf::from(s),
            _ => return Err(ErrorVal::new("arg_error", "cd expects path")),
        };
        let joined = if p.is_absolute() {
            p
        } else {
            self.exec.shell.cwd.join(p)
        };
        joined
            .canonicalize()
            .map_err(|e| ErrorVal::new("arg_error", e.to_string()))
    }

    /// `pushd [dir]` — the bash directory stack. With a `dir`: push the current
    /// cwd onto the stack and cd into `dir`. With no argument: swap the current
    /// cwd with the most-recent stacked directory (an error when the stack is
    /// empty). Returns the new stack, exactly as `dirs` renders it.
    pub(super) fn eval_pushd(&mut self, call: &CmdCall) -> VResult<Value> {
        self.ensure_cwd_mutable("pushd", call.span)?;
        if call.args.is_empty() {
            let Some(top) = self.exec.shell.dir_stack.first().cloned() else {
                return Err(ErrorVal::new(
                    "custom",
                    "pushd: no other directory on the stack to swap with",
                )
                .with_span(call.span));
            };
            // Swap: the current cwd takes the top slot, we move to the old top.
            self.exec.shell.dir_stack[0] = self.exec.shell.cwd.clone();
            self.change_cwd(top);
            return Ok(self.dir_stack_value());
        }
        let target = self.cd_target(call)?;
        let cwd = self.exec.shell.cwd.clone();
        self.exec.shell.dir_stack.insert(0, cwd);
        self.change_cwd(target);
        Ok(self.dir_stack_value())
    }

    /// `popd` — pop the most-recent stacked directory and cd into it. An empty
    /// stack is an error (nothing to pop). Returns the remaining stack.
    pub(super) fn eval_popd(&mut self, call: &CmdCall) -> VResult<Value> {
        self.ensure_cwd_mutable("popd", call.span)?;
        if self.exec.shell.dir_stack.is_empty() {
            return Err(
                ErrorVal::new("custom", "popd: directory stack is empty").with_span(call.span)
            );
        }
        let target = self.exec.shell.dir_stack.remove(0);
        self.change_cwd(target);
        Ok(self.dir_stack_value())
    }

    /// `dirs` — the directory stack as a typed `list<path>`, current directory
    /// first (`[cwd] ++ dir_stack`). Structured, not text, so it dot-chains:
    /// `dirs.len()`, `dirs.first()`, `dirs.where(...)`.
    pub(super) fn eval_dirs(&mut self, _call: &CmdCall) -> VResult<Value> {
        Ok(self.dir_stack_value())
    }

    /// Build the shared `dirs`/`pushd`/`popd` return value: `[cwd] ++ dir_stack`
    /// as a `list<path>`, current directory first (bash's left-to-right order).
    fn dir_stack_value(&self) -> Value {
        let mut out = Vec::with_capacity(self.exec.shell.dir_stack.len() + 1);
        out.push(Value::Path(self.exec.shell.cwd.clone()));
        out.extend(self.exec.shell.dir_stack.iter().cloned().map(Value::Path));
        Value::List(out)
    }
}
