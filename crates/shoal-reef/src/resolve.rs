//! The resolver — name → version → hash → binding (REEF.md §2), with the lock,
//! drift detection, conflict detection, and the interactive/script policy split.

use std::path::PathBuf;

use crate::error::{ReefError, ReefResult};
use crate::hashcache::HashCache;
use crate::lock::{LockEntry, Lockfile};
use crate::provider::{Candidate, Provider, ProviderCtx};
use crate::report::{ResolutionReport, ScopeDecision};
use crate::scope::ScopeChain;
use crate::timestamp::now_rfc3339;
use crate::version::{Constraint, Version};

use crate::provider::{CargoProvider, MiseProvider, NpmLocalProvider, SystemProvider, VenvProvider};

/// Lock policy at resolve time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Auto-lock on a miss and emit a notice via the callback.
    Interactive,
    /// Error (`reef_unlocked`) on a constrained miss — CI/scripts don't guess.
    Script,
}

/// A one-line notice emitted when the resolver auto-locks a tool.
#[derive(Debug, Clone)]
pub struct LockNotice {
    pub name: String,
    pub version: String,
    pub provider: String,
    pub path: PathBuf,
}

/// A completed resolution.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub report: ResolutionReport,
    pub path: PathBuf,
    pub version: Version,
    pub provider: String,
    pub hash: String,
    /// Whether some scope explicitly constrained this tool.
    pub constrained: bool,
    /// Whether this call wrote a fresh lock entry.
    pub locked_now: bool,
}

/// The effective decision for a tool after walking the chain.
struct Decision {
    constraint: Constraint,
    provider_pin: Option<String>,
    scope_label: String,
    #[allow(dead_code)]
    source: PathBuf,
    constrained: bool,
}

/// The resolver: an ordered provider list (index 0 = highest tiebreak
/// precedence) plus a content-hash cache.
pub struct Resolver {
    providers: Vec<Box<dyn Provider>>,
    hashes: HashCache,
}

impl Resolver {
    /// Build a resolver from an explicit, ordered provider list.
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Resolver {
        Resolver { providers, hashes: HashCache::new() }
    }

    /// The default provider stack, in tiebreak-precedence order:
    /// `npm-local, venv, mise, cargo, system`. Scoped providers (npm-local,
    /// venv) win ties over global ones; the ambient system provider is last.
    pub fn with_defaults() -> Resolver {
        Resolver::new(vec![
            Box::new(NpmLocalProvider::new()),
            Box::new(VenvProvider::new()),
            Box::new(MiseProvider::from_env()),
            Box::new(CargoProvider::from_env()),
            Box::new(SystemProvider::from_env()),
        ])
    }

    /// The providers, in precedence order.
    pub fn providers(&self) -> &[Box<dyn Provider>] {
        &self.providers
    }

    /// Resolve `name` against `chain` under `policy`, consulting and updating
    /// `lock`. `notice` is called once if a fresh lock entry is written.
    ///
    /// Errors: `reef_conflict` (incompatible scopes), `reef_unlocked` (script
    /// policy + constrained miss), `reef_drift` (on-disk hash ≠ lock),
    /// `reef_not_found` (no satisfying candidate).
    pub fn resolve(
        &self,
        name: &str,
        chain: &ScopeChain,
        lock: &mut Lockfile,
        policy: Policy,
        notice: &mut dyn FnMut(&LockNotice),
    ) -> ReefResult<Resolution> {
        let decision = self.effective_decision(chain, name)?;

        // Honor a valid, satisfying lock entry (with a drift check).
        if let Some(entry) = lock.get(name)
            && self.lock_entry_valid(entry, &decision)
        {
            return self.resolution_from_lock(name, chain, &decision, entry.clone());
        }

        // A constrained miss under script policy is a hard error.
        if decision.constrained && policy == Policy::Script {
            return Err(ReefError::unlocked(format!(
                "{name} is constrained ({}) but not locked",
                decision.constraint
            ))
            .with_hint("run `reef lock` (or resolve interactively) to pin it"));
        }

        self.resolve_fresh(name, chain, &decision, Some((lock, notice)))
    }

