//! shoal-value — the runtime value model for the shoal shell.
//!
//! Types per TDD §4.1. `path` is bytes-backed (`PathBuf`/`OsString`); `secret`
//! is opaque; `outcome` is an external command's result; `stream` is
//! single-consumption; equality is structural for data types and identity for
//! `task`/`stream`.

pub mod methods;
pub mod ops;
pub mod render;

use indexmap::IndexMap;
use shoal_ast as ast;
use shoal_ast::Span;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Ordered string-keyed record.
pub type Record = IndexMap<String, Value>;

/// Result type for all value-level operations.
pub type VResult<T> = Result<T, ErrorVal>;

// ---------------------------------------------------------------------------
// Value
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Bytes-backed filesystem path (TDD §13.1).
    Path(PathBuf),
    Glob(GlobVal),
    Regex(Arc<RegexVal>),
    /// Bytes (decimal size literal), e.g. `1.5gb`.
    Size(u64),
    /// Nanoseconds, e.g. `250ms`.
    Duration(i64),
    DateTime(Box<jiff::Zoned>),
    Time(TimeVal),
    Bytes(Arc<Vec<u8>>),
    List(Vec<Value>),
    Record(Record),
    /// Semantically a `list<record>`; rendered as a table.
    Table(Vec<Record>),
    Range(RangeVal),
    Stream(StreamVal),
    Error(Arc<ErrorVal>),
    Outcome(Arc<OutcomeVal>),
    Task(TaskVal),
    Closure(Arc<ClosureVal>),
    /// Alias / partial command application (TDD §1.8).
    CmdRef(Arc<ast::CmdCall>),
    Secret(SecretVal),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Path(_) => "path",
            Value::Glob(_) => "glob",
            Value::Regex(_) => "regex",
            Value::Size(_) => "size",
            Value::Duration(_) => "duration",
            Value::DateTime(_) => "datetime",
            Value::Time(_) => "time",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Record(_) => "record",
            Value::Table(_) => "table",
            Value::Range(_) => "range",
            Value::Stream(_) => "stream",
            Value::Error(_) => "error",
            Value::Outcome(_) => "outcome",
            Value::Task(_) => "task",
            Value::Closure(_) => "closure",
            Value::CmdRef(_) => "command",
            Value::Secret(_) => "secret",
        }
    }

    /// Condition coercion (TDD §1.10): `bool` is itself; an `outcome`'s truth
    /// is its success. Everything else is a type error.
    pub fn as_condition(&self) -> VResult<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Outcome(o) => Ok(o.ok),
            other => Err(ErrorVal::new(
                "type_error",
                format!("expected bool in condition, found {}", other.type_name()),
            )
            .with_hint("shoal has no truthiness — try .is_empty(), .is_some(), or != null")),
        }
    }

    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Closure(_) | Value::CmdRef(_))
    }
}

// ---------------------------------------------------------------------------
// Supporting payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct GlobVal {
    pub pattern: String,
    /// Origin cwd — expansion always happens against this (TDD §4.3).
    pub cwd: PathBuf,
    pub hidden: bool,
}

#[derive(Debug)]
pub struct RegexVal {
    pub src: String,
    pub re: regex::Regex,
}

