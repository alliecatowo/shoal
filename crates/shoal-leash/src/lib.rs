//! `leash`: shoal's semantic effect and policy engine.
//!
//! This crate evaluates concrete effects. It does not claim to enforce them at
//! the OS boundary; [`EnforcementStatus::detect`] reports that distinction
//! explicitly until a platform backend is installed by the host.

mod effects;
mod enforce;
mod policy;
mod seatbelt;

pub use effects::{Effect, Estimates, Plan, Reversibility};
pub use enforce::{
    EnforcementStatus, EnforcementTier, FsSandbox, NetPolicy, SandboxPolicy, SpawnPreflight,
    apply_landlock, apply_macos_sandbox, apply_sandbox, landlock_abi, preflight_spawn,
};
pub use policy::{
    AutoApply, OpaqueMode, POLICY_MAX_ASSIGNMENTS, POLICY_MAX_BYTES, POLICY_MAX_GRANT_BYTES,
    POLICY_MAX_GRANTS_PER_KIND, POLICY_MAX_NESTING, POLICY_MAX_PRINCIPALS, Policy, PolicyLoadError,
    PolicyParseError, PrincipalPolicy, Verdict,
};
pub use seatbelt::seatbelt_profile;

#[cfg(test)]
mod tests {
    use super::*;
    use policy::grant_root;
    use std::path::PathBuf;
    fn policy() -> Policy {
        Policy::from_toml(
            r#"
[principal.agent]
fs.read = ["/work/**", "/usr/**"]
fs.write = ["/work/generated/**"]
fs.delete = ["/work/generated/tmp/**"]
net_connect = ["github.com:443", "*.docs.rs:443"]
net_listen = [8080]
proc_spawn = ["cargo", "deadbeef"]
env_read = ["PATH", "HOME"]
secret_use = []
session_write = true
journal_read = true
time = true
auto_apply = "reversible"
opaque = "ask"
"#,
        )
        .unwrap()
    }