    /// Force a fresh resolution and rewrite the lock entry (the `reef lock
    /// --refresh` / relock path).
    pub fn refresh_lock(
        &self,
        name: &str,
        chain: &ScopeChain,
        lock: &mut Lockfile,
        notice: &mut dyn FnMut(&LockNotice),
    ) -> ReefResult<Resolution> {
        let decision = self.effective_decision(chain, name)?;
        lock.remove(name);
        self.resolve_fresh(name, chain, &decision, Some((lock, notice)))
    }

    // --- internals ---------------------------------------------------------

    fn effective_decision(&self, chain: &ScopeChain, name: &str) -> ReefResult<Decision> {
        let mentioning: Vec<_> = chain
            .scopes
            .iter()
            .filter(|s| s.manifest.tools.contains_key(name))
            .collect();

        if mentioning.is_empty() {
            return Ok(Decision {
                constraint: Constraint::Any,
                provider_pin: None,
                scope_label: "system".into(),
                source: PathBuf::new(),
                constrained: false,
            });
        }

        // Nearest scope supplies the base value.
        let nearest = mentioning[0];
        let base = &nearest.manifest.tools[name];
        let mut effective = base.constraint.clone();
        let mut provider_pin = base.provider.clone();

        for scope in &mentioning[1..] {
            let req = &scope.manifest.tools[name];
            if !effective.compatible(&req.constraint) {
                return Err(ReefError::conflict(format!(
                    "{name}: {} ({}) is incompatible with {} ({})",
                    nearest.label(),
                    effective,
                    scope.label(),
                    req.constraint
                ))
                .with_hint(format!(
                    "reconcile {} and {}",
                    nearest.source.display(),
                    scope.source.display()
                )));
            }
            effective = effective.refine(&req.constraint).clone();
            if let Some(p) = &req.provider {
                match &provider_pin {
                    Some(existing) if existing != p => {
                        return Err(ReefError::conflict(format!(
                            "{name}: provider pinned to both {existing} and {p}"
                        )));
                    }
                    None => provider_pin = Some(p.clone()),
                    _ => {}
                }
            }
        }

        Ok(Decision {
            constraint: effective,
            provider_pin,
            scope_label: nearest.label().to_string(),
            source: nearest.source.clone(),
            constrained: true,
        })
    }

    fn lock_entry_valid(&self, entry: &LockEntry, decision: &Decision) -> bool {
        if let Some(pin) = &decision.provider_pin
            && &entry.provider != pin
        {
            return false;
        }
        decision.constraint.satisfies(&Version::parse(&entry.version))
    }

    fn resolution_from_lock(
        &self,
        name: &str,
        chain: &ScopeChain,
        decision: &Decision,
        entry: LockEntry,
    ) -> ReefResult<Resolution> {
        // Drift check: re-hash the on-disk binary.
        let current = self.hashes.hash_file(&entry.path).map_err(|e| {
            ReefError::not_found(format!("locked binary {} unreadable: {e}", entry.path.display()))
                .with_hint("run `reef lock --refresh`")
        })?;
        if current != entry.blake3 {
            return Err(ReefError::drift(format!(
                "{name}: on-disk hash {} != locked {}",
                short(&current),
                short(&entry.blake3)
            ))
            .with_hint("run `reef lock --refresh`"));
        }

        let version = Version::parse(&entry.version);
        let chain_decisions = self.chain_decisions(chain, name, &decision.scope_label, true);
        let report = ResolutionReport {
            name: name.to_string(),
            scope: decision.scope_label.clone(),
            constraint: decision.constraint.to_string(),
            version: entry.version.clone(),
            path: entry.path.clone(),
            hash: entry.blake3.clone(),
            provider: entry.provider.clone(),
            chain: chain_decisions,
        };
        Ok(Resolution {
            report,
            path: entry.path,
            version,
            provider: entry.provider,
            hash: entry.blake3,
            constrained: decision.constrained,
            locked_now: false,
        })
    }

