//! Adapter-schema validation and lowering into an external argv.

use super::*;

impl Evaluator {
    pub(crate) fn eval_adapter(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        let adapter = self
            .host
            .adapters
            .lookup(&call.head)
            .expect("checked adapter")
            .clone();
        let (spec, sub, start) = match call.args.first() {
            Some(CmdArg::Word { text, .. }) if adapter.subs.contains_key(text) => {
                (adapter.subs[text].clone(), Some(text.clone()), 1)
            }
            _ => (adapter.top.clone(), None, 0),
        };
        let mut argv = crate::args::ArgvBuilder::new(OsString::from(&adapter.bin))?;
        match (&spec.invoke, &sub) {
            (Some(rewrite), _) => argv.extend(rewrite.iter().map(OsString::from))?,
            (None, Some(sub)) => argv.push(sub.into())?,
            (None, None) => {}
        }
        let mut positional = 0usize;
        let mut i = start;
        while i < call.args.len() {
            match &call.args[i] {
                CmdArg::FlagLong { name, value, .. } => {
                    let param = spec
                        .params
                        .iter()
                        .find(|p| p.name == *name)
                        .ok_or_else(|| {
                            ErrorVal::arg_error(format!(
                                "{}: unknown flag --{name}; expected {}",
                                call.head,
                                signature(&spec)
                            ))
                        })?;
                    // `consumed` flags stay recognized/validated (below) but
                    // must never reach the child's argv — see the module-level
                    // "consumed" rule doc in shoal-adapters.
                    let consumed = spec.consumed.iter().any(|c| c == name);
                    if !consumed {
                        // Single-character params emit the POSIX single-dash
                        // form: git has `-n`, not `--n` — this used to
                        // validate `--n` and forward it verbatim, which git
                        // rejects ("ambiguous argument"), leaving the
                        // adapter's own advertised flag unusable.
                        let spelled = if name.chars().count() == 1 {
                            format!("-{name}")
                        } else {
                            format!("--{}", name.replace('_', "-"))
                        };
                        argv.push(spelled.into())?;
                    }
                    if let Some(value) = value {
                        let v = self.cmd_arg_value(value)?;
                        validate_adapter_value(&v, &param.ty)?;
                        if !consumed {
                            argv.push(self.argv_value(v)?)?;
                        }
                    } else if !param.ty.trim_end_matches('?').eq("bool") {
                        i += 1;
                        let next = call.args.get(i).ok_or_else(|| {
                            ErrorVal::arg_error(format!("--{name} requires a value"))
                        })?;
                        let v = self.cmd_arg_value(next)?;
                        validate_adapter_value(&v, &param.ty)?;
                        if !consumed {
                            argv.push(self.argv_value(v)?)?;
                        }
                    }
                }
                CmdArg::FlagShort { chars, .. } => {
                    let mut kept = String::new();
                    for ch in chars.chars() {
                        let Some(pname) = spec.short_flags.get(&ch.to_string()) else {
                            return Err(ErrorVal::arg_error(format!(
                                "{}: unknown short flag -{ch}",
                                call.head
                            )));
                        };
                        // Same "consumed" rule as the long-flag branch above:
                        // stays a recognized short flag, just dropped from argv.
                        if !spec.consumed.iter().any(|c| c == pname) {
                            kept.push(ch);
                        }
                    }
                    if !kept.is_empty() {
                        argv.push(format!("-{kept}").into())?;
                    }
                }
                CmdArg::DashDash { .. } => argv.push("--".into())?,
                arg => {
                    let expected = spec
                        .positional
                        .get(positional)
                        .and_then(|name| spec.params.iter().find(|p| &p.name == name));
                    let value = self.cmd_arg_value(arg)?;
                    if let Some(param) = expected {
                        validate_adapter_value(&value, &param.ty)?;
                    }
                    // A parameter typed glob owns expansion; T0/list<path> expansion remains elsewhere.
                    if matches!(expected.map(|p| p.ty.trim_end_matches('?')), Some("glob")) {
                        match value {
                            Value::Glob(g) => argv.push(g.pattern.into())?,
                            v => argv.push(self.argv_value(v)?)?,
                        }
                    } else if matches!(value, Value::Glob(_)) {
                        for value in self.expand_arg(arg)? {
                            argv.push(self.argv_value(value)?)?;
                        }
                    } else {
                        argv.push(self.argv_value(value)?)?;
                    }
                    positional += 1;
                }
            }
            i += 1;
        }
        let ok_codes = spec.ok_codes.clone().unwrap_or(adapter.ok_codes);
        let meta = ExecMeta {
            ok_codes,
            class: adapter.class,
            parse: spec.parse,
            output_type: spec.output_type,
        };
        let mut stdin = StdinSpec::Null;
        for redirect in &call.redirects {
            if redirect.kind == RedirectKind::In {
                stdin = StdinSpec::File(self.arg_path(&redirect.target)?);
            }
        }
        let output_redirected = call
            .redirects
            .iter()
            .any(|redirect| matches!(redirect.kind, RedirectKind::Out | RedirectKind::Append));
        // Preserve statement failure semantics only after redirected bytes have
        // been committed by the dispatch layer. Running as Value here keeps a
        // non-zero child outcome available for that write-first ordering.
        let run_position = if output_redirected {
            Position::Value
        } else {
            position
        };
        let argv = argv.finish();
        if output_redirected {
            self.run_argv_redirected(
                argv,
                run_position,
                stdin,
                &call.env_prefix,
                call.span,
                Some(meta),
            )
        } else {
            self.run_argv(
                argv,
                run_position,
                stdin,
                &call.env_prefix,
                call.span,
                Some(meta),
            )
        }
    }
}
