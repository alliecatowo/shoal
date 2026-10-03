//! Bounded file and structural admission for Reef manifests and lockfiles.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use shoal_value::Fs;
#[cfg(test)]
use shoal_value::StdFs;

pub const REEF_MANIFEST_MAX_BYTES: usize = 1024 * 1024;
pub const REEF_MANIFEST_MAX_NESTING: usize = 64;
pub const REEF_MAX_TOOLS: usize = 1024;
pub const REEF_MAX_RUNNERS: usize = 256;
pub const REEF_MAX_SCOPES: usize = 256;
pub const REEF_MAX_STRING_BYTES: usize = 4 * 1024;
pub(crate) const REEF_MAX_RUNNER_ARGS: usize = 128;
pub(crate) const REEF_LOCK_MAX_TOOLS: usize = 4096;

#[derive(Debug)]
pub(crate) enum InputError {
    Io { path: PathBuf, source: io::Error },
    NotFile { path: PathBuf },
    TooLarge { path: PathBuf },
    Utf8 { path: PathBuf },
}

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::NotFile { path } => {
                write!(
                    formatter,
                    "{}: manifest is not a regular file",
                    path.display()
                )
            }
            Self::TooLarge { path } => write!(
                formatter,
                "{}: manifest exceeds the {REEF_MANIFEST_MAX_BYTES}-byte limit",
                path.display()
            ),
            Self::Utf8 { path } => {
                write!(formatter, "{}: manifest is not valid UTF-8", path.display())
            }
        }
    }
}

pub(crate) fn read_optional_with(fs: &dyn Fs, path: &Path) -> Result<Option<String>, InputError> {
    let metadata = match fs.metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InputError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.is_file() {
        return Err(InputError::NotFile {
            path: path.to_path_buf(),
        });
    }
    let file = fs.open_read(path).map_err(|source| InputError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(8 * 1024);
    file.take((REEF_MANIFEST_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| InputError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > REEF_MANIFEST_MAX_BYTES {
        return Err(InputError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| InputError::Utf8 {
            path: path.to_path_buf(),
        })
}

pub(crate) fn validate_toml_text(source: &str) -> Result<(), String> {
    if source.len() > REEF_MANIFEST_MAX_BYTES {
        return Err(format!(
            "manifest exceeds the {REEF_MANIFEST_MAX_BYTES}-byte limit"
        ));
    }
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut comment = false;
    for byte in source.bytes() {
        if comment {
            if byte == b'\n' {
                comment = false;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if delimiter == b'"' && escaped {
                escaped = false;
            } else if delimiter == b'"' && byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            continue;
        }
        match byte {
            b'#' => comment = true,
            b'"' | b'\'' => quote = Some(byte),
            b'[' | b'{' => {
                depth += 1;
                if depth > REEF_MANIFEST_MAX_NESTING {
                    return Err(format!(
                        "manifest exceeds the {REEF_MANIFEST_MAX_NESTING}-level TOML nesting limit"
                    ));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn validate_string(kind: &str, value: &str) -> Result<(), String> {
    if value.len() > REEF_MAX_STRING_BYTES {
        return Err(format!(
            "{kind} is {} UTF-8 bytes; maximum is {REEF_MAX_STRING_BYTES}",
            value.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn sparse_non_utf8_and_non_file_inputs_are_typed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest");
        let file = fs::File::create(&path).unwrap();
        file.set_len((REEF_MANIFEST_MAX_BYTES + 1) as u64).unwrap();
        assert!(matches!(
            read_optional_with(&StdFs, &path),
            Err(InputError::TooLarge { .. })
        ));
        fs::write(&path, [0xff]).unwrap();
        assert!(matches!(
            read_optional_with(&StdFs, &path),
            Err(InputError::Utf8 { .. })
        ));
        assert!(matches!(
            read_optional_with(&StdFs, directory.path()),
            Err(InputError::NotFile { .. })
        ));
    }

    #[test]
    fn deep_toml_is_rejected_before_deserialization() {
        let source = format!(
            "x={}0{}",
            "[".repeat(REEF_MANIFEST_MAX_NESTING + 1),
            "]".repeat(REEF_MANIFEST_MAX_NESTING + 1)
        );
        assert!(validate_toml_text(&source).is_err());
    }
}
