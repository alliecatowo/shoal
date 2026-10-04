//! `OutcomeVal` — a command's result (site/content/internals/language-conformance-contract.md), moved verbatim out of `lib.rs`.

use super::*;
use std::io::Read;

struct SharedStdoutReader {
    bytes: Arc<Vec<u8>>,
    position: usize,
}

impl Read for SharedStdoutReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let remaining = &self.bytes[self.position..];
        let count = remaining.len().min(output.len());
        output[..count].copy_from_slice(&remaining[..count]);
        self.position += count;
        Ok(count)
    }
}

/// A command's result (site/content/internals/language-conformance-contract.md). `out` is parsed lazily on first structured
/// access; the raw bytes are always retained.
#[derive(Debug)]
pub struct OutcomeVal {
    pub status: Option<i32>,
    /// Signal name (`"SIGSEGV"`) when the child died to a signal (site/content/internals/language-conformance-contract.md).
    pub signal: Option<String>,
    pub ok: bool,
    /// Captured stdout. When [`stdout_ref`](OutcomeVal::stdout_ref) is `Some`
    /// (a value-position capture that overflowed the RAM cap and spilled to the
    /// CAS, site/content/internals/language-conformance-contract.md), this holds only the bounded resident *preview*; the full
    /// bytes live in the CAS behind the ref. Otherwise it is the whole stdout.
    pub stdout: Arc<Vec<u8>>,
    /// `Some` when stdout overflowed the capture RAM cap and was spilled to the
    /// CAS (site/content/internals/language-conformance-contract.md): a lazy, ref-backed view of the *full* stdout. `.stdout`
    /// then surfaces this (see [`OutcomeVal::stdout_value`]) so `.len` is the
    /// true length and materialization loads from the CAS on demand. `None` is
    /// the ordinary fully-resident case — no behavior change.
    pub stdout_ref: Option<Arc<CasBytesVal>>,
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
    /// Source span of the invocation (site/content/internals/kernel-protocol.md), when one is in scope
    /// at construction. Carries the same byte-offset anchor `ErrorVal` uses so
    /// the sibling success/error paths of a command spawn agree. `None` when no
    /// meaningful source anchor exists (builtin-wrapped outcomes, values
    /// reconstructed without an invocation site); the kernel wire omits the
    /// field entirely in that case rather than fabricating one.
    pub span: Option<Span>,
}

impl OutcomeVal {
    /// Attach the invocation's source span (mirrors [`ErrorVal::with_span`]).
    pub fn with_span(mut self, span: Span) -> OutcomeVal {
        self.span = Some(span);
        self
    }

    /// The `.stdout` value: a lazy [`Value::CasBytes`] when stdout spilled to
    /// the CAS (site/content/internals/language-conformance-contract.md), else the resident [`Value::Bytes`]. Callers that
    /// surface `.stdout` use this so the ref-backed view is what users see for
    /// oversized captures (true `.len`, on-demand materialization), with zero
    /// change for the ordinary resident case.
    pub fn stdout_value(&self) -> Value {
        match &self.stdout_ref {
            Some(c) => Value::CasBytes(c.clone()),
            None => Value::Bytes(self.stdout.clone()),
        }
    }

    /// The **full** stdout bytes: loaded from the CAS when stdout spilled (see
    /// `site/content/internals/persistence.md`), else the resident bytes. Data sinks (redirects, `.save`) use this
    /// so an oversized capture is written whole, not just its preview.
    pub fn stdout_bytes(&self) -> VResult<Vec<u8>> {
        match &self.stdout_ref {
            Some(c) => c.resolve(),
            None => Ok(self.stdout.as_ref().clone()),
        }
    }

    /// Open stdout for incremental sinks without loading a CAS spill or
    /// cloning resident capture bytes. Redirects and other file consumers use
    /// this path; [`stdout_bytes`](Self::stdout_bytes) remains the explicit
    /// materializing API.
    pub fn open_stdout(&self) -> VResult<Box<dyn Read + Send>> {
        match &self.stdout_ref {
            Some(bytes) => bytes.open(),
            None => Ok(Box::new(SharedStdoutReader {
                bytes: self.stdout.clone(),
                position: 0,
            })),
        }
    }

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
            && preflight_json_numbers(trimmed, "command output JSON").is_ok()
            && let Ok(value) = json_to_value(&json)
        {
            return value;
        }
        Value::Str(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn bare() -> OutcomeVal {
        OutcomeVal {
            status: Some(0),
            signal: None,
            ok: true,
            stdout: Arc::new(Vec::new()),
            stdout_ref: None,
            stderr: Arc::new(Vec::new()),
            dur_ns: 0,
            pid: 0,
            cmd: "x".into(),
            parsed: None,
            streamed: false,
            span: None,
        }
    }

    #[test]
    fn span_defaults_to_none_and_with_span_round_trips() {
        assert_eq!(bare().span, None);
        let stamped = bare().with_span(Span::new(3, 9));
        assert_eq!(stamped.span, Some(Span::new(3, 9)));
    }

    #[test]
    fn structured_stdout_never_substitutes_a_rounded_integer() {
        let exact = br#"{"id":18446744073709551615}"#.to_vec();
        let mut outcome = bare();
        outcome.stdout = Arc::new(exact.clone());
        assert_eq!(
            outcome.out_value(),
            Value::Str(String::from_utf8(exact).unwrap())
        );

        let underflow = br#"{"id":-9223372036854775809}"#.to_vec();
        outcome.stdout = Arc::new(underflow.clone());
        assert_eq!(
            outcome.out_value(),
            Value::Str(String::from_utf8(underflow).unwrap())
        );
    }

    #[test]
    fn stdout_reader_streams_cas_without_materializing() {
        struct OpenOnly {
            opens: Arc<AtomicUsize>,
        }

        impl BytesLoad for OpenOnly {
            fn load(&self) -> std::io::Result<Vec<u8>> {
                panic!("incremental stdout must not call load")
            }

            fn open(&self) -> std::io::Result<Box<dyn Read + Send>> {
                self.opens.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(std::io::Cursor::new(b"spilled".to_vec())))
            }
        }

        let opens = Arc::new(AtomicUsize::new(0));
        let mut outcome = bare();
        outcome.stdout = Arc::new(b"preview".to_vec());
        outcome.stdout_ref = Some(Arc::new(CasBytesVal {
            hash: "a".repeat(64),
            len: 7,
            preview: Arc::new(b"preview".to_vec()),
            truncated: false,
            loader: Arc::new(OpenOnly {
                opens: opens.clone(),
            }),
        }));

        let mut bytes = Vec::new();
        outcome
            .open_stdout()
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"spilled");
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }
}
