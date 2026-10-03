//! Host-owned durable jump-history policy.
//!
//! Keeping authentication-derived persistence composition out of the main
//! Session router makes the boundary reviewable: only stable authenticated
//! scopes receive a store, and no caller-provided path reaches the evaluator.

use super::*;

impl Kernel {
    /// Select durable navigation history only from server-authenticated
    /// attachment facts. The path component is a domain-separated digest so
    /// principal/profile strings cannot become filenames or leak through a
    /// state-directory listing.
    pub(super) fn jump_history_policy(
        &self,
        principal: &str,
        profile: &str,
        trust: ConnectionTrust,
        local_human: bool,
        authenticated_bearer: bool,
    ) -> (String, Option<PathBuf>, &'static str) {
        if local_human && trust == ConnectionTrust::EmbeddedHuman {
            let root = self.state_dir.clone().unwrap_or_else(|| {
                shoal_paths::ShoalPaths::discover()
                    .state_dir()
                    .to_path_buf()
            });
            return (
                "embedded-human/local-human".into(),
                Some(root.join("jump.frecency")),
                "local-human",
            );
        }
        if !authenticated_bearer {
            return (
                format!("{}/restricted-agent", trust.as_str()),
                None,
                "disabled",
            );
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"shoal/kernel/frecency/v1\0");
        for field in [trust.as_str(), profile, principal] {
            hasher.update(&(field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        let scope = hasher.finalize().to_hex().to_string();
        let path = self
            .state_dir
            .as_ref()
            .map(|root| root.join("frecency").join(format!("{scope}.frecency")));
        (format!("bearer/{scope}"), path, "authenticated-scope")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(session: &Session, source: &str) -> shoal_value::VResult<Value> {
        let program = shoal_syntax::parse(source).unwrap();
        session.evaluator.lock().unwrap().eval_program(&program)
    }

    #[test]
    fn policy_partitions_authenticated_scopes_and_disables_restricted() {
        let state = tempfile::tempdir().unwrap();
        let kernel = Kernel::open(state.path()).unwrap();
        let (_, alpha, alpha_mode) = kernel.jump_history_policy(
            "agent:alpha",
            "readonly",
            ConnectionTrust::Public,
            false,
            true,
        );
        let (_, beta, _) = kernel.jump_history_policy(
            "agent:beta",
            "readonly",
            ConnectionTrust::Public,
            false,
            true,
        );
        let (_, alpha_supervisor, _) = kernel.jump_history_policy(
            "agent:alpha",
            "supervisor",
            ConnectionTrust::Public,
            false,
            true,
        );
        let (_, restricted, restricted_mode) = kernel.jump_history_policy(
            "agent:mcp",
            "restricted-agent",
            ConnectionTrust::Public,
            false,
            false,
        );

        assert_eq!(alpha_mode, "authenticated-scope");
        assert!(alpha.is_some());
        assert_ne!(alpha, beta);
        assert_ne!(alpha, alpha_supervisor);
        assert!(restricted.is_none());
        assert_eq!(restricted_mode, "disabled");
    }

    #[test]
    fn sessions_cannot_cross_observe_or_pollute_history() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let alpha_dir = root.path().join("ra34-alpha-private");
        let beta_dir = root.path().join("ra34-beta-private");
        let restricted_dir = root.path().join("ra34-restricted-work");
        std::fs::create_dir_all(&alpha_dir).unwrap();
        std::fs::create_dir_all(&beta_dir).unwrap();
        std::fs::create_dir_all(&restricted_dir).unwrap();
        let kernel = Kernel::open(state.path()).unwrap();
        let (alpha_scope, alpha_store, _) = kernel.jump_history_policy(
            "agent:alpha",
            "readonly",
            ConnectionTrust::Public,
            false,
            true,
        );
        let (beta_scope, beta_store, _) = kernel.jump_history_policy(
            "agent:beta",
            "readonly",
            ConnectionTrust::Public,
            false,
            true,
        );
        let alpha = kernel
            .session_with_surface(
                "same-name",
                "agent:alpha",
                shoal_host::Surface::Kernel,
                &alpha_scope,
                alpha_store.clone(),
            )
            .unwrap();
        let beta = kernel
            .session_with_surface(
                "same-name",
                "agent:beta",
                shoal_host::Surface::Kernel,
                &beta_scope,
                beta_store.clone(),
            )
            .unwrap();
        let restricted = kernel
            .session_with_surface(
                "same-name",
                "agent:mcp",
                shoal_host::Surface::Kernel,
                "public/restricted-agent",
                None,
            )
            .unwrap();

        eval(&alpha, &format!("cd {}", alpha_dir.display())).unwrap();
        eval(&beta, &format!("cd {}", beta_dir.display())).unwrap();
        eval(&restricted, &format!("cd {}", restricted_dir.display())).unwrap();
        assert!(eval(&alpha, "j ra34-beta-private").is_err());
        assert!(eval(&beta, "j ra34-alpha-private").is_err());
        assert!(eval(&restricted, "j ra34-alpha-private").is_err());

        let alpha_text = std::fs::read_to_string(alpha_store.unwrap()).unwrap();
        let beta_text = std::fs::read_to_string(beta_store.unwrap()).unwrap();
        assert!(alpha_text.contains("ra34-alpha-private"));
        assert!(!alpha_text.contains("ra34-beta-private"));
        assert!(beta_text.contains("ra34-beta-private"));
        assert!(!beta_text.contains("ra34-alpha-private"));
    }
}
