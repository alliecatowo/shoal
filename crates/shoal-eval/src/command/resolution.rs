//! Command-name classification, suggestions, and generic argument collection.

use super::*;

impl Evaluator {
    /// True when `name` resolves as a command (builtin, special head, adapter,
    /// or an executable on `PATH`) — drives command-in-expression (defect #5).
    pub(crate) fn is_command_name(&self, name: &str) -> bool {
        match self.resolve_head(name, false, false).source {
            CommandSource::SessionCallable
            | CommandSource::StructuredBuiltin
            | CommandSource::SpecialBuiltin
            | CommandSource::Script
            | CommandSource::Runner
            | CommandSource::Plugin
            | CommandSource::Adapter => return true,
            CommandSource::BoundValue => return false,
            CommandSource::External => {}
        }
        if name.contains('/') || name.contains('.') {
            return false;
        }
        let path = self
            .exec
            .shell
            .process_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str());
        shoal_exec::which(OsStr::new(name), path).is_some()
    }

    /// Command did-you-mean (site/content/internals/language-conformance-contract.md): when a command head fails to resolve,
    /// find the closest *known* command name so the `not_found` error can carry
    /// a `did you mean 'X'?` hint — the command-head analogue of the method
    /// did-you-mean (`shoal_value::methods::suggest`).
    ///
    /// The candidate vocabulary is deliberately host-INDEPENDENT so the hint is
    /// deterministic and testable: the canonical builtin registry
    /// (`shoal_syntax::commands::builtin_names`), the adapter command heads the
    /// evaluator holds, and the in-scope callable session bindings (fn/alias
    /// names). We do NOT scan `$PATH` — that would be noisy and non-reproducible.
    ///
    /// Threshold mirrors the method hint: names of ≥ 5 chars tolerate an edit
    /// distance of 2, shorter names only 1 (at distance 2 a 4-char typo matches
    /// half the table), and the match must be strictly closer than the typo's
    /// own length so a short head can't match unrelated noise.
    pub(super) fn command_suggestion(&self, head: &str) -> Option<String> {
        // A reef-rewritten `argv[0]` can be an absolute path, but the user typed
        // a bare name — compare against the final path component.
        let head = head
            .rsplit(['/', std::path::MAIN_SEPARATOR])
            .next()
            .unwrap_or(head);
        let len = head.chars().count();
        if len == 0 {
            return None;
        }
        let max_d = if len >= 5 { 2 } else { 1 };
        // Union the deterministic candidate sources, then sort+dedup so ties
        // break identically every run (the first minimum wins in `min_by_key`).
        let mut candidates: Vec<String> = builtins::builtin_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        candidates.extend(self.host.adapters.names().map(str::to_owned));
        if let Some(registry) = &self.host.wasm {
            candidates.extend(registry.command_names().map(str::to_owned));
        }
        for name in self.exec.shell.env.visible_names() {
            if self
                .exec
                .shell
                .env
                .get(&name)
                .is_some_and(|v| v.is_callable())
            {
                candidates.push(name);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        let (dist, best) = candidates
            .iter()
            .map(|c| (shoal_value::methods::levenshtein(head, c), c))
            .min_by_key(|(d, _)| *d)?;
        (dist <= max_d && dist < len).then(|| format!("did you mean '{best}'?"))
    }

    /// Resolve the optional `exit`/`quit` status argument to an `i32`
    /// (default `0`). Accepts a bare integer word (`exit 3`) or an int-valued
    /// expression; anything non-integer is an `arg_error`.
    pub(super) fn exit_code_arg(&mut self, call: &CmdCall) -> VResult<i32> {
        let vs = self.collect_cmd_values(call)?;
        let Some(first) = vs.into_iter().next() else {
            return Ok(0);
        };
        let code = match crate::coerce::coerce_word(first, "int")? {
            Value::Int(n) => n,
            other => {
                return Err(ErrorVal::arg_error(format!(
                    "exit expects an int status, found {}",
                    other.type_name()
                ))
                .with_span(call.span));
            }
        };
        Ok(code.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
    }

    /// Collect a command's positional (non-flag) argument values.
    pub(crate) fn collect_cmd_values(&mut self, call: &CmdCall) -> VResult<Vec<Value>> {
        let mut vs = Vec::new();
        for a in &call.args {
            match a {
                CmdArg::FlagLong { .. } | CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } => {}
                _ => vs.extend(self.expand_arg(a)?),
            }
        }
        Ok(vs)
    }
}
