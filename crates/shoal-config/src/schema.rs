//! The declarative shape of a `shoal.toml`: one static tree ([`ROOT`]) that
//! both the unknown-key scanner and the type checker walk in a single pass,
//! so the two can never disagree about what's valid. site/content/internals/configuration-reference.md is the
//! human-readable rendering of this same tree — keep them in sync.

use crate::error::ConfigError;

/// The expected shape of one key.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// A table with a fixed, known set of child keys.
    Table(&'static [(&'static str, Kind)]),
    /// A table whose keys are user-chosen (tool names, runner extensions,
    /// …) and whose per-entry shape belongs to somebody else's schema (e.g.
    /// `shoal-reef`'s manifest format re-parses `[reef]` independently). We
    /// only assert it's a table; entries are never recursed into, so no
    /// "unknown key" warning ever fires inside one.
    Opaque,
    Bool,
    /// A TOML integer that must be `>= 0`.
    UInt,
    Str,
    /// An array whose elements must all be strings (also used for
    /// path-shaped values — TOML has no dedicated path type).
    StrArray,
    /// A table whose values must all be strings (`aliases`, `env`,
    /// `editor.keybindings`).
    StrMap,
}

fn describe(kind: Kind) -> &'static str {
    match kind {
        Kind::Table(_) => "a table",
        Kind::Opaque => "a table",
        Kind::Bool => "a boolean",
        Kind::UInt => "a non-negative integer",
        Kind::Str => "a string",
        Kind::StrArray => "an array of strings",
        Kind::StrMap => "a table of strings",
    }
}

pub(crate) const ROOT: Kind = Kind::Table(&[
    ("version", Kind::UInt),
    ("prompt", Kind::Table(&[("template", Kind::Str)])),
    (
        "history",
        Kind::Table(&[
            ("enabled", Kind::Bool),
            ("max_entries", Kind::UInt),
            ("path", Kind::Str),
            ("dedup", Kind::Bool),
            ("ignore", Kind::StrArray),
            ("ignore_space", Kind::Bool),
        ]),
    ),
    (
        "render",
        Kind::Table(&[
            ("width", Kind::UInt),
            ("color", Kind::Bool),
            ("paging", Kind::Str),
            ("pager", Kind::Str),
            ("echo", Kind::Str),
        ]),
    ),
    (
        "editor",
        Kind::Table(&[
            ("mode", Kind::Str),
            ("bracketed_paste", Kind::Bool),
            ("keybindings", Kind::StrMap),
        ]),
    ),
    (
        "kernel",
        Kind::Table(&[("enabled", Kind::Bool), ("session", Kind::Str)]),
    ),
    ("adapters", Kind::Table(&[("dirs", Kind::StrArray)])),
    (
        "journal",
        Kind::Table(&[("enabled", Kind::Bool), ("state_dir", Kind::Str)]),
    ),
    ("leash", Kind::Table(&[("policy", Kind::Str)])),
    ("init", Kind::Table(&[("files", Kind::StrArray)])),
    ("aliases", Kind::StrMap),
    ("env", Kind::StrMap),
    (
        "completion",
        Kind::Table(&[
            ("fuzzy", Kind::Bool),
            ("case_insensitive", Kind::Bool),
            ("max_results", Kind::UInt),
            ("menu", Kind::Bool),
        ]),
    ),
    (
        "reef",
        Kind::Table(&[
            ("tools", Kind::Opaque),
            ("runners", Kind::Opaque),
            ("options", Kind::Table(&[("hermetic", Kind::Bool)])),
        ]),
    ),
]);

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn type_error(path: &str, expected: Kind, found: &toml::Value) -> ConfigError {
    ConfigError::Type {
        source: None,
        key: path.to_string(),
        expected: describe(expected),
        found: found.type_str(),
    }
}

/// Walk `value` against `kind`, recursively, collecting an "unknown key"
/// warning (with a did-you-mean suggestion, site/content/internals/configuration-reference.md) for every key
/// not in the schema, and returning the *first* type mismatch as a hard
/// error. Never panics: every TOML shape (including a scalar where a table
/// was expected) is handled as data.
pub(crate) fn check(
    value: &toml::Value,
    kind: Kind,
    path: &str,
    warnings: &mut Vec<String>,
) -> Result<(), ConfigError> {
    match kind {
        Kind::Table(children) => {
            let Some(table) = value.as_table() else {
                return Err(type_error(path, kind, value));
            };
            for (k, v) in table {
                let child_path = join(path, k);
                match children.iter().find(|(name, _)| name == k) {
                    Some((_, child_kind)) => check(v, *child_kind, &child_path, warnings)?,
                    None => warnings.push(unknown_key_warning(path, k, &child_path, children)),
                }
            }
            Ok(())
        }
        Kind::Opaque => {
            if value.as_table().is_none() {
                return Err(type_error(path, kind, value));
            }
            Ok(())
        }
        Kind::StrMap => {
            let Some(table) = value.as_table() else {
                return Err(type_error(path, kind, value));
            };
            for (k, v) in table {
                if v.as_str().is_none() {
                    return Err(type_error(&join(path, k), Kind::Str, v));
                }
            }
            Ok(())
        }
        Kind::StrArray => {
            let Some(arr) = value.as_array() else {
                return Err(type_error(path, kind, value));
            };
            for (i, v) in arr.iter().enumerate() {
                if v.as_str().is_none() {
                    return Err(type_error(&format!("{path}[{i}]"), Kind::Str, v));
                }
            }
            Ok(())
        }
        Kind::Bool => {
            if value.as_bool().is_some() {
                Ok(())
            } else {
                Err(type_error(path, kind, value))
            }
        }
        Kind::UInt => match value {
            toml::Value::Integer(n) if *n >= 0 => Ok(()),
            _ => Err(type_error(path, kind, value)),
        },
        Kind::Str => {
            if value.as_str().is_some() {
                Ok(())
            } else {
                Err(type_error(path, kind, value))
            }
        }
    }
}

