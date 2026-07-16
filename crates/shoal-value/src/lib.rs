//! shoal-value — the runtime value model for the shoal shell.
//!
//! Types per TDD §4.1. `path` is bytes-backed (`PathBuf`/`OsString`); `secret`
//! is opaque; `outcome` is an external command's result; `stream` is
//! single-consumption; equality is structural for data types and identity for
//! `task`/`stream`.
//!
//! # Module layout
//!
//! This file holds only the [`Value`] enum itself, its tiny inherent core,
//! `feed_bytes`, the eval↔methods bridge (`CallArgs`/`CallCtx`), and
//! structural equality. Every other area lives in its own file, each as
//! further top-level items sharing this file's imports via `use super::*;`
//! (the same multi-file pattern `shoal-eval`'s `reef.rs` and
//! `shoal-journal`'s `cas.rs`/`gc.rs`/… use for `impl Type { .. }` splits):
//!
//! - [`env`] — lexical scopes (`Env`/`Binding`).
//! - [`stream`] — `StreamVal` and the lazy combinators (docs/STREAMS.md).
//! - [`task`] — `TaskVal`/`TaskShared` job control (TDD §4.7).
//! - [`outcome`] — `OutcomeVal`, a command's result (TDD §4.1).
//! - [`value_types`] — `GlobVal`/`RegexVal`/`RangeVal`/`TimeVal`/`ClosureVal`/
//!   `SecretVal` plus the `parse_size`/`parse_duration`/`parse_time` word
//!   helpers.
//! - [`json`] — `json_to_value`/`value_to_json`.
//! - [`methods`] — the value-method standard library, split by receiver type.
//! - [`ops`]/[`render`]/[`ports`] — unchanged from before this split.

pub mod methods;
pub mod ops;
pub mod ports;
pub mod render;

mod env;
mod json;
mod outcome;
mod stream;
mod task;
mod value_types;

pub use ports::{BytesLoad, Clock, Fs, Opener, SecretPort, StdClock, StdFs, StdOpener};

pub use env::{AssignError, Binding, Env};
pub use json::{json_to_value, value_to_json};
pub use methods::{method_names, methods_for};
pub use outcome::OutcomeVal;
pub use stream::{Pull, StreamVal, Upstream, collect_stream, drive_stream};
pub use task::{TaskShared, TaskVal};
pub use value_types::{
    CasBytesVal, ClosureVal, GlobVal, RangeVal, RegexVal, SecretVal, TimeVal, parse_duration,
    parse_size, parse_time,
};

use indexmap::IndexMap;
use shoal_ast as ast;
use shoal_ast::Span;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    /// Lazy, content-addressed bytes (TDD §317): a value-position capture whose
    /// stdout overflowed the RAM cap and spilled to the CAS. Holds a bounded
    /// preview + `{hash, len}` and loads the full content on demand. Small
    /// captures stay plain [`Value::Bytes`]; this variant only appears past the
    /// cap, so the common path is untouched.
    CasBytes(Arc<CasBytesVal>),
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
            // A CAS-backed value is still, semantically, bytes — just lazy.
            Value::CasBytes(_) => "bytes",
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

