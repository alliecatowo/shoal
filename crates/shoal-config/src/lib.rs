//! shoal's layered configuration: `shoal.toml`/`​.shoal.toml` discovery,
//! merging, validation, and the typed [`Config`] model consumers read.
//!
//! Full reference: `site/content/internals/configuration-reference.md`. Short version:
//!
//! - **Layering** (lowest → highest precedence): system (`/etc/shoal/shoal.toml`)
//!   → user (`$XDG_CONFIG_HOME/shoal/shoal.toml`, falling back to
//!   `~/.config/shoal/shoal.toml`) → project (nearest `.shoal.toml` walking up
//!   from the cwd) → environment overrides (`NO_COLOR`, `SHOAL_*`). Each layer
//!   deep-merges over the previous one key-by-key — an unset key falls
//!   through to the layer below, it is never "the whole table replaces the
//!   whole table". See [`LoadOptions::discover`] and [`load`].
//! - **Validation** (site/content/internals/configuration-reference.md): an unknown key is a warning (never a
//!   silently dropped value) naming the exact dotted path plus a
//!   did-you-mean suggestion when one is close; a type mismatch is a hard
//!   [`ConfigError`] naming the key path and the expected type; malformed
//!   TOML is a hard [`ConfigError`] — nothing in this crate panics on
//!   attacker- or typo-controlled input.
//! - **Coverage**: every key in [`Config`] has a documented, sane default —
//!   an absent `shoal.toml` anywhere on the layer chain is a fully usable
//!   configuration.

mod error;
mod load;
mod schema;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use error::ConfigError;
pub use load::{LoadOptions, Loaded, find_project_config, load};

/// The full, typed shoal configuration — the merged result of every layer.
/// Every field has a default, so `Config::default()` is itself a valid,
/// complete configuration (site/content/internals/configuration-reference.md documents each one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub prompt: Prompt,
    pub history: History,
    pub render: Render,
    pub editor: Editor,
    pub kernel: Kernel,
    pub adapters: Adapters,
    pub journal: Journal,
    pub leash: Leash,
    pub init: Init,
    pub completion: Completion,
    pub reef: Reef,
    /// `name -> expansion`. AST-level partial application (site/content/internals/language-conformance-contract.md) — a
    /// config-declared `gs = "git status"` is equivalent to the session
    /// statement `alias gs = git status` run at startup, just persisted.
    pub aliases: BTreeMap<String, String>,
    /// `NAME -> value`, set in the session environment at startup (like a
    /// declarative `.profile`).
    pub env: BTreeMap<String, String>,
}

