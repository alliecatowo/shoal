use super::*;
use crate::provider::{Candidate, ProviderError};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        self.cands
            .iter()
            .filter(|c| c.tool == tool)
            .cloned()
            .collect()
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
fn denied_probe_never_executes_candidate_version_hook() {
    struct ProbeProvider {
        path: PathBuf,
        called: Arc<AtomicBool>,
    }
    impl Provider for ProbeProvider {
        fn name(&self) -> &'static str {
            "probe"
        }

        fn discover(&self, tool: &str, _ctx: &ProviderCtx) -> Vec<Candidate> {
            vec![Candidate::new(
                tool,
                Version::unknown(),
                self.path.clone(),
                "probe",
            )]
        }

        fn version_of(&self, _candidate: &Candidate, _ctx: &ProviderCtx) -> Version {
            self.called.store(true, Ordering::SeqCst);
            Version::parse("1.2.3")
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let binary = write_exe(directory.path(), "guarded", "bytes");
    let chain = chain_with("[tools]\nguarded = \"1.2.3\"\n", directory.path());
    let called = Arc::new(AtomicBool::new(false));
    let resolver = Resolver::new(vec![Box::new(ProbeProvider {
        path: binary,
        called: called.clone(),
    })]);
    let mut lock = Lockfile::new();
    let error = resolver
        .resolve_with_probe_guard(
            "guarded",
            &chain,
            &mut lock,
            Policy::Interactive,
            &mut |_| {},
            &mut |_| Err(ReefError::provider("probe denied by test policy")),
        )
        .unwrap_err();
    assert!(error.msg.contains("probe denied"));
    assert!(!called.load(Ordering::SeqCst));
    assert!(lock.get("guarded").is_none());
}

#[test]
fn interactive_auto_locks_on_miss() {
    let dir = tempfile::tempdir().unwrap();
    let bin = write_exe(dir.path(), "node", "node-22");
    let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
    let r = resolver_with(vec![(
        "mise",
        vec![Candidate::new(
            "node",
            Version::parse("22.3.0"),
            bin.clone(),
            "mise",
        )],
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
        vec![Candidate::new(
            "node",
            Version::parse("22.3.0"),
            bin,
            "mise",
        )],
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
        vec![Candidate::new(
            "node",
            Version::parse("22.3.0"),
            bin.clone(),
            "mise",
        )],
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
        vec![Candidate::new(
            "node",
            Version::parse("22.3.0"),
            bin.clone(),
            "mise",
        )],
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
    // hash, per site/content/internals/reef-resolution.md ("a hard error naming old/new hashes").
    let msg = err.to_string();
    let old_hash = short(&crate::hashcache::hash_bytes(b"node-22-orig"));
    let new_hash = short(&crate::hashcache::hash_bytes(b"node-22-TAMPERED"));
    assert_ne!(old_hash, new_hash);
    assert!(
        msg.contains(&old_hash),
        "message {msg:?} missing old hash {old_hash}"
    );
    assert!(
        msg.contains(&new_hash),
        "message {msg:?} missing new hash {new_hash}"
    );
    assert!(
        err.hint
            .as_deref()
            .unwrap_or("")
            .contains("reef lock --refresh")
    );
}

#[test]
fn refresh_lock_heals_drift() {
    let dir = tempfile::tempdir().unwrap();
    let bin = write_exe(dir.path(), "node", "orig");
    let chain = chain_with("[tools]\nnode = \"22\"\n", dir.path());
    let r = resolver_with(vec![(
        "mise",
        vec![Candidate::new(
            "node",
            Version::parse("22.3.0"),
            bin.clone(),
            "mise",
        )],
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
    r.refresh_lock("node", &chain, &mut lock, &mut |_| {})
        .unwrap();
    assert!(
        r.resolve("node", &chain, &mut lock, Policy::Script, &mut |_| {})
            .is_ok()
    );
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
    // site/content/internals/reef-resolution.md: "the error lists both sources" — no silent first-wins.
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
    std::fs::write(
        base.join(".reef.toml"),
        "[tools]\ngo = { provider = \"mise\" }\n",
    )
    .unwrap();
    let sub = base.join("proj");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(
        sub.join(".reef.toml"),
        "[tools]\ngo = { provider = \"system\" }\n",
    )
    .unwrap();
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
        vec![Candidate::new(
            "node",
            Version::parse("22.3.9"),
            bin,
            "mise",
        )],
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
        (
            "mise",
            vec![Candidate::new(
                "t",
                Version::parse("1.0.0"),
                a.clone(),
                "mise",
            )],
        ),
        (
            "system",
            vec![Candidate::new("t", Version::parse("1.0.0"), b, "system")],
        ),
    ]);
    let mut lock = Lockfile::new();
    let res = r
        .resolve("t", &chain, &mut lock, Policy::Interactive, &mut |_| {})
        .unwrap();
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
    let res = r
        .resolve("t", &chain, &mut lock, Policy::Interactive, &mut |_| {})
        .unwrap();
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
    let res = r
        .resolve("ls", &chain, &mut lock, Policy::Script, &mut |_| {})
        .unwrap();
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
        (
            "system",
            vec![Candidate::new("go", Version::unknown(), s, "system")],
        ),
        (
            "mise",
            vec![Candidate::new(
                "go",
                Version::parse("1.21"),
                m.clone(),
                "mise",
            )],
        ),
    ]);
    let mut lock = Lockfile::new();
    let res = r
        .resolve("go", &chain, &mut lock, Policy::Interactive, &mut |_| {})
        .unwrap();
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
        vec![Candidate::new(
            "node",
            Version::parse("22.0.0"),
            bin,
            "mise",
        )],
    )]);
    let mut lock = Lockfile::new();
    let res = r
        .resolve("node", &chain, &mut lock, Policy::Interactive, &mut |_| {})
        .unwrap();
    let sel = res
        .report
        .chain
        .iter()
        .find(|d| d.outcome == "selected")
        .unwrap();
    assert_eq!(sel.constraint.as_deref(), Some("22"));
}
