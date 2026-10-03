//! String-receiver methods (`.trim`, `.split`, `.matches`, `.str`, …).
//!
//! Named `strops` rather than `str` to avoid shadowing the `str` primitive
//! type in scopes that glob-import this module.

use super::materialize::{BoundedString, MaterializedCollection};
use super::*;

pub(crate) fn string_unary(v: Value, f: impl FnOnce(&str) -> Value) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(f(&s)),
        v => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn string_pred(v: Value, q: &str, f: fn(&str, &str) -> bool) -> VResult<Value> {
    string_unary(v, |s| Value::Bool(f(s, q)))
}
pub(crate) fn matches_method(v: Value, q: &Value) -> VResult<Value> {
    match (v, q) {
        (Value::Str(s), Value::Regex(r)) => {
            let mut out = MaterializedCollection::eager();
            for found in r.re.find_iter(&s) {
                out.push(Value::Str(found.as_str().into()))?;
            }
            Ok(out.finish())
        }
        _ => Err(ErrorVal::type_error(
            "matches expects str receiver and regex",
        )),
    }
}
/// `.replace(pat, rep)` — `pat` is a `str` (literal, all occurrences) or a
/// `regex` (all matches; `$1`/`$name` in `rep` expand capture groups, per the
/// `regex` crate). Mirrors the str/regex duality of `.matches`/`.match`.
pub(crate) fn replace_method(v: Value, pat: &Value, rep: &str) -> VResult<Value> {
    match (v, pat) {
        (Value::Str(s), Value::Str(p)) => replace_literal(&s, p, rep),
        (Value::Str(s), Value::Regex(r)) => replace_regex(&s, &r.re, rep),
        (Value::Str(_), other) => Err(ErrorVal::type_error(format!(
            "replace pattern must be str or regex, found {}",
            other.type_name()
        ))),
        (v, _) => Err(ErrorVal::type_error(format!(
            "replace expects a str receiver, found {}",
            v.type_name()
        ))),
    }
}

fn replace_literal(source: &str, pattern: &str, replacement: &str) -> VResult<Value> {
    let mut out = BoundedString::eager();
    let mut copied_through = 0;
    for found in source.match_indices(pattern).map(|(start, matched)| {
        let end = start + matched.len();
        (start, end)
    }) {
        out.push_str(&source[copied_through..found.0])?;
        out.push_str(replacement)?;
        copied_through = found.1;
    }
    out.push_str(&source[copied_through..])?;
    Ok(out.finish())
}

fn replace_regex(source: &str, regex: &regex::Regex, replacement: &str) -> VResult<Value> {
    let mut out = BoundedString::eager();
    let mut copied_through = 0;
    for captures in regex.captures_iter(source) {
        let found = captures.get(0).ok_or_else(|| {
            ErrorVal::new(
                "regex_error",
                "regex engine returned captures without a full match",
            )
        })?;
        out.push_str(&source[copied_through..found.start()])?;
        append_regex_replacement(&mut out, &captures, replacement)?;
        copied_through = found.end();
    }
    out.push_str(&source[copied_through..])?;
    Ok(out.finish())
}

/// Expand the `regex` crate's `$name`/`${name}` replacement grammar directly
/// into a bounded destination. Calling `Captures::expand` first would recreate
/// the very unbounded intermediate this guard exists to prevent.
fn append_regex_replacement(
    out: &mut BoundedString,
    captures: &regex::Captures<'_>,
    mut replacement: &str,
) -> VResult<()> {
    while let Some(dollar) = replacement.find('$') {
        out.push_str(&replacement[..dollar])?;
        replacement = &replacement[dollar..];
        if replacement.as_bytes().get(1) == Some(&b'$') {
            out.push_str("$")?;
            replacement = &replacement[2..];
            continue;
        }

        let (reference, consumed) = if replacement.as_bytes().get(1) == Some(&b'{') {
            match replacement[2..].find('}') {
                Some(close) => (&replacement[2..2 + close], 3 + close),
                None => {
                    out.push_str("$")?;
                    replacement = &replacement[1..];
                    continue;
                }
            }
        } else {
            let end = replacement
                .as_bytes()
                .iter()
                .skip(1)
                .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                .count()
                + 1;
            if end == 1 {
                out.push_str("$")?;
                replacement = &replacement[1..];
                continue;
            }
            (&replacement[1..end], end)
        };

        let captured = reference
            .parse::<usize>()
            .ok()
            .and_then(|index| captures.get(index))
            .or_else(|| captures.name(reference));
        if let Some(captured) = captured {
            out.push_str(captured.as_str())?;
        }
        replacement = &replacement[consumed..];
    }
    out.push_str(replacement)
}

pub(crate) fn lines_method(v: Value) -> VResult<Value> {
    let Value::Str(source) = v else {
        return Err(ErrorVal::type_error("lines expects a str receiver"));
    };
    let mut out = MaterializedCollection::eager();
    for line in source.lines() {
        out.push(Value::Str(line.trim_end_matches('\r').into()))?;
    }
    Ok(out.finish())
}

pub(crate) fn words_method(v: Value) -> VResult<Value> {
    let Value::Str(source) = v else {
        return Err(ErrorVal::type_error("words expects a str receiver"));
    };
    let mut out = MaterializedCollection::eager();
    for word in source.split_whitespace() {
        out.push(Value::Str(word.into()))?;
    }
    Ok(out.finish())
}

