//! The pure renderer: format-token walk (§3.4) + per-module `match` (§4) +
//! style application (§3.6), with the deadline degrade of §1. No I/O, no
//! logging, no side effects — a golden/snapshot suite drives it with hand-built
//! `PromptContext` values, no kernel required.

use std::time::{Duration, Instant};

use crate::config::{ParsedFormats, PromptConfig};
use crate::context::{CustomSegment, EditMode, Principal, PromptContext, RepoState};
use crate::format::FormatToken;
use crate::style::parse_style;

/// The four independent strings reedline asks for (§2.5). `indicator` is always
/// empty: shoal-prompt owns the entire visual symbol via `$character`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub left: String,
    pub right: String,
    pub continuation: String,
    pub indicator: String,
}

/// Which format string to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Continuation,
    Transient,
}

/// A cheap, allocation-light renderer. Parses `format.*` once ([`Renderer::new`])
/// and thereafter renders purely from a [`PromptContext`] snapshot.
#[derive(Debug, Clone)]
pub struct Renderer {
    config: PromptConfig,
    formats: ParsedFormats,
}

/// Classification of a rendered segment for the whitespace-collapse pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Literal,
    Whitespace,
    Module,
}

struct Seg {
    text: String,
    kind: Kind,
    empty: bool,
}

impl Renderer {
    /// Build a renderer, parsing all four format strings once and collecting any
    /// unknown-module / style warnings (§11).
    pub fn new(config: PromptConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let formats = config.parse_formats(&mut warnings);
        (Self { config, formats }, warnings)
    }

    pub fn config(&self) -> &PromptConfig {
        &self.config
    }

    /// Render the main prompt (left/right/continuation; indicator always empty).
    pub fn render(&self, ctx: &PromptContext) -> RenderedPrompt {
        RenderedPrompt {
            left: self.render_side(Side::Left, ctx),
            right: self.render_side(Side::Right, ctx),
            continuation: self.render_side(Side::Continuation, ctx),
            indicator: String::new(),
        }
    }

    /// Render a single side, honoring the §1 deadline: once elapsed exceeds the
    /// budget, every remaining module renders its cheapest fallback (empty).
    pub fn render_side(&self, side: Side, ctx: &PromptContext) -> String {
        let tokens = match side {
            Side::Left => &self.formats.left,
            Side::Right => &self.formats.right,
            Side::Continuation => &self.formats.continuation,
            Side::Transient => &self.formats.transient,
        };
        let start = Instant::now();
        let deadline = Duration::from_millis(self.config.budget.render_deadline_ms);
        let mut segs = Vec::with_capacity(tokens.len());
        self.render_tokens(tokens, ctx, start, deadline, &mut segs);
        join_collapsing(&segs)
    }

    fn render_tokens(
        &self,
        tokens: &[FormatToken],
        ctx: &PromptContext,
        start: Instant,
        deadline: Duration,
        out: &mut Vec<Seg>,
    ) {
        for tok in tokens {
            match tok {
                FormatToken::Literal { text, ws_only } => out.push(Seg {
                    text: text.clone(),
                    kind: if *ws_only {
                        Kind::Whitespace
                    } else {
                        Kind::Literal
                    },
                    empty: text.is_empty(),
                }),
                FormatToken::Placeholder(id) => {
                    // Deadline degrade: past budget, remaining modules go empty.
                    let text = if start.elapsed() > deadline {
                        String::new()
                    } else {
                        self.render_placeholder(id, ctx)
                    };
                    out.push(Seg {
                        empty: text.is_empty(),
                        text,
                        kind: Kind::Module,
                    });
                }
                FormatToken::Group { inner, style } => {
                    let mut inner_segs = Vec::new();
                    self.render_tokens(inner, ctx, start, deadline, &mut inner_segs);
                    let joined = join_collapsing(&inner_segs);
                    let text = self.paint(style, &joined, ctx);
                    out.push(Seg {
                        empty: joined.is_empty(),
                        text,
                        kind: Kind::Module,
                    });
                }
            }
        }
    }

