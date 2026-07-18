//! Bounded conversion at finite process-stdin and HTTP request-body boundaries.
//!
//! Resident strings can be moved, resident bytes stay shared, and lazy CAS
//! bytes remain lazy until a consumer asks for an incremental reader. Only
//! structured values require eager encoding; that path has one explicit wall.

use serde::Serialize as _;
use shoal_value::{
    CasBytesVal, ErrorVal, OpaqueHandling, RetainedError, RetainedLimits, VResult, Value,
    feed_bytes, retained_size, value_to_json,
};
use std::io::{self, Cursor, Read, Write};
use std::sync::Arc;

/// Maximum resident/transient bytes admitted while encoding a finite
/// structured stdin or HTTP body.
pub(crate) const FINITE_BODY_MAX_BYTES: usize = 16 * 1024 * 1024;
const FINITE_BODY_MAX_DEPTH: usize = 128;
const FINITE_BODY_MAX_NODES: usize = 131_072;

/// A finite byte source that does not imply eager CAS materialization or a
/// second whole-payload clone of resident bytes.
pub(crate) enum FiniteBody {
    Owned(Vec<u8>),
    Shared(Arc<Vec<u8>>),
    Cas(Arc<CasBytesVal>),
}

impl FiniteBody {
    pub(crate) fn from_value(value: Value, what: &str) -> VResult<Self> {
        match value {
            Value::Str(text) => Ok(Self::Owned(text.into_bytes())),
            Value::Bytes(bytes) => Ok(Self::Shared(bytes)),
            Value::CasBytes(bytes) => Ok(Self::Cas(bytes)),
            Value::Outcome(outcome) => {
                let logical = outcome.out_value();
                if matches!(logical, Value::Str(_)) {
                    match &outcome.stdout_ref {
                        Some(bytes) => Ok(Self::Cas(bytes.clone())),
                        None => Ok(Self::Shared(outcome.stdout.clone())),
                    }
                } else {
                    Self::from_value(logical, what)
                }
            }
            other => bounded_feed_bytes(&other, what).map(Self::Owned),
        }
    }

    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Owned(bytes) => bytes.len() as u64,
            Self::Shared(bytes) => bytes.len() as u64,
            Self::Cas(bytes) => bytes.len,
        }
    }

    /// Preserve the cheap owned-vector executor path where possible. Shared
    /// and CAS-backed data use the incremental stdin queue.
    pub(crate) fn into_feed_input(self) -> VResult<FeedInput> {
        match self {
            Self::Owned(bytes) => Ok(FeedInput::Bytes(bytes)),
            other => other.into_reader().map(FeedInput::Reader),
        }
    }

    fn into_reader(self) -> VResult<Box<dyn Read + Send>> {
        match self {
            Self::Owned(bytes) => Ok(Box::new(Cursor::new(bytes))),
            Self::Shared(bytes) => Ok(Box::new(SharedBytesReader::new(bytes))),
            Self::Cas(bytes) => bytes.open(),
        }
    }

    /// Apply the HTTP request-body wall before opening a CAS reader. A lying or
    /// growing reader is still stopped by `HardLimitReader`'s sentinel read.
    pub(crate) fn into_http_input(self, what: &str) -> VResult<HttpInput> {
        if self.len() > FINITE_BODY_MAX_BYTES as u64 {
            return Err(http_body_limit(what));
        }
        match self {
            Self::Owned(bytes) => Ok(HttpInput::Owned(bytes)),
            Self::Shared(bytes) => Ok(HttpInput::Shared(bytes)),
            Self::Cas(bytes) => Ok(HttpInput::Reader(HardLimitReader::new(
                bytes.open()?,
                FINITE_BODY_MAX_BYTES,
            ))),
        }
    }
}

pub(crate) enum FeedInput {
    Bytes(Vec<u8>),
    Reader(Box<dyn Read + Send>),
}

/// Ordinary resident bodies retain ureq's sized `Content-Length` path. A CAS
/// reader uses chunked transfer so the plus-one sentinel remains observable.
pub(crate) enum HttpInput {
    Owned(Vec<u8>),
    Shared(Arc<Vec<u8>>),
    Reader(HardLimitReader),
}

impl HttpInput {
    pub(crate) fn exceeded(&self) -> bool {
        matches!(self, Self::Reader(reader) if reader.exceeded())
    }
}

struct SharedBytesReader {
    bytes: Arc<Vec<u8>>,
    position: usize,
}

impl SharedBytesReader {
    fn new(bytes: Arc<Vec<u8>>) -> Self {
        Self { bytes, position: 0 }
    }
}

