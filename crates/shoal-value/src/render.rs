//! Rendering — the normative render rules pinned in docs/CONTRACTS.md §3.
//! `render_inline` is what the conformance corpus asserts against;
//! `render_block` is the REPL's pretty top-level form.

use crate::{Record, TimeVal, Value};
use unicode_width::UnicodeWidthStr;

/// Humanized decimal size: `237b`, `1.5mb`, `1.02kb`.
pub fn render_size(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("tb", 1e12), ("gb", 1e9), ("mb", 1e6), ("kb", 1e3)];
    if bytes < 1000 {
        return format!("{bytes}b");
    }
    for (i, (name, mult)) in UNITS.iter().enumerate() {
        if (bytes as f64) >= *mult {
            let mut x = bytes as f64 / mult;
            let mut name = *name;
            // Rounding may push us to the next unit (999_999 → "1mb", not "1000kb").
            if format!("{x:.2}").parse::<f64>().unwrap_or(x) >= 1000.0 && i > 0 {
                name = UNITS[i - 1].0;
                x = bytes as f64 / UNITS[i - 1].1;
            }
            let mut s = format!("{x:.2}");
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
            return format!("{s}{name}");
        }
    }
    unreachable!("bytes >= 1000 always matches a unit")
}

/// Compound duration: `1m30s`, `250ms`, `0s`. Negative durations are prefixed `-`.
pub fn render_duration(ns: i64) -> String {
    if ns == 0 {
        return "0s".to_string();
    }
    let (sign, mut rest) = if ns < 0 {
        ("-", ns.unsigned_abs())
    } else {
        ("", ns.unsigned_abs())
    };
    const UNITS: [(&str, u64); 8] = [
        ("w", 604_800_000_000_000),
        ("d", 86_400_000_000_000),
        ("h", 3_600_000_000_000),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("us", 1_000),
        ("ns", 1),
    ];
    let mut out = String::from(sign);
    for (name, mult) in UNITS {
        if rest >= mult {
            let n = rest / mult;
            rest %= mult;
            out.push_str(&format!("{n}{name}"));
        }
    }
    out
}

/// 24h `HH:MM` (`:SS` only when nonzero).
pub fn render_time(t: &TimeVal) -> String {
    if t.sec == 0 {
        format!("{:02}:{:02}", t.hour, t.min)
    } else {
        format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec)
    }
}

fn escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out
}

