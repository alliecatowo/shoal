//! `value.get` path resolution and the wire encoding / elision rule
//! (AGENT-SURFACE §3/§7). Split out of `lib.rs` (docs/ROADMAP.md wave R4,
//! scratch/audit-arch.md W1.3): pure mechanical move, zero behavior change.
use super::*;

/// `value.get`'s `path` grammar (TDD §7, AGENT-SURFACE §1): dot fields,
/// `[n]` indexes, and `[a..b]` half-open ranges — e.g. `rows[3].name`,
/// `out.lines[0]`, `rows[0..5]`. Structural fields on non-`Record` values
/// (outcome/error/range/task/table) are synthesized so an agent can walk
/// into them the same way it would a plain record.
#[derive(Debug, Clone)]
enum PathSeg {
    Field(String),
    Index(usize),
    Range(usize, usize),
}

fn parse_value_path(path: &str) -> Result<Vec<PathSeg>, String> {
    let mut segs = Vec::new();
    let bytes = path.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        match bytes[i] {
            b'.' => {
                i += 1;
                continue;
            }
            b'[' => {
                let close = path[i + 1..]
                    .find(']')
                    .map(|p| p + i + 1)
                    .ok_or_else(|| format!("unterminated `[` in path `{path}`"))?;
                let digits = &path[i + 1..close];
                // `[a..b]` half-open range (AGENT-SURFACE §1) — this used to
                // be rejected as a "bad index" despite the doc promising it.
                if let Some((a, b)) = digits.split_once("..") {
                    let a = a
                        .parse::<usize>()
                        .map_err(|_| format!("bad range start `{a}` in path `{path}`"))?;
                    let b = b
                        .parse::<usize>()
                        .map_err(|_| format!("bad range end `{b}` in path `{path}`"))?;
                    segs.push(PathSeg::Range(a, b));
                } else {
                    let idx = digits
                        .parse::<usize>()
                        .map_err(|_| format!("bad index `{digits}` in path `{path}`"))?;
                    segs.push(PathSeg::Index(idx));
                }
                i = close + 1;
                continue;
            }
            _ => {}
        }
        let start = i;
        while i < n && bytes[i] != b'.' && bytes[i] != b'[' {
            i += 1;
        }
        if i == start {
            return Err(format!("empty path segment in `{path}`"));
        }
        segs.push(PathSeg::Field(path[start..i].to_string()));
    }
    Ok(segs)
}

fn path_field(value: &Value, name: &str) -> Result<Value, String> {
    match value {
        Value::Record(rec) => rec
            .get(name)
            .cloned()
            .ok_or_else(|| format!("record has no field `{name}`")),
        Value::Outcome(o) => Ok(match name {
            "status" => o
                .status
                .map(|s| Value::Int(s as i64))
                .unwrap_or(Value::Null),
            "ok" => Value::Bool(o.ok),
            "signal" => o.signal.clone().map(Value::Str).unwrap_or(Value::Null),
            "out" => o.out_value(),
            "stdout" => Value::Bytes(o.stdout.clone()),
            "stderr" => Value::Bytes(o.stderr.clone()),
            "dur_ns" => Value::Duration(o.dur_ns),
            "pid" => Value::Int(o.pid as i64),
            "cmd" => Value::Str(o.cmd.clone()),
            // Unknown field names forward to the structured `.out` value,
            // mirroring eval's Value::Outcome field-access contract.
            _ => return path_field(&o.out_value(), name),
        }),
        Value::Error(e) => Ok(match name {
            "code" => Value::Str(e.code.clone()),
            "msg" => Value::Str(e.msg.clone()),
            "hint" => e.hint.clone().map(Value::Str).unwrap_or(Value::Null),
            "stderr" => e.stderr.clone().map(Value::Str).unwrap_or(Value::Null),
            "status" => e
                .status
                .map(|s| Value::Int(s as i64))
                .unwrap_or(Value::Null),
            _ => return Err(format!("error has no field `{name}`")),
        }),
        Value::Range(r) => Ok(match name {
            "start" => Value::Int(r.start),
            "end" => Value::Int(r.end),
            "inclusive" => Value::Bool(r.inclusive),
            _ => return Err(format!("range has no field `{name}`")),
        }),
        Value::Task(t) => Ok(match name {
            "id" => Value::Int(t.id as i64),
            "done" => Value::Bool(t.is_done()),
            _ => return Err(format!("task has no field `{name}`")),
        }),
        Value::Table(rows) => {
            if name == "rows" {
                Ok(Value::List(
                    rows.iter().cloned().map(Value::Record).collect(),
                ))
            } else if rows.iter().any(|r| r.contains_key(name)) {
                Ok(Value::List(
                    rows.iter()
                        .map(|r| r.get(name).cloned().unwrap_or(Value::Null))
                        .collect(),
                ))
            } else {
                Err(format!("table has no column `{name}`"))
            }
        }
        other => Err(format!(
            "cannot access field `{name}` on {}",
            other.type_name()
        )),
    }
}

