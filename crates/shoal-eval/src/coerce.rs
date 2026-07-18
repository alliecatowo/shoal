//! Word -> declared-type coercion (see
//! `site/content/internals/language-conformance-contract.md`): CMD words are
//! plain strings until coerced against a callee's/adapter's declared param
//! types.

use super::*;

/// Coerce a single CMD word value to a declared parameter type. Non-string
/// values pass through unchanged; unknown types keep the value verbatim (→ str).
pub(crate) fn coerce_word(v: Value, ty: &str) -> VResult<Value> {
    let ty = ty.trim_end_matches('?');
    let Value::Str(s) = &v else {
        return Ok(v);
    };
    let s = s.clone();
    match ty {
        "str" => Ok(Value::Str(s)),
        "int" => s
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| ErrorVal::arg_error(format!("expected int, found {s:?}"))),
        "float" => s
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| ErrorVal::arg_error(format!("expected float, found {s:?}"))),
        "size" => shoal_value::parse_size(&s)
            .map(Value::Size)
            .ok_or_else(|| ErrorVal::arg_error(format!("expected size, found {s:?}"))),
        "duration" => {
            // Accept bare integers as seconds so `sleep 1` and `sleep 10ms` both
            // work — and bind them AS a duration, not as a
            // stray int the callee then trips over in duration arithmetic.
            if let Some(ns) = shoal_value::parse_duration(&s) {
                Ok(Value::Duration(ns))
            } else if let Ok(secs) = s.parse::<i64>() {
                Ok(Value::Duration(secs.saturating_mul(1_000_000_000)))
            } else {
                Err(ErrorVal::arg_error(format!(
                    "expected duration, found {s:?}"
                )))
            }
        }
        "time" => shoal_value::parse_time(&s)
            .map(Value::Time)
            .ok_or_else(|| ErrorVal::arg_error(format!("expected time, found {s:?}"))),
        "datetime" => crate::helpers::parse_datetime(&s)
            .map(|z| Value::DateTime(Box::new(z)))
            .map_err(|_| ErrorVal::arg_error(format!("expected datetime, found {s:?}"))),
        "bool" => match s.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(ErrorVal::arg_error(format!("expected bool, found {s:?}"))),
        },
        "path" => Ok(Value::Path(PathBuf::from(s))),
        "glob" => Ok(Value::Glob(shoal_value::GlobVal {
            pattern: s,
            cwd: PathBuf::new(),
            hidden: false,
        })),
        _ => Ok(Value::Str(s)),
    }
}

/// Item type name a positional slot at `idx` coerces to: the declared param's
/// type name, or — once `idx` runs past the fixed params — the `...rest`
/// param's element type (unwrapping a `list<T>` annotation to `T`), so
/// `...nums: list<int>` and `...nums: int` both accumulate as `int`.
pub(crate) fn expected_param_ty<'a>(
    params: &'a [Param],
    rest: Option<&'a RestParam>,
    idx: usize,
) -> Option<&'a str> {
    if let Some(p) = params.get(idx) {
        return p.ty.as_ref().map(|t| t.name.as_str());
    }
    rest.and_then(|r| r.ty.as_ref()).map(|t| {
        if t.name == "list" {
            t.args.first().map(|a| a.name.as_str()).unwrap_or("str")
        } else {
            t.name.as_str()
        }
    })
}

/// Apply a parameter annotation at the one closure-call boundary shared by
/// expression and command-form calls. String values keep Shoal's deliberate
/// shell-word coercions, while already-tagged values must match (apart from
/// strict UTF-8 `path` -> `str`, which supports path-shaped command words).
pub(crate) fn coerce_param(v: Value, ty: &Type) -> VResult<Value> {
    apply_annotation(v, ty, true)
}

/// Enforce a return annotation without parsing or widening the function's
/// result. A declared return is a runtime guarantee, not an output conversion.
pub(crate) fn check_return(v: Value, ty: &Type) -> VResult<Value> {
    apply_annotation(v, ty, false)
}

/// Validate an annotation's name/generic shape before evaluating defaults or a
/// function body. Calls repeat this cheaply inside value validation so
/// programmatically constructed closures cannot bypass admission.
pub(crate) fn validate_annotation(ty: &Type) -> VResult<()> {
    valid_type_shape(ty)
}

/// Coerce and collect a variadic tail. `...xs: T` and `...xs: list<T>` both
/// mean that the closure binding is a list whose individual items satisfy `T`.
pub(crate) fn coerce_rest(items: Vec<Value>, ty: &Type) -> VResult<Value> {
    valid_type_shape(ty)?;
    let item_ty = if ty.name == "list" {
        ty.args.first()
    } else {
        Some(ty)
    };
    let items = match item_ty {
        Some(item_ty) => items
            .into_iter()
            .map(|item| coerce_param(item, item_ty))
            .collect::<VResult<Vec<_>>>()?,
        None => items,
    };
    Ok(Value::List(items))
}