/// Legacy/simple prompt config. `shoal-prompt` (the crate that actually
/// renders prompts) loads its own richer `[prompt]` schema (`format.left`,
/// `format.right`, `transient`, …) directly from the same files; this
/// `template` field is what that loader falls back to migrating from an
/// old-style config (site/content/internals/prompt-editor-lsp.md) and is kept here mainly so
/// `[prompt]` round-trips through this crate without tripping the
/// unknown-key scanner. New code should prefer `shoal-prompt`'s schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompt {
    pub template: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub enabled: bool,
    /// Maximum number of entries kept in the history file.
    pub max_entries: usize,
    /// History file path; `None` = the host's platform default (typically
    /// `$XDG_STATE_HOME/shoal/history` or similar — the host, not this
    /// crate, resolves the fallback).
    pub path: Option<PathBuf>,
    /// Skip appending an entry that's identical to the immediately
    /// preceding one (classic `HISTCONTROL=ignoredups`).
    pub dedup: bool,
    /// Glob-ish prefixes/patterns; a command line matching any entry here is
    /// never recorded to history (`HISTIGNORE`-equivalent). Matching
    /// semantics are the host's (this crate only carries the patterns).
    pub ignore: Vec<String>,
    /// Classic `HISTCONTROL=ignorespace`: a line typed with a **leading
    /// space** is never recorded.
    pub ignore_space: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Render {
    pub width: Option<usize>,
    pub color: bool,
    /// Opt-in gate for the interactive REPL's pager integration
    /// (site/content/internals/configuration-reference.md): `"never"` (the default — identical behavior to
    /// before this knob existed) never pages; `"auto"` pipes the REPL's
    /// *final* rendered result through a pager when stdout is a real TTY and
    /// the output would not fit on one screen. Never engages for `-c`/script
    /// runs, and never engages for mid-statement values inside a
    /// multi-statement line — see `crates/shoal/src/repl.rs`'s
    /// `render_result_paged`.
    pub paging: String,
    /// Explicit pager command, e.g. `"less -R"` or `"bat --paging=always"`.
    /// `None` (the default) falls back to `$PAGER`, then to the built-in
    /// `less -R` (the `-R` matters: rendered output is colorized ANSI, and
    /// plain `less` would print raw escape codes instead of color).
    pub pager: Option<String>,
    /// How much of a non-interactive (`-c`/script/stdin) run's top-level
    /// statement values auto-render (site/content/internals/configuration-reference.md): `"quiet"` renders
    /// only bare-command output plus the FINAL statement's value —
    /// intermediate pure expressions like `1+1`/`let x=…` stay silent;
    /// `"commands"` renders bare-command output only, not even the final
    /// expression; `"all"` echoes every statement (the REPL's behavior).
    /// `None` (the default) lets each host surface pick its own fallback: the
    /// non-interactive runner defaults to `"quiet"`, the interactive REPL to
    /// `"all"`. Setting it explicitly overrides both surfaces.
    pub echo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    pub mode: String,
    pub bracketed_paste: bool,
    /// `chord -> action`, e.g. `"ctrl-r" = "history_search_backward"`. Empty
    /// = the host's built-in bindings for `mode` are used unmodified.
    pub keybindings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kernel {
    pub enabled: bool,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Adapters {
    /// Extra adapter directories scanned in addition to the bundled pack, in
    /// order (later entries can shadow earlier ones for the same command
    /// name).
    pub dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Journal {
    pub enabled: bool,
    /// `None` = the host's platform default state directory.
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Leash {
    /// Path to the leash policy file (site/content/internals/reef-resolution.md, site/content/internals/intercrate-protocol-contracts.md leash tier).
    /// `None` = no policy loaded (unsandboxed).
    pub policy: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Init {
    /// Script files run, in order, at the start of every interactive
    /// session (a config-driven `.shoalrc`-equivalent).
    pub files: Vec<PathBuf>,
}

/// Tab-completion behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Completion {
    /// Allow non-contiguous/typo-tolerant matches, not just prefix matches.
    pub fuzzy: bool,
    pub case_insensitive: bool,
    /// Cap on how many candidates are computed/shown per completion; keeps a
    /// huge `PATH`/directory from making a keystroke feel laggy.
    pub max_results: usize,
    /// Show the interactive selection menu at all (vs. cycle-only).
    pub menu: bool,
}

/// User-scope reef bindings (site/content/internals/reef-resolution.md: `[reef]` in `shoal.toml` is the
/// *user* scope; project scope is the nearest `.reef.toml`). `shoal-reef`
/// re-parses `[reef]` directly out of the raw `shoal.toml` text with its own
/// richer manifest schema (constraints, providers, runner argv templates) —
/// the fields here are deliberately loose (`toml::Value` per entry) so this
/// crate only needs to agree with `shoal-reef` on "this is a table", not on
/// the full tool/runner grammar, which is `shoal-reef`'s to evolve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Reef {
    /// `tool name -> constraint` — a bare version string (`"22"`, `"*"`) or a
    /// table (`{ version = "1.4", provider = "mise" }`).
    pub tools: BTreeMap<String, toml::Value>,
    /// `extension -> invocation` — a bare tool name (`"python"`) or a table
    /// (`{ tool = "deno", args = ["run"] }`).
    pub runners: BTreeMap<String, toml::Value>,
    pub options: ReefOptions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReefOptions {
    /// Child PATH is synthesized-only (no ambient system tail) when true.
    pub hermetic: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            prompt: Prompt::default(),
            history: History::default(),
            render: Render::default(),
            editor: Editor::default(),
            kernel: Kernel::default(),
            adapters: Adapters::default(),
            journal: Journal::default(),
            leash: Leash::default(),
            init: Init::default(),
            completion: Completion::default(),
            reef: Reef::default(),
            aliases: BTreeMap::new(),
            env: BTreeMap::new(),
        }
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            template: "{cwd}".into(),
        }
    }
}

impl Default for History {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 10_000,
            path: None,
            dedup: true,
            ignore: Vec::new(),
            ignore_space: true,
        }
    }
}

impl Default for Render {
    fn default() -> Self {
        Self {
            width: None,
            color: true,
            paging: "never".into(),
            pager: None,
            echo: None,
        }
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            mode: "emacs".into(),
            bracketed_paste: true,
            keybindings: BTreeMap::new(),
        }
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self {
            enabled: true,
            session: "default".into(),
        }
    }
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            enabled: true,
            state_dir: None,
        }
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            fuzzy: true,
            case_insensitive: true,
            max_results: 100,
            menu: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_toml() {
        let text = toml::to_string(&Config::default()).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, Config::default());
    }

    #[test]
    fn default_config_passes_its_own_schema_check() {
        let value = toml::Value::try_from(Config::default()).unwrap();
        let mut warnings = Vec::new();
        schema::check(&value, schema::ROOT, "", &mut warnings).unwrap();
        assert!(
            warnings.is_empty(),
            "Config::default() must not trip its own unknown-key scanner: {warnings:?}"
        );
    }
}