fn path_index(value: &Value, idx: usize) -> Result<Value, String> {
    match value {
        Value::List(items) => items
            .get(idx)
            .cloned()
            .ok_or_else(|| format!("index [{idx}] out of bounds (len {})", items.len())),
        Value::Table(rows) => rows
            .get(idx)
            .cloned()
            .map(Value::Record)
            .ok_or_else(|| format!("index [{idx}] out of bounds (len {})", rows.len())),
        other => Err(format!("cannot index {} with [{idx}]", other.type_name())),
    }
}

/// `[a..b]` — half-open, saturating at the collection's length (the same
/// clamp `value.get`'s top-level `slice` parameter uses).
fn path_range(value: &Value, a: usize, b: usize) -> Result<Value, String> {
    match value {
        Value::List(items) => {
            let start = a.min(items.len());
            let end = b.max(start).min(items.len());
            Ok(Value::List(items[start..end].to_vec()))
        }
        Value::Table(rows) => {
            let start = a.min(rows.len());
            let end = b.max(start).min(rows.len());
            Ok(Value::Table(rows[start..end].to_vec()))
        }
        other => Err(format!("cannot range-slice {}", other.type_name())),
    }
}

pub(crate) fn resolve_value_path(value: &Value, path: &str) -> Result<Value, String> {
    let mut current = value.clone();
    for seg in parse_value_path(path)? {
        current = match seg {
            PathSeg::Field(name) => path_field(&current, &name)?,
            PathSeg::Index(idx) => path_index(&current, idx)?,
            PathSeg::Range(a, b) => path_range(&current, a, b)?,
        };
    }
    Ok(current)
}

/// `Outcome`'s wire `span` (AGENT-SURFACE §2: "source span of the
/// invocation"), honestly reported.
///
/// `OutcomeVal` (`crates/shoal-value/src/outcome.rs`) now carries an
/// `Option<Span>`, stamped on the command spawn path
/// (`crates/shoal-eval/src/command.rs`) with the *same* `span` the sibling
/// error path hands to `ErrorVal::with_span` — so a command's success and
/// failure report an identical source anchor here. Outcomes with no
/// invocation site in scope (builtin-wrapped results, values reconstructed
/// from the journal) carry `None`, and the wire omits the field entirely
/// (`skip_serializing_if`) rather than fabricating one. Mirrors exactly how
/// `wire_value` encodes `ErrorVal`'s span a few arms below.
fn outcome_span(o: &shoal_value::OutcomeVal) -> Option<WireSpan> {
    o.span.map(|s| WireSpan {
        start: s.start,
        end: s.end,
    })
}

