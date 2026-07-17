//! Small shared utilities used across evaluator modules: the bare-command
//! shape test, top-level value display, closure `--help` synthesis, the
//! default (no-host-wired) statement sink, and ISO datetime parsing.

use super::*;

/// A statement is a "bare command" when its root is a command invocation
/// (or a boolean composition of commands) — defect #1b / WP1's Binary{And,Cmd,Cmd}.
pub(crate) fn is_command_expr(e: &Expr) -> bool {
    match e {
        Expr::Cmd { .. } | Expr::LangBlock { .. } => true,
        Expr::Binary {
            op: BinOp::And | BinOp::Or,
            lhs,
            rhs,
            ..
        } => is_command_expr(lhs) && is_command_expr(rhs),
        _ => false,
    }
}

/// Top-level display of a value for the default statement sink and for `echo`:
/// strings/paths are unquoted at the top level; nested values use `render_inline`.
pub(crate) fn display_top(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Path(p) => p.to_string_lossy().into_owned(),
        Value::Null => String::new(),
        other => shoal_value::render::render_inline(other),
    }
}

/// Synthesised `--help` text for a user fn (site/content/internals/language-conformance-contract.md).
pub(crate) fn closure_help(c: &shoal_value::ClosureVal) -> String {
    let name = c.name.clone().unwrap_or_else(|| "fn".into());
    let mut params: Vec<String> = c
        .params
        .iter()
        .map(|p| match &p.ty {
            Some(t) => format!("{}: {}", p.name, t.name),
            None => p.name.clone(),
        })
        .collect();
    if let Some(rest) = &c.rest {
        params.push(format!("...{}", rest.name));
    }
    let mut out = format!("{name}({})", params.join(", "));
    if let Some(ret) = &c.ret {
        out.push_str(&format!(" -> {}", ret.name));
    }
    if let Some(doc) = &c.doc {
        out.push('\n');
        out.push_str(doc);
    }
    out
}

/// Default statement sink (no host wired): print command output to real stdout.
pub(crate) fn default_render(v: &Value) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match v {
        Value::Outcome(o) => {
            let _ = lock.write_all(&o.stdout);
        }
        other => {
            let _ = writeln!(lock, "{}", display_top(other));
        }
    }
}

/// Live wall-clock datetime in the system time zone, sourced from the
/// evaluator's [`Clock`] port (so tests can pin it). Backs the `now` relative
/// anchor and duration `.ago`/`.from_now` composition (site/content/internals/language-conformance-contract.md).
pub(crate) fn now_zoned(clock: &dyn shoal_value::Clock) -> jiff::Zoned {
    let ns = clock.now_ns();
    jiff::Timestamp::from_nanosecond(ns as i128)
        .map(|ts| ts.to_zoned(jiff::tz::TimeZone::system()))
        .unwrap_or_else(|_| jiff::Zoned::now())
}

/// Today at midnight in the system time zone (the `today` relative anchor,
/// site/content/internals/language-conformance-contract.md). Falls back to the raw `now` instant if start-of-day overflows.
pub(crate) fn today_zoned(clock: &dyn shoal_value::Clock) -> jiff::Zoned {
    let z = now_zoned(clock);
    z.start_of_day().unwrap_or(z)
}

pub(crate) fn parse_datetime(iso: &str) -> VResult<jiff::Zoned> {
    if let Ok(zoned) = iso.parse::<jiff::Zoned>() {
        return Ok(zoned);
    }
    if let Ok(timestamp) = iso.parse::<jiff::Timestamp>() {
        return Ok(timestamp.to_zoned(jiff::tz::TimeZone::UTC));
    }
    if let Ok(date) = iso.parse::<jiff::civil::Date>() {
        return date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .map_err(|e| ErrorVal::new("arg_error", format!("invalid datetime: {e}")));
    }
    Err(ErrorVal::new(
        "arg_error",
        format!("invalid datetime `{iso}`"),
    ))
}
