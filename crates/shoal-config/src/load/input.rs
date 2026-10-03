//! Bounded, path-aware ingestion for untrusted configuration layers.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use crate::ConfigError;

use super::{CONFIG_FILE_MAX_BYTES, CONFIG_TOML_MAX_NESTING};

fn io_error(path: &Path, error: io::Error) -> ConfigError {
    ConfigError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

/// Read one optional layer without trusting metadata length for allocation.
/// The preliminary metadata check rejects ordinary directories/devices/FIFOs
/// before open and follows symlinks, preserving symlink-to-file support. The
/// bounded reader remains authoritative if the file grows after that check.
pub(super) fn read_config_file(path: &Path) -> Result<Option<String>, ConfigError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(path, error)),
    };
    if !metadata.is_file() {
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            message: "configuration layer is not a regular file".into(),
        });
    }
    let file = fs::File::open(path).map_err(|error| io_error(path, error))?;
    read_config_utf8(path, file).map(Some)
}

fn read_config_utf8(path: &Path, reader: impl Read) -> Result<String, ConfigError> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    reader
        .take((CONFIG_FILE_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() > CONFIG_FILE_MAX_BYTES {
        return Err(ConfigError::TooLarge {
            path: path.to_path_buf(),
            max_bytes: CONFIG_FILE_MAX_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| ConfigError::Utf8 {
        path: path.to_path_buf(),
    })
}

/// Reject bracket-shaped TOML recursion before invoking the TOML parser. This
/// scanner deliberately tracks quoted strings/comments so data such as
/// `template = "[[["` does not consume the structure budget.
pub(super) fn check_toml_nesting(path: &Path, text: &str) -> Result<(), ConfigError> {
    #[derive(Clone, Copy)]
    enum Quote {
        Basic { triple: bool, escaped: bool },
        Literal { triple: bool },
    }

    let bytes = text.as_bytes();
    let mut quote = None;
    let mut comment = false;
    let mut depth = 0usize;
    let mut key_dots = 0usize;
    let mut in_value = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if comment {
            if byte == b'\n' {
                comment = false;
                key_dots = 0;
                in_value = false;
            }
            index += 1;
            continue;
        }
        match quote {
            Some(Quote::Basic {
                triple,
                mut escaped,
            }) => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"'
                    && (!triple || bytes.get(index..index + 3) == Some(b"\"\"\""))
                {
                    quote = None;
                    index += if triple { 3 } else { 1 };
                    continue;
                }
                quote = Some(Quote::Basic { triple, escaped });
            }
            Some(Quote::Literal { triple }) => {
                if byte == b'\'' && (!triple || bytes.get(index..index + 3) == Some(b"'''")) {
                    quote = None;
                    index += if triple { 3 } else { 1 };
                    continue;
                }
            }
            None => match byte {
                b'#' => comment = true,
                b'\n' => {
                    key_dots = 0;
                    in_value = false;
                }
                b'=' => in_value = true,
                b'.' if !in_value => {
                    key_dots += 1;
                    if key_dots >= CONFIG_TOML_MAX_NESTING {
                        return Err(ConfigError::Complexity {
                            path: path.to_path_buf(),
                            max_nesting: CONFIG_TOML_MAX_NESTING,
                        });
                    }
                }
                b'"' => {
                    let triple = bytes.get(index..index + 3) == Some(b"\"\"\"");
                    quote = Some(Quote::Basic {
                        triple,
                        escaped: false,
                    });
                    if triple {
                        index += 2;
                    }
                }
                b'\'' => {
                    let triple = bytes.get(index..index + 3) == Some(b"'''");
                    quote = Some(Quote::Literal { triple });
                    if triple {
                        index += 2;
                    }
                }
                b'[' | b'{' => {
                    depth += 1;
                    if depth > CONFIG_TOML_MAX_NESTING {
                        return Err(ConfigError::Complexity {
                            path: path.to_path_buf(),
                            max_nesting: CONFIG_TOML_MAX_NESTING,
                        });
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            },
        }
        index += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct GrowingReader {
        remaining: usize,
    }

    impl Read for GrowingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = self.remaining.min(buf.len());
            buf[..count].fill(b'x');
            self.remaining -= count;
            Ok(count)
        }
    }

    #[test]
    fn growing_reader_stops_after_the_limit_sentinel() {
        let path = Path::new("growing.toml");
        let error = read_config_utf8(
            path,
            GrowingReader {
                remaining: CONFIG_FILE_MAX_BYTES * 4,
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            ConfigError::TooLarge {
                path: path.to_path_buf(),
                max_bytes: CONFIG_FILE_MAX_BYTES,
            }
        );
    }

    #[test]
    fn layered_reader_cannot_regress_to_whole_file_reads() {
        let production = include_str!("input.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!production.contains("fs::read_to_string"));
        assert!(production.contains("CONFIG_FILE_MAX_BYTES + 1"));
    }
}
