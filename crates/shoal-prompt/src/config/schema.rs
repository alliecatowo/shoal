//! Unknown-key validation (site/content/internals/prompt-editor-lsp.md): the allow-list of keys per table
//! path, walked recursively over the merged config value before it is
//! deserialized into [`super::PromptConfig`].

/// Warn about unknown keys anywhere in the prompt table (site/content/internals/prompt-editor-lsp.md). Dynamic
/// `module.language.<tool>` / `module.custom.<name>` inner keys are validated
/// against their own schema; the user-chosen table name itself is never flagged.
pub(super) fn validate_keys(v: &toml::Value, warnings: &mut Vec<String>) {
    walk(v, "prompt", warnings);
}

fn allowed_for(prefix: &str) -> Option<&'static [&'static str]> {
    Some(match prefix {
        "prompt" => &[
            "theme",
            "nerd_font",
            "unicode",
            "right_prompt_on_last_line",
            "format",
            "transient",
            "budget",
            "style",
            "module",
        ],
        "prompt.format" => &["left", "right", "continuation", "transient"],
        "prompt.transient" => &["enabled"],
        "prompt.budget" => &["render_deadline_ms", "warn_on_exceed"],
        "prompt.style" => &["ok", "error", "warn", "info", "muted", "accent"],
        "prompt.module" => &[
            "character",
            "directory",
            "git_branch",
            "git_status",
            "git_state",
            "cmd_duration",
            "exit_status",
            "jobs",
            "time",
            "username",
            "hostname",
            "reef",
            "principal",
            "leash",
            "battery",
            "language",
            "custom",
        ],
        "prompt.module.character" => &[
            "enabled",
            "success_symbol",
            "error_symbol",
            "vicmd_symbol",
            "success_style",
            "error_style",
            "vicmd_style",
        ],
        "prompt.module.directory" => &[
            "enabled",
            "truncate_to",
            "truncate_style",
            "repo_relative",
            "read_only_symbol",
            "symbol",
            "style",
            "home_symbol",
        ],
        "prompt.module.git_branch" => &[
            "enabled",
            "symbol",
            "ascii_symbol",
            "style",
            "truncate_to",
            "truncate_symbol",
            "format",
        ],
        "prompt.module.git_status" => &[
            "enabled",
            "style",
            "ahead",
            "behind",
            "diverged",
            "staged",
            "unstaged",
            "untracked",
            "conflicted",
            "stashed",
            "stale_symbol",
        ],
        "prompt.module.git_state" => &[
            "enabled",
            "rebase",
            "merge",
            "cherry_pick",
            "bisect",
            "revert",
            "style",
        ],
        "prompt.module.cmd_duration" => &["enabled", "min_ms", "style"],
        "prompt.module.exit_status" => &["enabled", "show_on_success", "format", "style"],
        "prompt.module.jobs" => &["enabled", "symbol", "style", "threshold", "format"],
        "prompt.module.time" => &["enabled", "format", "style"],
        "prompt.module.username" => &["enabled", "show_always", "style", "root_style"],
        "prompt.module.hostname" => &["enabled", "show_always", "symbol", "style"],
        "prompt.module.reef" => &[
            "enabled",
            "style",
            "format",
            "show_when_empty",
            "show_ambient",
        ],
        "prompt.module.principal" => &[
            "enabled",
            "human_symbol",
            "agent_symbol",
            "show_agent_name",
            "style",
            "agent_style",
        ],
        "prompt.module.leash" => &[
            "enabled",
            "style_by_tier",
            "symbol_by_tier",
            "hide_when_enforced",
        ],
        "prompt.module.battery" => &[
            "enabled",
            "charging_symbol",
            "discharging_symbol",
            "low_threshold",
            "low_style",
            "style",
            "sample_interval_s",
        ],
        _ => return None,
    })
}

fn walk(v: &toml::Value, prefix: &str, warnings: &mut Vec<String>) {
    // The two dynamic tables: their direct children are user-named, so we
    // validate one level deeper against the language/custom module schema.
    if prefix == "prompt.module.language" || prefix == "prompt.module.custom" {
        let inner_allowed: &[&str] = if prefix.ends_with("language") {
            &[
                "enabled",
                "tool",
                "symbol",
                "ascii_symbol",
                "style",
                "when",
                "format",
            ]
        } else {
            &["enabled", "command", "when", "cache_ttl", "style", "format"]
        };
        if let Some(t) = v.as_table() {
            for (_name, entry) in t {
                if let Some(inner) = entry.as_table() {
                    for k in inner.keys() {
                        if !inner_allowed.contains(&k.as_str()) {
                            warnings.push(format!("unknown config key {prefix}.<name>.{k}"));
                        }
                    }
                }
            }
        }
        return;
    }

    let Some(allowed) = allowed_for(prefix) else {
        return;
    };
    if let Some(t) = v.as_table() {
        for (k, x) in t {
            if !allowed.contains(&k.as_str()) {
                warnings.push(format!("unknown config key {prefix}.{k}"));
            } else if x.is_table() {
                walk(x, &format!("{prefix}.{k}"), warnings);
            }
        }
    }
}