fn apply_annotation(v: Value, ty: &Type, coerce_input: bool) -> VResult<Value> {
    valid_type_shape(ty)?;
    if ty.optional && v == Value::Null {
        return Ok(v);
    }
    match (&*ty.name, v) {
        ("list", Value::List(items)) => {
            let Some(elem) = ty.args.first() else {
                return Ok(Value::List(items));
            };
            Ok(Value::List(
                items
                    .into_iter()
                    .map(|item| apply_annotation(item, elem, coerce_input))
                    .collect::<VResult<Vec<_>>>()?,
            ))
        }
        ("table", Value::Table(rows)) => {
            let Some(elem) = ty.args.first() else {
                return Ok(Value::Table(rows));
            };
            let rows = rows
                .into_iter()
                .map(
                    |row| match apply_annotation(Value::Record(row), elem, coerce_input)? {
                        Value::Record(row) => Ok(row),
                        _ => unreachable!("a table row can only satisfy a record annotation"),
                    },
                )
                .collect::<VResult<Vec<_>>>()?;
            Ok(Value::Table(rows))
        }
        ("str", Value::Path(path)) if coerce_input => path
            .into_os_string()
            .into_string()
            .map(Value::Str)
            .map_err(|_| ErrorVal::type_error("expected UTF-8 str, found non-UTF-8 path")),
        (name, value) if value_matches_name(&value, name) => Ok(value),
        (name, Value::Str(value)) if coerce_input && word_coercible(name) => {
            coerce_word(Value::Str(value), name)
        }
        (_, value) => Err(ErrorVal::type_error(format!(
            "expected {}, found {}",
            render_type(ty),
            value.type_name()
        ))),
    }
}

fn word_coercible(name: &str) -> bool {
    matches!(
        name,
        "str"
            | "int"
            | "float"
            | "size"
            | "duration"
            | "time"
            | "datetime"
            | "bool"
            | "path"
            | "glob"
    )
}

fn valid_type_shape(ty: &Type) -> VResult<()> {
    let max_args = match ty.name.as_str() {
        "list" | "table" => 1,
        "null" | "bool" | "int" | "float" | "str" | "path" | "glob" | "regex" | "size"
        | "duration" | "datetime" | "time" | "bytes" | "record" | "range" | "stream" | "error"
        | "outcome" | "task" | "closure" | "command" | "secret" => 0,
        name => {
            return Err(ErrorVal::type_error(format!(
                "unknown type annotation `{name}`"
            )));
        }
    };
    if ty.args.len() > max_args {
        return Err(ErrorVal::type_error(format!(
            "type `{}` accepts at most {max_args} type argument{}",
            ty.name,
            if max_args == 1 { "" } else { "s" }
        )));
    }
    if ty.name == "table"
        && let Some(elem) = ty.args.first()
        && elem.name != "record"
    {
        return Err(ErrorVal::type_error(
            "table element annotation must be `record`",
        ));
    }
    for arg in &ty.args {
        valid_type_shape(arg)?;
    }
    Ok(())
}

fn value_matches_name(value: &Value, name: &str) -> bool {
    match (name, value) {
        ("bytes", Value::Bytes(_) | Value::CasBytes(_)) => true,
        ("null", Value::Null) => true,
        _ => value.type_name() == name,
    }
}

fn render_type(ty: &Type) -> String {
    let mut rendered = ty.name.clone();
    if !ty.args.is_empty() {
        rendered.push('<');
        rendered.push_str(
            &ty.args
                .iter()
                .map(render_type)
                .collect::<Vec<_>>()
                .join(", "),
        );
        rendered.push('>');
    }
    if ty.optional {
        rendered.push('?');
    }
    rendered
}

pub(crate) fn signature(spec: &SubSpec) -> String {
    spec.params
        .iter()
        .map(|p| format!("--{} <{}>", p.name.replace('_', "-"), p.ty))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn validate_adapter_value(value: &Value, ty: &str) -> VResult<()> {
    let ty = ty.trim_end_matches('?');
    let valid = match ty {
        "str" => matches!(value, Value::Str(_)),
        "bool" => matches!(value, Value::Bool(_) | Value::Str(_)),
        "int" => {
            matches!(value, Value::Int(_))
                || matches!(value, Value::Str(s) if s.parse::<i64>().is_ok())
        }
        "float" => {
            matches!(value, Value::Int(_) | Value::Float(_))
                || matches!(value, Value::Str(s) if s.parse::<f64>().is_ok())
        }
        "path" => matches!(value, Value::Path(_) | Value::Str(_)),
        "glob" => matches!(value, Value::Glob(_) | Value::Str(_)),
        "size" => {
            matches!(value, Value::Size(_))
                || matches!(value, Value::Str(s) if shoal_value::parse_size(s).is_some())
        }
        "duration" => {
            matches!(value, Value::Duration(_))
                || matches!(value, Value::Str(s) if shoal_value::parse_duration(s).is_some())
        }
        "time" => {
            matches!(value, Value::Time(_))
                || matches!(value, Value::Str(s) if shoal_value::parse_time(s).is_some())
        }
        ty if ty.starts_with("list<") => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ErrorVal::arg_error(format!(
            "expected {ty}, found {}",
            value.type_name()
        )))
    }
}