    #[allow(clippy::type_complexity)]
    fn resolve_fresh(
        &self,
        name: &str,
        chain: &ScopeChain,
        decision: &Decision,
        lock_and_notice: Option<(&mut Lockfile, &mut dyn FnMut(&LockNotice))>,
    ) -> ReefResult<Resolution> {
        let ctx = ProviderCtx::new(chain.cwd.clone());
        let chosen = self
            .best_candidate(name, &decision.constraint, decision.provider_pin.as_deref(), &ctx)
            .ok_or_else(|| {
                ReefError::not_found(format!(
                    "no candidate for {name} satisfying {}",
                    decision.constraint
                ))
                .with_hint(format!("try `reef fetch {name}`"))
            })?;

        let hash = self.hashes.hash_file(&chosen.path).map_err(|e| {
            ReefError::provider(format!("hashing {}: {e}", chosen.path.display()))
        })?;

        // Report scope: manifest scope when constrained, else system/ambient.
        let scope_label = if decision.constrained {
            decision.scope_label.clone()
        } else if chosen.ambient {
            "ambient".to_string()
        } else {
            "system".to_string()
        };

        let version_str = chosen.version.to_string();
        let mut locked_now = false;
        if decision.constrained
            && let Some((lock, notice)) = lock_and_notice
        {
            let entry = LockEntry {
                name: name.to_string(),
                version: version_str.clone(),
                provider: chosen.provider.to_string(),
                path: chosen.path.clone(),
                blake3: hash.clone(),
                resolved_at: now_rfc3339(),
            };
            lock.insert(entry.clone());
            notice(&LockNotice {
                name: entry.name,
                version: entry.version,
                provider: entry.provider,
                path: entry.path,
            });
            locked_now = true;
        }

        let chain_decisions = self.chain_decisions(chain, name, &scope_label, decision.constrained);
        let report = ResolutionReport {
            name: name.to_string(),
            scope: scope_label,
            constraint: decision.constraint.to_string(),
            version: version_str,
            path: chosen.path.clone(),
            hash: hash.clone(),
            provider: chosen.provider.to_string(),
            chain: chain_decisions,
        };
        Ok(Resolution {
            report,
            path: chosen.path,
            version: chosen.version,
            provider: chosen.provider.to_string(),
            hash,
            constrained: decision.constrained,
            locked_now,
        })
    }

    /// Enumerate, version-fill (lazily), filter, and rank candidates. Returns the
    /// highest satisfying version; ties broken by provider precedence then path.
    fn best_candidate(
        &self,
        name: &str,
        constraint: &Constraint,
        provider_pin: Option<&str>,
        ctx: &ProviderCtx,
    ) -> Option<Candidate> {
        let mut ranked: Vec<(Version, usize, PathBuf, Candidate)> = Vec::new();
        for (idx, provider) in self.providers.iter().enumerate() {
            if let Some(pin) = provider_pin
                && provider.name() != pin
            {
                continue;
            }
            for mut cand in provider.discover(name, ctx) {
                // Probe a version only when the constraint actually needs one.
                if constraint.needs_version() && cand.version.is_unknown() {
                    cand.version = provider.version_of(&cand);
                }
                if constraint.satisfies(&cand.version) {
                    ranked.push((cand.version.clone(), idx, cand.path.clone(), cand));
                }
            }
        }
        ranked
            .into_iter()
            .max_by(|a, b| {
                // Higher version wins; then lower provider index; then lower path.
                a.0.cmp(&b.0)
                    .then(b.1.cmp(&a.1))
                    .then(b.2.cmp(&a.2))
            })
            .map(|(_, _, _, c)| c)
    }