impl RegexVal {
    pub fn compile(src: &str) -> VResult<RegexVal> {
        regex::Regex::new(src)
            .map(|re| RegexVal { src: src.to_string(), re })
            .map_err(|e| ErrorVal::new("arg_error", format!("invalid regex: {e}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeVal {
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeVal {
    pub start: i64,
    pub end: i64,
    pub inclusive: bool,
}

impl RangeVal {
    pub fn iter(&self) -> impl Iterator<Item = i64> + Send + use<> {
        let (start, end, inclusive) = (self.start, self.end, self.inclusive);
        let last = if inclusive { end } else { end.saturating_sub(1) };
        start..=last
    }
    pub fn len(&self) -> usize {
        let last = if self.inclusive { self.end } else { self.end - 1 };
        if last < self.start { 0 } else { (last - self.start + 1) as usize }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn contains(&self, v: i64) -> bool {
        v >= self.start && (if self.inclusive { v <= self.end } else { v < self.end })
    }
}

/// A command's result (TDD §4.1). `out` is parsed lazily on first structured
/// access; the raw bytes are always retained.
#[derive(Debug)]
pub struct OutcomeVal {
    pub status: Option<i32>,
    /// Signal name (`"SIGSEGV"`) when the child died to a signal (TDD §13.6).
    pub signal: Option<String>,
    pub ok: bool,
    pub stdout: Arc<Vec<u8>>,
    pub stderr: Arc<Vec<u8>>,
    pub dur_ns: i64,
    pub pid: u32,
    /// Display form of the invocation, for errors and rendering.
    pub cmd: String,
}

impl OutcomeVal {
    /// `outcome.out` — utf-8 text with the trailing newline trimmed; if the
    /// payload parses as JSON it becomes structured data (T1, lazy).
    pub fn out_value(&self) -> Value {
        let text = String::from_utf8_lossy(&self.stdout);
        let trimmed = text.strip_suffix('\n').unwrap_or(&text);
        let first = trimmed.trim_start().chars().next();
        if matches!(first, Some('{') | Some('[')) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
                return json_to_value(&json);
            }
        }
        Value::Str(trimmed.to_string())
    }
}

pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(xs) => {
            let vals: Vec<Value> = xs.iter().map(json_to_value).collect();
            // A uniform non-empty array of objects is a table.
            if !vals.is_empty() && vals.iter().all(|v| matches!(v, Value::Record(_))) {
                Value::Table(
                    vals.into_iter()
                        .map(|v| match v {
                            Value::Record(r) => r,
                            _ => unreachable!(),
                        })
                        .collect(),
                )
            } else {
                Value::List(vals)
            }
        }
        serde_json::Value::Object(m) => {
            Value::Record(m.iter().map(|(k, v)| (k.clone(), json_to_value(v))).collect())
        }
    }
}

pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Str(s) => json!(s),
        Value::Path(p) => json!(p.to_string_lossy()),
        Value::Glob(g) => json!(g.pattern),
        Value::Regex(r) => json!(r.src),
        Value::Size(n) => json!(n),
        Value::Duration(ns) => json!(ns),
        Value::DateTime(z) => json!(z.to_string()),
        Value::Time(t) => json!(render::render_time(t)),
        Value::Bytes(b) => json!(String::from_utf8_lossy(b)),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Record(r) => {
            serde_json::Value::Object(r.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect())
        }
        Value::Table(rows) => serde_json::Value::Array(
            rows.iter()
                .map(|r| {
                    serde_json::Value::Object(
                        r.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect(),
                    )
                })
                .collect(),
        ),
        Value::Range(r) => serde_json::Value::Array(r.iter().map(|i| json!(i)).collect()),
        Value::Outcome(o) => json!({
            "status": o.status, "ok": o.ok,
            "out": String::from_utf8_lossy(&o.stdout),
            "err": String::from_utf8_lossy(&o.stderr),
        }),
        Value::Error(e) => json!({"code": e.code, "msg": e.msg}),
        Value::Secret(s) => json!(format!("secret({})", s.name)),
        other => json!(render::render_inline(other)),
    }
}

/// Single-consumption stream (TDD §1.9). Identity equality.
#[derive(Clone)]
pub struct StreamVal {
    pub label: String,
    inner: Arc<Mutex<StreamState>>,
}

enum StreamState {
    Ready(Box<dyn Iterator<Item = VResult<Value>> + Send>),
    Consumed,
}

impl std::fmt::Debug for StreamVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stream<{}>", self.label)
    }
}

impl StreamVal {
    pub fn from_iter<I>(label: impl Into<String>, iter: I) -> StreamVal
    where
        I: Iterator<Item = VResult<Value>> + Send + 'static,
    {
        StreamVal {
            label: label.into(),
            inner: Arc::new(Mutex::new(StreamState::Ready(Box::new(iter)))),
        }
    }

    /// Take the underlying iterator; second call is `stream_consumed`.
    pub fn take(&self) -> VResult<Box<dyn Iterator<Item = VResult<Value>> + Send>> {
        let mut g = self.inner.lock().unwrap();
        match std::mem::replace(&mut *g, StreamState::Consumed) {
            StreamState::Ready(it) => Ok(it),
            StreamState::Consumed => Err(ErrorVal::new("stream_consumed", "stream already consumed")
                .with_hint("collect first (`.collect()`), or `.tee(2)` to split")),
        }
    }

