//! Bounded, mediated reads for Shoal program text and script headers.

use super::*;
use std::io::Read;

const SHEBANG_MAX_BYTES: usize = 8 * 1024;

fn read_source_utf8(path: &Path, kind: &str, reader: impl Read) -> Result<String, ErrorVal> {
    let max_bytes = shoal_syntax::MAX_SOURCE_BYTES;
    let mut bytes = Vec::with_capacity(8 * 1024);
    reader
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ErrorVal::new(
                "io_error",
                format!("cannot read {kind} {}: {error}", path.display()),
            )
        })?;
    if bytes.len() > max_bytes {
        return Err(ErrorVal::new(
            "source_too_large",
            format!(
                "{kind} {} exceeds the {max_bytes}-byte source limit",
                path.display()
            ),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        ErrorVal::new(
            "source_utf8",
            format!("{kind} {} is not valid UTF-8", path.display()),
        )
    })
}

impl Evaluator {
    pub(crate) fn read_shoal_source(&self, path: &Path, kind: &str) -> VResult<String> {
        let reader = self.host.fs.open_read(path).map_err(|error| {
            ErrorVal::new(
                "io_error",
                format!("cannot read {kind} {}: {error}", path.display()),
            )
        })?;
        read_source_utf8(path, kind, reader)
    }

    /// Read, parse, and evaluate one Shoal source file through this evaluator's
    /// inherited filesystem capability. Interactive init uses this entry point
    /// so a host cannot accidentally bypass an installed `Fs` adapter.
    pub fn eval_source_file(&mut self, path: &Path) -> VResult<Value> {
        let source = self.read_shoal_source(path, "source")?;
        let program = shoal_syntax::parse(&source)
            .map_err(|error| ErrorVal::new("parse_error", error.to_string()))?;
        self.eval_program(&program)
    }

    /// Read only a bounded script header. A non-UTF-8 body after the first
    /// newline does not invalidate an otherwise valid shebang.
    pub(crate) fn read_shebang_line(&self, path: &Path) -> Option<String> {
        let reader = self.host.fs.open_read(path).ok()?;
        let mut bytes = Vec::with_capacity(256);
        reader
            .take((SHEBANG_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        let end = bytes.iter().position(|byte| *byte == b'\n');
        if end.is_none() && bytes.len() > SHEBANG_MAX_BYTES {
            return None;
        }
        let line = &bytes[..end.unwrap_or(bytes.len())];
        std::str::from_utf8(line).ok().map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Seek, SeekFrom};

    struct GrowingReader {
        position: u64,
        length: u64,
    }

    impl Read for GrowingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let remaining = self.length.saturating_sub(self.position) as usize;
            let count = remaining.min(buffer.len());
            buffer[..count].fill(b'x');
            self.position += count as u64;
            Ok(count)
        }
    }

    impl Seek for GrowingReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.position = match position {
                SeekFrom::Start(position) => position,
                SeekFrom::Current(delta) => self.position.saturating_add_signed(delta),
                SeekFrom::End(delta) => self.length.saturating_add_signed(delta),
            };
            Ok(self.position)
        }
    }

    #[test]
    fn growing_source_stops_at_the_parser_limit_sentinel() {
        let error = read_source_utf8(
            Path::new("growing.shl"),
            "module",
            GrowingReader {
                position: 0,
                length: (shoal_syntax::MAX_SOURCE_BYTES as u64) * 8,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "source_too_large");
        assert!(error.msg.contains("growing.shl"));
    }

    #[test]
    fn non_utf8_source_has_a_typed_path_aware_error() {
        let error = read_source_utf8(
            Path::new("binary.shl"),
            "script",
            io::Cursor::new(vec![0xff]),
        )
        .unwrap_err();
        assert_eq!(error.code, "source_utf8");
        assert!(error.msg.contains("binary.shl"));
    }

    #[test]
    fn owned_source_consumers_do_not_bypass_the_bounded_reader() {
        for (name, source) in [
            ("command", include_str!("command.rs")),
            ("modules", include_str!("modules.rs")),
            ("script", include_str!("script.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap();
            assert!(
                !production.contains("read_to_string"),
                "{name} reintroduced an unbounded whole-source read"
            );
        }
    }
}