    #[test]
    fn effect_codec_roundtrip() {
        let e = Effect::ProcSpawn {
            bin_hash: "abc".into(),
            argv0: "cargo".into(),
        };
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Effect>(&j).unwrap(), e);
    }
    #[test]
    fn path_scope_does_not_allow_siblings_or_dotdot() {
        let p = policy();
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::FsRead {
                    paths: vec!["/work/src/a.rs".into()]
                }
            ),
            Verdict::Allow
        );
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::FsWrite {
                    paths: vec!["/work/generated/../src/a".into()]
                }
            ),
            Verdict::Deny
        );
    }
    #[test]
    fn network_wildcards_and_ports() {
        let p = policy();
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::NetConnect {
                    host: "foo.docs.rs".into(),
                    port: 443
                }
            ),
            Verdict::Allow
        );
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::NetConnect {
                    host: "foo.docs.rs".into(),
                    port: 80
                }
            ),
            Verdict::Deny
        );
    }
    #[test]
    fn spawn_matches_hash_or_name() {
        let p = policy();
        for e in [
            Effect::ProcSpawn {
                bin_hash: "x".into(),
                argv0: "/usr/bin/cargo".into(),
            },
            Effect::ProcSpawn {
                bin_hash: "deadbeef".into(),
                argv0: "renamed".into(),
            },
        ] {
            assert_eq!(p.evaluate_effect("agent", &e), Verdict::Allow)
        }
    }
    #[test]
    fn spawn_pinning_active_only_when_proc_spawn_is_set() {
        // The load-bearing no-regression guard: a principal is "pinning" iff it
        // declares a non-empty `proc_spawn` allowlist. The default-permissive
        // policy sets none, so it never pins — ordinary spawns stay allowed.
        let p = policy();
        assert!(p.spawn_pinning_active("agent")); // has proc_spawn = ["cargo", "deadbeef"]
        assert!(!p.spawn_pinning_active("missing")); // unknown principal
        let permissive = Policy::permissive("uid:1000");
        assert!(!permissive.spawn_pinning_active("uid:1000"));
        let no_spawn = Policy::from_toml("[principal.agent]\nopaque='allow'\n").unwrap();
        assert!(!no_spawn.spawn_pinning_active("agent"));
    }

    #[test]
    fn proc_spawn_allowlist_admits_matching_hash_and_denies_others() {
        // A principal that pins a concrete content hash allows exactly that hash
        // (regardless of the argv0 name) and denies an unlisted one. This is the
        // enforcement the spawn gate relies on once pinning is active.
        let p =
            Policy::from_toml("[principal.agent]\nproc_spawn = [\"aa11bb22ccddeeff\"]\n").unwrap();
        assert!(p.spawn_pinning_active("agent"));
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::ProcSpawn {
                    bin_hash: "aa11bb22ccddeeff".into(),
                    argv0: "/opt/vendor/tool".into(), // name is unlisted; hash carries it
                }
            ),
            Verdict::Allow
        );
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::ProcSpawn {
                    bin_hash: "0000000000000000".into(), // a different binary's hash
                    argv0: "/usr/bin/tool".into(),
                }
            ),
            Verdict::Deny
        );
    }

    #[test]
    fn empty_proc_spawn_would_default_deny_hence_the_guard() {
        // Documents *why* `spawn_pinning_active` exists: with an empty allowlist
        // `evaluate_effect` denies every spawn (nothing matches). Callers must
        // gate on `spawn_pinning_active` and skip the evaluator entirely, or an
        // otherwise-unrestricted principal would be unable to run any command.
        let p = Policy::from_toml("[principal.agent]\nopaque='allow'\n").unwrap();
        assert!(!p.spawn_pinning_active("agent"));
        assert_eq!(
            p.evaluate_effect(
                "agent",
                &Effect::ProcSpawn {
                    bin_hash: "anyhash".into(),
                    argv0: "/bin/ls".into(),
                }
            ),
            Verdict::Deny,
            "an empty allowlist denies — which is exactly why the gate must skip it"
        );
    }

    #[test]
    fn plan_evaluation_uses_the_same_optional_spawn_pinning_contract() {
        let spawn = Plan::new(
            vec![Effect::ProcSpawn {
                bin_hash: "anyhash".into(),
                argv0: "/usr/bin/printf".into(),
            }],
            Reversibility::Irreversible,
            Estimates::default(),
        );
        let permissive = Policy::permissive("uid:1000");
        assert_eq!(
            permissive.evaluate_plan("uid:1000", &spawn),
            Verdict::Allow,
            "the kernel plan gate must not contradict the concrete spawn gate"
        );

        let pinned =
            Policy::from_toml("[principal.agent]\nproc_spawn=['cargo']\nauto_apply='in-grant'\n")
                .unwrap();
        assert_eq!(pinned.evaluate_plan("agent", &spawn), Verdict::Deny);
    }

    #[test]
    fn opaque_and_unknown_principal() {
        let p = policy();
        assert_eq!(
            p.evaluate_effect("agent", &Effect::Opaque),
            Verdict::ApprovalRequired
        );
        assert_eq!(p.evaluate_effect("missing", &Effect::Time), Verdict::Deny);
    }
    #[test]
    fn plan_ref_is_stable_and_auto_apply_respects_reversibility() {
        let p = policy();
        let a = Plan::new(
            vec![Effect::FsWrite {
                paths: vec!["/work/generated/a".into()],
            }],
            Reversibility::Reversible,
            Estimates {
                bytes: Some(4),
                items: Some(1),
            },
        );
        let b = Plan::new(a.effects.clone(), a.reversibility, a.estimates.clone());
        assert_eq!(a.plan_ref, b.plan_ref);
        assert_eq!(p.evaluate_plan("agent", &a), Verdict::Allow);
        let irreversible = Plan::new(
            a.effects.clone(),
            Reversibility::Irreversible,
            Estimates::default(),
        );
        assert_eq!(
            p.evaluate_plan("agent", &irreversible),
            Verdict::ApprovalRequired
        );
    }
    #[test]
    fn denial_dominates_plan() {
        let p = policy();
        let plan = Plan::new(
            vec![
                Effect::Opaque,
                Effect::FsDelete {
                    paths: vec!["/etc/passwd".into()],
                },
            ],
            Reversibility::Unknown,
            Estimates::default(),
        );
        assert_eq!(p.evaluate_plan("agent", &plan), Verdict::Deny);
    }
    #[test]
    fn enforcement_is_honest() {
        let s = EnforcementStatus::detect();
        assert!(!s.enforced);
        assert_eq!(s.active_tier, None);
        // `detect()`'s wording of *why* nothing is enforced is intentionally
        // platform-specific (this crate installs no backend itself, so the
        // message just describes what host mechanism is missing): Linux
        // phrases it as a landlock/seccomp/network mechanism being
        // "unavailable"; macOS phrases it as the Seatbelt backend being "not
        // installed"; anything else falls back to "advisory". All three
        // honestly report the same underlying fact (no OS-level enforcement
        // is active) in the phrasing appropriate to that platform's branch
        // in `detect()`.
        if cfg!(target_os = "linux") {
            assert!(s.detail.contains("unavailable"), "{}", s.detail);
        } else if cfg!(target_os = "macos") {
            assert!(s.detail.contains("not installed"), "{}", s.detail);
        } else {
            assert!(s.detail.contains("advisory"), "{}", s.detail);
        }
    }
    #[test]
    fn spawn_preflight_hashes_content_and_labels_toctou() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tool");
        std::fs::write(&p, b"binary").unwrap();
        let first = preflight_spawn(&p, &[]).unwrap();
        assert!(!first.allowed);
        assert!(first.assurance.contains("TOCTOU"));
        assert!(
            preflight_spawn(&p, std::slice::from_ref(&first.hash))
                .unwrap()
                .allowed
        );
    }
    #[test]
    fn seatbelt_profile_is_canonical_sorted_and_escaped() {
        let d = tempfile::tempdir().unwrap();
        let weird = d.path().join("quote\"and\\slash");
        std::fs::create_dir(&weird).unwrap();
        let profile = seatbelt_profile(&FsSandbox {
            read: vec![weird.clone(), weird.clone()],
            write: vec![],
            delete: vec![],
        })
        .unwrap();
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert_eq!(profile.matches("file-read* (subpath").count(), 1);
        assert!(profile.contains("quote\\\"and\\\\slash"));
    }
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn seatbelt_application_is_explicitly_unavailable() {
        assert!(
            apply_macos_sandbox(&FsSandbox::default())
                .unwrap_err()
                .contains("only available on macOS")
        );
    }
    #[test]
    fn sandbox_policy_defaults_are_unrestricted_and_advisory() {
        let p = SandboxPolicy::default();
        assert_eq!(p.net, NetPolicy::Unrestricted);
        assert!(p.spawn_hash.is_none());
        assert!(!p.hermetic);
        assert!(p.fs.read.is_empty());
    }

    #[test]
    fn grant_root_reduces_globs_to_concrete_prefix() {
        assert_eq!(grant_root("/work/**"), Some(PathBuf::from("/work")));
        assert_eq!(grant_root("/**"), Some(PathBuf::from("/")));
        assert_eq!(grant_root("/etc/hosts"), Some(PathBuf::from("/etc/hosts")));
        assert_eq!(grant_root("/a/b*/c"), Some(PathBuf::from("/a")));
        assert_eq!(grant_root("**/foo"), None);
    }

    #[test]
    fn permissive_policy_is_fs_unrestricted_and_yields_no_sandbox() {
        // The default-permissive policy must never wrap a spawn — that is the
        // zero-regression guarantee. `sandbox_for` returns None so the child
        // runs exactly as it does today.
        let p = Policy::permissive("uid:1000");
        assert!(p.principal("uid:1000").unwrap().is_fs_unrestricted());
        assert!(p.sandbox_for("uid:1000").is_none());
        // Every effect a normal command needs is allowed.
        assert_eq!(
            p.evaluate_effect(
                "uid:1000",
                &Effect::FsRead {
                    paths: vec!["/anywhere/at/all".into()]
                }
            ),
            Verdict::Allow
        );
    }

    #[test]
    fn scoped_policy_yields_a_sandbox_with_existing_roots_only() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("work");
        std::fs::create_dir(&real).unwrap();
        let src = format!(
            "[principal.agent]\nhermetic=true\n\n[principal.agent.fs]\n\
             read=[\"{}/**\", \"/does/not/exist/**\"]\nwrite=[\"{}/**\"]\n",
            real.display(),
            real.display()
        );
        let policy = Policy::from_toml(&src).unwrap();
        let sandbox = policy.sandbox_for("agent").expect("scoped → Some sandbox");
        // The existing root survives; the non-existent one is dropped.
        assert_eq!(sandbox.fs.read, vec![real.clone()]);
        assert_eq!(sandbox.fs.write, vec![real.clone()]);
        assert!(sandbox.fs.delete.is_empty());
        // `hermetic` is carried through from the principal.
        assert!(sandbox.hermetic);
        assert!(!policy.principal("agent").unwrap().is_fs_unrestricted());
    }

    #[test]
    fn unscoped_or_unknown_principal_yields_no_sandbox() {
        // A principal that declares no fs scope has nothing concrete to
        // confine to (the plan layer denies its fs effects instead), and an
        // unknown principal likewise never wraps a spawn.
        let policy = Policy::from_toml("[principal.agent]\nopaque='deny'\n").unwrap();
        assert!(policy.sandbox_for("agent").is_none());
        assert!(policy.sandbox_for("nobody").is_none());
    }

    #[test]
    fn load_user_or_permissive_reads_file_then_falls_back() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("shoal");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("leash.toml"),
            "[principal.agent]\n\n[principal.agent.fs]\nread=[\"/srv/**\"]\n",
        )
        .unwrap();
        // Point XDG_CONFIG_HOME at the fixture for the duration of this test.
        // Serialized against other env-touching tests via a process-global lock.
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        unsafe { std::env::set_var("XDG_CONFIG_HOME", d.path()) };
        let loaded = Policy::load_user_or_permissive("uid:0");
        // The file defines `agent`, not `uid:0`; the loaded policy reflects the
        // file, so `uid:0` is unknown there (not permissive).
        assert!(loaded.principal("agent").is_some());
        assert!(loaded.principal("uid:0").is_none());
        std::fs::write(cfg.join("leash.toml"), "[principal.agent\n").unwrap();
        let quarantined = Policy::load_user_or_permissive("uid:0");
        assert!(quarantined.is_fail_closed());
        assert!(quarantined.spawn_pinning_active("uid:0"));
        assert_eq!(
            quarantined.evaluate_effect("uid:0", &Effect::Time),
            Verdict::Deny
        );
        // With no config file, we get the permissive fallback for the principal.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", d.path().join("empty")) };
        let fallback = Policy::load_user_or_permissive("uid:0");
        assert!(fallback.principal("uid:0").unwrap().is_fs_unrestricted());
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
