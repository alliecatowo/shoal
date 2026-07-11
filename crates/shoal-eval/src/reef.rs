//! reef integration for the evaluator (docs/REEF.md §1–§6).
//!
//! The whole path is gated so that a repo with **no** `.reef.toml` (and no user
//! `[reef]` config) behaves EXACTLY as before: [`Evaluator::reef_apply`] fast-
//! bails to today's PATH/`which` behavior whenever the cached scope chain has no
//! manifest entry for the command head. Only a *constrained* head (a tool a
//! manifest in scope actually mentions) engages the resolver, the lock, the
//! interactive/script policy split, and child-PATH synthesis.
//!
//! Split across three files (the multi-file `impl Evaluator { .. }` pattern
//! `shoal-journal` also uses): this file holds the prompt-facing snapshot
//! types/method and the runner lookup; [`crate::reef_resolve`] holds the
//! scope-chain cache, override stack, and spawn-time resolution;
//! [`crate::reef_builtins`] holds the `which`/`reef` builtin commands built on
//! top of it.

use super::*;

/// One tool binding for the prompt's `reef`/`language.<tool>` segments —
/// shaped to drop straight into `shoal_prompt::ReefBinding` (docs/AGENT-
/// SURFACE.md §12.1). See [`Evaluator::prompt_reef_snapshot`].
#[derive(Debug, Clone)]
pub struct PromptReefBinding {
    pub tool: String,
    /// The locked version, if any scope's tool has been resolved and locked.
    /// `None` is an honest "not resolved yet" gap, never a guess.
    pub version: Option<String>,
    /// The provider that produced the locked binding (`"mise"`, `"system"`,
    /// …), if locked.
    pub provider: Option<String>,
    /// Label of the nearest scope constraining this tool (`"reef"`, `"mise"`,
    /// `"tool-versions"`, `"user"`).
    pub scope: Option<String>,
    pub constrained: bool,
}

/// The prompt-facing reef snapshot returned by
/// [`Evaluator::prompt_reef_snapshot`]: the active scope + every constrained
/// tool's binding, sourced entirely from cached/loaded state (zero
/// subprocess).
#[derive(Debug, Clone, Default)]
pub struct PromptReefSnapshot {
    /// Label of the nearest scope in the cached chain, if any manifest is in
    /// scope for the current cwd.
    pub active_scope: Option<String>,
    /// One entry per tool any scope in the chain mentions.
    pub bindings: Vec<PromptReefBinding>,
}

impl Evaluator {
    // --- prompt integration (docs/AGENT-SURFACE.md §12.1) ------------------

    /// The active reef scope + resolved tool bindings for the prompt's
    /// `reef_context`/`language.<tool>` segments. **Zero subprocess, zero
    /// fresh resolution** — this is the whole speed thesis (design §1/§5.4):
    /// it only ever ensures the cached [`shoal_reef::ScopeChain`] (a pure
    /// filesystem walk, at most once per command, and a no-op at all when the
    /// cwd hasn't changed) and reads the already-loaded
    /// [`shoal_reef::Lockfile`]. It never calls into
    /// [`shoal_reef::Resolver::resolve`], so it never probes a `--version`,
    /// never fetches, and never writes a lock entry.
    ///
    /// A tool a scope constrains but that isn't locked yet renders with
    /// `version: None`/`provider: None` — an honest gap (mirrors
    /// `shoal/src/prompt.rs`'s degraded-git-count contract), not a guess.
    /// Run `reef lock` (or let an interactive spawn auto-lock it) to populate
    /// it; the next command's snapshot will pick the fresh lock entry up for
    /// free, since [`Evaluator::ensure_reef_chain`] reloads it whenever the
    /// cwd changes and callers reload it themselves after any lock write.
    pub fn prompt_reef_snapshot(&mut self) -> PromptReefSnapshot {
        self.ensure_reef_chain();
        let Some((_, chain)) = self.reef_chain.as_ref() else {
            return PromptReefSnapshot::default();
        };
        let active_scope = chain.scopes.first().map(|s| s.label().to_string());

        let mut names: Vec<String> = Vec::new();
        for scope in &chain.scopes {
            for tool in scope.manifest.tools.keys() {
                if !names.contains(tool) {
                    names.push(tool.clone());
                }
            }
        }
        names.sort();

        let bindings = names
            .into_iter()
            .map(|tool| {
                let scope = chain.nearest_for(&tool).map(|s| s.label().to_string());
                match self.reef_lock.get(&tool) {
                    Some(entry) => PromptReefBinding {
                        tool,
                        version: Some(entry.version.clone()),
                        provider: Some(entry.provider.clone()),
                        scope,
                        constrained: true,
                    },
                    None => PromptReefBinding {
                        tool,
                        version: None,
                        provider: None,
                        scope,
                        constrained: true,
                    },
                }
            })
            .collect();

        PromptReefSnapshot {
            active_scope,
            bindings,
        }
    }

    // --- runners (REEF §5) -------------------------------------------------

    /// When a manifest is in scope, resolve the runner for `path` through reef
    /// (extension → tool, shebang fallback) and return the argv template
    /// (`[tool, ...args_template]`) whose tool the spawn will itself reef-
    /// resolve. `None` ⇒ no manifest in scope or `self`-runner (`.shl`): the
    /// caller keeps today's behavior. A pure lookup — no spawning here.
    pub(crate) fn reef_runner_argv(&mut self, path: &Path) -> Option<Vec<OsString>> {
        if !self.reef_manifest_in_scope() {
            return None;
        }
        let chain = self.reef_chain_snapshot();
        let table = chain.runner_table();
        let inv = shoal_reef::resolve_runner(path, &table)?;
        if inv.tool == "self" {
            return None;
        }
        let mut argv: Vec<OsString> = vec![OsString::from(&inv.tool)];
        argv.extend(inv.args_template.iter().map(OsString::from));
        Some(argv)
    }
}
