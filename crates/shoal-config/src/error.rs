//! [`ConfigError`] — every way loading a `shoal.toml` can fail.
//!
//! The overriding rule (site/content/internals/configuration-reference.md): a bad config is always a value,
//! never a panic. Every variant here carries enough context — which file
//! (when known), which dotted key path, what was expected vs found — to print
//! one precise, single-line diagnostic without the caller doing any more work
//! than `eprintln!("{err}")`.

use std::fmt;
use std::path::PathBuf;

/// Everything that can go wrong loading a layered shoal configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    /// A config file exists but couldn't be read (permissions, a directory
    /// where a file was expected, …). Note: a *missing* file is not an
    /// error — every layer is optional (site/content/internals/configuration-reference.md) — this variant is
    /// only for a file that exists but can't be opened/read.
    Io { path: PathBuf, message: String },
    /// Malformed TOML syntax. `message` is `toml`'s own rendered diagnostic
    /// (it already includes a line/column pointer).
    Parse { path: PathBuf, message: String },
    /// A key held a value of the wrong TOML type (e.g. `history.enabled =
    /// "yes"` instead of a boolean).
    Type {
        source: Option<PathBuf>,
        key: String,
        expected: &'static str,
        found: &'static str,
    },
    /// A value parsed as the right *type* but failed a semantic check (e.g.
    /// `history.max_entries = 0`, an unsupported `version`, an empty alias
    /// name).
    Value {
        source: Option<PathBuf>,
        key: String,
        message: String,
    },
    /// An environment variable override held a value shoal couldn't coerce
    /// to the target key's type (e.g. `SHOAL_HISTORY_ENABLED=maybe`).
    Env { var: String, message: String },
}

impl ConfigError {
    /// Attach the source file `path` to this error if it doesn't already
    /// carry one — used to point a schema-check failure (which only sees the
    /// in-memory value tree) back at the file that produced it.
    pub(crate) fn with_source(mut self, path: &std::path::Path) -> Self {
        match &mut self {
            ConfigError::Type { source, .. } | ConfigError::Value { source, .. } => {
                if source.is_none() {
                    *source = Some(path.to_path_buf());
                }
            }
            ConfigError::Io { .. } | ConfigError::Parse { .. } | ConfigError::Env { .. } => {}
        }
        self
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, message } => write!(f, "{}: {message}", path.display()),
            ConfigError::Parse { path, message } => write!(f, "{}: {message}", path.display()),
            ConfigError::Type {
                source,
                key,
                expected,
                found,
            } => {
                if let Some(p) = source {
                    write!(
                        f,
                        "{}: {key}: expected {expected}, found {found}",
                        p.display()
                    )
                } else {
                    write!(f, "{key}: expected {expected}, found {found}")
                }
            }
            ConfigError::Value {
                source,
                key,
                message,
            } => {
                if let Some(p) = source {
                    write!(f, "{}: {key}: {message}", p.display())
                } else {
                    write!(f, "{key}: {message}")
                }
            }
            ConfigError::Env { var, message } => write!(f, "{var}: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

// Lets every existing `fn foo() -> Result<_, String> { ...; shoal_config::load(&o)?; ... }`
// call site keep compiling unchanged: `?` reaches for `String: From<ConfigError>`.
impl From<ConfigError> for String {
    fn from(e: ConfigError) -> String {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_error_display_with_and_without_source() {
        let with_source = ConfigError::Type {
            source: Some(PathBuf::from("/etc/shoal/shoal.toml")),
            key: "history.enabled".into(),
            expected: "a boolean",
            found: "string",
        };
        assert_eq!(
            with_source.to_string(),
            "/etc/shoal/shoal.toml: history.enabled: expected a boolean, found string"
        );
        let without_source = ConfigError::Type {
            source: None,
            key: "history.enabled".into(),
            expected: "a boolean",
            found: "string",
        };
        assert_eq!(
            without_source.to_string(),
            "history.enabled: expected a boolean, found string"
        );
    }

    #[test]
    fn converts_into_string_via_question_mark() {
        fn produces() -> Result<(), ConfigError> {
            Err(ConfigError::Env {
                var: "SHOAL_HISTORY_ENABLED".into(),
                message: "expected true/false, got `maybe`".into(),
            })
        }
        fn caller() -> Result<(), String> {
            produces()?;
            Ok(())
        }
        assert_eq!(
            caller().unwrap_err(),
            "SHOAL_HISTORY_ENABLED: expected true/false, got `maybe`"
        );
    }

    #[test]
    fn with_source_does_not_override_existing() {
        let e = ConfigError::Value {
            source: Some(PathBuf::from("/first")),
            key: "k".into(),
            message: "m".into(),
        }
        .with_source(std::path::Path::new("/second"));
        match e {
            ConfigError::Value { source, .. } => {
                assert_eq!(source, Some(PathBuf::from("/first")))
            }
            _ => panic!("wrong variant"),
        }
    }
}
