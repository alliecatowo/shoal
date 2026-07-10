//! Lenient version + constraint types.
//!
//! Versions are parsed leniently: a leading run of dot-separated integers forms
//! the numeric release; anything else (a git sha, `latest-lts`, an opaque
//! `--version` blob we could not parse) is kept as an *opaque* string that only
//! compares by raw text. Cargo binaries whose version we never learn are
//! [`Version::unknown`].
//!
//! Constraints are equally lenient: `"22"`, `"22.3"`, `"3.12.4"` are numeric
//! prefixes; `"*"`/`"latest"` match anything; anything else is a raw prefix
//! match on the candidate's version string.

use std::cmp::Ordering;
use std::fmt;

/// A parsed version.
///
/// `PartialEq`/`Eq` are implemented explicitly (not derived) to agree with
/// [`Ord`]: two versions are equal exactly when [`Version::cmp`] says
/// [`Ordering::Equal`], so `Version::parse("22") == Version::parse("22.0")`
/// (zero-padded numeric equality) even though their raw text differs.
#[derive(Debug, Clone)]
pub struct Version {
    /// Numeric release components (e.g. `[3, 12, 4]`). Empty when opaque/unknown.
    parts: Vec<u64>,
    /// Pre-release tag (`-rc1`), if any. A version *with* a pre-release sorts
    /// below the same release *without* one.
    pre: Option<String>,
    /// Original text, used for display and opaque comparison.
    raw: String,
    /// `true` when we have no numeric information at all (cargo bins).
    unknown: bool,
}

impl Version {
    /// Parse a version string leniently. Never fails.
    pub fn parse(s: &str) -> Version {
        let raw = s.trim().to_string();
        if raw.is_empty() {
            return Version::unknown();
        }
        // Strip a leading 'v' (v22.3.0).
        let body = raw.strip_prefix('v').unwrap_or(&raw);
        // Split off pre-release / build metadata on the first '-' or '+'.
        let (release, pre) = match body.find(['-', '+']) {
            Some(i) => (&body[..i], Some(body[i + 1..].to_string())),
            None => (body, None),
        };
        let mut parts = Vec::new();
        for seg in release.split('.') {
            match seg.parse::<u64>() {
                Ok(n) => parts.push(n),
                Err(_) => {
                    // Non-numeric segment: stop; if we have nothing, it's opaque.
                    break;
                }
            }
        }
        Version { parts, pre, raw, unknown: false }
    }

    /// A version we could not determine (cargo bins). Compares below everything.
    pub fn unknown() -> Version {
        Version { parts: Vec::new(), pre: None, raw: String::new(), unknown: true }
    }

    /// `true` when no numeric information is available.
    pub fn is_unknown(&self) -> bool {
        self.unknown
    }

    /// `true` when there is no numeric release (opaque non-semver text or unknown).
    pub fn is_opaque(&self) -> bool {
        self.parts.is_empty()
    }

    /// Numeric release components.
    pub fn parts(&self) -> &[u64] {
        &self.parts
    }

    /// Original text.
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.unknown {
            f.write_str("unknown")
        } else {
            f.write_str(&self.raw)
        }
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Unknown sorts below everything; two unknowns are equal.
        match (self.unknown, other.unknown) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        // Numeric versions outrank opaque ones.
        match (self.parts.is_empty(), other.parts.is_empty()) {
            (true, true) => return self.raw.cmp(&other.raw),
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (false, false) => {}
        }
        // Component-wise, padding the shorter with zeros.
        let n = self.parts.len().max(other.parts.len());
        for i in 0..n {
            let a = self.parts.get(i).copied().unwrap_or(0);
            let b = other.parts.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                ne => return ne,
            }
        }
        // Equal release: a pre-release sorts below a final release.
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