    pub fn same(&self, other: &StreamVal) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct TaskVal {
    pub id: u64,
    pub shared: Arc<TaskShared>,
}

pub struct TaskShared {
    pub desc: String,
    state: Mutex<TaskState>,
    cond: Condvar,
    cancel_requested: AtomicBool,
    /// Hooks run on cancel (e.g. cancel exec tokens of children).
    on_cancel: Mutex<Vec<Box<dyn Fn() + Send>>>,
}

impl std::fmt::Debug for TaskShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TaskShared({})", self.desc)
    }
}

enum TaskState {
    Running,
    Done(VResult<Value>),
}

impl TaskVal {
    pub fn new(desc: impl Into<String>) -> TaskVal {
        TaskVal {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            shared: Arc::new(TaskShared {
                desc: desc.into(),
                state: Mutex::new(TaskState::Running),
                cond: Condvar::new(),
                cancel_requested: AtomicBool::new(false),
                on_cancel: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn finish(&self, result: VResult<Value>) {
        let mut g = self.shared.state.lock().unwrap();
        *g = TaskState::Done(result);
        self.shared.cond.notify_all();
    }

    pub fn wait(&self) -> VResult<Value> {
        let mut g = self.shared.state.lock().unwrap();
        loop {
            match &*g {
                TaskState::Done(r) => return r.clone(),
                TaskState::Running => g = self.shared.cond.wait(g).unwrap(),
            }
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(&*self.shared.state.lock().unwrap(), TaskState::Done(_))
    }

    pub fn cancel(&self) {
        self.shared.cancel_requested.store(true, Ordering::SeqCst);
        for hook in self.shared.on_cancel.lock().unwrap().iter() {
            hook();
        }
    }

    pub fn cancel_requested(&self) -> bool {
        self.shared.cancel_requested.load(Ordering::SeqCst)
    }

    pub fn on_cancel(&self, hook: Box<dyn Fn() + Send>) {
        if self.cancel_requested() {
            hook();
        } else {
            self.shared.on_cancel.lock().unwrap().push(hook);
        }
    }

    pub fn same(&self, other: &TaskVal) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

// ---------------------------------------------------------------------------
// Closures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ClosureVal {
    /// `None` for lambdas; `Some` for `fn` declarations (drives `--help`).
    pub name: Option<String>,
    pub params: Vec<ast::Param>,
    pub rest: Option<ast::RestParam>,
    pub ret: Option<ast::Type>,
    pub body: ast::Expr,
    pub env: Env,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SecretVal {
    pub name: String,
    /// The secret material; never rendered, never journaled.
    pub value: Arc<str>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A shoal error value. Codes are pinned in docs/CONTRACTS.md §4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorVal {
    pub code: String,
    pub msg: String,
    pub span: Option<Span>,
    pub hint: Option<String>,
    pub stderr: Option<String>,
}

impl ErrorVal {
    pub fn new(code: impl Into<String>, msg: impl Into<String>) -> ErrorVal {
        ErrorVal { code: code.into(), msg: msg.into(), span: None, hint: None, stderr: None }
    }
    pub fn with_span(mut self, span: Span) -> ErrorVal {
        self.span = Some(span);
        self
    }
    /// Attach a span only if one isn't already present (innermost wins).
    pub fn or_span(mut self, span: Span) -> ErrorVal {
        self.span.get_or_insert(span);
        self
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> ErrorVal {
        self.hint = Some(hint.into());
        self
    }
    pub fn type_error(msg: impl Into<String>) -> ErrorVal {
        ErrorVal::new("type_error", msg)
    }
    pub fn arg_error(msg: impl Into<String>) -> ErrorVal {
        ErrorVal::new("arg_error", msg)
    }
}

impl std::fmt::Display for ErrorVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.msg)
    }
}

impl std::error::Error for ErrorVal {}

// ---------------------------------------------------------------------------
// Environment (lexical scopes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Env {
    inner: Arc<Mutex<EnvInner>>,
}

#[derive(Debug)]
struct EnvInner {
    vars: HashMap<String, Binding>,
    parent: Option<Env>,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    NotFound,
    Immutable,
}

impl Env {
    pub fn root() -> Env {
        Env { inner: Arc::new(Mutex::new(EnvInner { vars: HashMap::new(), parent: None })) }
    }

    pub fn child(&self) -> Env {
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: Some(self.clone()),
            })),
        }
    }