fn ident_shaped(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn render_record_inline(r: &Record) -> String {
    let fields: Vec<String> = r
        .iter()
        .map(|(k, v)| {
            let key = if ident_shaped(k) {
                k.clone()
            } else {
                format!("\"{}\"", escape_str(k))
            };
            format!("{key}: {}", render_inline(v))
        })
        .collect();
    format!("{{{}}}", fields.join(", "))
}

/// One-line canonical rendering (the conformance corpus compares against this).
pub fn render_inline(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.is_nan() {
                "nan".into()
            } else if f.is_infinite() {
                if *f > 0.0 {
                    "inf".into()
                } else {
                    "-inf".into()
                }
            } else {
                format!("{f}")
            }
        }
        Value::Str(s) => format!("\"{}\"", escape_str(s)),
        Value::Path(p) => {
            let d = p.to_string_lossy();
            if d.contains(' ') {
                format!("\"{d}\"")
            } else {
                d.into_owned()
            }
        }
        Value::Glob(g) => g.pattern.clone(),
        Value::Regex(r) => format!("re\"{}\"", r.src),
        Value::Size(n) => render_size(*n),
        Value::Duration(ns) => render_duration(*ns),
        Value::DateTime(z) => z.timestamp().to_string(),
        Value::Time(t) => render_time(t),
        Value::Bytes(b) => format!("bytes({})", render_size(b.len() as u64)),
        Value::List(xs) => {
            let items: Vec<String> = xs.iter().map(render_inline).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Record(r) => render_record_inline(r),
        Value::Table(rows) => {
            let items: Vec<String> = rows.iter().map(render_record_inline).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Range(r) => {
            format!(
                "{}{}{}",
                r.start,
                if r.inclusive { "..=" } else { ".." },
                r.end
            )
        }
        Value::Stream(s) => format!("stream<{}>", s.label),
        Value::Error(e) => format!("error({}: {})", e.code, e.msg),
        Value::Outcome(o) => match (&o.signal, o.status) {
            (Some(sig), _) => format!("outcome(signal: {sig}, ok: {})", o.ok),
            (None, s) => format!("outcome(status: {}, ok: {})", s.unwrap_or(-1), o.ok),
        },
        Value::Task(t) => format!("task({})", t.id),
        Value::Closure(c) => match &c.name {
            Some(n) => format!("fn {n}({})", param_names(c)),
            None => format!("closure({})", param_names(c)),
        },
        Value::CmdRef(c) => format!("command({})", c.head),
        Value::Secret(s) => format!("secret({})", s.name),
    }
}

fn param_names(c: &crate::ClosureVal) -> String {
    let mut names: Vec<String> = c.params.iter().map(|p| p.name.clone()).collect();
    if let Some(r) = &c.rest {
        names.push(format!("...{}", r.name));
    }
    names.join(", ")
}

/// Cell display form: like `render_inline` but strings/paths unquoted.
fn render_cell(v: &Value) -> String {
    match v {
        Value::Str(s) => s.replace('\n', "␤"),
        Value::Path(p) => p.to_string_lossy().into_owned(),
        Value::Null => String::new(),
        other => render_inline(other),
    }
}

fn truncate_display(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn pad_to(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

fn color_for_value(v: &Value) -> &'static str {
    match v {
        Value::Int(_) | Value::Float(_) | Value::Size(_) | Value::Duration(_) | Value::DateTime(_) | Value::Time(_) => "\x1b[36m",
        Value::Bool(_) | Value::Null => "\x1b[96m",
        Value::Str(_) => "\x1b[32m",
        Value::Path(p) => if p.is_dir() { "\x1b[34;1m" } else { "\x1b[39m" },
        Value::Glob(_) | Value::Regex(_) => "\x1b[95m",
        Value::Error(_) => "\x1b[31m",
        _ => "",
    }
}

/// Pretty table for `list<record>`-shaped data.
fn render_table(rows: &[Record], max_width: usize) -> String {
    if rows.is_empty() {
        return "(empty table)".to_string();
    }
    // Column order: first-seen key order across rows.
    let mut cols: Vec<String> = Vec::new();
    for r in rows {
        for k in r.keys() {
            if !cols.iter().any(|c| c == k) {
                cols.push(k.clone());
            }
        }
    }
    let cell_cap = 60usize;
    let mut widths: Vec<usize> = cols.iter().map(|c| c.width()).collect();
    let mut cells: Vec<Vec<(String, &'static str)>> = Vec::with_capacity(rows.len());
    for r in rows {
        let mut line = Vec::with_capacity(cols.len());
        for (i, c) in cols.iter().enumerate() {
            let val = r.get(c);
            let cell = val.map(render_cell).unwrap_or_default();
            let color = val.map(color_for_value).unwrap_or("");
            let cell = truncate_display(&cell, cell_cap);
            widths[i] = widths[i].max(cell.width());
            line.push((cell, color));
        }
        cells.push(line);
    }
    // Shrink to terminal width if needed (proportionally cap widest columns).
    let total: usize = widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * 2;
    if total > max_width && max_width > cols.len() * 4 {
        let over = total - max_width;
        if let Some(idx) = (0..widths.len()).max_by_key(|&i| widths[i]) {
            widths[idx] = widths[idx].saturating_sub(over).max(8);
        }
    }
    let mut out = String::new();
    let header: Vec<String> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| pad_to(&truncate_display(c, widths[i]), widths[i]))
        .collect();
    out.push_str(&header.join("  "));
    out.push('\n');
    let rule_width = widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * 2;
    out.push_str(&"─".repeat(rule_width.min(max_width)));
    out.push('\n');
    for line in &cells {
        let row: Vec<String> = line
            .iter()
            .enumerate()
            .map(|(i, (c, color))| {
                let padded = pad_to(&truncate_display(c, widths[i]), widths[i]);
                if color.is_empty() {
                    padded
                } else {
                    format!("{color}{padded}\x1b[0m")
                }
            })
            .collect();
        out.push_str(row.join("  ").trim_end());
        out.push('\n');
    }
    out.pop();
    out
}

/// Multi-line top-level rendering for the REPL.
pub fn render_block(v: &Value, width: usize) -> String {
    match v {
        Value::Null => String::new(),
        Value::Str(s) => s.clone(),
        Value::Table(rows) => render_table(rows, width),
        Value::List(xs) if !xs.is_empty() && xs.iter().all(|x| matches!(x, Value::Record(_))) => {
            let rows: Vec<Record> = xs
                .iter()
                .map(|x| match x {
                    Value::Record(r) => r.clone(),
                    _ => unreachable!(),
                })
                .collect();
            render_table(&rows, width)
        }
        Value::List(xs) => {
            let lines: Vec<String> = xs.iter().map(|x| render_cell(x).to_string()).collect();
            lines.join("\n")
        }
        Value::Record(r) => {
            let keyw = r.keys().map(|k| k.width()).max().unwrap_or(0);
            let lines: Vec<String> = r
                .iter()
                .map(|(k, v)| format!("{}  {}", pad_to(k, keyw), render_cell(v)))
                .collect();
            lines.join("\n")
        }
        Value::Outcome(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            if text.is_empty() {
                render_inline(v)
            } else {
                text.to_string()
            }
        }
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Error(e) => {
            let mut s = format!("error({}): {}", e.code, e.msg);
            if let Some(h) = &e.hint {
                s.push_str(&format!("\n  hint: {h}"));
            }
            s
        }
        other => render_inline(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(render_size(237), "237b");
        assert_eq!(render_size(1_500_000), "1.5mb");
        assert_eq!(render_size(1024), "1.02kb");
        assert_eq!(render_size(999_999), "1mb");
        assert_eq!(render_size(1_500_000_000), "1.5gb");
    }

    #[test]
    fn durations() {
        assert_eq!(render_duration(0), "0s");
        assert_eq!(render_duration(250_000_000), "250ms");
        assert_eq!(render_duration(90_000_000_000), "1m30s");
        assert_eq!(render_duration(1_500_000_000), "1s500ms");
        assert_eq!(render_duration(-1_000_000_000), "-1s");
    }

    #[test]
    fn inline_forms() {
        assert_eq!(render_inline(&Value::Str("a b".into())), "\"a b\"");
        assert_eq!(
            render_inline(&Value::List(vec![Value::Int(1), Value::Str("x".into())])),
            "[1, \"x\"]"
        );
        let mut r = Record::new();
        r.insert("name".into(), Value::Str("x".into()));
        r.insert("n".into(), Value::Int(3));
        assert_eq!(render_inline(&Value::Record(r)), "{name: \"x\", n: 3}");
    }

    #[test]
    fn table_render() {
        let mut a = Record::new();
        a.insert("name".into(), Value::Str("foo.rs".into()));
        a.insert("size".into(), Value::Size(1500));
        let mut b = Record::new();
        b.insert("name".into(), Value::Str("bar.rs".into()));
        b.insert("size".into(), Value::Size(999_999));
        let out = render_block(&Value::Table(vec![a, b]), 80);
        assert!(out.contains("name"));
        assert!(out.contains("1.5kb"));
        assert!(out.contains("1mb"));
    }
}
