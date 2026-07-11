//! `OutcomeVal` — a command's result (TDD §4.1), moved verbatim out of `lib.rs`.

use super::*;

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
    pub parsed: Option<Value>,
    /// True only when the child's bytes actually reached the real terminal via
    /// the `ExecMode::PtyTee` passthrough path (defect #1). The interactive
    /// result renderer suppresses re-rendering exactly these outcomes to avoid
    /// double-printing; captured externals and builtins (which stream nothing)
    /// leave this `false` so their `.out` still renders.
    pub streamed: bool,
}

impl OutcomeVal {
    /// `outcome.out` — utf-8 text with the trailing newline trimmed; if the
    /// payload parses as JSON it becomes structured data (T1, lazy).
    pub fn out_value(&self) -> Value {
        if let Some(value) = &self.parsed {
            return value.clone();
        }
        let text = String::from_utf8_lossy(&self.stdout);
        let trimmed = text.strip_suffix('\n').unwrap_or(&text);
        let first = trimmed.trim_start().chars().next();
        if matches!(first, Some('{') | Some('['))
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return json_to_value(&json);
        }
        Value::Str(trimmed.to_string())
    }
}