/// A version constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// `"*"` — any version. Never needs a version probe to satisfy.
    Any,
    /// `"latest"` — any version, but semantically "prefer the newest".
    Latest,
    /// Numeric prefix: `"22"` → `[22]`, `"3.12.4"` → `[3, 12, 4]`. A version
    /// satisfies it when its leading numeric parts equal these.
    Prefix(Vec<u64>),
    /// Non-semver: prefix match on the candidate's raw version string.
    RawPrefix(String),
}

impl Constraint {
    /// Parse a constraint string leniently. Never fails.
    pub fn parse(s: &str) -> Constraint {
        let t = s.trim();
        match t {
            "*" | "" => return Constraint::Any,
            "latest" => return Constraint::Latest,
            _ => {}
        }
        let body = t.strip_prefix('v').unwrap_or(t);
        // Is it a pure dotted-integer prefix?
        let mut parts = Vec::new();
        let mut all_numeric = true;
        for seg in body.split('.') {
            match seg.parse::<u64>() {
                Ok(n) => parts.push(n),
                Err(_) => {
                    all_numeric = false;
                    break;
                }
            }
        }
        if all_numeric && !parts.is_empty() {
            Constraint::Prefix(parts)
        } else {
            Constraint::RawPrefix(t.to_string())
        }
    }

    /// Does this constraint require knowing a concrete version to test? `*` and
    /// `latest` do not (so the system provider need not probe `--version`).
    pub fn needs_version(&self) -> bool {
        matches!(self, Constraint::Prefix(_) | Constraint::RawPrefix(_))
    }

    /// Does `v` satisfy this constraint?
    pub fn satisfies(&self, v: &Version) -> bool {
        match self {
            Constraint::Any | Constraint::Latest => true,
            Constraint::Prefix(want) => {
                if v.parts.len() < want.len() {
                    return false;
                }
                v.parts[..want.len()] == want[..]
            }
            Constraint::RawPrefix(p) => v.raw.starts_with(p.as_str()),
        }
    }

    /// Are two constraints *compatible* — could they co-exist without conflict?
    ///
    /// `Any`/`Latest` are compatible with everything. Two numeric prefixes are
    /// compatible when one is a prefix of the other (`22` refines to `22.3`).
    /// Two raw prefixes are compatible when one is a prefix of the other. A
    /// numeric and a raw prefix are only compatible via a wildcard.
    pub fn compatible(&self, other: &Constraint) -> bool {
        use Constraint::*;
        match (self, other) {
            (Any, _) | (_, Any) | (Latest, _) | (_, Latest) => true,
            (Prefix(a), Prefix(b)) => {
                let n = a.len().min(b.len());
                a[..n] == b[..n]
            }
            (RawPrefix(a), RawPrefix(b)) => a.starts_with(b.as_str()) || b.starts_with(a.as_str()),
            _ => false,
        }
    }

    /// The more specific of two *compatible* constraints (used when a nearer and
    /// farther scope both refine the same tool). Falls back to `self` when they
    /// are the same specificity.
    pub fn refine<'a>(&'a self, other: &'a Constraint) -> &'a Constraint {
        use Constraint::*;
        match (self, other) {
            (Any | Latest, _) => other,
            (_, Any | Latest) => self,
            (Prefix(a), Prefix(b)) => {
                if b.len() > a.len() { other } else { self }
            }
            (RawPrefix(a), RawPrefix(b)) => {
                if b.len() > a.len() { other } else { self }
            }
            _ => self,
        }
    }
}

