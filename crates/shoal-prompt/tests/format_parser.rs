//! design §13: the format parser against a fixed corpus of every construct, and
//! the whitespace-collapse rule as observed through a real render.

use std::path::PathBuf;

use shoal_prompt::{
    FormatToken, GitSnapshot, PromptConfig, PromptContext, Renderer, Side, parse_format,
};

fn no_color_ctx() -> PromptContext {
    let mut ctx = PromptContext::empty(PathBuf::from("/home/dev/proj"));
    ctx.home = Some(PathBuf::from("/home/dev"));
    ctx.no_color = true;
    ctx.time_local = (1, 2, 3);
    ctx
}

#[test]
fn parses_every_construct() {
    // bare placeholder
    assert_eq!(
        parse_format("$time"),
        vec![FormatToken::Placeholder("time".into())]
    );
    // style group
    assert!(matches!(
        parse_format("[$time](red)").as_slice(),
        [FormatToken::Group { .. }]
    ));
    // nested style group
    let nested = parse_format("[a[$b](red)](green)");
    assert!(matches!(nested.as_slice(), [FormatToken::Group { .. }]));
    // unknown placeholder still parses (warning happens at config load)
    assert_eq!(
        parse_format("$definitely_not_a_module"),
        vec![FormatToken::Placeholder("definitely_not_a_module".into())]
    );
    // newline literal (splits out as its own token between two placeholders)
    assert!(
        parse_format("$a\n$b")
            .iter()
            .any(|t| matches!(t, FormatToken::Literal { text, .. } if text == "\n"))
    );
}

#[test]
fn whitespace_collapses_next_to_empty_module_but_keeps_real_literals() {
    // `$directory $git_branch` with no repo → the space after directory is
    // dropped because git_branch renders empty.
    let mut cfg = PromptConfig::default();
    cfg.format.left = "$directory $git_branch".into();
    let (r, _) = Renderer::new(cfg);
    let ctx = no_color_ctx(); // no git
    let out = r.render_side(Side::Left, &ctx);
    assert!(
        !out.ends_with(' '),
        "trailing space should collapse: {out:?}"
    );
    assert_eq!(out, "~/proj");
}

#[test]
fn non_whitespace_literal_is_never_dropped() {
    let mut cfg = PromptConfig::default();
    cfg.format.left = "$git_branch:$directory".into();
    let (r, _) = Renderer::new(cfg);
    let ctx = no_color_ctx(); // git_branch empty
    let out = r.render_side(Side::Left, &ctx);
    // The literal colon a user typed on purpose survives even though its left
    // neighbor rendered empty.
    assert_eq!(out, ":~/proj");
}

#[test]
fn multiple_empty_right_modules_collapse_cleanly() {
    let mut cfg = PromptConfig::default();
    cfg.format.right = "$cmd_duration $jobs $time".into();
    let (r, _) = Renderer::new(cfg);
    let mut ctx = no_color_ctx();
    ctx.time_local = (12, 0, 0);
    // no last_outcome (cmd_duration empty), no jobs → only time renders, no
    // leading double-space.
    let out = r.render_side(Side::Right, &ctx);
    assert_eq!(out, "12:00:00");
}

#[test]
fn repo_relative_directory_and_branch_render_together() {
    let mut cfg = PromptConfig::default();
    cfg.format.left = "$directory $git_branch".into();
    let (r, _) = Renderer::new(cfg);
    let mut ctx = no_color_ctx();
    ctx.git = Some(GitSnapshot::pending(
        PathBuf::from("/home/dev/proj"),
        PathBuf::from("src"),
        Some("main".into()),
    ));
    let out = r.render_side(Side::Left, &ctx);
    // directory shows "proj/src"; branch shows the ascii "git:main" (no nerd font)
    assert_eq!(out, "proj/src git:main");
}
