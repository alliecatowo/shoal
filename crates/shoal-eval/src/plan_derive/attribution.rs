//! Literal/source attribution for paths, modules, command arguments, and URLs.

use super::*;

impl Evaluator {
    pub(super) fn plan_abs(&self, value: &str) -> PathBuf {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            self.exec.shell.cwd.join(path)
        }
    }

    pub(super) fn cmd_arg_path_literal(&self, arg: &CmdArg) -> Option<PathBuf> {
        let value = match arg {
            CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => text.clone(),
            CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => match expr {
                Expr::Str { value, .. } => value.clone(),
                _ => return None,
            },
            _ => return None,
        };
        Some(self.plan_abs(&value))
    }

    /// Resolve the same `.shl` candidate the module loader would inspect,
    /// using only the injected filesystem port.
    pub(super) fn plan_module_path(&self, path: &str) -> PathBuf {
        let base = PathBuf::from(path);
        let base = if base.is_absolute() {
            base
        } else {
            self.exec.shell.cwd.join(base)
        };
        let candidate = if base.extension().is_some() {
            base
        } else {
            let with_shl = base.with_extension("shl");
            if self.host.fs.is_file(&with_shl) {
                with_shl
            } else {
                base
            }
        };
        self.host.fs.canonicalize(&candidate).unwrap_or(candidate)
    }

    pub(super) fn path_literal(&self, expr: &Expr) -> Option<PathBuf> {
        str_literal(expr).map(|value| self.plan_abs(&value))
    }
}

pub(super) fn is_path_read_method(name: &str) -> bool {
    matches!(
        name,
        "read" | "read_bytes" | "lines" | "exists" | "is_dir" | "is_file" | "size" | "modified"
    )
}

pub(super) fn str_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str { value, .. } => Some(value.clone()),
        Expr::FnCall { name, args, .. }
            if name == "path" && args.named.is_empty() && args.pos.len() == 1 =>
        {
            str_literal(&args.pos[0])
        }
        _ => None,
    }
}

pub(super) fn cmd_arg_str_literal(arg: &CmdArg) -> Option<String> {
    match arg {
        CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => Some(text.clone()),
        CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => str_literal(expr),
        _ => None,
    }
}

pub(super) fn url_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Str { value, .. } => Some(value.clone()),
        _ => None,
    }
}

/// Extract authority and port without accepting malformed ports as numeric.
/// An out-of-range port degrades to the scheme default instead of panicking.
pub(super) fn url_host_port(url: &str) -> (String, u16) {
    let default_port = if url.starts_with("https") { 443 } else { 80 };
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    match host_port.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (host.to_string(), port.parse().unwrap_or(default_port))
        }
        _ => (host_port.to_string(), default_port),
    }
}
