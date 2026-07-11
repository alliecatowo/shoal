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
                ps.extend(plan_paths(arg, &self.cwd)?);
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
                    vec![self.cwd.clone()]
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
            "cd" => vec![Effect::SessionWrite],
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
fn plan_paths(arg: &CmdArg, cwd: &Path) -> VResult<Vec<PathBuf>> {
    match arg {
        CmdArg::Glob { pattern, .. } => {
            let pat = cwd.join(pattern).to_string_lossy().into_owned();
            let mut ps = glob::glob(&pat)
                .map_err(|e| ErrorVal::arg_error(e.to_string()))?
                .filter_map(Result::ok)
                .collect::<Vec<_>>();
            ps.sort();
            Ok(ps)
        }
        _ => {
            let p = PathBuf::from(plan_text(arg)?);
            Ok(vec![if p.is_absolute() { p } else { cwd.join(p) }])
        }
    }
}
pub(crate) fn parse_declared_effect(
    raw: &str,
    bindings: &std::collections::HashMap<String, Vec<String>>,
    cwd: &Path,
) -> Vec<Effect> {
    let Some((kind, arg)) = raw
        .split_once('(')
        .and_then(|(k, a)| a.strip_suffix(')').map(|a| (k, a)))
    else {
        return vec![];
    };
    let values = if arg == "cwd" {
        vec![cwd.to_string_lossy().into_owned()]
    } else if let Some(key) = arg.strip_prefix('$') {
        bindings.get(key).cloned().unwrap_or_default()
    } else {
        vec![arg.to_owned()]
    };
    match kind {
        "fs.read" => vec![Effect::FsRead {
            paths: values
                .into_iter()
                .map(|p| {
                    let p = PathBuf::from(p);
                    if p.is_absolute() { p } else { cwd.join(p) }
                })
                .collect(),
        }],
        "fs.write" => vec![Effect::FsWrite {
            paths: values
                .into_iter()
                .map(|p| {
                    let p = PathBuf::from(p);
                    if p.is_absolute() { p } else { cwd.join(p) }
                })
                .collect(),
        }],
        "fs.delete" => vec![Effect::FsDelete {
            paths: values
                .into_iter()
                .map(|p| {
                    let p = PathBuf::from(p);
                    if p.is_absolute() { p } else { cwd.join(p) }
                })
                .collect(),
        }],
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
        _ => vec![],
    }
}

/// Push `effect` unless it's already present (small-N linear scan; effect
/// lists are short per plan).
pub(crate) fn push_effect(out: &mut Vec<Effect>, effect: Effect) {
    if !out.contains(&effect) {
        out.push(effect)
    }
}