pub(crate) fn chars_method(v: Value) -> VResult<Value> {
    let Value::Str(source) = v else {
        return Err(ErrorVal::type_error("chars expects a str receiver"));
    };
    let mut out = MaterializedCollection::eager();
    for character in source.chars() {
        out.push(Value::Str(character.to_string()))?;
    }
    Ok(out.finish())
}

pub(crate) fn split_method(v: Value, separator: &str) -> VResult<Value> {
    let Value::Str(source) = v else {
        return Err(ErrorVal::type_error("split expects a str receiver"));
    };
    let mut out = MaterializedCollection::eager();
    for part in source.split(separator) {
        out.push(Value::Str(part.into()))?;
    }
    Ok(out.finish())
}

pub(crate) fn case_method(v: Value, uppercase: bool) -> VResult<Value> {
    let Value::Str(source) = v else {
        return Err(ErrorVal::type_error(
            "case conversion expects a str receiver",
        ));
    };
    let mut out = BoundedString::eager();
    for character in source.chars() {
        if uppercase {
            for mapped in character.to_uppercase() {
                out.push_char(mapped)?;
            }
        } else {
            for mapped in character.to_lowercase() {
                out.push_char(mapped)?;
            }
        }
    }
    Ok(out.finish())
}
pub(crate) fn match_method(v: Value, q: &Value) -> VResult<Value> {
    match (v, q) {
        (Value::Str(s), Value::Regex(r)) => Ok(r
            .re
            .find(&s)
            .map(|m| Value::Str(m.as_str().into()))
            .unwrap_or(Value::Null)),
        _ => Err(ErrorVal::type_error("match expects str receiver and regex")),
    }
}
pub(crate) fn string_parse(v: Value, ty: &str) -> VResult<Value> {
    match v {
        Value::Str(s) => match ty {
            "int" => s
                .parse()
                .map(Value::Int)
                .map_err(|_| ErrorVal::arg_error(format!("cannot parse {s:?} as int"))),
            _ => s
                .parse()
                .map(Value::Float)
                .map_err(|_| ErrorVal::arg_error(format!("cannot parse {s:?} as float"))),
        },
        v => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn to_str(v: Value, lossy: bool) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(Value::Str(s)),
        Value::Path(p) => {
            if lossy {
                Ok(Value::Str(p.to_string_lossy().into()))
            } else {
                p.into_os_string()
                    .into_string()
                    .map(Value::Str)
                    .map_err(|_| ErrorVal::new("utf8_error", "path is not valid UTF-8"))
            }
        }
        Value::Bytes(b) => {
            if lossy {
                Ok(Value::Str(String::from_utf8_lossy(&b).into()))
            } else {
                String::from_utf8((*b).clone())
                    .map(Value::Str)
                    .map_err(|_| ErrorVal::new("utf8_error", "bytes are not valid UTF-8"))
            }
        }
        // int/float/bool → their canonical render form ("42", "1.5", "true"),
        // exactly what `"{n}"` interpolation produces (site/content/internals/intercrate-protocol-contracts.md render
        // rules) — `42.str()` erroring taught nothing and helped nobody.
        v @ (Value::Int(_) | Value::Float(_) | Value::Bool(_)) => {
            Ok(Value::Str(crate::render::render_inline(&v)))
        }
        // Everything else keeps its render form reachable via interpolation;
        // `.str()` stays a *conversion*, not a formatter (datetime/list/… are
        // pinned as type errors by the corpus).
        v => Err(
            ErrorVal::type_error(format!("cannot convert {} to str", v.type_name()))
                .with_hint("format any value with string interpolation instead: \"{x}\""),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eager_string_partitions_stop_at_the_shared_value_limit() {
        let hostile = Value::Str("x".repeat(16_385));
        assert_eq!(
            chars_method(hostile).unwrap_err().code,
            "collection_materialization_limit"
        );
    }

    #[test]
    fn literal_replace_rejects_multiplicative_output_incrementally() {
        let source = Value::Str("a".repeat(8_193));
        let replacement = "x".repeat(2_048);
        assert_eq!(
            replace_method(source, &Value::Str("a".into()), &replacement)
                .unwrap_err()
                .code,
            "string_materialization_limit"
        );
    }

    #[test]
    fn bounded_regex_replace_preserves_capture_and_escape_grammar() {
        let regex = Value::Regex(Arc::new(
            RegexVal::compile("(?P<word>[a-z]+)-(\\d+)").unwrap(),
        ));
        assert_eq!(
            replace_method(
                Value::Str("abc-123".into()),
                &regex,
                "${2}:$word/$$/$missing"
            )
            .unwrap(),
            Value::Str("123:abc/$/".into())
        );
    }

    #[test]
    fn bounded_replacers_match_the_reference_implementations_below_the_wall() {
        for (source, pattern, replacement) in
            [("abc", "", "-"), ("aaaa", "aa", "x"), ("a🪸b🪸", "🪸", "$")]
        {
            assert_eq!(
                replace_literal(source, pattern, replacement).unwrap(),
                Value::Str(source.replace(pattern, replacement))
            );
        }

        let regex = regex::Regex::new("(?P<word>[a-z]+)(?:-(\\d+))?").unwrap();
        for replacement in ["$word", "${2}:$1", "$$/$missing", "$", "${unclosed"] {
            assert_eq!(
                replace_regex("abc-123 def", &regex, replacement).unwrap(),
                Value::Str(regex.replace_all("abc-123 def", replacement).into_owned())
            );
        }
    }
}
