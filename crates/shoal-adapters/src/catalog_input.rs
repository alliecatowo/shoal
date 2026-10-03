//! Bounded advisory input for runtime adapter manifests.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub const MAX_ADAPTER_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_ADAPTER_MANIFEST_FILES: usize = 256;
pub const MAX_ADAPTER_CATALOG_COMMANDS: usize = 1024;
pub const MAX_ADAPTER_TOML_NESTING: usize = 64;
pub const MAX_ADAPTER_MANIFEST_NODES: usize = 16 * 1024;
pub const MAX_ADAPTER_MANIFEST_STRING_BYTES: usize = 64 * 1024;

pub(crate) fn manifest_paths(dir: &Path) -> io::Result<(Vec<PathBuf>, usize)> {
    let mut selected = BTreeSet::new();
    let mut total = 0usize;
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "toml") {
            continue;
        }
        total = total.saturating_add(1);
        selected.insert(path);
        if selected.len() > MAX_ADAPTER_MANIFEST_FILES
            && let Some(last) = selected.iter().next_back().cloned()
        {
            selected.remove(&last);
        }
    }
    let omitted = total.saturating_sub(selected.len());
    Ok((selected.into_iter().collect(), omitted))
}

pub(crate) fn read_manifest(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("adapter manifest is not a regular file".into());
    }
    let file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(8 * 1024);
    file.take((MAX_ADAPTER_MANIFEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_ADAPTER_MANIFEST_BYTES {
        return Err(format!(
            "adapter manifest exceeds the {MAX_ADAPTER_MANIFEST_BYTES}-byte limit"
        ));
    }
    let source =
        String::from_utf8(bytes).map_err(|_| "adapter manifest is not valid UTF-8".to_string())?;
    validate_source(&source)?;
    Ok(source)
}

fn validate_source(source: &str) -> Result<(), String> {
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
                if depth > MAX_ADAPTER_TOML_NESTING {
                    return Err(format!(
                        "adapter manifest exceeds the {MAX_ADAPTER_TOML_NESTING}-level TOML nesting limit"
                    ));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn validate_document(document: &toml::Value) -> Result<(), String> {
    let mut stack = vec![(document, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_ADAPTER_MANIFEST_NODES {
            return Err(format!(
                "adapter manifest exceeds the {MAX_ADAPTER_MANIFEST_NODES}-node limit"
            ));
        }
        if depth > MAX_ADAPTER_TOML_NESTING {
            return Err(format!(
                "adapter manifest exceeds the {MAX_ADAPTER_TOML_NESTING}-level value-depth limit"
            ));
        }
        match value {
            toml::Value::String(string) => validate_string(string)?,
            toml::Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            toml::Value::Table(values) => {
                for (key, value) in values {
                    validate_string(key)?;
                    stack.push((value, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_string(string: &str) -> Result<(), String> {
    if string.len() > MAX_ADAPTER_MANIFEST_STRING_BYTES {
        return Err(format!(
            "adapter manifest string is {} UTF-8 bytes; maximum is {MAX_ADAPTER_MANIFEST_STRING_BYTES}",
            string.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growing_reader_shape_is_bounded_by_the_file_helper() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.toml");
        let file = fs::File::create(&path).unwrap();
        file.set_len((MAX_ADAPTER_MANIFEST_BYTES + 1) as u64)
            .unwrap();
        assert!(read_manifest(&path).unwrap_err().contains("byte limit"));
        fs::write(&path, [0xff]).unwrap();
        assert!(read_manifest(&path).unwrap_err().contains("UTF-8"));
        assert!(
            read_manifest(directory.path())
                .unwrap_err()
                .contains("regular file")
        );
    }

    #[test]
    fn deep_and_wide_documents_are_rejected() {
        let source = format!(
            "x = {}0{}\n",
            "[".repeat(MAX_ADAPTER_TOML_NESTING + 1),
            "]".repeat(MAX_ADAPTER_TOML_NESTING + 1)
        );
        assert!(validate_source(&source).is_err());

        let wide = toml::Value::Array(
            (0..=MAX_ADAPTER_MANIFEST_NODES)
                .map(|_| toml::Value::Integer(1))
                .collect(),
        );
        assert!(validate_document(&wide).is_err());
    }
}