impl Read for SharedBytesReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.bytes[self.position..];
        let count = remaining.len().min(output.len());
        output[..count].copy_from_slice(&remaining[..count]);
        self.position += count;
        Ok(count)
    }
}

/// A reader hard wall with a one-byte sentinel read at the exact boundary.
/// `exceeded` lets the HTTP caller recover a stable language error after ureq
/// wraps an injected reader failure.
pub(crate) struct HardLimitReader {
    inner: Box<dyn Read + Send>,
    remaining: usize,
    exceeded: bool,
    checked_end: bool,
}

impl HardLimitReader {
    fn new(inner: Box<dyn Read + Send>, max_bytes: usize) -> Self {
        Self {
            inner,
            remaining: max_bytes,
            exceeded: false,
            checked_end: false,
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }
}

impl Read for HardLimitReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.checked_end {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut sentinel = [0u8; 1];
            if self.inner.read(&mut sentinel)? == 0 {
                self.checked_end = true;
                return Ok(0);
            }
            self.exceeded = true;
            return Err(io::Error::other("HTTP request body limit exceeded"));
        }
        let limit = output.len().min(self.remaining);
        let count = self.inner.read(&mut output[..limit])?;
        self.remaining -= count;
        if count == 0 {
            self.checked_end = true;
        }
        Ok(count)
    }
}

pub(crate) fn http_body_limit(what: &str) -> ErrorVal {
    ErrorVal::new(
        "http_body_limit",
        format!("{what} exceeds the {FINITE_BODY_MAX_BYTES}-byte request-body limit"),
    )
    .with_hint("send a smaller body or split it into bounded requests")
}

fn bounded_feed_bytes(value: &Value, what: &str) -> VResult<Vec<u8>> {
    if let Value::List(values) = value
        && values.iter().all(|value| matches!(value, Value::Str(_)))
    {
        let length = values.iter().try_fold(0usize, |length, value| {
            let Value::Str(text) = value else {
                unreachable!("all list values were checked as strings")
            };
            length.checked_add(text.len().saturating_add(1))
        });
        let Some(length) = length.filter(|length| *length <= FINITE_BODY_MAX_BYTES) else {
            return Err(feed_limit(what, "newline-delimited list output"));
        };
        let mut output = Vec::with_capacity(length);
        for value in values {
            let Value::Str(text) = value else {
                unreachable!()
            };
            output.extend_from_slice(text.as_bytes());
            output.push(b'\n');
        }
        return Ok(output);
    }

    if matches!(value, Value::Record(_) | Value::Table(_) | Value::List(_)) {
        // `value_to_json` owns a structural projection before serde writes the
        // final bytes. Admission therefore bounds three simultaneous payload
        // representations (the caller's value, a <=16 MiB projection, and a
        // <=16 MiB output) plus node overhead capped below. No fourth
        // unbounded `to_vec`/string copy is permitted at this boundary.
        retained_size(
            value,
            RetainedLimits {
                max_bytes: FINITE_BODY_MAX_BYTES,
                max_depth: FINITE_BODY_MAX_DEPTH,
                max_nodes: FINITE_BODY_MAX_NODES,
                opaque: OpaqueHandling::Charge(1024),
                allow_secret: false,
            },
        )
        .map_err(|error| match error {
            RetainedError::Secret => ErrorVal::new(
                "feed_error",
                format!("{what} contains a secret, which cannot be fed as data"),
            )
            .with_hint("inject secrets at spawn time or use an explicit authorization header"),
            other => feed_limit(what, format!("value admission failed: {other:?}")),
        })?;
        let json = value_to_json(value)?;
        let mut output = LimitedWriter::new(FINITE_BODY_MAX_BYTES);
        let result = json.serialize(&mut serde_json::Serializer::new(&mut output));
        if output.exceeded {
            return Err(feed_limit(what, "compact JSON output"));
        }
        result.map_err(|error| ErrorVal::new("feed_error", format!("{what}: {error}")))?;
        return Ok(output.bytes);
    }

    let bytes = feed_bytes(value)?;
    if bytes.len() > FINITE_BODY_MAX_BYTES {
        return Err(feed_limit(what, "eager scalar output"));
    }
    Ok(bytes)
}

fn feed_limit(what: &str, detail: impl std::fmt::Display) -> ErrorVal {
    ErrorVal::new(
        "feed_materialization_limit",
        format!("{what}: {detail} exceeds the {FINITE_BODY_MAX_BYTES}-byte finite-body limit"),
    )
    .with_hint("use bytes/CAS streaming or split structured data into a bounded stream")
}