    fn chain_decisions(
        &self,
        chain: &ScopeChain,
        name: &str,
        selected_scope: &str,
        constrained: bool,
    ) -> Vec<ScopeDecision> {
        let mut out = Vec::new();
        let mut selected_marked = false;
        for scope in &chain.scopes {
            match scope.manifest.tools.get(name) {
                Some(req) => {
                    let outcome = if !selected_marked {
                        selected_marked = true;
                        "selected"
                    } else {
                        "shadowed"
                    };
                    out.push(ScopeDecision::new(
                        scope.label(),
                        scope.source.clone(),
                        Some(req.constraint.to_string()),
                        outcome,
                    ));
                }
                None => out.push(ScopeDecision::new(
                    scope.label(),
                    scope.source.clone(),
                    None,
                    "absent",
                )),
            }
        }
        // Unconstrained resolutions land in a provider scope not present as a
        // manifest — record it explicitly so the chain shows the winner.
        if !constrained {
            out.push(ScopeDecision::new(
                selected_scope.to_string(),
                PathBuf::new(),
                None,
                "selected",
            ));
        }
        out
    }
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Candidate, ProviderError};
    use std::path::Path;

    // A deterministic fake provider backed by an in-memory candidate table.
    struct FakeProvider {
        name: &'static str,
        cands: Vec<Candidate>,
    }
    impl Provider for FakeProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        fn discover(&self, tool: &str, _ctx: &ProviderCtx) -> Vec<Candidate> {
            self.cands.iter().filter(|c| c.tool == tool).cloned().collect()
        }
        fn fetch(
            &self,
            _t: &str,
            _r: &Constraint,
            _c: &ProviderCtx,
        ) -> Option<Result<Candidate, ProviderError>> {
            None
        }
    }

    fn write_exe(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body.as_bytes()).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    fn chain_with(text: &str, cwd: &Path) -> ScopeChain {
        std::fs::write(cwd.join(".reef.toml"), text).unwrap();
        ScopeChain::discover(cwd, None)
    }

    fn resolver_with(cands: Vec<(&'static str, Vec<Candidate>)>) -> Resolver {
        let providers: Vec<Box<dyn Provider>> = cands
            .into_iter()
            .map(|(name, c)| Box::new(FakeProvider { name, cands: c }) as Box<dyn Provider>)
            .collect();
        Resolver::new(providers)
    }

    #[test]
    fn interactive_auto_locks_on_miss() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "node", "node-22");
        let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.3.0"), bin.clone(), "mise")],
        )]);
        let mut lock = Lockfile::new();
        let mut notices = Vec::new();
        let res = r
            .resolve("node", &chain, &mut lock, Policy::Interactive, &mut |n| {
                notices.push(n.name.clone())
            })
            .unwrap();
        assert!(res.locked_now);
        assert_eq!(res.version.raw(), "22.3.0");
        assert_eq!(notices, vec!["node".to_string()]);
        assert!(lock.get("node").is_some());
    }

    #[test]
    fn script_errors_on_unlocked_constraint() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "node", "node-22");
        let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.3.0"), bin, "mise")],
        )]);
        let mut lock = Lockfile::new();
        let err = r
            .resolve("node", &chain, &mut lock, Policy::Script, &mut |_| {})
            .unwrap_err();
        assert_eq!(err.code_str(), "reef_unlocked");
    }

    #[test]
    fn script_resolves_locked_tool() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "node", "node-22");
        let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.3.0"), bin.clone(), "mise")],
        )]);
        // Pre-lock interactively, then resolve under script policy.
        let mut lock = Lockfile::new();
        r.resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap();
        let res = r
            .resolve("node", &chain, &mut lock, Policy::Script, &mut |_| {})
            .unwrap();
        assert!(!res.locked_now);
        assert_eq!(res.path, bin);
    }

    #[test]
    fn drift_detected_when_binary_changes() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "node", "node-22-orig");
        let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.3.0"), bin.clone(), "mise")],
        )]);
        let mut lock = Lockfile::new();
        r.resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap();
        // Flip a byte in the "binary".
        write_exe(dir.path(), "node", "node-22-TAMPERED");
        let err = r
            .resolve("node", &chain, &mut lock, Policy::Script, &mut |_| {})
            .unwrap_err();
        assert_eq!(err.code_str(), "reef_drift");
        // The error names *both* the stale locked hash and the current on-disk
        // hash, per REEF.md §2 ("a hard error naming old/new hashes").
        let msg = err.to_string();
        let old_hash = short(&crate::hashcache::hash_bytes(b"node-22-orig"));
        let new_hash = short(&crate::hashcache::hash_bytes(b"node-22-TAMPERED"));
        assert_ne!(old_hash, new_hash);
        assert!(msg.contains(&old_hash), "message {msg:?} missing old hash {old_hash}");
        assert!(msg.contains(&new_hash), "message {msg:?} missing new hash {new_hash}");
        assert!(err.hint.as_deref().unwrap_or("").contains("reef lock --refresh"));
    }

    #[test]
    fn refresh_lock_heals_drift() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "node", "orig");
        let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.3.0"), bin.clone(), "mise")],
        )]);
        let mut lock = Lockfile::new();
        r.resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap();
        write_exe(dir.path(), "node", "changed");
        // resolve now drifts…
        assert_eq!(
            r.resolve("node", &chain, &mut lock, Policy::Script, &mut |_| {})
                .unwrap_err()
                .code_str(),
            "reef_drift"
        );
        // …refresh heals it.
        r.refresh_lock("node", &chain, &mut lock, &mut |_| {}).unwrap();
        assert!(r.resolve("node", &chain, &mut lock, Policy::Script, &mut |_| {}).is_ok());
    }

    #[test]
    fn conflict_across_scopes() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        std::fs::write(base.join(".reef.toml"), "[tools]\nnode = \"18\"\n").unwrap();
        let sub = base.join("proj");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".reef.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        let chain = ScopeChain::discover(&sub, None);
        let r = resolver_with(vec![("mise", vec![])]);
        let mut lock = Lockfile::new();
        let err = r
            .resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap_err();
        assert_eq!(err.code_str(), "reef_conflict");
        // REEF.md §2: "the error lists both sources" — no silent first-wins.
        let msg = err.to_string();
        assert!(msg.contains("18"), "message should cite the 18 constraint");
        assert!(msg.contains("22"), "message should cite the 22 constraint");
        let hint = err.hint.clone().unwrap_or_default();
        assert!(hint.contains(base.join(".reef.toml").to_string_lossy().as_ref()));
        assert!(hint.contains(sub.join(".reef.toml").to_string_lossy().as_ref()));
    }

    #[test]
    fn conflicting_provider_pins_across_scopes() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        std::fs::write(base.join(".reef.toml"), "[tools]\ngo = { provider = \"mise\" }\n").unwrap();
        let sub = base.join("proj");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".reef.toml"), "[tools]\ngo = { provider = \"system\" }\n").unwrap();
        let chain = ScopeChain::discover(&sub, None);
        let r = resolver_with(vec![("mise", vec![]), ("system", vec![])]);
        let mut lock = Lockfile::new();
        let err = r
            .resolve("go", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap_err();
        assert_eq!(err.code_str(), "reef_conflict");
        let msg = err.to_string();
        assert!(msg.contains("mise"));
        assert!(msg.contains("system"));
    }

    #[test]
    fn compatible_scopes_refine_not_conflict() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        let bin = write_exe(base, "node", "n");
        std::fs::write(base.join(".reef.toml"), "[tools]\nnode = \"22\"\n").unwrap();
        let sub = base.join("proj");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".reef.toml"), "[tools]\nnode = \"22.3\"\n").unwrap();
        let chain = ScopeChain::discover(&sub, None);
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.3.9"), bin, "mise")],
        )]);
        let mut lock = Lockfile::new();
        let res = r
            .resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap();
        // Effective constraint is the more specific 22.3.
        assert_eq!(res.report.constraint, "22.3");
    }

    #[test]
    fn ranking_prefers_highest_then_provider_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_exe(dir.path(), "toolA", "a");
        let b = write_exe(dir.path(), "toolB", "b");
        let chain = ScopeChain::discover(dir.path(), None);
        // Same version from two providers → provider order (index 0) wins.
        let r = resolver_with(vec![
            ("mise", vec![Candidate::new("t", Version::parse("1.0.0"), a.clone(), "mise")]),
            ("system", vec![Candidate::new("t", Version::parse("1.0.0"), b, "system")]),
        ]);
        let mut lock = Lockfile::new();
        let res = r.resolve("t", &chain, &mut lock, Policy::Interactive, &mut |_| {}).unwrap();
        assert_eq!(res.provider, "mise");
        assert_eq!(res.path, a);
    }

    #[test]
    fn highest_version_wins() {
        let dir = tempfile::tempdir().unwrap();
        let lo = write_exe(dir.path(), "lo", "lo");
        let hi = write_exe(dir.path(), "hi", "hi");
        let chain = ScopeChain::discover(dir.path(), None);
        let r = resolver_with(vec![(
            "mise",
            vec![
                Candidate::new("t", Version::parse("1.2.0"), lo, "mise"),
                Candidate::new("t", Version::parse("1.10.0"), hi.clone(), "mise"),
            ],
        )]);
        let mut lock = Lockfile::new();
        let res = r.resolve("t", &chain, &mut lock, Policy::Interactive, &mut |_| {}).unwrap();
        assert_eq!(res.path, hi);
        assert_eq!(res.version.raw(), "1.10.0");
    }

    #[test]
    fn unconstrained_resolves_without_lock() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "ls", "ls");
        let chain = ScopeChain::discover(dir.path(), None);
        let r = resolver_with(vec![(
            "system",
            vec![Candidate::new("ls", Version::unknown(), bin, "system")],
        )]);
        let mut lock = Lockfile::new();
        // Even under Script policy, unconstrained tools resolve fine.
        let res = r.resolve("ls", &chain, &mut lock, Policy::Script, &mut |_| {}).unwrap();
        assert!(!res.constrained);
        assert!(!res.locked_now);
        assert!(lock.get("ls").is_none());
        assert_eq!(res.report.scope, "system");
    }

    #[test]
    fn not_found_when_no_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let chain = chain_with("[tools]\nghost = \"9\"\n", dir.path());
        let r = resolver_with(vec![("mise", vec![])]);
        let mut lock = Lockfile::new();
        let err = r
            .resolve("ghost", &chain, &mut lock, Policy::Interactive, &mut |_| {})
            .unwrap_err();
        assert_eq!(err.code_str(), "reef_not_found");
    }

    #[test]
    fn provider_pin_filters_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let m = write_exe(dir.path(), "m", "m");
        let s = write_exe(dir.path(), "s", "s");
        let chain = chain_with("[tools]\ngo = { provider = \"mise\" }\n", dir.path());
        let r = resolver_with(vec![
            ("system", vec![Candidate::new("go", Version::unknown(), s, "system")]),
            ("mise", vec![Candidate::new("go", Version::parse("1.21"), m.clone(), "mise")]),
        ]);
        let mut lock = Lockfile::new();
        let res = r.resolve("go", &chain, &mut lock, Policy::Interactive, &mut |_| {}).unwrap();
        assert_eq!(res.provider, "mise");
        assert_eq!(res.path, m);
    }

    #[test]
    fn report_chain_marks_selected_and_absent() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_exe(dir.path(), "node", "n");
        let chain = chain_with("[tools]\nnode = \"22\"\nother = \"1\"\n", dir.path());
        let r = resolver_with(vec![(
            "mise",
            vec![Candidate::new("node", Version::parse("22.0.0"), bin, "mise")],
        )]);
        let mut lock = Lockfile::new();
        let res = r.resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {}).unwrap();
        let sel = res.report.chain.iter().find(|d| d.outcome == "selected").unwrap();
        assert_eq!(sel.constraint.as_deref(), Some("22"));
    }
}
