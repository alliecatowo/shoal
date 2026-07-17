//! Binary-side prompt wiring: build a [`PromptContext`] from live session state
//! and drive the pure `shoal-prompt` renderer as reedline's prompt.
//!
//! The site/content/internals/prompt-editor-lsp.md invariant (a prompt render performs zero I/O / zero subprocess spawns)
//! is honored structurally here: [`build_context`] runs **once per command**
//! (between keystrokes, right before the next `read_line`), freezing a snapshot
//! into a shared cell. reedline calls the render methods on every keystroke, but
//! those only ever read the frozen snapshot — never git, never a clock syscall,
//! never a spawn. This retires the `git_suffix()` subprocess-per-keystroke bug
//! the old `DefaultPrompt` path carried (site/content/internals/prompt-editor-lsp.md).
//!
//! `jobs` and `reef`/`language_*` are populated from the evaluator's typed
//! accessors (`Evaluator::jobs_snapshot`, `Evaluator::prompt_reef_snapshot` —
//! site/content/internals/prompt-editor-lsp.md): both read only in-memory/cached state (the live task
//! registry; the cached reef `ScopeChain` + already-loaded `Lockfile`), so
//! folding them into [`build_context`] costs no I/O beyond what the evaluator
//! already pays elsewhere. Git status *counts* (staged/unstaged/untracked/
//! ahead/behind) come from exactly one `git status --porcelain=v2 --branch`
//! subprocess per call to [`build_context`] — i.e. once per command, never
//! per keystroke (site/content/internals/prompt-editor-lsp.md); a non-git `cwd` never spawns it at all (`.git`
//! discovery is a pure filesystem walk that bails out first). Branch name and
//! in-progress state (`rebase`/`merge`/…) stay `gix`-free, read straight out
//! of `.git`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use reedline::{Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus};
use shoal_eval::Evaluator;
use shoal_prompt::{
    EditMode, GitSnapshot, LeashSnapshot, LeashTier, OutcomeSnapshot, Principal, PromptConfig,
    PromptContext, Renderer, RepoState, SessionSnapshot, Side,
};
use shoal_value::Value;

use crate::repl_state::ProtocolSnapshot;

mod cli;
mod config;
mod git;

pub use cli::{PromptAction, parse_action, run};
pub use config::load_prompt_config;
pub use git::read_git;
#[cfg(test)]
use git::{discover_repo, git_status_counts, parse_porcelain_v2_counts, read_head, read_state};

/// Shared, atomically-swappable snapshot cell. The REPL loop writes a fresh
/// `Arc<PromptContext>` once per command; reedline's per-keystroke render reads
/// it under a short read-lock (a lock, never I/O — inside site/content/internals/prompt-editor-lsp.md budget).
pub type SharedCtx = Arc<RwLock<Arc<PromptContext>>>;

/// The reedline `Prompt` impl. Thin dispatch onto the pure renderer (site/content/internals/prompt-editor-lsp.md).
pub struct ShoalPrompt {
    renderer: Arc<Renderer>,
    ctx: SharedCtx,
    /// When true this is the transient (post-Enter) prompt (site/content/internals/prompt-editor-lsp.md).
    transient: bool,
}

impl ShoalPrompt {
    pub fn new(renderer: Arc<Renderer>, ctx: SharedCtx, transient: bool) -> Self {
        Self {
            renderer,
            ctx,
            transient,
        }
    }

    fn snapshot(&self) -> Arc<PromptContext> {
        self.ctx
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
}

impl Prompt for ShoalPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let ctx = self.snapshot();
        let side = if self.transient {
            Side::Transient
        } else {
            Side::Left
        };
        Cow::Owned(self.renderer.render_side(side, &ctx))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        if self.transient {
            return Cow::Borrowed("");
        }
        let ctx = self.snapshot();
        Cow::Owned(self.renderer.render_side(Side::Right, &ctx))
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        // Locked decision (site/content/internals/prompt-editor-lsp.md): shoal-prompt owns the entire visual symbol via
        // the `$character` module inside `format.left`. Returning reedline's own
        // indicator here too would print the chevron twice.
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        let ctx = self.snapshot();
        Cow::Owned(self.renderer.render_side(Side::Continuation, &ctx))
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        // Out of scope (site/content/internals/prompt-editor-lsp.md): reuse reedline's own default text shape.
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({prefix}reverse-search: {}) ",
            history_search.term
        ))
    }

    fn right_prompt_on_last_line(&self) -> bool {
        self.renderer.config().right_prompt_on_last_line
    }
}

