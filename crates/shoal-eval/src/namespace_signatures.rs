//! Canonical namespace method names and argument admission.

use shoal_value::{CallArgs, ErrorVal, VResult};

#[derive(Clone, Copy)]
struct MethodSpec {
    namespace: &'static str,
    method: &'static str,
    min_positional: usize,
    max_positional: usize,
    named: &'static [&'static str],
}

const fn method(namespace: &'static str, name: &'static str, positional: usize) -> MethodSpec {
    MethodSpec {
        namespace,
        method: name,
        min_positional: positional,
        max_positional: positional,
        named: &[],
    }
}

const METHODS: &[MethodSpec] = &[
    method("json", "parse", 1),
    MethodSpec {
        namespace: "json",
        method: "stringify",
        min_positional: 1,
        max_positional: 2,
        named: &["pretty"],
    },
    method("yaml", "parse", 1),
    method("yaml", "stringify", 1),
    method("toml", "parse", 1),
    method("toml", "stringify", 1),
    method("csv", "parse", 1),
    method("csv", "stringify", 1),
    method("math", "sqrt", 1),
    method("math", "cbrt", 1),
    method("math", "sin", 1),
    method("math", "cos", 1),
    method("math", "tan", 1),
    method("math", "asin", 1),
    method("math", "acos", 1),
    method("math", "atan", 1),
    method("math", "atan2", 2),
    method("math", "ln", 1),
    method("math", "log10", 1),
    method("math", "log2", 1),
    method("math", "log", 2),
    method("math", "exp", 1),
    method("math", "floor", 1),
    method("math", "ceil", 1),
    method("math", "round", 1),
    method("math", "trunc", 1),
    method("math", "abs", 1),
    method("math", "sign", 1),
    method("math", "pow", 2),
    method("math", "min", 2),
    method("math", "max", 2),
    method("math", "hypot", 2),
    method("math", "clamp", 3),
    MethodSpec {
        namespace: "http",
        method: "get",
        min_positional: 1,
        max_positional: 2,
        named: &["headers"],
    },
    MethodSpec {
        namespace: "http",
        method: "delete",
        min_positional: 1,
        max_positional: 2,
        named: &["headers"],
    },
    MethodSpec {
        namespace: "http",
        method: "post",
        min_positional: 1,
        max_positional: 3,
        named: &["headers"],
    },
    MethodSpec {
        namespace: "http",
        method: "put",
        min_positional: 1,
        max_positional: 3,
        named: &["headers"],
    },
    method("os", "platform", 0),
    method("os", "arch", 0),
    method("os", "pid", 0),
    method("os", "hostname", 0),
    method("os", "username", 0),
    method("os", "cpus", 0),
    method("os", "uptime", 0),
    method("os", "env", 0),
    method("config", "all", 0),
    method("config", "get", 1),
];

pub(crate) fn validate(namespace: &str, method: &str, args: &CallArgs) -> VResult<()> {
    let Some(spec) = METHODS
        .iter()
        .find(|spec| spec.namespace == namespace && spec.method == method)
    else {
        return Ok(());
    };
    if !(spec.min_positional..=spec.max_positional).contains(&args.pos.len()) {
        let expected = if spec.min_positional == spec.max_positional {
            spec.min_positional.to_string()
        } else {
            format!("{}..={}", spec.min_positional, spec.max_positional)
        };
        return Err(ErrorVal::arg_error(format!(
            "{namespace}.{method} expects {expected} positional argument(s), found {}",
            args.pos.len()
        )));
    }
    if let Some((name, _)) = args
        .named
        .iter()
        .find(|(name, _)| !spec.named.contains(&name.as_str()))
    {
        return Err(ErrorVal::arg_error(format!(
            "{namespace}.{method} does not accept named argument `{name}`"
        )));
    }
    Ok(())
}

/// Method names for one synthetic namespace, shared with completion clients.
pub fn namespace_method_names(namespace: &str) -> impl Iterator<Item = &'static str> + '_ {
    METHODS
        .iter()
        .filter(move |spec| spec.namespace == namespace)
        .map(|spec| spec.method)
}

/// Every synthetic namespace method name, deduplicated by consumers as needed.
pub fn all_namespace_method_names() -> impl Iterator<Item = &'static str> {
    METHODS.iter().map(|spec| spec.method)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_unique_qualified_names() {
        let mut names = std::collections::BTreeSet::new();
        for spec in METHODS {
            assert!(names.insert((spec.namespace, spec.method)));
            assert!(spec.min_positional <= spec.max_positional);
        }
    }
}
