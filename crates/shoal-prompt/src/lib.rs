//! # shoal-prompt — the prompt that already knows
//!
//! A **pure domain crate** (no IO, no process spawning) that takes a structured
//! [`PromptContext`] snapshot and renders a styled prompt string. It is the
//! normative realization of `site/content/internals/prompt-editor-lsp.md` (v0.1).
//!
//! ## Dependency rule (site/content/internals/prompt-editor-lsp.md)
//!
//! This crate depends on *nothing* under `shoal-*`. The hexagonal seam is made
//! real by the type system: shoal-prompt cannot spawn a process, resolve a
//! tool, touch the journal, or import a git library — the render path is
//! guaranteed I/O-free because the crate has no way to do I/O. The producing
//! side (the `shoal` binary) resolves every uncertain value into a "known" or
//! "known-to-be-stale/pending" field *before* building the [`PromptContext`],
//! so [`Renderer::render`] is a pure format-string walk (site/content/internals/prompt-editor-lsp.md invariant).
//!
//! ## Speed
//!
//! Rendering the common path is sub-millisecond: every module reads an
//! already-computed field or a small owned value. `tests/speed.rs` asserts the
//! common-path render stays comfortably inside the p99 budget; `benches/render.rs`
//! is the criterion counterpart.

mod config;
mod context;
mod fmt;
mod format;
mod render;
pub mod style;
pub mod themes;

pub use config::{
    BatteryModule, BudgetConfig, CharacterModule, CmdDurationModule, CustomModule, DirectoryModule,
    ExitStatusModule, FormatConfig, GitBranchModule, GitStateModule, GitStatusModule,
    HostnameModule, JobsModule, LanguageModule, LeashModule, ModuleConfig, ParsedFormats,
    PrincipalModule, PromptConfig, ReefModule, STATIC_MODULE_IDS, StylePalette, TimeModule,
    TransientConfig, UsernameModule, env_overrides, load,
};
pub use context::{
    BatterySnapshot, CustomSegment, EditMode, GitSnapshot, JobsSnapshot, LeashSnapshot, LeashTier,
    OutcomeSnapshot, Principal, PromptContext, ReefBinding, RepoState, SessionSnapshot,
};
pub use fmt::{format_duration, format_duration_ns, format_size};
pub use format::{FormatToken, parse_format, referenced_ids};
pub use render::{RenderedPrompt, Renderer, Side};