    pub fn declare(&self, name: impl Into<String>, value: Value, mutable: bool) {
        self.inner.lock().unwrap().vars.insert(name.into(), Binding { value, mutable });
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let parent = {
            let g = self.inner.lock().unwrap();
            if let Some(b) = g.vars.get(name) {
                return Some(b.value.clone());
            }
            g.parent.clone()
        };
        parent.and_then(|p| p.get(name))
    }

    pub fn is_bound(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Assign to an existing binding, walking up the scope chain.
    pub fn assign(&self, name: &str, value: Value) -> Result<(), AssignError> {
        let parent = {
            let mut g = self.inner.lock().unwrap();
            if let Some(b) = g.vars.get_mut(name) {
                if !b.mutable {
                    return Err(AssignError::Immutable);
                }
                b.value = value;
                return Ok(());
            }
            g.parent.clone()
        };
        match parent {
            Some(p) => p.assign(name, value),
            None => Err(AssignError::NotFound),
        }
    }

    /// Snapshot of every visible name (innermost shadowing wins) — for
    /// completion and introspection.
    pub fn visible_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = Some(self.clone());
        while let Some(env) = cur {
            let g = env.inner.lock().unwrap();
            for k in g.vars.keys() {
                if seen.insert(k.clone()) {
                    names.push(k.clone());
                }
            }
            cur = g.parent.clone();
        }
        names
    }
}

// ---------------------------------------------------------------------------
// Equality (structural for data, identity for stream/task)
// ---------------------------------------------------------------------------

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => a == b,
            // Mixed numeric equality compares numerically (promotion).
            (Int(a), Float(b)) | (Float(b), Int(a)) => (*a as f64) == *b,
            (Str(a), Str(b)) => a == b,
            (Path(a), Path(b)) => a == b,
            // A path and a str compare by display form (pragmatic equality).
            (Path(p), Str(s)) | (Str(s), Path(p)) => p.to_string_lossy() == *s,
            (Glob(a), Glob(b)) => a.pattern == b.pattern,
            (Regex(a), Regex(b)) => a.src == b.src,
            (Size(a), Size(b)) => a == b,
            (Duration(a), Duration(b)) => a == b,
            (DateTime(a), DateTime(b)) => a.timestamp() == b.timestamp(),
            (Time(a), Time(b)) => a == b,
            (Bytes(a), Bytes(b)) => a == b,
            (List(a), List(b)) => a == b,
            (Record(a), Record(b)) => a == b,
            (Table(a), Table(b)) => a == b,
            // A table is semantically a list<record>.
            (Table(t), List(l)) | (List(l), Table(t)) => {
                t.len() == l.len()
                    && t.iter().zip(l.iter()).all(|(r, v)| match v {
                        Record(vr) => r == vr,
                        _ => false,
                    })
            }
            (Range(a), Range(b)) => a == b,
            (Stream(a), Stream(b)) => a.same(b),
            (Error(a), Error(b)) => a == b,
            (Outcome(a), Outcome(b)) => Arc::ptr_eq(a, b),
            (Task(a), Task(b)) => a.same(b),
            (Closure(a), Closure(b)) => Arc::ptr_eq(a, b),
            (CmdRef(a), CmdRef(b)) => a == b,
            (Secret(a), Secret(b)) => a.name == b.name && a.value == b.value,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Eval ↔ methods bridge (pinned in docs/CONTRACTS.md §7)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CallArgs {
    pub pos: Vec<Value>,
    pub named: Vec<(String, Value)>,
}

