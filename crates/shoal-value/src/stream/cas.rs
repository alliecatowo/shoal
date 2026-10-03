//! Lazy, bounded-memory line streaming for CAS-backed command output.

use super::*;
use std::io::{BufRead, BufReader};

const CAS_STREAM_LINE_MAX_BYTES: usize = 1024 * 1024;

struct CasLineSource {
    value: Arc<CasBytesVal>,
    reader: Option<BufReader<Box<dyn std::io::Read + Send>>>,
    finished: bool,
}

impl CasLineSource {
    fn reader(&mut self) -> VResult<&mut BufReader<Box<dyn std::io::Read + Send>>> {
        if self.reader.is_none() {
            self.reader = Some(BufReader::new(self.value.open()?));
        }
        self.reader
            .as_mut()
            .ok_or_else(|| ErrorVal::new("io_error", "CAS stream reader failed to initialize"))
    }
}

impl Upstream for CasLineSource {
    fn pull(
        &mut self,
        _ctx: &mut dyn CallCtx,
        _timeout: Option<std::time::Duration>,
    ) -> VResult<Pull> {
        if self.finished {
            return Ok(Pull::End);
        }
        let mut line = Vec::new();
        loop {
            let reader = self.reader()?;
            let available = reader
                .fill_buf()
                .map_err(|error| ErrorVal::new("io_error", format!("CAS stream read: {error}")))?;
            if available.is_empty() {
                self.finished = true;
                if line.is_empty() {
                    return Ok(Pull::End);
                }
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let content_len = newline.unwrap_or(available.len());
            if line.len().saturating_add(content_len) > CAS_STREAM_LINE_MAX_BYTES {
                self.finished = true;
                return Err(ErrorVal::new(
                    "stream_line_limit",
                    format!(
                        "CAS-backed stream line exceeds its {CAS_STREAM_LINE_MAX_BYTES}-byte limit"
                    ),
                )
                .with_hint(
                    "produce line-framed output or load/process the blob in explicit chunks",
                ));
            }
            line.extend_from_slice(&available[..content_len]);
            let consumed = content_len + usize::from(newline.is_some());
            reader.consume(consumed);
            if newline.is_some() {
                break;
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(Pull::Item(Value::Str(
            String::from_utf8_lossy(&line).into_owned(),
        )))
    }
}

impl StreamVal {
    pub(crate) fn from_cas_lines(value: Arc<CasBytesVal>) -> StreamVal {
        StreamVal::from_source(
            "str",
            true,
            Box::new(CasLineSource {
                value,
                reader: None,
                finished: false,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value_types::test_support;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct C;
    impl CallCtx for C {
        fn call_closure(&mut self, _f: &Value, _args: Vec<Value>) -> VResult<Value> {
            unreachable!()
        }
        fn buffer_stream(&mut self, _stream: StreamVal, _capacity: usize) -> VResult<StreamVal> {
            unreachable!()
        }
        fn cwd(&self) -> PathBuf {
            std::env::temp_dir()
        }
        fn fs(&self) -> &dyn Fs {
            static FS: StdFs = StdFs;
            &FS
        }
    }

    #[test]
    fn cas_line_stream_is_lazy_and_preserves_lines() {
        let calls = Arc::new(AtomicUsize::new(0));
        let value = Arc::new(test_support::cas_bytes(
            b"one",
            b"one\r\ntwo\nthree",
            calls.clone(),
        ));
        let stream = StreamVal::from_cas_lines(value);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            collect_stream(&mut C, &stream).unwrap(),
            vec![
                Value::Str("one".into()),
                Value::Str("two".into()),
                Value::Str("three".into())
            ]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cas_line_stream_bounds_a_single_unframed_line() {
        let calls = Arc::new(AtomicUsize::new(0));
        let hostile = vec![b'x'; CAS_STREAM_LINE_MAX_BYTES + 1];
        let value = Arc::new(test_support::cas_bytes(b"x", &hostile, calls));
        let stream = StreamVal::from_cas_lines(value);
        assert_eq!(
            collect_stream(&mut C, &stream).unwrap_err().code,
            "stream_line_limit"
        );
    }
}
