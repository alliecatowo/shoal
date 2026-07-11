//! Word -> declared-type coercion (TDD §4.2 site 2, defect #12): CMD words are
//! plain strings until coerced against a callee's/adapter's declared param
//! types.

use super::*;

/// Coerce a single CMD word value to a declared parameter type (TDD §4.2 site 2,
/// defect #12). Non-string values pass through unchanged; unknown types keep the
/// value verbatim (→ str).
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
            // work — and bind them AS a duration (TDD §4.2 site 2), not as a
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

/// Coerce positional + named CMD-word arguments against a function's parameters
/// (TDD §4.2 site 2 / §4.4 `...rest`). Variadic tails accumulate: every
/// positional word beyond the fixed params is coerced to the rest param's
/// element type before `call_value_inner` collects them into a `list`.
pub(crate) fn coerce_call_args(
    params: &[Param],
    rest: Option<&RestParam>,
    pos: &mut [Value],
    named: &mut [(String, Value)],
) -> VResult<()> {
    for (i, p) in params.iter().enumerate() {
        let Some(ty) = &p.ty else { continue };
        let slot = if let Some(slot) = named.iter_mut().find(|(n, _)| n == &p.name) {
            &mut slot.1
        } else if let Some(slot) = pos.get_mut(i) {
            slot
        } else {
            continue;
        };
        let v = std::mem::replace(slot, Value::Null);
        *slot = if ty.name == "list" {
            // `list<T>`: coerce each element to `T`. The CMD word path
            // assembles+coerces word lists before this runs; an
            // expression-valued list lands here verbatim.
            coerce_list_param(v, Some(ty))?
        } else {
            coerce_word(v, &ty.name)?
        };
    }
    if rest.is_some()
        && let Some(item_ty) = expected_param_ty(params, rest, params.len())
    {
        for slot in pos.iter_mut().skip(params.len()) {
            *slot = coerce_word(std::mem::replace(slot, Value::Null), item_ty)?;
        }
    }
    Ok(())
}

/// Coerce each element of a `list<T>`-annotated argument (TDD §4.2 site 2's
/// `list<T>` row): a bound `list` value gets per-element word coercion to `T`;
/// any other value — or a non-`list` annotation — passes through verbatim.
pub(crate) fn coerce_list_param(v: Value, ty: Option<&Type>) -> VResult<Value> {
    let Some(ty) = ty.filter(|t| t.name == "list") else {
        return Ok(v);
    };
    let Value::List(items) = v else { return Ok(v) };
    let elem = ty.args.first().map(|a| a.name.as_str()).unwrap_or("str");
    Ok(Value::List(
        items
            .into_iter()
            .map(|x| coerce_word(x, elem))
            .collect::<VResult<_>>()?,
    ))
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