    /// Render one `$placeholder` to its styled string, or `""` when hidden.
    pub fn render_placeholder(&self, id: &str, ctx: &PromptContext) -> String {
        if let Some(tool) = id.strip_prefix("language_") {
            return self.render_language(tool, ctx);
        }
        if let Some(name) = id.strip_prefix("custom_") {
            return self.render_custom(name, ctx);
        }
        match id {
            "character" => self.render_character(ctx),
            "directory" => self.render_directory(ctx),
            "git_branch" => self.render_git_branch(ctx),
            "git_status" => self.render_git_status(ctx),
            "git_state" => self.render_git_state(ctx),
            "cmd_duration" => self.render_cmd_duration(ctx),
            "exit_status" => self.render_exit_status(ctx),
            "jobs" => self.render_jobs(ctx),
            "time" => self.render_time(ctx),
            "username" => self.render_username(ctx),
            "hostname" => self.render_hostname(ctx),
            "reef" => self.render_reef(ctx),
            "principal" => self.render_principal(ctx),
            "leash" => self.render_leash(ctx),
            "battery" => self.render_battery(ctx),
            "indent" => String::new(),
            _ => String::new(),
        }
    }

    // -- styling helpers ----------------------------------------------------

    fn paint(&self, spec: &str, text: &str, ctx: &PromptContext) -> String {
        if text.is_empty() {
            return String::new();
        }
        let resolved = self.config.style.resolve(spec);
        let style = parse_style(resolved, &mut Vec::new());
        style.paint(text, ctx.no_color)
    }

    /// nerd-font symbol when available (§3.5), else the ascii fallback.
    fn use_nerd(&self, ctx: &PromptContext) -> bool {
        ctx.nerd_font && ctx.unicode
    }

    fn pick_symbol<'a>(&self, nerd: &'a str, ascii: &'a str, ctx: &PromptContext) -> &'a str {
        if self.use_nerd(ctx) { nerd } else { ascii }
    }

