//! reef error type — a dependency-light mirror of `shoal-value::ErrorVal`.
//!
//! The crate deliberately does **not** depend on `shoal-value` (so `shoal-exec`
//! and other leaf crates can reuse the resolver). The exec/eval integration
//! converts [`ReefError`] into an `ErrorVal` by copying `code`/`msg`/`hint`.

use std::fmt;

/// Pinned error codes (site/content/internals/reef-resolution.md, mirrored into site/content/internals/intercrate-protocol-contracts.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReefCode {
    /// A constrained tool has no lock entry and policy forbids auto-locking.
    Unlocked,
    /// The on-disk binary hash no longer matches the lock.
    Drift,
    /// Two scopes constrain one tool incompatibly.
    Conflict,
    /// No provider offers a candidate satisfying the constraint.
    NotFound,
    /// A provider failed (probe error, install error, malformed layout).
    Provider,
}

impl ReefCode {
    /// The stable string code used on the wire and in the corpus.
    pub fn as_str(self) -> &'static str {
        match self {
            ReefCode::Unlocked => "reef_unlocked",
            ReefCode::Drift => "reef_drift",
            ReefCode::Conflict => "reef_conflict",
            ReefCode::NotFound => "reef_not_found",
            ReefCode::Provider => "reef_provider",
        }
    }
}

impl fmt::Display for ReefCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A reef error. Shape mirrors `shoal-value`'s `ErrorVal` (code/msg/hint) so
/// conversion at the integration boundary is a field copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReefError {
    pub code: ReefCode,
    pub msg: String,
    pub hint: Option<String>,
}

impl ReefError {
    pub fn new(code: ReefCode, msg: impl Into<String>) -> ReefError {
        ReefError {
            code,
            msg: msg.into(),
            hint: None,
        }
    }

    /// Attach a fix-it hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> ReefError {
        self.hint = Some(hint.into());
        self
    }

    pub fn unlocked(msg: impl Into<String>) -> ReefError {
        ReefError::new(ReefCode::Unlocked, msg)
    }
    pub fn drift(msg: impl Into<String>) -> ReefError {
        ReefError::new(ReefCode::Drift, msg)
    }
    pub fn conflict(msg: impl Into<String>) -> ReefError {
        ReefError::new(ReefCode::Conflict, msg)
    }
    pub fn not_found(msg: impl Into<String>) -> ReefError {
        ReefError::new(ReefCode::NotFound, msg)
    }
    pub fn provider(msg: impl Into<String>) -> ReefError {
        ReefError::new(ReefCode::Provider, msg)
    }

    /// The stable string code (e.g. `"reef_drift"`).
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
}

impl fmt::Display for ReefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.msg)?;
        if let Some(h) = &self.hint {
            write!(f, " ({h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ReefError {}

/// Result alias used throughout the crate.
pub type ReefResult<T> = Result<T, ReefError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_render_stable_strings() {
        assert_eq!(ReefCode::Unlocked.as_str(), "reef_unlocked");
        assert_eq!(ReefCode::Drift.as_str(), "reef_drift");
        assert_eq!(ReefCode::Conflict.as_str(), "reef_conflict");
        assert_eq!(ReefCode::NotFound.as_str(), "reef_not_found");
        assert_eq!(ReefCode::Provider.as_str(), "reef_provider");
    }

    #[test]
    fn display_includes_code_and_hint() {
        let e = ReefError::drift("hash mismatch").with_hint("reef lock --refresh");
        let s = e.to_string();
        assert!(s.contains("reef_drift"));
        assert!(s.contains("hash mismatch"));
        assert!(s.contains("reef lock --refresh"));
    }
}