pub(crate) fn wire_value(value: &Value) -> WireValue {
    match value {
        Value::Null => WireValue::Null,
        Value::Bool(v) => WireValue::Bool { v: *v },
        Value::Int(v) => WireValue::Int { v: *v },
        Value::Float(v) => WireValue::Float { v: *v },
        Value::Str(v) => WireValue::Str { v: v.clone() },
        Value::Path(v) => {
            let p = WirePath::encode(v.as_os_str());
            WireValue::Path {
                v: p.display,
                raw: p.raw,
            }
        }
        Value::Glob(g) => WireValue::Glob {
            pattern: g.pattern.clone(),
        },
        Value::Regex(r) => WireValue::Regex { src: r.src.clone() },
        Value::Size(v) => WireValue::Size { v: *v },
        Value::Duration(v) => WireValue::Duration { v: *v },
        Value::DateTime(z) => WireValue::DateTime {
            v: z.timestamp().to_string(),
        },
        Value::Time(t) => WireValue::Time {
            v: format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec),
        },
        Value::Bytes(v) => WireValue::Bytes {
            v: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &**v),
        },
        Value::List(v) => WireValue::List {
            v: v.iter().map(wire_value).collect(),
        },
        Value::Record(rec) => WireValue::Record {
            v: rec
                .iter()
                .map(|(k, v)| (k.clone(), wire_value(v)))
                .collect(),
        },
        Value::Table(rows) => {
            let mut names: Vec<&String> = Vec::new();
            for row in rows {
                for k in row.keys() {
                    if !names.contains(&k) {
                        names.push(k);
                    }
                }
            }
            let cols = names
                .into_iter()
                .map(|name| {
                    let col = rows
                        .iter()
                        .map(|row| row.get(name).map(wire_value).unwrap_or(WireValue::Null))
                        .collect();
                    (name.clone(), col)
                })
                .collect();
            WireValue::Table {
                cols,
                n: rows.len(),
            }
        }
        Value::Range(r) => WireValue::Range {
            start: r.start,
            end: r.end,
            inclusive: r.inclusive,
        },
        Value::Stream(s) => WireValue::Stream {
            label: s.label.clone(),
        },
        Value::Error(e) => WireValue::Error {
            code: e.code.clone(),
            msg: e.msg.clone(),
            span: e.span.map(|s| WireSpan {
                start: s.start,
                end: s.end,
            }),
            hint: e.hint.clone(),
            stderr: e.stderr.clone(),
        },
        Value::Outcome(o) => WireValue::Outcome {
            status: o.status,
            ok: o.ok,
            signal: o.signal.clone(),
            out: Box::new(wire_value(&o.out_value())),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            dur_ns: o.dur_ns,
            pid: o.pid,
            cmd: o.cmd.clone(),
            span: outcome_span(o),
        },
        Value::Task(t) => WireValue::Task {
            id: t.id,
            done: t.is_done(),
        },
        Value::Closure(_) | Value::CmdRef(_) => {
            let repr = shoal_value::render::render_inline(value);
            if matches!(value, Value::Closure(_)) {
                WireValue::Closure { repr }
            } else {
                WireValue::Cmd { repr }
            }
        }
        Value::Secret(s) => WireValue::Secret {
            name: s.name.clone(),
        },
    }
}

// ---------------------------------------------------------------------------
// The elision rule (AGENT-SURFACE §3) — wire-level, automatic.
// ---------------------------------------------------------------------------

/// Kernel defaults; a caller's `elide` param may tighten or loosen these, but
/// `max_bytes`/`max_bytes_raw` never loosen past `ELIDE_HARD_CAP`.
pub(crate) const ELIDE_DEFAULT_MAX_BYTES: usize = 8 * 1024;
pub(crate) const ELIDE_DEFAULT_MAX_ROWS: usize = 100;
pub(crate) const ELIDE_DEFAULT_MAX_BYTES_RAW: usize = 4 * 1024;
pub(crate) const ELIDE_DEFAULT_MAX_ITEMS: usize = 500;
/// A misbehaving agent cannot flood itself: no per-call override widens the
/// byte budget past this, regardless of what it asks for.
pub(crate) const ELIDE_HARD_CAP: usize = 64 * 1024;
/// Rows/items kept in the `preview` field and the human `render_head`.
const ELIDE_PREVIEW_ITEMS: usize = 5;
const ELIDE_PREVIEW_BYTES: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct ElideBudget {
    pub(crate) max_bytes: usize,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes_raw: usize,
    pub(crate) max_items: usize,
}

impl Default for ElideBudget {
    fn default() -> Self {
        Self {
            max_bytes: ELIDE_DEFAULT_MAX_BYTES,
            max_rows: ELIDE_DEFAULT_MAX_ROWS,
            max_bytes_raw: ELIDE_DEFAULT_MAX_BYTES_RAW,
            max_items: ELIDE_DEFAULT_MAX_ITEMS,
        }
    }
}

