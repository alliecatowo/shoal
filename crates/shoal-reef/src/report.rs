//! Diagnostics: the serde-serializable resolution report `which` renders
//! (site/content/internals/reef-resolution.md) — everything needed to show the full scope chain and the winner.

use std::path::PathBuf;

use serde::Serialize;

/// What a single scope decided about the tool, for the `which` chain rendering.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScopeDecision {
    /// Scope label: `reef`, `mise`, `tool-versions`, `user`, `system`, `ambient`.
    pub scope: String,
    /// Manifest path this scope came from (empty for provider scopes).
    pub source: PathBuf,
    /// The constraint this scope declared, if it mentioned the tool.
    pub constraint: Option<String>,
    /// Outcome: `selected`, `shadowed`, `absent`, or `conflict`.
    pub outcome: String,
}

impl ScopeDecision {
    pub fn new(
        scope: impl Into<String>,
        source: PathBuf,
        constraint: Option<String>,
        outcome: &str,
    ) -> ScopeDecision {
        ScopeDecision {
            scope: scope.into(),
            source,
            constraint,
            outcome: outcome.into(),
        }
    }
}

/// The full resolution record for one tool.
#[derive(Debug, Clone, Serialize)]
pub struct ResolutionReport {
    pub name: String,
    /// Scope that supplied the winning constraint (`reef`, `system`, …).
    pub scope: String,
    /// The effective constraint, rendered (`22`, `*`, …).
    pub constraint: String,
    /// Resolved version (`unknown` when opaque).
    pub version: String,
    pub path: PathBuf,
    /// blake3 hex of the resolved binary.
    pub hash: String,
    pub provider: String,
    /// The full ordered scope chain, nearest first.
    pub chain: Vec<ScopeDecision>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_serializable() {
        let r = ResolutionReport {
            name: "node".into(),
            scope: "reef".into(),
            constraint: "22".into(),
            version: "22.3.0".into(),
            path: PathBuf::from("/x/node"),
            hash: "abcd".into(),
            provider: "mise".into(),
            chain: vec![ScopeDecision::new(
                "reef",
                PathBuf::from("/proj/.reef.toml"),
                Some("22".into()),
                "selected",
            )],
        };
        // serde_json isn't a dependency; assert serializability via toml.
        let t = toml::to_string(&r).unwrap();
        assert!(t.contains("node"));
        assert!(t.contains("selected"));
    }
}
