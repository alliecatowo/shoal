//! Bounded policy input loading and structural validation.

use super::*;

pub(super) fn flatten_namespace(table: &mut toml::Table, namespace: &str, fields: &[&str]) {
    let Some(nested) = table.remove(namespace).and_then(|v| v.as_table().cloned()) else {
        return;
    };
    for field in fields {
        if let Some(value) = nested.get(*field) {
            table.insert(format!("{namespace}.{field}"), value.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyParseError {
    pub msg: String,
}

impl PolicyParseError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self {
            msg: message.into(),
        }
    }

    pub(super) fn toml(error: toml::de::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl std::fmt::Display for PolicyParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for PolicyParseError {}

#[derive(Debug)]
pub enum PolicyLoadError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    NotFile {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        max_bytes: usize,
    },
    Utf8 {
        path: PathBuf,
    },
    Parse {
        path: PathBuf,
        source: PolicyParseError,
    },
}
impl std::fmt::Display for PolicyLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::NotFile { path } => {
                write!(f, "{}: policy is not a regular file", path.display())
            }
            Self::TooLarge { path, max_bytes } => write!(
                f,
                "{}: policy exceeds the {max_bytes}-byte limit",
                path.display()
            ),
            Self::Utf8 { path } => write!(f, "{}: policy is not valid UTF-8", path.display()),
            Self::Parse { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}
impl std::error::Error for PolicyLoadError {}

pub(super) fn read_policy_utf8(path: &Path, reader: impl Read) -> Result<String, PolicyLoadError> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    reader
        .take((POLICY_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| PolicyLoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > POLICY_MAX_BYTES {
        return Err(PolicyLoadError::TooLarge {
            path: path.to_path_buf(),
            max_bytes: POLICY_MAX_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| PolicyLoadError::Utf8 {
        path: path.to_path_buf(),
    })
}

pub(super) fn validate_policy_text(source: &str) -> Result<(), PolicyParseError> {
    if source.len() > POLICY_MAX_BYTES {
        return Err(PolicyParseError::new(format!(
            "policy exceeds the {POLICY_MAX_BYTES}-byte limit"
        )));
    }
    let mut depth = 0usize;
    let mut assignments = 0usize;
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
                if depth > POLICY_MAX_NESTING {
                    return Err(PolicyParseError::new(format!(
                        "policy exceeds the {POLICY_MAX_NESTING}-level TOML nesting limit"
                    )));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            b'=' => {
                assignments += 1;
                if assignments > POLICY_MAX_ASSIGNMENTS {
                    return Err(PolicyParseError::new(format!(
                        "policy exceeds the {POLICY_MAX_ASSIGNMENTS}-assignment limit"
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn validate_policy_doc(doc: &PolicyDoc) -> Result<(), PolicyParseError> {
    if doc.principal.len() > POLICY_MAX_PRINCIPALS {
        return Err(PolicyParseError::new(format!(
            "policy has {} principals; maximum is {POLICY_MAX_PRINCIPALS}",
            doc.principal.len()
        )));
    }
    for (name, policy) in &doc.principal {
        validate_policy_string("principal name", name)?;
        for (kind, grants) in [
            ("fs.read", &policy.fs_read),
            ("fs.write", &policy.fs_write),
            ("fs.delete", &policy.fs_delete),
            ("net_connect", &policy.net_connect),
            ("proc_spawn", &policy.proc_spawn),
            ("env_read", &policy.env_read),
            ("env_write", &policy.env_write),
            ("secret_use", &policy.secret_use),
        ] {
            if grants.len() > POLICY_MAX_GRANTS_PER_KIND {
                return Err(PolicyParseError::new(format!(
                    "principal {name:?} has {} {kind} grants; maximum is {POLICY_MAX_GRANTS_PER_KIND}",
                    grants.len()
                )));
            }
            for grant in grants {
                validate_policy_string(kind, grant)?;
            }
        }
        if policy.net_listen.len() > POLICY_MAX_GRANTS_PER_KIND {
            return Err(PolicyParseError::new(format!(
                "principal {name:?} has {} net_listen grants; maximum is {POLICY_MAX_GRANTS_PER_KIND}",
                policy.net_listen.len()
            )));
        }
        if policy.process_cpu_seconds == Some(0) {
            return Err(PolicyParseError::new(format!(
                "principal {name:?} process_cpu_seconds must be greater than zero"
            )));
        }
        if policy.process_memory_bytes == Some(0) {
            return Err(PolicyParseError::new(format!(
                "principal {name:?} process_memory_bytes must be greater than zero"
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_policy_string(kind: &str, value: &str) -> Result<(), PolicyParseError> {
    if value.len() > POLICY_MAX_GRANT_BYTES {
        return Err(PolicyParseError::new(format!(
            "{kind} value is {} UTF-8 bytes; maximum is {POLICY_MAX_GRANT_BYTES}",
            value.len()
        )));
    }
    Ok(())
}