    fn ellipsis(&self, ctx: &PromptContext) -> &'static str {
        if ctx.unicode { "…" } else { "..." }
    }

    // -- modules ------------------------------------------------------------

    fn render_character(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.character;
        if !m.enabled {
            return String::new();
        }
        let ok = ctx.last_outcome.as_ref().map(|o| o.ok).unwrap_or(true);
        let vicmd = ctx.edit_mode == EditMode::ViNormal;
        let (symbol, style) = if vicmd {
            (m.vicmd_symbol.as_str(), m.vicmd_style.as_str())
        } else if ok {
            (m.success_symbol.as_str(), m.success_style.as_str())
        } else {
            (m.error_symbol.as_str(), m.error_style.as_str())
        };
        // Strict-ASCII fallback (§4.1): the chevron is hardcoded, not configured.
        let symbol = if ctx.unicode {
            symbol.to_string()
        } else if vicmd {
            "<".to_string()
        } else {
            ">".to_string()
        };
        self.paint(style, &symbol, ctx)
    }

    fn render_directory(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.directory;
        if !m.enabled {
            return String::new();
        }
        let mut display = if let (true, Some(git)) = (m.repo_relative, ctx.git.as_ref()) {
            let repo_name = git
                .repo_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel = git.repo_relative.to_string_lossy();
            if rel.is_empty() || rel == "." {
                repo_name
            } else {
                format!("{repo_name}/{rel}")
            }
        } else {
            collapse_home(ctx, &m.home_symbol)
        };
        if m.truncate_to > 0 {
            display = truncate_path(&display, m.truncate_to, self.ellipsis(ctx));
        }
        let symbol = &m.symbol;
        let ro = if ctx.read_only {
            m.read_only_symbol.as_str()
        } else {
            ""
        };
        let text = format!("{symbol}{display}{ro}");
        self.paint(&m.style, &text, ctx)
    }

    fn render_git_branch(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.git_branch;
        let Some(git) = &ctx.git else {
            return String::new();
        };
        if !m.enabled {
            return String::new();
        }
        let branch = git
            .branch
            .clone()
            .or_else(|| git.detached_at.clone())
            .unwrap_or_default();
        if branch.is_empty() {
            return String::new();
        }
        let branch = truncate_branch(&branch, m.truncate_to, &m.truncate_symbol);
        let symbol = self.pick_symbol(&m.symbol, &m.ascii_symbol, ctx);
        let text = m
            .format
            .replace("${symbol}", symbol)
            .replace("${branch}", &branch);
        self.paint(&m.style, &text, ctx)
    }

    fn render_git_status(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.git_status;
        let Some(git) = &ctx.git else {
            return String::new();
        };
        if !m.enabled {
            return String::new();
        }
        let mut parts = String::new();
        let push = |parts: &mut String, tmpl: &str, count: u32| {
            if count > 0 {
                parts.push_str(&tmpl.replace("${count}", &count.to_string()));
            }
        };
        push(&mut parts, &m.staged, git.staged);
        push(&mut parts, &m.unstaged, git.unstaged);
        push(&mut parts, &m.untracked, git.untracked);
        push(&mut parts, &m.conflicted, git.conflicted);
        push(&mut parts, &m.stashed, git.stashed);
        if git.ahead > 0 && git.behind > 0 {
            parts.push_str(
                &m.diverged
                    .replace("${ahead}", &git.ahead.to_string())
                    .replace("${behind}", &git.behind.to_string()),
            );
        } else {
            push(&mut parts, &m.ahead, git.ahead);
            push(&mut parts, &m.behind, git.behind);
        }
        if parts.is_empty() && !git.degraded {
            return String::new();
        }
        let mut out = self.paint(&m.style, &parts, ctx);
        if git.degraded && !m.stale_symbol.is_empty() {
            out.push_str(&self.paint("muted", &m.stale_symbol, ctx));
        }
        out
    }

    fn render_git_state(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.git_state;
        let Some(git) = &ctx.git else {
            return String::new();
        };
        if !m.enabled {
            return String::new();
        }
        let text = match git.state {
            RepoState::Clean => return String::new(),
            RepoState::Rebasing => &m.rebase,
            RepoState::Merging => &m.merge,
            RepoState::CherryPicking => &m.cherry_pick,
            RepoState::Bisecting => &m.bisect,
            RepoState::Reverting => &m.revert,
        };
        self.paint(&m.style, text, ctx)
    }

    fn render_cmd_duration(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.cmd_duration;
        if !m.enabled {
            return String::new();
        }
        let Some(o) = &ctx.last_outcome else {
            return String::new();
        };
        if (o.dur.as_millis() as u64) < m.min_ms {
            return String::new();
        }
        self.paint(&m.style, &crate::fmt::format_duration(o.dur), ctx)
    }

    fn render_exit_status(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.exit_status;
        if !m.enabled {
            return String::new();
        }
        let Some(o) = &ctx.last_outcome else {
            return String::new();
        };
        if o.ok && !m.show_on_success {
            return String::new();
        }
        let status = o
            .signal
            .clone()
            .unwrap_or_else(|| o.status.unwrap_or(0).to_string());
        let text = m.format.replace("${status}", &status);
        self.paint(&m.style, &text, ctx)
    }

    fn render_jobs(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.jobs;
        if !m.enabled || ctx.jobs.total < m.threshold {
            return String::new();
        }
        let text = m
            .format
            .replace("${symbol}", &m.symbol)
            .replace("${total}", &ctx.jobs.total.to_string())
            .replace("${running}", &ctx.jobs.running.to_string())
            .replace("${suspended}", &ctx.jobs.suspended.to_string());
        self.paint(&m.style, &text, ctx)
    }

    fn render_time(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.time;
        if !m.enabled {
            return String::new();
        }
        let (h, min, s) = ctx.time_local;
        let text = strftime_hms(&m.format, h, min, s);
        self.paint(&m.style, &text, ctx)
    }

    fn render_username(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.username;
        if !m.enabled {
            return String::new();
        }
        if !(m.show_always || ctx.session.is_ssh || ctx.session.is_root) {
            return String::new();
        }
        let style = if ctx.session.is_root {
            &m.root_style
        } else {
            &m.style
        };
        self.paint(style, &ctx.session.user, ctx)
    }

    fn render_hostname(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.hostname;
        if !m.enabled {
            return String::new();
        }
        if !(m.show_always || ctx.session.is_ssh) {
            return String::new();
        }
        let text = format!("{}{}", m.symbol, ctx.session.host);
        self.paint(&m.style, &text, ctx)
    }

    fn render_reef(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.reef;
        if !m.enabled {
            return String::new();
        }
        let tools: Vec<String> = ctx
            .reef
            .iter()
            .filter(|b| m.show_ambient || b.constrained)
            .map(|b| {
                let ver = b.version.as_deref().map(short_version).unwrap_or_default();
                format!("{}{ver}", b.tool)
            })
            .collect();
        if tools.is_empty() && !m.show_when_empty {
            return String::new();
        }
        let text = m.format.replace("${tools}", &tools.join(" "));
        self.paint(&m.style, &text, ctx)
    }

    fn render_principal(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.principal;
        if !m.enabled {
            return String::new();
        }
        match &ctx.principal {
            Principal::Human => self.paint(&m.style, &m.human_symbol, ctx),
            Principal::Agent(name) => {
                let text = if m.show_agent_name && !name.is_empty() {
                    format!("{} {name}", m.agent_symbol)
                } else {
                    m.agent_symbol.clone()
                };
                self.paint(&m.agent_style, &text, ctx)
            }
        }
    }

    fn render_leash(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.leash;
        if !m.enabled {
            return String::new();
        }
        if m.hide_when_enforced && ctx.leash.enforced {
            return String::new();
        }
        let tier = ctx.leash.tier.as_str();
        let symbol = m.symbol_by_tier.get(tier).cloned().unwrap_or_default();
        if symbol.is_empty() {
            return String::new();
        }
        let style = m.style_by_tier.get(tier).cloned().unwrap_or_default();
        self.paint(&style, &symbol, ctx)
    }

    fn render_battery(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.battery;
        if !m.enabled {
            return String::new();
        }
        let Some(b) = &ctx.battery else {
            return String::new();
        };
        let symbol = if b.charging {
            &m.charging_symbol
        } else {
            &m.discharging_symbol
        };
        let low = b.pct <= m.low_threshold;
        let style = if low { &m.low_style } else { &m.style };
        let text = format!("{symbol}{}%", b.pct);
        self.paint(style, &text, ctx)
    }

    fn render_language(&self, tool_id: &str, ctx: &PromptContext) -> String {
        let Some(m) = self.config.module.language.get(tool_id) else {
            return String::new();
        };
        if !m.enabled {
            return String::new();
        }
        let tool = if m.tool.is_empty() { tool_id } else { &m.tool };
        let Some(binding) = ctx.reef.iter().find(|b| b.tool == tool) else {
            return String::new();
        };
        let show = match m.when.as_str() {
            "constrained" => binding.constrained,
            // "resolved" and "probe" both surface any resolved binding on the
            // render path; the actual probe subprocess (probe mode) runs in a
            // background task and only ever populates `ctx.reef`, never here.
            _ => binding.version.is_some() || binding.constrained,
        };
        if !show {
            return String::new();
        }
        let version = binding.version.clone().unwrap_or_default();
        let symbol = self.pick_symbol(&m.symbol, &m.ascii_symbol, ctx);
        let text = m
            .format
            .replace("${symbol}", symbol)
            .replace("${version}", &version);
        self.paint(&m.style, &text, ctx)
    }

    fn render_custom(&self, name: &str, ctx: &PromptContext) -> String {
        let Some(m) = self.config.module.custom.get(name) else {
            return String::new();
        };
        if !m.enabled {
            return String::new();
        }
        let Some(seg) = ctx.custom.get(name) else {
            return String::new();
        };
        let output = match seg {
            CustomSegment::Ready(s) => s.clone(),
            CustomSegment::Stale(s, _) => s.clone(),
            CustomSegment::Pending | CustomSegment::Error(_) => return String::new(),
        };
        let text = m.format.replace("${output}", &output);
        self.paint(&m.style, &text, ctx)
    }
}

