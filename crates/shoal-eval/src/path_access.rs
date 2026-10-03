//! Bounded filesystem-backed `path` accessors.

use super::*;
use std::io::{BufRead, BufReader, Read};

pub(crate) const MAX_PATH_READ_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_PATH_READ_LINES: usize = 16_384;
const MAX_PATH_LINES_RETAINED_BYTES: usize = 16 * 1024 * 1024;

fn path_limit(path: &Path, detail: impl std::fmt::Display) -> ErrorVal {
    ErrorVal::new("path_read_limit", format!("{}: {detail}", path.display())).with_hint(
        "use `.stream()` with incremental transforms/sinks, or read a bounded prefix with `head`",
    )
}

fn path_io_error(path: &Path, error: std::io::Error) -> ErrorVal {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        "not_found"
    } else {
        "custom"
    };
    ErrorVal::new(code, format!("{}: {error}", path.display()))
}

fn read_bytes_with_limit(path: &Path, reader: impl Read, max_bytes: usize) -> VResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8 * 1024));
    reader
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| path_io_error(path, error))?;
    if bytes.len() > max_bytes {
        return Err(path_limit(
            path,
            format_args!("eager read exceeds the {max_bytes}-byte limit"),
        ));
    }
    Ok(bytes)
}

fn read_lines_with_limits(
    path: &Path,
    reader: impl Read,
    max_bytes: usize,
    max_lines: usize,
    max_retained_bytes: usize,
) -> VResult<Value> {
    let limited = reader.take((max_bytes + 1) as u64);
    let mut reader = BufReader::new(limited);
    let mut values = Vec::new();
    let mut total_bytes = 0usize;
    let mut retained_bytes = 0usize;
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| path_io_error(path, error))?;
        if read == 0 {
            break;
        }
        total_bytes = total_bytes
            .checked_add(read)
            .ok_or_else(|| path_limit(path, "read byte accounting overflowed"))?;
        if total_bytes > max_bytes {
            return Err(path_limit(
                path,
                format_args!("eager line read exceeds the {max_bytes}-byte limit"),
            ));
        }
        if values.len() >= max_lines {
            return Err(path_limit(
                path,
                format_args!("eager line read exceeds the {max_lines}-line limit"),
            ));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let text = String::from_utf8(line).map_err(|_| {
            ErrorVal::new("utf8_error", format!("{}: not valid UTF-8", path.display()))
        })?;
        let value = Value::Str(text);
        let retained = shoal_value::retained_size(
            &value,
            shoal_value::RetainedLimits {
                max_bytes: max_retained_bytes.saturating_sub(retained_bytes),
                max_depth: 1,
                max_nodes: 1,
                opaque: shoal_value::OpaqueHandling::Reject,
                allow_secret: false,
            },
        )
        .map_err(|_| {
            path_limit(
                path,
                format_args!(
                    "eager line result exceeds the {max_retained_bytes}-byte retained-value limit"
                ),
            )
        })?;
        retained_bytes = retained_bytes
            .checked_add(retained)
            .ok_or_else(|| path_limit(path, "retained-value accounting overflowed"))?;
        values.push(value);
    }
    Ok(Value::List(values))
}

impl Evaluator {
    /// These accessors live in the evaluator because they perform I/O through
    /// the injected [`Fs`] port and resolve relative paths against session cwd.
    pub(crate) fn path_fs_method(&self, path: &Path, name: &str) -> VResult<Value> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.exec.shell.cwd.join(path)
        };
        let open = || {
            self.host
                .fs
                .open_read(&absolute)
                .map_err(|error| path_io_error(&absolute, error))
        };
        let metadata = || {
            self.host
                .fs
                .metadata(&absolute)
                .map_err(|error| path_io_error(&absolute, error))
        };
        match name {
            "read" => {
                let bytes = read_bytes_with_limit(&absolute, open()?, MAX_PATH_READ_BYTES)?;
                String::from_utf8(bytes).map(Value::Str).map_err(|_| {
                    ErrorVal::new(
                        "utf8_error",
                        format!("{}: not valid UTF-8", absolute.display()),
                    )
                })
            }
            "read_bytes" => Ok(Value::Bytes(Arc::new(read_bytes_with_limit(
                &absolute,
                open()?,
                MAX_PATH_READ_BYTES,
            )?))),
            "lines" => read_lines_with_limits(
                &absolute,
                open()?,
                MAX_PATH_READ_BYTES,
                MAX_PATH_READ_LINES,
                MAX_PATH_LINES_RETAINED_BYTES,
            ),
            "exists" => Ok(Value::Bool(self.host.fs.metadata(&absolute).is_ok())),
            "is_dir" => Ok(Value::Bool(
                self.host
                    .fs
                    .metadata(&absolute)
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
            )),
            "is_file" => Ok(Value::Bool(
                self.host
                    .fs
                    .metadata(&absolute)
                    .map(|m| m.is_file())
                    .unwrap_or(false),
            )),
            "size" => Ok(Value::Size(metadata()?.len())),
            "modified" => Ok(metadata()?
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|duration| {
                    jiff::Timestamp::from_nanosecond(duration.as_nanos() as i128).ok()
                })
                .map(|timestamp| {
                    Value::DateTime(Box::new(timestamp.to_zoned(jiff::tz::TimeZone::system())))
                })
                .unwrap_or(Value::Null)),
            _ => unreachable!("path_fs_method called with unexpected name `{name}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct GrowingReader;

    impl Read for GrowingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    #[test]
    fn growing_reader_stops_at_the_plus_one_sentinel() {
        let error = read_bytes_with_limit(Path::new("growing"), GrowingReader, 32).unwrap_err();
        assert_eq!(error.code, "path_read_limit");
        assert!(error.msg.contains("32-byte"));
    }

    #[test]
    fn line_reader_enforces_count_and_retained_state_incrementally() {
        let error = read_lines_with_limits(
            Path::new("many-lines"),
            io::Cursor::new(b"a\nb\nc\n"),
            64,
            2,
            1024,
        )
        .unwrap_err();
        assert_eq!(error.code, "path_read_limit");
        assert!(error.msg.contains("2-line"));

        let error = read_lines_with_limits(
            Path::new("wide-lines"),
            io::Cursor::new(b"abcdefgh\n"),
            64,
            8,
            4,
        )
        .unwrap_err();
        assert_eq!(error.code, "path_read_limit");
        assert!(error.msg.contains("retained-value"));

        let error = read_lines_with_limits(Path::new("growing-line"), GrowingReader, 32, 8, 1024)
            .unwrap_err();
        assert_eq!(error.code, "path_read_limit");
        assert!(error.msg.contains("32-byte"));
    }
}