struct LimitedWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8 * 1024)),
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("finite body output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_value::{BytesLoad, OutcomeVal, Record, SecretVal};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingLoader {
        bytes: Vec<u8>,
        opens: Arc<AtomicUsize>,
        loads: Arc<AtomicUsize>,
    }

    impl BytesLoad for CountingLoader {
        fn load(&self) -> io::Result<Vec<u8>> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(self.bytes.clone())
        }

        fn open(&self) -> io::Result<Box<dyn Read + Send>> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Cursor::new(self.bytes.clone())))
        }
    }

    fn cas(bytes: Vec<u8>, declared_len: u64) -> (Value, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let opens = Arc::new(AtomicUsize::new(0));
        let loads = Arc::new(AtomicUsize::new(0));
        let loader = CountingLoader {
            bytes,
            opens: opens.clone(),
            loads: loads.clone(),
        };
        (
            Value::CasBytes(Arc::new(CasBytesVal {
                hash: "a".repeat(64),
                len: declared_len,
                preview: Arc::new(Vec::new()),
                truncated: false,
                loader: Arc::new(loader),
            })),
            opens,
            loads,
        )
    }

    #[test]
    fn oversized_http_cas_is_denied_before_opening_or_loading() {
        let (value, opens, loads) = cas(Vec::new(), FINITE_BODY_MAX_BYTES as u64 + 1);
        let error = FiniteBody::from_value(value, "http.post body")
            .unwrap()
            .into_http_input("http.post body")
            .err()
            .expect("known oversized body must fail");
        assert_eq!(error.code, "http_body_limit");
        assert_eq!(opens.load(Ordering::SeqCst), 0);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn http_exact_boundary_is_admitted_and_one_extra_byte_is_detected() {
        let exact = vec![b'x'; FINITE_BODY_MAX_BYTES];
        let mut reader = HardLimitReader::new(Box::new(Cursor::new(exact)), FINITE_BODY_MAX_BYTES);
        assert_eq!(
            io::copy(&mut reader, &mut io::sink()).unwrap(),
            FINITE_BODY_MAX_BYTES as u64
        );
        assert!(!reader.exceeded());

        let growing = vec![b'x'; FINITE_BODY_MAX_BYTES + 1];
        let mut reader =
            HardLimitReader::new(Box::new(Cursor::new(growing)), FINITE_BODY_MAX_BYTES);
        assert!(io::copy(&mut reader, &mut io::sink()).is_err());
        assert!(reader.exceeded());
    }

    #[test]
    fn structured_amplification_fails_without_retaining_partial_output() {
        let mut record = Record::new();
        record.insert("quoted".into(), Value::Str("\"".repeat(10 * 1024 * 1024)));
        let error = FiniteBody::from_value(Value::Record(record), "test body")
            .err()
            .expect("escaped output exceeds the wall");
        assert_eq!(error.code, "feed_materialization_limit");
    }

    #[test]
    fn structured_secrets_are_never_projected_into_a_body() {
        let mut record = Record::new();
        record.insert(
            "token".into(),
            Value::Secret(SecretVal {
                name: "api".into(),
                value: Arc::from("classified"),
            }),
        );
        let error = FiniteBody::from_value(Value::Record(record), "test body")
            .err()
            .expect("nested secret must fail");
        assert_eq!(error.code, "feed_error");
    }

    #[test]
    fn small_string_and_shared_bytes_roundtrip_normally() {
        for value in [
            Value::Str("hello".into()),
            Value::Bytes(Arc::new(b"world".to_vec())),
        ] {
            let mut reader = FiniteBody::from_value(value, "test")
                .unwrap()
                .into_reader()
                .unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert!(bytes == b"hello" || bytes == b"world");
        }
    }

    #[test]
    fn cas_feed_opens_incrementally_and_never_uses_full_load() {
        let (value, opens, loads) = cas(b"incremental".to_vec(), 11);
        let FeedInput::Reader(mut reader) = FiniteBody::from_value(value, ".feed body")
            .unwrap()
            .into_feed_input()
            .unwrap()
        else {
            panic!("CAS feed must use a reader")
        };
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"incremental");
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn parsed_string_outcome_still_feeds_raw_stdout() {
        let outcome = Value::Outcome(Arc::new(OutcomeVal {
            status: Some(0),
            signal: None,
            ok: true,
            stdout: Arc::new(b"raw\n".to_vec()),
            stdout_ref: None,
            stderr: Arc::new(Vec::new()),
            dur_ns: 0,
            pid: 0,
            cmd: "fixture".into(),
            parsed: Some(Value::Str("parsed".into())),
            streamed: false,
            span: None,
        }));
        let mut reader = FiniteBody::from_value(outcome, ".feed body")
            .unwrap()
            .into_reader()
            .unwrap();
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"raw\n");
    }
}
