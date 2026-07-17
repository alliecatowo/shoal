//! Terminal rendering and pager policy for the interactive REPL.

use std::io::{self, IsTerminal, Write as _};

use shoal_value::Value;

use crate::kernel_repl::ProtocolOutcome;
use crate::maybe_strip;

/// `true` when an outcome already reached the terminal through PtyTee.
pub(super) fn already_streamed(value: &Value, pty_was_live: bool) -> bool {
    pty_was_live && matches!(value, Value::Outcome(o) if o.streamed)
}

pub(crate) fn render_result(value: &Value, pty_was_live: bool) -> io::Result<()> {
    if already_streamed(value, pty_was_live) {
        return Ok(());
    }
    print_value(value)
}

/// Render one value through the shared colorized block renderer.
pub(crate) fn print_value(value: &Value) -> io::Result<()> {
    let rendered = shoal_value::render::render_block(value, terminal_width());
    if !rendered.is_empty() {
        println!("{}", maybe_strip(rendered));
    }
    Ok(())
}

pub(super) fn report_protocol_error(error: &str) {
    eprintln!(
        "{}",
        maybe_strip(format!("\x1b[31;1merror:\x1b[0m {error}"))
    );
}

pub(super) fn render_protocol_outcome(
    outcome: &ProtocolOutcome,
    pager: &PagerContext,
) -> io::Result<()> {
    if outcome.state == "cancelled" {
        println!("{}", maybe_strip("\x1b[90m^C\x1b[0m".to_string()));
        return Ok(());
    }
    let Some(rendered) = protocol_render_text(outcome) else {
        return Ok(());
    };
    render_text_paged(rendered, pager)
}

pub(super) fn protocol_render_text(outcome: &ProtocolOutcome) -> Option<&str> {
    (outcome.state != "cancelled" && !outcome.streamed)
        .then_some(outcome.render.as_deref())
        .flatten()
        .filter(|render| !render.is_empty())
}

fn render_text_paged(rendered: &str, pager: &PagerContext) -> io::Result<()> {
    let width = terminal_width();
    let is_tty = io::stdout().is_terminal();
    let line_count = wrapped_line_count(rendered, width);
    if should_page(pager.enabled, is_tty, line_count, terminal_height()) {
        let env_pager = std::env::var("PAGER").ok();
        let argv = pager_command(pager.pager.as_deref(), env_pager.as_deref());
        if spawn_pager(&argv, &maybe_strip(rendered.to_string())) {
            return Ok(());
        }
    }
    println!("{}", maybe_strip(rendered.to_string()));
    Ok(())
}

/// Resolved pager state for one interactive session.
pub(crate) struct PagerContext {
    pub(crate) enabled: bool,
    pub(crate) pager: Option<String>,
}

/// Render a final interactive result, paging only when configured and useful.
pub(crate) fn render_result_paged(
    value: &Value,
    pty_was_live: bool,
    pager: &PagerContext,
) -> io::Result<()> {
    if already_streamed(value, pty_was_live) {
        return Ok(());
    }
    let width = terminal_width();
    let rendered = shoal_value::render::render_block(value, width);
    if rendered.is_empty() {
        return Ok(());
    }
    render_text_paged(&rendered, pager)
}

pub(super) fn should_page(
    enabled: bool,
    is_tty: bool,
    rendered_line_count: usize,
    term_height: usize,
) -> bool {
    enabled && is_tty && term_height > 0 && rendered_line_count > term_height
}

pub(super) fn pager_command(config_pager: Option<&str>, env_pager: Option<&str>) -> Vec<String> {
    let chosen = config_pager
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| env_pager.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or("less -R");
    chosen.split_whitespace().map(str::to_string).collect()
}

/// Pipe text through a pager. `false` means the child could not be launched.
pub(super) fn spawn_pager(argv: &[String], text: &str) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
    true
}

/// Visual row count after terminal wrapping; ANSI CSI sequences have no width.
pub(super) fn wrapped_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut total = 0usize;
    for line in text.split('\n') {
        let mut cols = 0usize;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        break;
                    }
                }
                continue;
            }
            cols += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
        total += if cols == 0 { 1 } else { cols.div_ceil(width) };
    }
    total.max(1)
}

pub(super) fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(80)
}

fn terminal_height() -> usize {
    crossterm::terminal::size()
        .map(|(_, height)| usize::from(height))
        .unwrap_or(24)
}
