//! Command, redirect, adapter, plugin, and `.feed` plan derivation.

use super::*;
use crate::plan_effects::{parse_declared_effect, push_effect};
use shoal_syntax::commands::CommandSource;

impl Evaluator {
    pub(super) fn plan_call(
        &mut self,
        call: &CmdCall,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        if depth > 64 {
            return Err(ErrorVal::new(
                "recursion_limit",
                "planning function recursion exceeded 64",
            ));
        }
        if let Some(target) = aliases.get(&call.head) {
            plan_background_registration(call, out);
            self.plan_command_inputs(call, functions, aliases, out, depth)?;
            self.plan_redirects(call, out);
            return self.plan_call(target, functions, aliases, out, depth + 1);
        }
        if let Some(body) = functions.get(&call.head) {
            plan_background_registration(call, out);
            self.plan_command_inputs(call, functions, aliases, out, depth)?;
            self.plan_redirects(call, out);
            return self.plan_block(body, functions, aliases, out, depth + 1);
        }

        let resolution = self.resolve_command(call);
        match resolution.source {
            CommandSource::SessionCallable => {
                self.plan_command_inputs(call, functions, aliases, out, depth)?;
                self.plan_redirects(call, out);
                push_effect(out, Effect::Opaque);
                return Ok(());
            }
            CommandSource::BoundValue => {
                self.plan_redirects(call, out);
                return Ok(());
            }
            _ => {}
        }

        // Runtime resolves builtin help before effects: even a destructive builtin with extra
        // operands, a redirect, or `&` has no effects when help was requested.
        if matches!(
            resolution.source,
            CommandSource::StructuredBuiltin | CommandSource::SpecialBuiltin
        ) && crate::command::call_requests_help(call)
        {
            return Ok(());
        }

        plan_background_registration(call, out);
        self.plan_command_inputs(call, functions, aliases, out, depth)?;
        self.plan_redirects(call, out);
        self.plan_resolved_call(call, resolution.source, out)
    }

    /// Heads intercepted before ordinary builtin/adapter dispatch continue here.
    fn plan_resolved_call(
        &mut self,
        call: &CmdCall,
        source: CommandSource,
        out: &mut Vec<Effect>,
    ) -> VResult<()> {
        if self.plan_intercepted_head(call, source, out) {
            return Ok(());
        }
        if source == CommandSource::Script {
            push_effect(
                out,
                Effect::FsRead {
                    paths: vec![self.plan_abs(&call.head)],
                },
            );
            push_effect(out, Effect::Opaque);
            return Ok(());
        }
        if source == CommandSource::StructuredBuiltin {
            for effect in self.builtin_effects(call)? {
                push_effect(out, effect);
            }
            return Ok(());
        }
        if source == CommandSource::SpecialBuiltin {
            self.plan_special_builtin(&call.head, out);
            return Ok(());
        }
        self.plan_extension_or_external(call, source, out)
    }

    /// Classify heads whose runtime meaning precedes ordinary dispatch.
    fn plan_intercepted_head(
        &self,
        call: &CmdCall,
        source: CommandSource,
        out: &mut Vec<Effect>,
    ) -> bool {
        match (source, call.head.as_str()) {
            (CommandSource::SpecialBuiltin, "interact") => {
                match call.args.first().and_then(cmd_arg_str_literal) {
                    Some(head) => self.plan_external_spawn(&head, out),
                    None => push_effect(out, Effect::Opaque),
                }
            }
            (CommandSource::SpecialBuiltin, "open") => {
                self.plan_open(
                    call.args
                        .first()
                        .and_then(|arg| self.cmd_arg_path_literal(arg)),
                    out,
                );
            }
            (CommandSource::SpecialBuiltin, "save") => {
                self.plan_save(
                    call.args
                        .first()
                        .and_then(|arg| self.cmd_arg_path_literal(arg)),
                    out,
                );
            }
            (CommandSource::SpecialBuiltin, "run") => {
                self.plan_run_target(call.args.first().and_then(cmd_arg_str_literal), out);
            }
            (CommandSource::SpecialBuiltin, "source") => {
                if let Some(path) = call
                    .args
                    .first()
                    .and_then(|arg| self.cmd_arg_path_literal(arg))
                {
                    push_effect(out, Effect::FsRead { paths: vec![path] });
                }
                push_effect(out, Effect::Opaque);
            }
            _ => return false,
        }
        true
    }

    fn plan_special_builtin(&self, head: &str, out: &mut Vec<Effect>) {
        match head {
            "cd" | "pushd" | "popd" | "j" | "jump" | "exit" | "quit" => {
                push_effect(out, Effect::SessionWrite);
            }
            "journal" | "history" => push_effect(out, Effect::JournalRead),
            "undo" => {
                push_effect(out, Effect::JournalRead);
                push_effect(out, Effect::Opaque);
            }
            "apply" | "reef" => push_effect(out, Effect::Opaque),
            "plan" => push_effect(out, Effect::SessionWrite),
            "jobs" | "dirs" | "pwd" | "assert" | "explain" => {}
            _ => push_effect(out, Effect::Opaque),
        }
    }

