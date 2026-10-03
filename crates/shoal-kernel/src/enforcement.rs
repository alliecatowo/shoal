//! Principal-specific OS enforcement forecasts shared by kernel surfaces.
//!
//! These values describe policy intent and backend availability. Actual
//! activation is child-local and remains the executor's responsibility.

use super::*;

impl Kernel {
    /// The real filesystem enforcement truth for `principal`: true only when
    /// a genuine OS backend exists and the policy resolves a usable sandbox.
    /// Attach and capability responses share this exact calculation.
    pub(crate) fn caps_enforced_for(&self, principal: &str) -> bool {
        self.enforcement_preview(principal).filesystem_enforceable
    }

    /// Forecast each containment dimension without claiming it is active.
    pub(crate) fn enforcement_preview(&self, principal: &str) -> EnforcementPreview {
        let status = EnforcementStatus::detect();
        let available_tier = tier_letter(status.available_tier).to_string();
        let backend_present = matches!(
            status.available_tier,
            EnforcementTier::A | EnforcementTier::C
        );
        let filesystem_requested = self.policy.filesystem_scoping_active(principal);
        let filesystem_resolved = self.policy.sandbox_for(principal).is_some_and(|sandbox| {
            !sandbox.fs.read.is_empty()
                || !sandbox.fs.write.is_empty()
                || !sandbox.fs.delete.is_empty()
        });
        let filesystem_enforceable = backend_present && filesystem_resolved;
        let network_scope_requested = self.policy.network_scoping_active(principal);
        let spawn_pin_requested = self.policy.spawn_pinning_active(principal);
        let process_limits_requested = self.policy.process_limits_active(principal);
        // The kernel and executor are Unix-only today; the sibling child
        // launcher applies setrlimit immediately before exec across capture,
        // bounded-probe, and PTY surfaces.
        let process_limits_enforceable = process_limits_requested && cfg!(unix);
        let hermetic = self.policy.hermetic_active(principal);
        let mut limitations = Vec::new();
        if filesystem_requested && !filesystem_resolved {
            limitations.push("filesystem-scope-unresolved".into());
        } else if filesystem_requested && !backend_present {
            limitations.push("filesystem-backend-unavailable".into());
        }
        if network_scope_requested {
            limitations.push("network-policy-only".into());
        }
        if spawn_pin_requested {
            limitations.push("spawn-pin-preflight-toctou".into());
        }
        if process_limits_requested {
            limitations.push("process-limits-not-aggregate-tree-budget".into());
        }
        let unmet_hermetic = hermetic
            && ((filesystem_requested && !filesystem_enforceable)
                || network_scope_requested
                || spawn_pin_requested
                || (process_limits_requested && !process_limits_enforceable));
        let spawn_disposition = if unmet_hermetic {
            "refuse-unmet-hermetic"
        } else if filesystem_enforceable || process_limits_enforceable {
            "enforce-at-spawn"
        } else if filesystem_requested {
            "best-effort-unconfined"
        } else if network_scope_requested || spawn_pin_requested {
            "policy-preflight-only"
        } else {
            "no-os-scope-requested"
        };
        EnforcementPreview {
            available_tier,
            activation: "deferred-to-spawn".into(),
            filesystem_requested,
            filesystem_enforceable,
            network_scope_requested,
            network_enforceable: false,
            spawn_pin_requested,
            spawn_pin_atomic: false,
            process_limits_requested,
            process_limits_enforceable,
            hermetic,
            spawn_disposition: spawn_disposition.into(),
            limitations,
        }
    }
}