// -- free helpers -----------------------------------------------------------

/// Join rendered segments applying the whitespace-collapse rule (§3.4): a
/// whitespace-only literal immediately adjacent to a module that rendered empty
/// is dropped along with it. Non-whitespace literals are never dropped.
fn join_collapsing(segs: &[Seg]) -> String {
    let mut out = String::new();
    for (i, seg) in segs.iter().enumerate() {
        if seg.kind == Kind::Whitespace {
            let left_empty = i > 0 && segs[i - 1].kind == Kind::Module && segs[i - 1].empty;
            let right_empty =
                i + 1 < segs.len() && segs[i + 1].kind == Kind::Module && segs[i + 1].empty;
            if left_empty || right_empty {
                continue;
            }
        }
        out.push_str(&seg.text);
    }
    out
}

fn collapse_home(ctx: &PromptContext, home_symbol: &str) -> String {
    if let Some(home) = &ctx.home
        && let Ok(tail) = ctx.cwd.strip_prefix(home)
    {
        if tail.as_os_str().is_empty() {
            return home_symbol.to_string();
        }
        return format!("{home_symbol}/{}", tail.to_string_lossy());
    }
    ctx.cwd.to_string_lossy().into_owned()
}

/// Keep the last `n` path segments, prefixing an ellipsis when more existed.
fn truncate_path(path: &str, n: usize, ellipsis: &str) -> String {
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let leading_slash = path.starts_with('/');
    if comps.len() <= n {
        return path.to_string();
    }
    let kept = &comps[comps.len() - n..];
    // Truncated: the ellipsis stands in for the elided prefix. `leading_slash`
    // is intentionally dropped here — a truncated absolute path shows the
    // ellipsis, not the original root.
    let _ = leading_slash;
    format!("{ellipsis}/{}", kept.join("/"))
}

