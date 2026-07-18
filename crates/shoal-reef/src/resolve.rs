//! The resolver — name → version → hash → binding (site/content/internals/reef-resolution.md), with the lock,
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

use crate::provider::{
    CargoProvider, MiseProvider, NpmLocalProvider, SystemProvider, VenvProvider,
};

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

/// Authority check and subprocess context used for one fresh probe pass.
pub struct ProbeExecution<'a> {
    pub guard: &'a mut dyn FnMut(&Candidate) -> ReefResult<()>,
    pub context: &'a ProviderCtx,
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
        Resolver {
            providers,
            hashes: HashCache::new(),
        }
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
        self.resolve_with_probe_guard(name, chain, lock, policy, notice, &mut |_| Ok(()))
    }

    /// Resolve with an authority check immediately before any candidate whose
    /// version is unknown would be executed as `<candidate> --version`.
    /// Returning an error prevents that probe and aborts resolution.
    pub fn resolve_with_probe_guard(
        &self,
        name: &str,
        chain: &ScopeChain,
        lock: &mut Lockfile,
        policy: Policy,
        notice: &mut dyn FnMut(&LockNotice),
        probe_guard: &mut dyn FnMut(&Candidate) -> ReefResult<()>,
    ) -> ReefResult<Resolution> {
        let context = ProviderCtx::new(chain.cwd.clone());
        self.resolve_with_probe_context(
            name,
            chain,
            lock,
            policy,
            notice,
            ProbeExecution {
                guard: probe_guard,
                context: &context,
            },
        )
    }

    /// Resolve through an explicitly supplied provider subprocess context.
    /// Embedders use this to carry their environment, sandbox, and
    /// cancellation authority into lazy version probes.
    pub fn resolve_with_probe_context(
        &self,
        name: &str,
        chain: &ScopeChain,
        lock: &mut Lockfile,
        policy: Policy,
        notice: &mut dyn FnMut(&LockNotice),
        probe: ProbeExecution<'_>,
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

        self.resolve_fresh(
            name,
            chain,
            &decision,
            Some((lock, notice)),
            probe.guard,
            probe.context,
        )
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
        self.refresh_lock_with_probe_guard(name, chain, lock, notice, &mut |_| Ok(()))
    }

    /// Force a fresh resolution with the same pre-probe authority callback as
    /// [`Self::resolve_with_probe_guard`].
    pub fn refresh_lock_with_probe_guard(
        &self,
        name: &str,
        chain: &ScopeChain,
        lock: &mut Lockfile,
        notice: &mut dyn FnMut(&LockNotice),
        probe_guard: &mut dyn FnMut(&Candidate) -> ReefResult<()>,
    ) -> ReefResult<Resolution> {
        let context = ProviderCtx::new(chain.cwd.clone());
        self.refresh_lock_with_probe_context(name, chain, lock, notice, probe_guard, &context)
    }

    /// Refresh through an explicitly supplied provider subprocess context.
    pub fn refresh_lock_with_probe_context(
        &self,
        name: &str,
        chain: &ScopeChain,
        lock: &mut Lockfile,
        notice: &mut dyn FnMut(&LockNotice),
        probe_guard: &mut dyn FnMut(&Candidate) -> ReefResult<()>,
        context: &ProviderCtx,
    ) -> ReefResult<Resolution> {
        let decision = self.effective_decision(chain, name)?;
        lock.remove(name);
        self.resolve_fresh(
            name,
            chain,
            &decision,
            Some((lock, notice)),
            probe_guard,
            context,
        )
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
        decision
            .constraint
            .satisfies(&Version::parse(&entry.version))
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
            ReefError::not_found(format!(
                "locked binary {} unreadable: {e}",
                entry.path.display()
            ))
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
        probe_guard: &mut dyn FnMut(&Candidate) -> ReefResult<()>,
        context: &ProviderCtx,
    ) -> ReefResult<Resolution> {
        let chosen = self
            .best_candidate(
                name,
                &decision.constraint,
                decision.provider_pin.as_deref(),
                context,
                probe_guard,
            )?
            .ok_or_else(|| {
                ReefError::not_found(format!(
                    "no candidate for {name} satisfying {}",
                    decision.constraint
                ))
                .with_hint(format!("try `reef fetch {name}`"))
            })?;

        let hash = self
            .hashes
            .hash_file(&chosen.path)
            .map_err(|e| ReefError::provider(format!("hashing {}: {e}", chosen.path.display())))?;

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
            lock.try_insert(entry.clone()).map_err(|error| {
                ReefError::provider(format!("retaining lock entry for {name}: {error}"))
            })?;
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
        probe_guard: &mut dyn FnMut(&Candidate) -> ReefResult<()>,
    ) -> ReefResult<Option<Candidate>> {
        let mut best: Option<(usize, Candidate)> = None;
        for (idx, provider) in self.providers.iter().enumerate() {
            if let Some(pin) = provider_pin
                && provider.name() != pin
            {
                continue;
            }
            let discovery = provider
                .discover(name, ctx)
                .map_err(|error| ReefError::provider(error.to_string()))?;
            for mut cand in discovery.into_candidates() {
                // Probe a version only when the constraint actually needs one.
                if constraint.needs_version() && cand.version.is_unknown() {
                    probe_guard(&cand)?;
                    cand.version = provider.version_of(&cand, ctx);
                }
                if constraint.satisfies(&cand.version) {
                    let replace = best.as_ref().is_none_or(|(best_idx, current)| {
                        cand.version > current.version
                            || (cand.version == current.version
                                && (idx < *best_idx
                                    || (idx == *best_idx && cand.path < current.path)))
                    });
                    if replace {
                        best = Some((idx, cand));
                    }
                }
            }
        }
        Ok(best.map(|(_, candidate)| candidate))
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
mod tests;