// ---------------------------------------------------------------------------
// Static session facts — resolved once at startup (site/content/internals/prompt-editor-lsp.md)
// ---------------------------------------------------------------------------

/// Facts that never change over a process lifetime: session identity, leash
/// tier, font/color resolution. Computed once, then folded into every snapshot.
pub struct StaticFacts {
    pub session: SessionSnapshot,
    pub leash: LeashSnapshot,
    pub home: Option<PathBuf>,
    pub no_color: bool,
    pub nerd_font: bool,
    pub unicode: bool,
    pub principal: Principal,
}

impl StaticFacts {
    pub fn resolve(config: &PromptConfig, no_color: bool) -> Self {
        let session = resolve_session();
        let leash = resolve_leash();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let nerd_font = resolve_nerd_font(&config.nerd_font);
        let unicode = config.unicode;
        Self {
            session,
            leash,
            home,
            no_color,
            nerd_font,
            unicode,
            principal: Principal::Human,
        }
    }
}

fn resolve_session() -> SessionSnapshot {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let host = hostname();
    let is_ssh =
        std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some();
    // SAFETY: getuid is always safe; it only reads the caller's real uid.
    let is_root = unsafe { libc::getuid() } == 0;
    SessionSnapshot {
        user,
        host,
        is_ssh,
        is_root,
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into our buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return std::env::var("HOSTNAME").unwrap_or_default();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn resolve_leash() -> LeashSnapshot {
    let status = shoal_leash::EnforcementStatus::detect();
    let tier = match status.available_tier {
        shoal_leash::EnforcementTier::A => LeashTier::A,
        shoal_leash::EnforcementTier::B => LeashTier::B,
        shoal_leash::EnforcementTier::C => LeashTier::C,
        shoal_leash::EnforcementTier::D => LeashTier::D,
    };
    LeashSnapshot {
        tier,
        enforced: status.enforced,
    }
}

/// nerd-font resolution (site/content/internals/prompt-editor-lsp.md): exactly these checks, in this order.
fn resolve_nerd_font(mode: &str) -> bool {
    match mode {
        "always" => true,
        "never" => false,
        _ => {
            std::env::var_os("WEZTERM_PANE").is_some()
                || std::env::var_os("KITTY_WINDOW_ID").is_some()
                || std::env::var_os("WT_SESSION").is_some()
                || std::env::var("SHOAL_NERD_FONT").ok().as_deref() == Some("1")
        }
    }
}

// ---------------------------------------------------------------------------
// Per-command context construction (site/content/internals/prompt-editor-lsp.md build_context)
// ---------------------------------------------------------------------------

/// Build a full [`PromptContext`] from live session state. Runs once per command
/// (never per keystroke), so its handful of `stat`s reading `.git` (plus, when
/// `cwd` is inside a repo, the one `git status` subprocess for status counts)
/// sit in the post-command budget (site/content/internals/prompt-editor-lsp.md trigger 1), not the keystroke budget
/// (site/content/internals/prompt-editor-lsp.md). Takes `&mut Evaluator` because [`Evaluator::prompt_reef_snapshot`]
/// may need to (re)discover the cached reef scope chain when `cwd` changed —
/// still zero subprocess, just a cache-freshness check.
pub fn build_context(ev: &mut Evaluator, facts: &StaticFacts, width: u16) -> PromptContext {
    let cwd = ev.cwd().to_path_buf();
    let read_only = is_read_only(&cwd);
    let last_outcome = outcome_from(ev.it());
    let git = read_git(&cwd);
    let jobs = jobs_snapshot_from(ev);
    let reef = reef_bindings_from(ev);
    let (h, m, s) = local_hms();

    PromptContext {
        cwd,
        home: facts.home.clone(),
        read_only,
        width,
        no_color: facts.no_color,
        nerd_font: facts.nerd_font,
        unicode: facts.unicode,
        edit_mode: EditMode::Emacs,
        multiline: false,
        last_outcome,
        jobs,
        principal: facts.principal.clone(),
        leash: facts.leash.clone(),
        session: facts.session.clone(),
        time_local: (h, m, s),
        git,
        reef,
        battery: None,
        custom: std::collections::BTreeMap::new(),
    }
}

/// Build the same frozen per-command prompt context from the authenticated
/// kernel snapshot used by the protocol-backed REPL. Filesystem/git facts are
/// resolved against the remote session cwd; language state comes only from
/// the snapshot, never from the UI's local bootstrap evaluator.
pub fn build_context_from_protocol(
    snapshot: &ProtocolSnapshot,
    facts: &StaticFacts,
    width: u16,
) -> PromptContext {
    let cwd = snapshot.cwd.clone();
    let (h, m, s) = local_hms();
    let last_outcome = match &snapshot.last_value {
        shoal_proto::WireValue::Outcome {
            status,
            ok,
            signal,
            dur_ns,
            cmd,
            ..
        } => Some(OutcomeSnapshot {
            ok: *ok,
            status: *status,
            signal: signal.clone(),
            dur: Duration::from_nanos((*dur_ns).max(0) as u64),
            cmd_head: cmd.split_whitespace().next().unwrap_or("").to_string(),
        }),
        _ => None,
    };
    PromptContext {
        cwd: cwd.clone(),
        home: facts.home.clone(),
        read_only: is_read_only(&cwd),
        width,
        no_color: facts.no_color,
        nerd_font: facts.nerd_font,
        unicode: facts.unicode,
        edit_mode: EditMode::Emacs,
        multiline: false,
        last_outcome,
        jobs: shoal_prompt::JobsSnapshot {
            running: snapshot.jobs.running,
            suspended: snapshot.jobs.suspended,
            total: snapshot.jobs.total,
        },
        principal: facts.principal.clone(),
        leash: facts.leash.clone(),
        session: facts.session.clone(),
        time_local: (h, m, s),
        git: read_git(&cwd),
        reef: snapshot
            .reef
            .iter()
            .map(|binding| shoal_prompt::ReefBinding {
                tool: binding.tool.clone(),
                version: binding.version.clone(),
                provider: binding.provider.clone(),
                scope: binding.scope.clone(),
                constrained: binding.constrained,
            })
            .collect(),
        battery: None,
        custom: std::collections::BTreeMap::new(),
    }
}

/// Map the evaluator's [`shoal_eval::JobsSnapshot`] onto the prompt's own
/// (site/content/internals/prompt-editor-lsp.md): same shape, different crate, so the binary is the seam
/// that converts.
fn jobs_snapshot_from(ev: &Evaluator) -> shoal_prompt::JobsSnapshot {
    let s = ev.jobs_snapshot();
    shoal_prompt::JobsSnapshot {
        running: s.running,
        suspended: s.suspended,
        total: s.total,
    }
}

/// Map the evaluator's [`shoal_eval::PromptReefSnapshot`] bindings onto
/// [`shoal_prompt::ReefBinding`] rows (site/content/internals/prompt-editor-lsp.md). Zero subprocess: reads
/// only the evaluator's cached scope chain + already-loaded lockfile.
fn reef_bindings_from(ev: &mut Evaluator) -> Vec<shoal_prompt::ReefBinding> {
    ev.prompt_reef_snapshot()
        .bindings
        .into_iter()
        .map(|b| shoal_prompt::ReefBinding {
            tool: b.tool,
            version: b.version,
            provider: b.provider,
            scope: b.scope,
            constrained: b.constrained,
        })
        .collect()
}

fn outcome_from(it: &Value) -> Option<OutcomeSnapshot> {
    match it {
        Value::Outcome(o) => Some(OutcomeSnapshot {
            ok: o.ok,
            status: o.status,
            signal: o.signal.clone(),
            dur: Duration::from_nanos(o.dur_ns.max(0) as u64),
            cmd_head: o.cmd.split_whitespace().next().unwrap_or("").to_string(),
        }),
        _ => None,
    }
}

fn is_read_only(cwd: &Path) -> bool {
    // SAFETY: access(2) only reads permission bits for the given path.
    let c = match std::ffi::CString::new(cwd.as_os_str().to_string_lossy().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) != 0 }
}

fn local_hms() -> (u8, u8, u8) {
    // SAFETY: time()/localtime_r are the standard libc time path; localtime_r
    // writes into a caller-owned tm and is thread-safe.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return (0, 0, 0);
        }
        (tm.tm_hour as u8, tm.tm_min as u8, tm.tm_sec as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nerd_font_modes() {
        assert!(resolve_nerd_font("always"));
        assert!(!resolve_nerd_font("never"));
    }

    #[test]
    fn head_parses_branch_and_detached() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(read_head(dir.path()).0.as_deref(), Some("x"));
        std::fs::write(
            dir.path().join("HEAD"),
            "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\n",
        )
        .unwrap();
        assert_eq!(read_head(dir.path()).1.as_deref(), Some("a1b2c3d"));
    }

    #[test]
    fn state_detects_merge() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_state(dir.path()), RepoState::Clean);
        std::fs::write(dir.path().join("MERGE_HEAD"), "x").unwrap();
        assert_eq!(read_state(dir.path()), RepoState::Merging);
    }

    #[test]
    fn discover_finds_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let (root, gitdir) = discover_repo(&sub).unwrap();
        assert_eq!(root, dir.path());
        assert!(gitdir.ends_with(".git"));
    }

    // -----------------------------------------------------------------------
    // Git status counts (site/content/internals/prompt-editor-lsp.md)
    // -----------------------------------------------------------------------

    #[test]
    fn porcelain_v2_counts_every_shape() {
        let text = "# branch.oid abc123\n\
                     # branch.head main\n\
                     # branch.upstream origin/main\n\
                     # branch.ab +2 -3\n\
                     1 M. N... 100644 100644 100644 aaaa bbbb staged.txt\n\
                     1 .M N... 100644 100644 100644 aaaa bbbb modified.txt\n\
                     2 R. N... 100644 100644 100644 aaaa bbbb R100 renamed.txt\toriginal.txt\n\
                     u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.txt\n\
                     ? untracked.txt\n\
                     ! ignored.txt\n";
        let c = parse_porcelain_v2_counts(text.as_bytes());
        assert_eq!(c.ahead, 2);
        assert_eq!(c.behind, 3);
        assert_eq!(c.staged, 2, "the `1 M.` and `2 R.` entries are staged");
        assert_eq!(c.unstaged, 1, "the `1 .M` entry is unstaged");
        assert_eq!(c.conflicted, 1);
        assert_eq!(c.untracked, 1);
    }

    #[test]
    fn porcelain_v2_counts_no_upstream_leaves_ahead_behind_zero() {
        let text = "# branch.oid abc123\n# branch.head main\n? new.txt\n";
        let c = parse_porcelain_v2_counts(text.as_bytes());
        assert_eq!(c.ahead, 0);
        assert_eq!(c.behind, 0);
        assert_eq!(c.untracked, 1);
    }

    #[test]
    fn git_status_counts_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // `dir` is never `git init`-ed: a real "not a git repository" failure
        // must degrade to `None`, never a fabricated all-zero "clean" answer.
        assert!(git_status_counts(dir.path()).is_none());
    }

    #[test]
    fn git_status_counts_from_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "t@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        run_git(root, &["add", "a.txt"]);
        std::fs::write(root.join("b.txt"), "untracked\n").unwrap();

        let counts = git_status_counts(root).expect("git is available in test env");
        assert_eq!(counts.staged, 1, "a.txt is staged (added, uncommitted)");
        assert_eq!(counts.unstaged, 0);
        assert_eq!(counts.untracked, 1, "b.txt is untracked");
    }

    #[test]
    fn read_git_fills_counts_for_a_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "t@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        run_git(root, &["add", "a.txt"]);

        let snap = read_git(root).expect("root is a git repo");
        assert!(!snap.degraded, "git ran successfully");
        assert_eq!(snap.staged, 1);
        assert_eq!(snap.unstaged, 0);
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git available in test environment");
        assert!(status.success(), "git {args:?} failed");
    }

    // -----------------------------------------------------------------------
    // build_context integration: jobs/reef/git wiring (site/content/internals/kernel-protocol.md)
    // -----------------------------------------------------------------------

    fn test_facts() -> StaticFacts {
        StaticFacts {
            session: SessionSnapshot {
                user: "t".into(),
                host: "h".into(),
                is_ssh: false,
                is_root: false,
            },
            leash: LeashSnapshot {
                tier: LeashTier::A,
                enforced: false,
            },
            home: None,
            no_color: true,
            nerd_font: false,
            unicode: true,
            principal: Principal::Human,
        }
    }

    #[test]
    fn build_context_populates_jobs_from_the_evaluator() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = shoal_eval::Evaluator::new(dir.path().to_path_buf());
        ev.eval_program(&shoal_syntax::parse("spawn { 1 + 1 }").unwrap())
            .unwrap();
        let ctx = build_context(&mut ev, &test_facts(), 80);
        assert_eq!(ctx.jobs.total, 1, "the spawned task is registered");
    }

    #[test]
    fn build_context_populates_reef_bindings_from_the_evaluator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools]\nnode = \"18\"\n").unwrap();
        let mut ev = shoal_eval::Evaluator::new(dir.path().to_path_buf());
        let ctx = build_context(&mut ev, &test_facts(), 80);
        let binding = ctx
            .reef
            .iter()
            .find(|b| b.tool == "node")
            .expect("node is constrained by .reef.toml");
        assert!(binding.constrained);
        assert_eq!(binding.scope.as_deref(), Some("reef"));
        // Nothing has resolved/locked it yet — an honest gap, not a guess.
        assert!(binding.version.is_none());
    }

    #[test]
    fn build_context_populates_git_counts_once_per_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "t@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        run_git(root, &["add", "a.txt"]);

        let mut ev = shoal_eval::Evaluator::new(root.to_path_buf());
        let ctx = build_context(&mut ev, &test_facts(), 80);
        let git = ctx.git.expect("root is a git repo");
        assert!(!git.degraded);
        assert_eq!(git.staged, 1);
    }

    #[test]
    fn protocol_context_uses_the_authenticated_session_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = ProtocolSnapshot::parse(serde_json::json!({
            "cwd": {"display": dir.path().to_string_lossy()},
            "bindings": [],
            "jobs": {"running": 2, "suspended": 1, "total": 4},
            "reef": {"bindings": [{
                "tool": "node",
                "version": "22.1.0",
                "provider": "mise",
                "scope": "project",
                "constrained": true
            }]},
            "last_value": {
                "$": "outcome",
                "status": 7,
                "ok": false,
                "signal": null,
                "out": {"$": "null"},
                "err": "",
                "dur_ns": 12,
                "pid": 99,
                "cmd": "false --example"
            }
        }))
        .unwrap();

        let context = build_context_from_protocol(&snapshot, &test_facts(), 93);
        assert_eq!(context.cwd, dir.path());
        assert_eq!(context.width, 93);
        assert_eq!(context.jobs.running, 2);
        assert_eq!(context.jobs.suspended, 1);
        assert_eq!(context.jobs.total, 4);
        let outcome = context.last_outcome.expect("kernel outcome reaches prompt");
        assert!(!outcome.ok);
        assert_eq!(outcome.status, Some(7));
        assert_eq!(outcome.cmd_head, "false");
        assert_eq!(outcome.dur, Duration::from_nanos(12));
        assert_eq!(context.reef.len(), 1);
        assert_eq!(context.reef[0].tool, "node");
        assert_eq!(context.reef[0].version.as_deref(), Some("22.1.0"));
        assert!(context.reef[0].constrained);
    }
}