fn truncate_branch(branch: &str, n: usize, symbol: &str) -> String {
    if n == 0 {
        return branch.to_string();
    }
    let chars: Vec<char> = branch.chars().collect();
    if chars.len() <= n {
        return branch.to_string();
    }
    let head: String = chars[..n].iter().collect();
    format!("{head}{symbol}")
}

/// Drop the patch component of a semver-ish version (§4.12: "no patch").
fn short_version(v: &str) -> String {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() >= 3 {
        parts[..2].join(".")
    } else {
        v.to_string()
    }
}

/// A tiny strftime subset over `(hour, min, sec)` only (§4.9 — no date in v1).
fn strftime_hms(fmt: &str, h: u8, m: u8, s: u8) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('H') => out.push_str(&format!("{h:02}")),
                Some('M') => out.push_str(&format!("{m:02}")),
                Some('S') => out.push_str(&format!("{s:02}")),
                Some('%') => out.push('%'),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_path_keeps_last_n() {
        assert_eq!(truncate_path("~/a/b/c/d", 2, "…"), "…/c/d");
        assert_eq!(truncate_path("~/a", 3, "…"), "~/a");
    }

    #[test]
    fn short_version_drops_patch() {
        assert_eq!(short_version("3.12.1"), "3.12");
        assert_eq!(short_version("22"), "22");
    }

    #[test]
    fn strftime_subset() {
        assert_eq!(strftime_hms("%H:%M:%S", 9, 5, 30), "09:05:30");
        assert_eq!(strftime_hms("%H%%", 12, 0, 0), "12%");
    }
}
