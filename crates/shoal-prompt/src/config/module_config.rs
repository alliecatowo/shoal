//! Per-module `[prompt.module.*]` config tables: one struct per fixed module,
//! plus the dynamic `language`/`custom` tables keyed by user-chosen names.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModuleConfig {
    pub character: CharacterModule,
    pub directory: DirectoryModule,
    pub git_branch: GitBranchModule,
    pub git_status: GitStatusModule,
    pub git_state: GitStateModule,
    pub cmd_duration: CmdDurationModule,
    pub exit_status: ExitStatusModule,
    pub jobs: JobsModule,
    pub time: TimeModule,
    pub username: UsernameModule,
    pub hostname: HostnameModule,
    pub reef: ReefModule,
    pub principal: PrincipalModule,
    pub leash: LeashModule,
    pub battery: BatteryModule,
    pub language: BTreeMap<String, LanguageModule>,
    pub custom: BTreeMap<String, CustomModule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CharacterModule {
    pub enabled: bool,
    pub success_symbol: String,
    pub error_symbol: String,
    pub vicmd_symbol: String,
    pub success_style: String,
    pub error_style: String,
    pub vicmd_style: String,
}
impl Default for CharacterModule {
    fn default() -> Self {
        Self {
            enabled: true,
            success_symbol: "❯".into(),
            error_symbol: "❯".into(),
            vicmd_symbol: "❮".into(),
            success_style: "green bold".into(),
            error_style: "red bold".into(),
            vicmd_style: "yellow".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectoryModule {
    pub enabled: bool,
    pub truncate_to: usize,
    pub truncate_style: String,
    pub repo_relative: bool,
    pub read_only_symbol: String,
    pub symbol: String,
    pub style: String,
    pub home_symbol: String,
}
impl Default for DirectoryModule {
    fn default() -> Self {
        Self {
            enabled: true,
            truncate_to: 3,
            truncate_style: "middle".into(),
            repo_relative: true,
            read_only_symbol: "🔒".into(),
            symbol: String::new(),
            style: "cyan bold".into(),
            home_symbol: "~".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitBranchModule {
    pub enabled: bool,
    pub symbol: String,
    pub ascii_symbol: String,
    pub style: String,
    pub truncate_to: usize,
    pub truncate_symbol: String,
    pub format: String,
}
impl Default for GitBranchModule {
    fn default() -> Self {
        Self {
            enabled: true,
            symbol: " ".into(),
            ascii_symbol: "git:".into(),
            style: "purple".into(),
            truncate_to: 32,
            truncate_symbol: "…".into(),
            format: "${symbol}${branch}".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitStatusModule {
    pub enabled: bool,
    pub style: String,
    pub ahead: String,
    pub behind: String,
    pub diverged: String,
    pub staged: String,
    pub unstaged: String,
    pub untracked: String,
    pub conflicted: String,
    pub stashed: String,
    pub stale_symbol: String,
}
impl Default for GitStatusModule {
    fn default() -> Self {
        Self {
            enabled: true,
            style: "red".into(),
            ahead: "⇡${count}".into(),
            behind: "⇣${count}".into(),
            diverged: "⇕⇡${ahead}⇣${behind}".into(),
            staged: "+${count}".into(),
            unstaged: "!${count}".into(),
            untracked: "?${count}".into(),
            conflicted: "=${count}".into(),
            stashed: "*${count}".into(),
            stale_symbol: "…".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitStateModule {
    pub enabled: bool,
    pub rebase: String,
    pub merge: String,
    pub cherry_pick: String,
    pub bisect: String,
    pub revert: String,
    pub style: String,
}
impl Default for GitStateModule {
    fn default() -> Self {
        Self {
            enabled: true,
            rebase: "REBASING".into(),
            merge: "MERGING".into(),
            cherry_pick: "CHERRY-PICKING".into(),
            bisect: "BISECTING".into(),
            revert: "REVERTING".into(),
            style: "yellow bold".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CmdDurationModule {
    pub enabled: bool,
    pub min_ms: u64,
    pub style: String,
}
impl Default for CmdDurationModule {
    fn default() -> Self {
        Self {
            enabled: true,
            min_ms: 500,
            style: "yellow".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExitStatusModule {
    pub enabled: bool,
    pub show_on_success: bool,
    pub format: String,
    pub style: String,
}
impl Default for ExitStatusModule {
    fn default() -> Self {
        Self {
            enabled: false,
            show_on_success: false,
            format: "[${status}]".into(),
            style: "red bold".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JobsModule {
    pub enabled: bool,
    pub symbol: String,
    pub style: String,
    pub threshold: usize,
    pub format: String,
}
impl Default for JobsModule {
    fn default() -> Self {
        Self {
            enabled: true,
            symbol: "✦".into(),
            style: "blue".into(),
            threshold: 1,
            format: "${symbol}${total}".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeModule {
    pub enabled: bool,
    pub format: String,
    pub style: String,
}
impl Default for TimeModule {
    fn default() -> Self {
        Self {
            enabled: true,
            format: "%H:%M:%S".into(),
            style: "dim".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsernameModule {
    pub enabled: bool,
    pub show_always: bool,
    pub style: String,
    pub root_style: String,
}
impl Default for UsernameModule {
    fn default() -> Self {
        Self {
            enabled: true,
            show_always: false,
            style: "yellow bold".into(),
            root_style: "red bold".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HostnameModule {
    pub enabled: bool,
    pub show_always: bool,
    pub symbol: String,
    pub style: String,
}
impl Default for HostnameModule {
    fn default() -> Self {
        Self {
            enabled: true,
            show_always: false,
            symbol: "@".into(),
            style: "green".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReefModule {
    pub enabled: bool,
    pub style: String,
    pub format: String,
    pub show_when_empty: bool,
    pub show_ambient: bool,
}
impl Default for ReefModule {
    fn default() -> Self {
        Self {
            enabled: true,
            style: "green".into(),
            format: "reef:${tools}".into(),
            show_when_empty: false,
            show_ambient: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PrincipalModule {
    pub enabled: bool,
    pub human_symbol: String,
    pub agent_symbol: String,
    pub show_agent_name: bool,
    pub style: String,
    pub agent_style: String,
}
impl Default for PrincipalModule {
    fn default() -> Self {
        Self {
            enabled: false,
            human_symbol: String::new(),
            agent_symbol: "🤖".into(),
            show_agent_name: true,
            style: "muted".into(),
            agent_style: "cyan".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LeashModule {
    pub enabled: bool,
    pub style_by_tier: BTreeMap<String, String>,
    pub symbol_by_tier: BTreeMap<String, String>,
    pub hide_when_enforced: bool,
}
impl Default for LeashModule {
    fn default() -> Self {
        Self {
            enabled: false,
            style_by_tier: [
                ("A", "green"),
                ("B", "yellow"),
                ("C", "yellow"),
                ("D", "red bold"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            symbol_by_tier: [("A", "🔒"), ("B", "🔓"), ("C", "🔓"), ("D", "⚠")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            hide_when_enforced: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BatteryModule {
    pub enabled: bool,
    pub charging_symbol: String,
    pub discharging_symbol: String,
    pub low_threshold: u8,
    pub low_style: String,
    pub style: String,
    pub sample_interval_s: u64,
}
impl Default for BatteryModule {
    fn default() -> Self {
        Self {
            enabled: false,
            charging_symbol: "⚡".into(),
            discharging_symbol: String::new(),
            low_threshold: 20,
            low_style: "red bold".into(),
            style: "green".into(),
            sample_interval_s: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LanguageModule {
    pub enabled: bool,
    pub tool: String,
    pub symbol: String,
    pub ascii_symbol: String,
    pub style: String,
    pub when: String,
    pub format: String,
}
impl Default for LanguageModule {
    fn default() -> Self {
        Self {
            enabled: true,
            tool: String::new(),
            symbol: String::new(),
            ascii_symbol: String::new(),
            style: "green".into(),
            when: "constrained".into(),
            format: "${symbol}${version}".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomModule {
    pub enabled: bool,
    pub command: String,
    pub when: String,
    pub cache_ttl: String,
    pub style: String,
    pub format: String,
}
impl Default for CustomModule {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
            when: String::new(),
            cache_ttl: "5s".into(),
            style: "blue".into(),
            format: "${output}".into(),
        }
    }
}