    fn plan_extension_or_external(
        &mut self,
        call: &CmdCall,
        source: CommandSource,
        out: &mut Vec<Effect>,
    ) -> VResult<()> {
        if source == CommandSource::Plugin {
            let registry = self
                .host
                .wasm
                .as_ref()
                .expect("plugin resolution carries a registry");
            let command = registry
                .command(&call.head)
                .expect("plugin resolution carries command metadata");
            for effect in command.effects {
                push_effect(out, effect.clone());
            }
        } else if source == CommandSource::Adapter {
            self.plan_adapter_call(call, out)?;
        } else {
            debug_assert_eq!(source, CommandSource::External);
            self.plan_external_spawn(&call.head, out);
        }
        Ok(())
    }

    fn plan_adapter_call(&mut self, call: &CmdCall, out: &mut Vec<Effect>) -> VResult<()> {
        let adapter = self
            .host
            .adapters
            .lookup(&call.head)
            .cloned()
            .expect("adapter resolution carries a catalog entry");
        let (spec, start) = match call.args.first() {
            Some(CmdArg::Word { text, .. }) if adapter.subs.contains_key(text) => {
                (adapter.subs[text].clone(), 1)
            }
            _ => (adapter.top.clone(), 0),
        };
        let bindings = self.plan_bindings(call, &spec, start)?;
        for declared in &spec.effects {
            for effect in parse_declared_effect(declared, &bindings, &self.exec.shell.cwd) {
                push_effect(out, effect);
            }
        }
        let bin_hash = self
            .hash_resolved_bin(OsStr::new(&adapter.bin))
            .unwrap_or_default();
        push_effect(
            out,
            Effect::ProcSpawn {
                bin_hash,
                argv0: adapter.bin,
            },
        );
        Ok(())
    }

    fn plan_redirects(&self, call: &CmdCall, out: &mut Vec<Effect>) {
        for redirect in &call.redirects {
            match redirect.kind {
                RedirectKind::Out | RedirectKind::Append => {
                    match self.cmd_arg_path_literal(&redirect.target) {
                        Some(path) => push_effect(out, Effect::FsWrite { paths: vec![path] }),
                        None => push_effect(out, Effect::Opaque),
                    }
                }
                RedirectKind::In => match self.cmd_arg_path_literal(&redirect.target) {
                    Some(path) => push_effect(out, Effect::FsRead { paths: vec![path] }),
                    None => push_effect(out, Effect::Opaque),
                },
            }
        }
    }

    pub(super) fn plan_external_spawn(&self, head: &str, out: &mut Vec<Effect>) {
        let bin_hash = self.hash_resolved_bin(OsStr::new(head)).unwrap_or_default();
        push_effect(
            out,
            Effect::ProcSpawn {
                bin_hash,
                argv0: head.to_string(),
            },
        );
    }

    pub(super) fn plan_save(&self, path: Option<PathBuf>, out: &mut Vec<Effect>) {
        match path {
            Some(path) => push_effect(out, Effect::FsWrite { paths: vec![path] }),
            None => push_effect(out, Effect::Opaque),
        }
    }

    pub(super) fn plan_open(&self, path: Option<PathBuf>, out: &mut Vec<Effect>) {
        if let Some(path) = path {
            push_effect(out, Effect::FsRead { paths: vec![path] });
        }
        push_effect(out, Effect::Opaque);
    }

    pub(super) fn plan_run_target(&self, target: Option<String>, out: &mut Vec<Effect>) {
        match target {
            Some(name)
                if !name.is_empty()
                    && !name.contains('/')
                    && !name.starts_with('.')
                    && !name.starts_with('~')
                    && Path::new(&name).extension().is_none() =>
            {
                debug_assert_eq!(
                    self.resolve_dynamic_run(&name, false).source,
                    CommandSource::External
                );
                self.plan_external_spawn(&name, out);
            }
            Some(name) => {
                debug_assert_eq!(
                    self.resolve_dynamic_run(&name, true).source,
                    CommandSource::Runner
                );
                push_effect(
                    out,
                    Effect::FsRead {
                        paths: vec![self.resolved_abs_path(&name)],
                    },
                );
                push_effect(out, Effect::Opaque);
            }
            None => push_effect(out, Effect::Opaque),
        }
    }

    pub(super) fn plan_command_ref(
        &mut self,
        name: &str,
        span: Span,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        let call = CmdCall {
            head: name.to_string(),
            forced: false,
            args: Vec::new(),
            redirects: Vec::new(),
            env_prefix: Vec::new(),
            background: false,
            trailing: None,
            span,
        };
        self.plan_call(&call, functions, aliases, out, depth + 1)
    }

    fn is_command_operand(&self, expr: &Expr) -> bool {
        match expr {
            Expr::LangBlock { .. } | Expr::Cmd { .. } => true,
            Expr::Var { name, .. } => self.exec.shell.env.get(name).is_none(),
            _ => false,
        }
    }

    pub(super) fn plan_feed(
        &mut self,
        recv: &Expr,
        arg: &Expr,
        functions: &Functions,
        aliases: &Aliases,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        let (value_expr, command_expr) = if self.is_command_operand(recv) {
            (arg, recv)
        } else {
            (recv, arg)
        };
        self.plan_expr(value_expr, functions, aliases, out, depth)?;
        match command_expr {
            Expr::LangBlock { .. } => push_effect(out, Effect::Opaque),
            Expr::Cmd { call, .. } => self.plan_external_spawn(&call.head, out),
            Expr::Var { name, .. } => self.plan_external_spawn(name, out),
            other => self.plan_expr(other, functions, aliases, out, depth)?,
        }
        Ok(())
    }
}
