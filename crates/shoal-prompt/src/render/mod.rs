//! The pure renderer: format-token walk (site/content/internals/prompt-editor-lsp.md) + per-module `match` (site/content/internals/prompt-editor-lsp.md) +
//! style application (site/content/internals/prompt-editor-lsp.md), with the deadline degrade of site/content/internals/prompt-editor-lsp.md. No I/O, no
//! logging, no side effects — a golden/snapshot suite drives it with hand-built
//! `PromptContext` values, no kernel required.

use std::time::{Duration, Instant};

use crate::config::{ParsedFormats, PromptConfig};
use crate::context::{EditMode, PromptContext};
use crate::format::FormatToken;
use crate::style::parse_style;

mod helpers;
mod modules;

/// The four independent strings reedline asks for (site/content/internals/prompt-editor-lsp.md). `indicator` is always
/// empty: shoal-prompt owns the entire visual symbol via `$character`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedPrompt {
    pub left: String,
    pub right: String,
    pub continuation: String,
    pub indicator: String,
}

/// Timing result from rendering every interactive prompt side once.
///
/// Hosts can sample this between commands and surface a bounded warning without
/// putting logging or other side effects in the per-keystroke render path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderBudgetReport {
    pub slowest: Duration,
    pub over_budget: bool,
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
    /// unknown-module / style warnings (site/content/internals/prompt-editor-lsp.md).
    pub fn new(config: PromptConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        if !matches!(
            config.module.directory.truncate_style.as_str(),
            "start" | "middle" | "end"
        ) {
            warnings.push(format!(
                "invalid prompt.module.directory.truncate_style `{}`; using `middle`",
                config.module.directory.truncate_style
            ));
        }
        for (name, module) in &config.module.language {
            if !matches!(module.when.as_str(), "constrained" | "resolved") {
                warnings.push(format!(
                    "invalid prompt.module.language.{name}.when `{}`; using `constrained`",
                    module.when
                ));
            }
        }
        let mut config = config;
        if !matches!(
            config.module.directory.truncate_style.as_str(),
            "start" | "middle" | "end"
        ) {
            config.module.directory.truncate_style = "middle".into();
        }
        for module in config.module.language.values_mut() {
            if !matches!(module.when.as_str(), "constrained" | "resolved") {
                module.when = "constrained".into();
            }
        }
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

    /// Render each interactive side once and report the slowest call.
    pub fn budget_report(&self, ctx: &PromptContext) -> RenderBudgetReport {
        let mut slowest = Duration::ZERO;
        for side in [Side::Left, Side::Right, Side::Continuation] {
            let start = Instant::now();
            let _ = self.render_side(side, ctx);
            slowest = slowest.max(start.elapsed());
        }
        RenderBudgetReport {
            slowest,
            over_budget: slowest > Duration::from_millis(self.config.budget.render_deadline_ms),
        }
    }

    /// Render a single side, honoring the site/content/internals/prompt-editor-lsp.md deadline: once elapsed exceeds the
    /// budget, every remaining module renders its cheapest fallback (empty).
    pub fn render_side(&self, side: Side, ctx: &PromptContext) -> String {
        self.render_side_with_edit_mode(side, ctx, ctx.edit_mode)
    }

    /// Render a side with editor state supplied by the live line-editor
    /// adapter. All other values still come from the immutable context.
    pub fn render_side_with_edit_mode(
        &self,
        side: Side,
        ctx: &PromptContext,
        edit_mode: EditMode,
    ) -> String {
        let tokens = match side {
            Side::Left => &self.formats.left,
            Side::Right => &self.formats.right,
            Side::Continuation => &self.formats.continuation,
            Side::Transient => &self.formats.transient,
        };
        let start = Instant::now();
        let deadline = Duration::from_millis(self.config.budget.render_deadline_ms);
        let mut segs = Vec::with_capacity(tokens.len());
        self.render_tokens(tokens, ctx, edit_mode, start, deadline, &mut segs);
        join_collapsing(&segs)
    }

    fn render_tokens(
        &self,
        tokens: &[FormatToken],
        ctx: &PromptContext,
        edit_mode: EditMode,
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
                        self.render_placeholder_with_edit_mode(id, ctx, edit_mode)
                    };
                    out.push(Seg {
                        empty: text.is_empty(),
                        text,
                        kind: Kind::Module,
                    });
                }
                FormatToken::Group { inner, style } => {
                    let mut inner_segs = Vec::new();
                    self.render_tokens(inner, ctx, edit_mode, start, deadline, &mut inner_segs);
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
        self.render_placeholder_with_edit_mode(id, ctx, ctx.edit_mode)
    }

    fn render_placeholder_with_edit_mode(
        &self,
        id: &str,
        ctx: &PromptContext,
        edit_mode: EditMode,
    ) -> String {
        if let Some(tool) = id.strip_prefix("language_") {
            return self.render_language(tool, ctx);
        }
        if let Some(name) = id.strip_prefix("custom_") {
            return self.render_custom(name, ctx);
        }
        match id {
            "character" => self.render_character(ctx, edit_mode),
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

    /// nerd-font symbol when available (site/content/internals/prompt-editor-lsp.md), else the ascii fallback.
    fn use_nerd(&self, ctx: &PromptContext) -> bool {
        ctx.nerd_font && ctx.unicode
    }

    fn pick_symbol<'a>(&self, nerd: &'a str, ascii: &'a str, ctx: &PromptContext) -> &'a str {
        if self.use_nerd(ctx) { nerd } else { ascii }
    }

    fn ellipsis(&self, ctx: &PromptContext) -> &'static str {
        if ctx.unicode { "…" } else { "..." }
    }
}

/// Join rendered segments applying the whitespace-collapse rule (site/content/internals/prompt-editor-lsp.md): a
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