/// Serialize a value to the bytes `.feed` writes to a command's stdin,
/// following IO.md §1.2's feedability table exactly. Anything not in the
/// table is an error — `feed_error` (with a specialized message) for the
/// types that are *deliberately* never feedable, plain `type_error`
/// otherwise.
pub fn feed_bytes(v: &Value) -> Result<Vec<u8>, ErrorVal> {
    match v {
        // str → its UTF-8 bytes, verbatim (no trailing newline added).
        Value::Str(s) => Ok(s.clone().into_bytes()),
        // bytes → raw.
        Value::Bytes(b) => Ok(b.as_ref().clone()),
        // CAS-backed bytes → load the full content from the store, then raw.
        Value::CasBytes(c) => c.resolve(),
        // path → NOT directly feedable (IO.md §1.2): a bare path is a name,
        // not content. Feeding the name's bytes silently did the wrong thing.
        Value::Path(_) => Err(ErrorVal::type_error(
            "cannot feed a path to a command's stdin — a bare path is a name, not the file's contents",
        )
        .with_hint("feed the contents instead: path(\"x\").read.feed(cmd)")),
        // int/float/bool/size/duration/datetime/time → decimal text via the
        // same rule as `render_inline` (IO.md §1.2), no trailing newline.
        Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Size(_)
        | Value::Duration(_)
        | Value::DateTime(_)
        | Value::Time(_) => Ok(render::render_inline(v).into_bytes()),
        // list<str> → each element joined with `\n`, plus one trailing `\n`.
        Value::List(xs) if xs.iter().all(|x| matches!(x, Value::Str(_))) => {
            let mut out = String::new();
            for x in xs {
                if let Value::Str(s) = x {
                    out.push_str(s);
                    out.push('\n');
                }
            }
            Ok(out.into_bytes())
        }
        // table / record / list-of-records (and any other list) → compact JSON.
        Value::Record(_) | Value::Table(_) | Value::List(_) => {
            Ok(serde_json::to_vec(&value_to_json(v)).unwrap_or_default())
        }
        // outcome → its structured `.out` re-encoded per the rules above when
        // one exists, else its raw stdout bytes (IO.md §1.2:
        // `outcome.feed(cmd)` ≡ `outcome.out.feed(cmd)` when `.out` is
        // structured, else the stdout bytes verbatim).
        Value::Outcome(o) => match o.out_value() {
            Value::Str(_) => Ok(o.stdout.as_ref().clone()),
            structured => feed_bytes(&structured),
        },
        // stream → §1.2 promises *incremental* feeding as items arrive, which
        // needs evaluator/exec support (a live child stdin pipe); an honest
        // error until that lands rather than a buffering fake.
        Value::Stream(_) => Err(ErrorVal::type_error(
            "feeding a stream to a command's stdin is not implemented yet",
        )
        .with_hint("collect a bounded stream first: stream.collect().feed(cmd)")),
        // Deliberately never feedable (IO.md §1.2 / §5) → `feed_error`.
        Value::Secret(_) => Err(ErrorVal::new(
            "feed_error",
            "a secret cannot be fed as stdin data",
        )
        .with_hint("secrets are injected at spawn time, not fed as data")),
        Value::Task(_) | Value::Closure(_) | Value::Error(_) | Value::Glob(_) | Value::Regex(_) => {
            Err(ErrorVal::new(
                "feed_error",
                format!("a {} cannot be fed as stdin data", v.type_name()),
            ))
        }
        // Anything else (null, cmd refs) has no serialization in §1.2's table
        // → generic type_error.
        other => Err(ErrorVal::type_error(format!(
            "cannot feed a {} to a command's stdin",
            other.type_name()
        ))),
    }
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
    /// External process exit status when this error originated from a command.
    pub status: Option<i32>,
}

impl ErrorVal {
    pub fn new(code: impl Into<String>, msg: impl Into<String>) -> ErrorVal {
        ErrorVal {
            code: code.into(),
            msg: msg.into(),
            span: None,
            hint: None,
            stderr: None,
            status: None,
        }
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
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> ErrorVal {
        self.stderr = Some(stderr.into());
        self
    }
    pub fn with_status(mut self, status: Option<i32>) -> ErrorVal {
        self.status = status;
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
            // Content-addressed: identical hash (and length) ⇒ identical bytes,
            // without loading either. A CAS-backed value and a resident one are
            // not compared here (would need a load); materialize with `.load()`
            // first if that comparison is wanted.
            (CasBytes(a), CasBytes(b)) => a.hash == b.hash && a.len == b.len,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_bytes_serialization() {
        // str → verbatim UTF-8, no trailing newline.
        assert_eq!(feed_bytes(&Value::Str("hi".into())).unwrap(), b"hi");
        // bytes → raw.
        assert_eq!(
            feed_bytes(&Value::Bytes(Arc::new(vec![1, 2, 3]))).unwrap(),
            vec![1, 2, 3]
        );
        // list<str> → newline-joined + trailing newline.
        assert_eq!(
            feed_bytes(&Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into())
            ]))
            .unwrap(),
            b"a\nb\n"
        );
        // record → compact JSON, no trailing newline.
        let mut rec = Record::new();
        rec.insert("n".into(), Value::Int(2));
        assert_eq!(feed_bytes(&Value::Record(rec)).unwrap(), br#"{"n":2}"#);
        // table → row-major JSON.
        let mut row = Record::new();
        row.insert("x".into(), Value::Int(1));
        assert_eq!(
            feed_bytes(&Value::Table(vec![row])).unwrap(),
            br#"[{"x":1}]"#
        );
        // non-str list → JSON array.
        assert_eq!(
            feed_bytes(&Value::List(vec![Value::Int(3), Value::Int(1)])).unwrap(),
            b"[3,1]"
        );
    }