impl ElideBudget {
    pub(crate) fn from_spec(spec: Option<&ElideSpec>) -> Self {
        let mut budget = Self::default();
        if let Some(spec) = spec {
            if let Some(max_bytes) = spec.max_bytes {
                let clamped = max_bytes.min(ELIDE_HARD_CAP);
                budget.max_bytes = clamped;
                budget.max_bytes_raw = clamped;
            }
            if let Some(max_rows) = spec.max_rows {
                budget.max_rows = max_rows;
            }
            if let Some(max_items) = spec.max_items {
                budget.max_items = max_items;
            }
        }
        budget
    }
}

/// `shoal://kind/id[?path=...]` from a short ref (`kind:id`), per
/// AGENT-SURFACE §1.
pub(crate) fn short_ref_to_uri(r: &Ref, path: Option<&str>) -> String {
    let mut uri = match r.0.split_once(':') {
        Some((kind, rest)) => format!("shoal://{kind}/{rest}"),
        None => format!("shoal://{}", r.0),
    };
    if let Some(path) = path.filter(|p| !p.is_empty()) {
        uri.push_str("?path=");
        uri.push_str(path);
    }
    uri
}

/// A small, bounded stand-in for `value` — first `ELIDE_PREVIEW_ITEMS`
/// rows/items, or the first `ELIDE_PREVIEW_BYTES` bytes/chars — never the
/// full payload, by construction (it never passes an unbounded child
/// through unchanged).
fn preview_value(value: &Value) -> Value {
    match value {
        Value::Table(rows) => {
            Value::Table(rows.iter().take(ELIDE_PREVIEW_ITEMS).cloned().collect())
        }
        Value::List(items) => {
            Value::List(items.iter().take(ELIDE_PREVIEW_ITEMS).cloned().collect())
        }
        Value::Bytes(b) => Value::Bytes(std::sync::Arc::new(
            b.iter().take(ELIDE_PREVIEW_BYTES).copied().collect(),
        )),
        Value::Str(s) => Value::Str(s.chars().take(ELIDE_PREVIEW_BYTES).collect()),
        Value::Record(rec) => Value::Record(
            rec.keys()
                .take(ELIDE_PREVIEW_ITEMS)
                .map(|k| (k.clone(), Value::Null))
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// Column name -> type name, from the first row that carries each key.
fn table_cols(rows: &[shoal_value::Record]) -> std::collections::BTreeMap<String, String> {
    let mut cols = std::collections::BTreeMap::new();
    for row in rows {
        for (k, v) in row {
            cols.entry(k.clone())
                .or_insert_with(|| v.type_name().to_string());
        }
    }
    cols
}

/// `<uri>?path=<sub>`, chaining onto any path already present so a nested
/// drill (e.g. a successful command's `.out`) stays reachable through
/// `value.get`.
fn join_path_uri(uri: &str, sub_path: &str) -> String {
    match uri.split_once("?path=") {
        Some((base, existing)) => format!("{base}?path={existing}.{sub_path}"),
        None => format!("{uri}?path={sub_path}"),
    }
}

/// The elision rule (AGENT-SURFACE §3): if `value`'s wire encoding exceeds
/// `budget`, or it is an over-threshold table/list/bytes, emit an elided
/// `WireValue::Ref` (shape + small preview + render head) instead of the
/// payload. `uri` is how a caller re-fetches the full value later.
///
/// A successful `Outcome` whose structured `.out` is what actually carries
/// size (table/list/bytes/big string) is unwrapped one level for the
/// elision *decision* — mirroring `render_block`'s outcome-unification (P1c):
/// `ls` reads as a table to the elision rule too, not as an opaque
/// `outcome` wrapper. The outer outcome fields (`status`/`ok`/`cmd`/…)
/// always travel; only `.out` itself is replaced with the elided form.
pub(crate) fn elide_wire_value(value: &Value, uri: &str, budget: &ElideBudget) -> WireValue {
    if let Value::Outcome(o) = value
        && o.ok
    {
        let out_value = o.out_value();
        let out_uri = join_path_uri(uri, "out");
        return WireValue::Outcome {
            status: o.status,
            ok: o.ok,
            signal: o.signal.clone(),
            out: Box::new(elide_wire_value(&out_value, &out_uri, budget)),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
            dur_ns: o.dur_ns,
            pid: o.pid,
            cmd: o.cmd.clone(),
            span: outcome_span(o),
        };
    }
    let wire = wire_value(value);
    let encoded_len = serde_json::to_vec(&wire)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    let too_big = encoded_len > budget.max_bytes
        || matches!(value, Value::Table(rows) if rows.len() > budget.max_rows)
        || matches!(value, Value::List(items) if items.len() > budget.max_items)
        || matches!(value, Value::Bytes(b) if b.len() > budget.max_bytes_raw);
    if !too_big {
        return wire;
    }
    let n = match value {
        Value::Table(rows) => rows.len(),
        Value::List(items) => items.len(),
        Value::Bytes(b) => b.len(),
        Value::Str(s) => s.len(),
        Value::Record(rec) => rec.len(),
        _ => 1,
    };
    let cols = match value {
        Value::Table(rows) => Some(table_cols(rows)),
        _ => None,
    };
    let preview = preview_value(value);
    let render_head = shoal_value::render::render_block(&preview, 80)
        .lines()
        .take(10)
        .collect::<Vec<_>>()
        .join("\n");
    WireValue::Ref {
        uri: uri.to_string(),
        of: value.type_name().to_string(),
        n,
        cols,
        preview: Box::new(wire_value(&preview)),
        render_head,
    }
}

/// Strip ANSI escape sequences (SGR color codes and other CSI-final-byte
/// sequences — cursor movement, etc.) from `s`.
///
/// `shoal_value::render::render_block`/`render_inline` unconditionally emit
/// ANSI (`color_for_value` et al. in `shoal-value/src/render.rs`) — fine for
/// `shoal`'s own interactive REPL, which reads a real terminal and wants the
/// color, but agent-hostile noise on the kernel/MCP wire: `session.attach`
/// forces every kernel exec headless (`evaluator.interactive = false`,
/// `handlers_exec.rs`) and a cold-agent field test found the escape bytes
/// still landing verbatim in `structuredContent.render` and `content[].text`
/// — an agent has no terminal to interpret them, so they read as junk
/// characters. Delegates to the `vte`-based `strip-ansi-escapes` crate
/// (already resolved in the workspace's dependency graph via `reedline`,
/// `shoal`'s REPL line-editor) rather than a hand-rolled regex, so this
/// correctly handles the full ECMA-48 CSI grammar (`ESC '[' params
/// intermediates final`), not just the `ESC [ ... m` SGR subset.
pub(crate) fn strip_ansi(s: &str) -> String {
    strip_ansi_escapes::strip_str(s)
}

/// Bound a human render string to `ELIDE_HARD_CAP`, the same hard cap
/// `shoal-mcp`'s `content[0].text` is bounded to (AGENT-SURFACE §3). Without
/// this, `ExecResult.render`/`value.get`'s `format=render` response can carry
/// an arbitrarily large render string (e.g. a huge outcome's ANSI-laden
/// stdout) right next to a properly-elided structured `value` — the render
/// field bypassing the wall the structured value already respects. Applied
/// at the wire boundary (here) rather than only at the MCP facade so every
/// kernel client, not just `shoal-mcp`, gets the same honest bound.
///
/// `strip` is `true` on the headless/MCP path (the attaching client did not
/// declare itself a real tty — `Attachment::tty`) and `false` for a genuine
/// interactive kernel-hosted client; stripping happens *before* bounding so
/// the byte budget is spent on content, not escape codes a client can't use
/// anyway.
///
/// Keeps a head of whole lines under the budget and appends a
/// `…(N more lines, fetch via <uri>)` marker — mirroring `shoal-mcp::tools::
/// bound_text`'s truncation shape so an agent sees the same "how do I get
/// the rest" hint everywhere a render is bounded.
pub(crate) fn bound_render(render: String, uri: &str, strip: bool) -> String {
    let render = if strip { strip_ansi(&render) } else { render };
    if render.len() <= ELIDE_HARD_CAP {
        return render;
    }
    let budget = ELIDE_HARD_CAP.saturating_sub(96);
    let total_lines = render.lines().count();
    let mut head = String::new();
    let mut kept = 0usize;
    for line in render.lines() {
        if head.len() + line.len() + 1 > budget {
            break;
        }
        head.push_str(line);
        head.push('\n');
        kept += 1;
    }
    // Degenerate case: a single line longer than the whole budget.
    if head.is_empty() {
        head = render.chars().take(budget).collect();
    }
    let remaining = total_lines.saturating_sub(kept);
    format!("{head}…({remaining} more lines, fetch via {uri})")
}
