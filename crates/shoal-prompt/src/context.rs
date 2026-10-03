//! `PromptContext` — the one and only input to the renderer (site/content/internals/prompt-editor-lsp.md).
//!
//! Every field is either copy-cheap or a small owned value already sized for a
//! single repo/session. No field is a `Result` or a `Future`: by the time a
//! value reaches `PromptContext`, ambiguity has already been resolved into
//! "known" or "known-to-be-stale/pending" by whatever background task owns it.
//! Constructing a `PromptContext` is a handful of clones — never I/O.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// The complete, pre-resolved snapshot a single render reads. Built once by the
/// binary (per command, not per keystroke) and handed to [`crate::Renderer`].
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// cwd not writable by the current euid (cached per-cwd by the producer).
    pub read_only: bool,
    /// Terminal columns (from the last resize event).
    pub width: u16,
    /// `NO_COLOR` present (checked once at process start).
    pub no_color: bool,
    /// Nerd-font glyphs available (resolved once at startup, site/content/internals/prompt-editor-lsp.md).
    pub nerd_font: bool,
    /// Unicode allowed; ascii fallback when false (site/content/internals/prompt-editor-lsp.md).
    pub unicode: bool,
    pub edit_mode: EditMode,

    /// `None`: no command has run yet this session.
    pub last_outcome: Option<OutcomeSnapshot>,
    pub jobs: JobsSnapshot,
    pub principal: Principal,
    pub leash: LeashSnapshot,
    pub session: SessionSnapshot,
    /// `(hour, min, sec)`, read live every render (a vDSO clock read; ~ns).
    pub time_local: (u8, u8, u8),

    /// `None`: cwd is not inside a repo.
    pub git: Option<GitSnapshot>,
    /// Possibly empty; one entry per tool any scope constrains.
    pub reef: Vec<ReefBinding>,
    pub battery: Option<BatterySnapshot>,

    /// Keyed by `[prompt.module.custom.<name>]`.
    pub custom: BTreeMap<String, CustomSegment>,
}

impl PromptContext {
    /// A blank context — every module renders its hidden/empty form. Handy as a
    /// test/bench baseline and as the binary's "nothing known yet" starting
    /// point before the first command runs.
    pub fn empty(cwd: PathBuf) -> Self {
        Self {
            cwd,
            home: None,
            read_only: false,
            width: 80,
            no_color: false,
            nerd_font: false,
            unicode: true,
            edit_mode: EditMode::Emacs,
            last_outcome: None,
            jobs: JobsSnapshot::default(),
            principal: Principal::Human,
            leash: LeashSnapshot {
                tier: LeashTier::A,
                enforced: false,
            },
            session: SessionSnapshot {
                user: String::new(),
                host: String::new(),
                is_ssh: false,
                is_root: false,
            },
            time_local: (0, 0, 0),
            git: None,
            reef: Vec::new(),
            battery: None,
            custom: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    Emacs,
    ViNormal,
    ViInsert,
    ViVisual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Human,
    Agent(String),
}

#[derive(Debug, Clone)]
pub struct OutcomeSnapshot {
    pub ok: bool,
    pub status: Option<i32>,
    pub signal: Option<String>,
    pub dur: Duration,
    pub cmd_head: String,
}

#[derive(Debug, Clone, Default)]
pub struct JobsSnapshot {
    pub running: usize,
    pub suspended: usize,
    /// Active jobs (`running + suspended`), used for threshold/display.
    pub total: usize,
    /// Bounded completed history retained by the evaluator.
    pub completed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeashTier {
    A,
    B,
    C,
    D,
}

impl LeashTier {
    pub fn as_str(self) -> &'static str {
        match self {
            LeashTier::A => "A",
            LeashTier::B => "B",
            LeashTier::C => "C",
            LeashTier::D => "D",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LeashSnapshot {
    pub tier: LeashTier,
    pub enforced: bool,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub user: String,
    pub host: String,
    pub is_ssh: bool,
    pub is_root: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Rebasing,
    Merging,
    CherryPicking,
    Bisecting,
    Reverting,
}

#[derive(Debug, Clone)]
pub struct GitSnapshot {
    pub repo_root: PathBuf,
    /// cwd relative to `repo_root`.
    pub repo_relative: PathBuf,
    /// `None`: detached HEAD.
    pub branch: Option<String>,
    /// Short sha, when `branch` is `None`.
    pub detached_at: Option<String>,
    pub state: RepoState,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub stashed: u32,
    /// Last recompute failed or is still pending; counts are stale.
    pub degraded: bool,
}

impl GitSnapshot {
    /// A placeholder snapshot for a repo whose first recompute has not landed
    /// yet — known root/branch, zeroed counts, flagged degraded (site/content/internals/prompt-editor-lsp.md).
    pub fn pending(repo_root: PathBuf, repo_relative: PathBuf, branch: Option<String>) -> Self {
        Self {
            repo_root,
            repo_relative,
            branch,
            detached_at: None,
            state: RepoState::Clean,
            ahead: 0,
            behind: 0,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicted: 0,
            stashed: 0,
            degraded: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReefBinding {
    pub tool: String,
    pub version: Option<String>,
    pub provider: Option<String>,
    pub scope: Option<String>,
    pub constrained: bool,
}

#[derive(Debug, Clone)]
pub struct BatterySnapshot {
    pub pct: u8,
    pub charging: bool,
}

/// State of a `[prompt.module.custom.<name>]` background computation (site/content/internals/prompt-editor-lsp.md).
#[derive(Debug, Clone)]
pub enum CustomSegment {
    Ready(String),
    Pending,
    Stale(String, Duration),
    Error(String),
}
