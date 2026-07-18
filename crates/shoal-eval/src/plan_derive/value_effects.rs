//! Effects owned by value fields, methods, namespaces, and constructors.

use super::attribution::{is_path_read_method, str_literal, url_host_port, url_literal};
use super::*;
use crate::plan_effects::push_effect;

impl Evaluator {
    pub(super) fn plan_field_effects(&self, recv: &Expr, name: &str, out: &mut Vec<Effect>) {
        if matches!(recv, Expr::Var { name: ns, .. } if ns == "env")
            && self.exec.shell.env.get("env").is_none()
        {
            push_effect(
                out,
                Effect::EnvRead {
                    names: vec![name.to_owned()],
                },
            );
        }
        self.plan_path_read_effect(recv, name, out);
    }

    pub(super) fn plan_method_effects(
        &self,
        recv: &Expr,
        name: &str,
        args: &Args,
        out: &mut Vec<Effect>,
    ) {
        if matches!(recv, Expr::Var { name: ns, .. } if ns == "secret")
            && self.exec.shell.env.get("secret").is_none()
            && name == "get"
        {
            let names = args
                .pos
                .first()
                .and_then(str_literal)
                .map_or_else(|| vec!["*".into()], |name| vec![name]);
            push_effect(out, Effect::SecretUse { names });
        }
        if let Expr::Var { name: ns, .. } = recv
            && self.exec.shell.env.get(ns).is_none()
        {
            if ns == "http" && matches!(name, "get" | "post" | "put" | "delete") {
                let (host, port) = args
                    .pos
                    .first()
                    .and_then(url_literal)
                    .map(|url| url_host_port(&url))
                    .unwrap_or_else(|| ("*".into(), 443));
                push_effect(out, Effect::NetConnect { host, port });
            }
            if ns == "os" && name == "env" {
                push_effect(
                    out,
                    Effect::EnvRead {
                        names: vec!["*".into()],
                    },
                );
            }
        }
        if matches!(name, "save" | "append") {
            self.plan_save(args.pos.first().and_then(|arg| self.path_literal(arg)), out);
        }
        self.plan_path_read_effect(recv, name, out);
    }

    fn plan_path_read_effect(&self, recv: &Expr, name: &str, out: &mut Vec<Effect>) {
        if !is_path_read_method(name) {
            return;
        }
        match self.path_literal(recv) {
            Some(path) => push_effect(out, Effect::FsRead { paths: vec![path] }),
            None => push_effect(out, Effect::Opaque),
        }
    }

    /// Returns true when `name` is a canonical constructor, including pure
    /// constructors whose correct effect contribution is empty.
    pub(super) fn plan_constructor_effects(
        &self,
        name: &str,
        args: &Args,
        out: &mut Vec<Effect>,
    ) -> bool {
        let Some(constructor) = crate::constructors::Constructor::named(name) else {
            return false;
        };
        match constructor {
            crate::constructors::Constructor::Every => push_effect(out, Effect::Time),
            crate::constructors::Constructor::Watch | crate::constructors::Constructor::Tail => {
                match args.pos.first().and_then(|arg| self.path_literal(arg)) {
                    Some(path) => push_effect(out, Effect::FsRead { paths: vec![path] }),
                    None => push_effect(out, Effect::Opaque),
                }
            }
            crate::constructors::Constructor::Path
            | crate::constructors::Constructor::Glob
            | crate::constructors::Constructor::Regex
            | crate::constructors::Constructor::Channel => {}
        }
        true
    }
}
