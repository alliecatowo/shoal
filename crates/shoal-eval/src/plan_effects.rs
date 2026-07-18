//! Per-builtin/adapter effect computation for plan derivation (see
//! [`crate::plan`] for the split rationale). Called into by the AST walk in
//! [`crate::plan_derive`].

use super::*;

impl Evaluator {
    pub(crate) fn builtin_effects(&self, call: &CmdCall) -> VResult<Vec<Effect>> {
        let mut ps = Vec::new();
        for arg in &call.args {
            if !matches!(
                arg,
                CmdArg::FlagLong { .. } | CmdArg::FlagShort { .. } | CmdArg::DashDash { .. }
            ) {
                ps.extend(self.plan_paths(arg)?);
            }
        }
        let e = match call.head.as_str() {
            "echo" | "sleep" | "pwd" => vec![],
            "env" => vec![Effect::EnvRead {
                names: vec!["*".into()],
            }],
            "which" => vec![Effect::EnvRead {
                names: vec!["PATH".into()],
            }],
            "ls" | "cat" | "stat" | "head" => vec![Effect::FsRead {
                paths: if ps.is_empty() {
                    vec![self.exec.shell.cwd.clone()]
                } else {
                    ps
                },
            }],
            "mkdir" | "touch" => vec![Effect::FsWrite { paths: ps }],
            "ln" => vec![Effect::FsWrite {
                paths: ps.into_iter().skip(1).collect(),
            }],
            "cp" => {
                if ps.len() < 2 {
                    return Err(ErrorVal::arg_error("cp requires source and destination"));
                }
                let dst = ps.last().cloned().unwrap();
                vec![
                    Effect::FsRead {
                        paths: ps[..ps.len() - 1].to_vec(),
                    },
                    Effect::FsWrite { paths: vec![dst] },
                ]
            }
            "mv" => {
                if ps.len() < 2 {
                    return Err(ErrorVal::arg_error("mv requires source and destination"));
                }
                let dst = ps.last().cloned().unwrap();
                vec![
                    Effect::FsRead {
                        paths: ps[..ps.len() - 1].to_vec(),
                    },
                    Effect::FsWrite { paths: vec![dst] },
                    Effect::FsDelete {
                        paths: ps[..ps.len() - 1].to_vec(),
                    },
                ]
            }
            "rm" => vec![Effect::FsDelete { paths: ps }],
            "cd" | "j" | "jump" => vec![Effect::SessionWrite],
            _ => vec![],
        };
        Ok(e)
    }

    pub(crate) fn plan_bindings(
        &self,
        call: &CmdCall,
        spec: &SubSpec,
        start: usize,
    ) -> VResult<std::collections::HashMap<String, Vec<String>>> {
        let mut bindings = std::collections::HashMap::new();
        let mut positional = 0;
        for arg in &call.args[start..] {
            match arg {
                CmdArg::FlagLong { name, value, .. } => {
                    if let Some(value) = value {
                        bindings
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(plan_text(value)?);
                    }
                }
                CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } => {}
                arg => {
                    if let Some(name) = spec.positional.get(positional) {
                        bindings
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(plan_text(arg)?);
                    }
                    positional += 1;
                }
            }
        }
        Ok(bindings)
    }

    fn plan_paths(&self, arg: &CmdArg) -> VResult<Vec<PathBuf>> {
        match arg {
            CmdArg::Glob { pattern, .. } => {
                crate::args::expand_glob_paths(&self.exec.shell.cwd, pattern, false)
            }
            CmdArg::Path { text, .. } => Ok(vec![self.resolved_abs_path(text)]),
            _ => Ok(vec![self.plan_abs(&plan_text(arg)?)]),
        }
    }
}