impl CallArgs {
    pub fn get_named(&self, name: &str) -> Option<&Value> {
        self.named.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

pub trait CallCtx {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value>;
    fn cwd(&self) -> PathBuf;
}

// ---------------------------------------------------------------------------
// Word parsing helpers (bind-time coercion, TDD §4.2 site 2)
// ---------------------------------------------------------------------------

/// Parse a size word like `1.5gb`, `4kib`, `237b`. Decimal units and binary
/// (`*ib`) units per TDD §2.1.
pub fn parse_size(word: &str) -> Option<u64> {
    let lower = word.to_ascii_lowercase();
    let split = lower.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = lower.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    if num < 0.0 {
        return None;
    }
    Some((num * mult).round() as u64)
}

/// Parse a duration word like `250ms`, `1.5h`, `30d`, or compound `1m30s`.
pub fn parse_duration(word: &str) -> Option<i64> {
    let lower = word.to_ascii_lowercase();
    let (neg, rest) = match lower.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, lower.as_str()),
    };
    let mut total: f64 = 0.0;
    let mut cur = rest;
    let mut any = false;
    while !cur.is_empty() {
        let split = cur.find(|c: char| c.is_ascii_alphabetic())?;
        if split == 0 {
            return None;
        }
        let (num, tail) = cur.split_at(split);
        let unit_end = tail.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(tail.len());
        let (unit, next) = tail.split_at(unit_end);
        let num: f64 = num.parse().ok()?;
        let ns: f64 = match unit {
            "ns" => 1.0,
            "us" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3_600e9,
            "d" => 86_400e9,
            "w" => 604_800e9,
            _ => return None,
        };
        total += num * ns;
        cur = next;
        any = true;
    }
    if !any {
        return None;
    }
    let v = total.round() as i64;
    Some(if neg { -v } else { v })
}

/// Parse a time word like `10:00am`, `23:15`, `07:30:15`.
pub fn parse_time(word: &str) -> Option<TimeVal> {
    let lower = word.to_ascii_lowercase();
    let (body, meridiem) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut hour: u8 = parts[0].parse().ok()?;
    let min: u8 = parts[1].parse().ok()?;
    let sec: u8 = if parts.len() == 3 { parts[2].parse().ok()? } else { 0 };
    match meridiem {
        Some(pm) => {
            if hour == 0 || hour > 12 {
                return None;
            }
            if pm && hour != 12 {
                hour += 12;
            }
            if !pm && hour == 12 {
                hour = 0;
            }
        }
        None => {
            if hour > 23 {
                return None;
            }
        }
    }
    if min > 59 || sec > 59 {
        return None;
    }
    Some(TimeVal { hour, min, sec })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_scoping() {
        let root = Env::root();
        root.declare("x", Value::Int(1), false);
        let child = root.child();
        assert_eq!(child.get("x"), Some(Value::Int(1)));
        child.declare("x", Value::Int(2), true);
        assert_eq!(child.get("x"), Some(Value::Int(2)));
        assert_eq!(root.get("x"), Some(Value::Int(1)));
        assert_eq!(root.assign("x", Value::Int(9)), Err(AssignError::Immutable));
        assert_eq!(child.assign("x", Value::Int(3)), Ok(()));
        assert_eq!(child.get("x"), Some(Value::Int(3)));
    }

    #[test]
    fn stream_single_consumption() {
        let s = StreamVal::from_iter("int", (0..3).map(|i| Ok(Value::Int(i))));
        assert!(s.take().is_ok());
        let err = s.take().err().expect("second take must fail");
        assert_eq!(err.code, "stream_consumed");
    }

    #[test]
    fn parse_units() {
        assert_eq!(parse_size("1.5gb"), Some(1_500_000_000));
        assert_eq!(parse_size("4kib"), Some(4096));
        assert_eq!(parse_size("237b"), Some(237));
        assert_eq!(parse_size("nope"), None);
        assert_eq!(parse_duration("250ms"), Some(250_000_000));
        assert_eq!(parse_duration("1m30s"), Some(90_000_000_000));
        assert_eq!(parse_duration("1.5h"), Some(5_400_000_000_000));
        assert_eq!(parse_time("10:00am"), Some(TimeVal { hour: 10, min: 0, sec: 0 }));
        assert_eq!(parse_time("12:15pm"), Some(TimeVal { hour: 12, min: 15, sec: 0 }));
        assert_eq!(parse_time("12:15am"), Some(TimeVal { hour: 0, min: 15, sec: 0 }));
        assert_eq!(parse_time("23:15"), Some(TimeVal { hour: 23, min: 15, sec: 0 }));
    }

    #[test]
    fn json_uniform_objects_become_table() {
        let j: serde_json::Value = serde_json::from_str(r#"[{"a":1},{"a":2}]"#).unwrap();
        assert!(matches!(json_to_value(&j), Value::Table(_)));
    }
}