fn unknown_key_warning(
    prefix: &str,
    leaf: &str,
    full_path: &str,
    siblings: &'static [(&'static str, Kind)],
) -> String {
    let candidates: Vec<&str> = siblings.iter().map(|(n, _)| *n).collect();
    match suggest(leaf, &candidates) {
        Some(s) => {
            let suggestion_path = join(prefix, s);
            format!("unknown config key `{full_path}` (did you mean `{suggestion_path}`?)")
        }
        None => format!("unknown config key `{full_path}`"),
    }
}

/// Nearest-match did-you-mean suggestion, or `None` if nothing is close
/// enough to be worth suggesting (a wildly wrong key gets no guess rather
/// than a misleading one).
pub(crate) fn suggest<'a>(key: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(&str, usize)> = None;
    for &cand in candidates {
        let d = levenshtein(key, cand);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((cand, d));
        }
    }
    best.filter(|(_, d)| *d <= threshold(key.chars().count()))
        .map(|(c, _)| c)
}

fn threshold(len: usize) -> usize {
    match len {
        0..=3 => 1,
        4..=6 => 2,
        _ => 3,
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_typo() {
        assert_eq!(
            suggest("hsitory", &["version", "prompt", "history"]),
            Some("history")
        );
        assert_eq!(suggest("mdoe", &["mode", "bracketed_paste"]), Some("mode"));
    }

    #[test]
    fn no_suggestion_when_nothing_close() {
        assert_eq!(suggest("zzzzzzzz", &["version", "prompt", "history"]), None);
    }

    #[test]
    fn table_type_mismatch_is_precise() {
        let v: toml::Value = toml::from_str("history = \"nope\"").unwrap();
        let mut warnings = Vec::new();
        let err = check(&v, ROOT, "", &mut warnings).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Type {
                source: None,
                key: "history".into(),
                expected: "a table",
                found: "string",
            }
        );
    }

    #[test]
    fn scalar_leaf_type_mismatch_is_precise() {
        let v: toml::Value = toml::from_str("[history]\nenabled = \"yes\"").unwrap();
        let mut warnings = Vec::new();
        let err = check(&v, ROOT, "", &mut warnings).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Type {
                source: None,
                key: "history.enabled".into(),
                expected: "a boolean",
                found: "string",
            }
        );
    }

    #[test]
    fn array_element_type_mismatch_names_the_index() {
        let v: toml::Value = toml::from_str("[adapters]\ndirs = [\"a\", 1]").unwrap();
        let mut warnings = Vec::new();
        let err = check(&v, ROOT, "", &mut warnings).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Type {
                source: None,
                key: "adapters.dirs[1]".into(),
                expected: "a string",
                found: "integer",
            }
        );
    }

    #[test]
    fn unknown_key_warns_with_suggestion() {
        let v: toml::Value = toml::from_str("[prompt]\ntempalte = \"x\"").unwrap();
        let mut warnings = Vec::new();
        check(&v, ROOT, "", &mut warnings).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("prompt.tempalte"));
        assert!(warnings[0].contains("did you mean `prompt.template`?"));
    }

    #[test]
    fn opaque_reef_tables_never_warn_on_arbitrary_keys() {
        let v: toml::Value = toml::from_str(
            "[reef.tools]\nnode = \"22\"\ngo = { provider = \"mise\" }\n[reef.runners]\npy = \"python\"\n",
        )
        .unwrap();
        let mut warnings = Vec::new();
        check(&v, ROOT, "", &mut warnings).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn str_map_rejects_non_string_values() {
        let v: toml::Value = toml::from_str("[aliases]\ngs = 1").unwrap();
        let mut warnings = Vec::new();
        let err = check(&v, ROOT, "", &mut warnings).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Type {
                source: None,
                key: "aliases.gs".into(),
                expected: "a string",
                found: "integer",
            }
        );
    }
}