fn plan_text(arg: &CmdArg) -> VResult<String> {
    match arg {
        CmdArg::Word { text, .. }
        | CmdArg::Path { text, .. }
        | CmdArg::Glob { pattern: text, .. } => Ok(text.clone()),
        CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => match expr {
            Expr::Str { value, .. } => Ok(value.clone()),
            Expr::Int { value, .. } => Ok(value.to_string()),
            _ => Err(ErrorVal::arg_error("planning requires a literal argument")),
        },
        _ => Err(ErrorVal::arg_error("planning requires a value argument")),
    }
}
/// Parse one declared adapter effect against the **full** effect vocabulary
/// (site/content/internals/effects-plans-security.md). Recognized kinds map to
/// concrete [`Effect`]s; a declaration whose kind is not in the vocabulary
/// (a typo, or a future kind this build predates) becomes a conservative
/// [`Effect::Opaque`] rather than being silently dropped — fail-closed (A7).
///
/// Accepts both parenthesized (`fs.read($paths)`, `proc.spawn(docker)`) and
/// bare (`session.write`, `time`) forms.
pub(crate) fn parse_declared_effect(
    raw: &str,
    bindings: &std::collections::HashMap<String, Vec<String>>,
    cwd: &Path,
) -> Vec<Effect> {
    let (kind, values) = match raw
        .split_once('(')
        .and_then(|(k, a)| a.strip_suffix(')').map(|a| (k, a)))
    {
        Some((kind, arg)) => {
            let values = if arg == "cwd" {
                vec![cwd.to_string_lossy().into_owned()]
            } else if let Some(key) = arg.strip_prefix('$') {
                bindings.get(key).cloned().unwrap_or_default()
            } else if arg.is_empty() {
                vec![]
            } else {
                vec![arg.to_owned()]
            };
            (kind, values)
        }
        // A bare kind with no `(…)` — e.g. `session.write`, `journal.read`,
        // `time`. Any other bare token stays conservative (Opaque) below.
        None => (raw, Vec::new()),
    };
    let abs = |values: Vec<String>| -> Vec<PathBuf> {
        values
            .into_iter()
            .map(|p| {
                let p = PathBuf::from(p);
                if p.is_absolute() { p } else { cwd.join(p) }
            })
            .collect()
    };
    match kind {
        "fs.read" => vec![Effect::FsRead { paths: abs(values) }],
        "fs.write" => vec![Effect::FsWrite { paths: abs(values) }],
        "fs.delete" => vec![Effect::FsDelete { paths: abs(values) }],
        // A declared spawn (`proc.spawn(container)`) is name-only: the argument
        // is a description, not a locatable binary, so the hash stays empty
        // (matching the name-only fallback the adapter's own bin uses when it
        // isn't installed). Previously silently ignored (A7).
        "proc.spawn" => values
            .into_iter()
            .map(|v| Effect::ProcSpawn {
                bin_hash: String::new(),
                argv0: v,
            })
            .collect(),
        "net.connect" => values
            .into_iter()
            .map(|v| {
                let (host, port) = v
                    .rsplit_once(':')
                    .and_then(|(h, p)| p.parse().ok().map(|p| (h.to_owned(), p)))
                    .unwrap_or((v, 443));
                Effect::NetConnect { host, port }
            })
            .collect(),
        "net.listen" => values
            .into_iter()
            .map(|v| Effect::NetListen {
                port: v.parse().unwrap_or(0),
            })
            .collect(),
        "env.read" => vec![Effect::EnvRead { names: values }],
        "env.write" => vec![Effect::EnvWrite { names: values }],
        "secret.use" => vec![Effect::SecretUse { names: values }],
        "session.write" => vec![Effect::SessionWrite],
        "journal.read" => vec![Effect::JournalRead],
        "time" => vec![Effect::Time],
        // Unrecognized effect kind: never silently dropped — require approval by
        // planning it as opaque (A7, fail-closed).
        _ => vec![Effect::Opaque],
    }
}

/// Push `effect` unless it's already present (small-N linear scan; effect
/// lists are short per plan).
pub(crate) fn push_effect(out: &mut Vec<Effect>, effect: Effect) {
    if !out.contains(&effect) {
        out.push(effect)
    }
}