    #[test]
    fn feed_bytes_never_feedable_is_feed_error() {
        let e = feed_bytes(&Value::Secret(SecretVal {
            name: "tok".into(),
            value: Arc::from("x"),
        }))
        .unwrap_err();
        assert_eq!(e.code, "feed_error");
        assert!(e.hint.unwrap().contains("injected at spawn time"));
    }

    #[test]
    fn feed_bytes_scalars_feed_their_render_form() {
        // IO.md §1.2: int/float/bool/size/duration/datetime/time feed their
        // `render_inline` text, UTF-8, no trailing newline.
        assert_eq!(feed_bytes(&Value::Int(4)).unwrap(), b"4");
        assert_eq!(feed_bytes(&Value::Float(1.5)).unwrap(), b"1.5");
        assert_eq!(feed_bytes(&Value::Bool(true)).unwrap(), b"true");
        assert_eq!(feed_bytes(&Value::Size(1_500)).unwrap(), b"1.5kb");
        assert_eq!(
            feed_bytes(&Value::Duration(90_000_000_000)).unwrap(),
            b"1m30s"
        );
        assert_eq!(
            feed_bytes(&Value::Time(TimeVal {
                hour: 10,
                min: 0,
                sec: 0
            }))
            .unwrap(),
            b"10:00"
        );
    }

    #[test]
    fn feed_bytes_path_is_type_error_with_read_hint() {
        // IO.md §1.2: a bare path is a name, not content — never fed silently.
        let e = feed_bytes(&Value::Path(PathBuf::from("/a/b"))).unwrap_err();
        assert_eq!(e.code, "type_error");
        assert!(e.msg.contains("a name, not"));
        assert!(e.hint.unwrap().contains(".read"));
    }

    #[test]
    fn feed_bytes_outcome_feeds_out_or_stdout() {
        fn outcome(stdout: &[u8], parsed: Option<Value>) -> Value {
            Value::Outcome(Arc::new(OutcomeVal {
                status: Some(0),
                signal: None,
                ok: true,
                stdout: Arc::new(stdout.to_vec()),
                stdout_ref: None,
                stderr: Arc::new(Vec::new()),
                dur_ns: 0,
                pid: 0,
                cmd: "x".into(),
                parsed,
                streamed: false,
                span: None,
            }))
        }
        // Structured `.out` → re-encoded JSON.
        let mut rec = Record::new();
        rec.insert("n".into(), Value::Int(2));
        assert_eq!(
            feed_bytes(&outcome(b"{\"n\": 2}\n", Some(Value::Record(rec)))).unwrap(),
            br#"{"n":2}"#
        );
        // Text `.out` → the raw stdout bytes, verbatim.
        assert_eq!(feed_bytes(&outcome(b"hi\n", None)).unwrap(), b"hi\n");
    }

    #[test]
    fn feed_bytes_stream_is_unimplemented_type_error() {
        let s = StreamVal::from_iter("int", (0..2).map(|i| Ok(Value::Int(i))));
        let e = feed_bytes(&Value::Stream(s)).unwrap_err();
        assert_eq!(e.code, "type_error");
        assert!(e.hint.unwrap().contains("collect"));
    }

    #[test]
    fn feed_bytes_null_is_type_error() {
        assert_eq!(feed_bytes(&Value::Null).unwrap_err().code, "type_error");
    }

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
        assert!(s.take_upstream().is_ok());
        let err = s.take_upstream().err().expect("second take must fail");
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
        assert_eq!(
            parse_time("10:00am"),
            Some(TimeVal {
                hour: 10,
                min: 0,
                sec: 0
            })
        );
        assert_eq!(
            parse_time("12:15pm"),
            Some(TimeVal {
                hour: 12,
                min: 15,
                sec: 0
            })
        );
        assert_eq!(
            parse_time("12:15am"),
            Some(TimeVal {
                hour: 0,
                min: 15,
                sec: 0
            })
        );
        assert_eq!(
            parse_time("23:15"),
            Some(TimeVal {
                hour: 23,
                min: 15,
                sec: 0
            })
        );
    }

    #[test]
    fn json_uniform_objects_become_table() {
        let j: serde_json::Value = serde_json::from_str(r#"[{"a":1},{"a":2}]"#).unwrap();
        assert!(matches!(json_to_value(&j), Value::Table(_)));
    }
}