impl fmt::Display for Constraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Constraint::Any => f.write_str("*"),
            Constraint::Latest => f.write_str("latest"),
            Constraint::Prefix(p) => {
                let s: Vec<String> = p.iter().map(|n| n.to_string()).collect();
                f.write_str(&s.join("."))
            }
            Constraint::RawPrefix(s) => f.write_str(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_versions() {
        let v = Version::parse("3.12.4");
        assert_eq!(v.parts(), &[3, 12, 4]);
        assert!(!v.is_opaque());
    }

    #[test]
    fn parses_v_prefix_and_prerelease() {
        let v = Version::parse("v22.3.0-rc1");
        assert_eq!(v.parts(), &[22, 3, 0]);
        let final_v = Version::parse("22.3.0");
        assert!(final_v > v, "final release outranks pre-release");
    }

    #[test]
    fn opaque_version_from_nonnumeric() {
        let v = Version::parse("deadbeef");
        assert!(v.is_opaque());
        assert!(!v.is_unknown());
        assert_eq!(v.raw(), "deadbeef");
    }

    #[test]
    fn ordering_pads_and_compares() {
        assert!(Version::parse("22") < Version::parse("22.1"));
        assert!(Version::parse("22.0.0") == Version::parse("22.0.0"));
        assert!(Version::parse("1.10.0") > Version::parse("1.9.0"));
        assert!(Version::parse("22") == Version::parse("22.0"));
    }

    #[test]
    fn unknown_sorts_lowest() {
        assert!(Version::unknown() < Version::parse("0.0.1"));
        assert!(Version::unknown() < Version::parse("deadbeef"));
        assert_eq!(Version::unknown().cmp(&Version::unknown()), Ordering::Equal);
    }

    #[test]
    fn numeric_outranks_opaque() {
        assert!(Version::parse("1.0.0") > Version::parse("deadbeef"));
    }

    #[test]
    fn constraint_parse_variants() {
        assert_eq!(Constraint::parse("*"), Constraint::Any);
        assert_eq!(Constraint::parse("latest"), Constraint::Latest);
        assert_eq!(Constraint::parse("22"), Constraint::Prefix(vec![22]));
        assert_eq!(Constraint::parse("3.12.4"), Constraint::Prefix(vec![3, 12, 4]));
        assert_eq!(Constraint::parse("nightly"), Constraint::RawPrefix("nightly".into()));
    }

    #[test]
    fn satisfaction_matrix() {
        let c22 = Constraint::parse("22");
        assert!(c22.satisfies(&Version::parse("22.3.0")));
        assert!(c22.satisfies(&Version::parse("22")));
        assert!(!c22.satisfies(&Version::parse("20.1.0")));
        assert!(!c22.satisfies(&Version::parse("2.22.0")));

        let c3124 = Constraint::parse("3.12.4");
        assert!(c3124.satisfies(&Version::parse("3.12.4")));
        assert!(!c3124.satisfies(&Version::parse("3.12.5")));
        assert!(!c3124.satisfies(&Version::parse("3.12")));

        assert!(Constraint::Any.satisfies(&Version::unknown()));
        assert!(!c22.satisfies(&Version::unknown()));

        let raw = Constraint::parse("nightly");
        assert!(raw.satisfies(&Version::parse("nightly-2024")));
        assert!(!raw.satisfies(&Version::parse("stable")));
    }

    #[test]
    fn needs_version_flag() {
        assert!(!Constraint::Any.needs_version());
        assert!(!Constraint::Latest.needs_version());
        assert!(Constraint::parse("22").needs_version());
        assert!(Constraint::parse("nightly").needs_version());
    }

    #[test]
    fn compatibility() {
        let c22 = Constraint::parse("22");
        let c223 = Constraint::parse("22.3");
        let c20 = Constraint::parse("20");
        assert!(c22.compatible(&c223), "22 and 22.3 refine");
        assert!(!c22.compatible(&c20), "22 and 20 disjoint");
        assert!(c22.compatible(&Constraint::Any));
        assert!(!c22.compatible(&Constraint::parse("nightly")));
    }

    #[test]
    fn refine_picks_more_specific() {
        let c22 = Constraint::parse("22");
        let c223 = Constraint::parse("22.3");
        assert_eq!(c22.refine(&c223), &c223);
        assert_eq!(c223.refine(&c22), &c223);
        assert_eq!(Constraint::Any.refine(&c22), &c22);
    }
}
