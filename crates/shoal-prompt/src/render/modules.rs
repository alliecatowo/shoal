//! Per-placeholder rendering (§4): one method per `$module`, each reading
//! its config table and the relevant [`PromptContext`] field(s) and
//! returning a styled (or empty, when hidden/disabled) string.

use crate::context::{CustomSegment, EditMode, Principal, PromptContext, RepoState};

use super::Renderer;
use super::helpers::{collapse_home, short_version, strftime_hms, truncate_branch, truncate_path};

impl Renderer {
    pub(super) fn render_character(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_directory(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_git_branch(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_git_status(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_git_state(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_cmd_duration(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_exit_status(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_jobs(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_time(&self, ctx: &PromptContext) -> String {
        let m = &self.config.module.time;
        if !m.enabled {
            return String::new();
        }
        let (h, min, s) = ctx.time_local;
        let text = strftime_hms(&m.format, h, min, s);
        self.paint(&m.style, &text, ctx)
    }

    pub(super) fn render_username(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_hostname(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_reef(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_principal(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_leash(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_battery(&self, ctx: &PromptContext) -> String {
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

    pub(super) fn render_language(&self, tool_id: &str, ctx: &PromptContext) -> String {
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

    pub(super) fn render_custom(&self, name: &str, ctx: &PromptContext) -> String {
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
