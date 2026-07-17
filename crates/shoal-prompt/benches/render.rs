//! Criterion bench for the common-path render (site/content/internals/prompt-editor-lsp.md). Run in CI on every
//! PR touching shoal-prompt; the p99 contract is < 5 ms (site/content/internals/prompt-editor-lsp.md) — on real hardware
//! this reads low tens of µs. A regression that reintroduces a syscall on the
//! render path would show up here before it shows up in a user's terminal.

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use shoal_prompt::{
    EditMode, GitSnapshot, OutcomeSnapshot, PromptConfig, PromptContext, ReefBinding, Renderer,
    RepoState, Side,
};

fn fixture() -> PromptContext {
    let mut ctx = PromptContext::empty(PathBuf::from("/home/dev/develop/shoal"));
    ctx.home = Some(PathBuf::from("/home/dev"));
    ctx.nerd_font = true;
    ctx.edit_mode = EditMode::Emacs;
    ctx.time_local = (14, 3, 9);
    ctx.last_outcome = Some(OutcomeSnapshot {
        ok: true,
        status: Some(0),
        signal: None,
        dur: Duration::from_millis(1250),
        cmd_head: "cargo".into(),
    });
    ctx.git = Some(GitSnapshot {
        repo_root: PathBuf::from("/home/dev/develop/shoal"),
        repo_relative: PathBuf::from("crates/shoal-prompt"),
        branch: Some("main".into()),
        detached_at: None,
        state: RepoState::Clean,
        ahead: 1,
        behind: 0,
        staged: 2,
        unstaged: 1,
        untracked: 3,
        conflicted: 0,
        stashed: 0,
        degraded: false,
        age: Duration::ZERO,
    });
    ctx.reef = vec![ReefBinding {
        tool: "rust".into(),
        version: Some("1.97.0".into()),
        provider: Some("rustup".into()),
        scope: Some("project".into()),
        constrained: true,
    }];
    ctx
}

fn bench_render(c: &mut Criterion) {
    let (renderer, _) = Renderer::new(PromptConfig::default());
    let ctx = fixture();
    c.bench_function("render_left", |b| {
        b.iter(|| black_box(renderer.render_side(Side::Left, black_box(&ctx))))
    });
    c.bench_function("render_full", |b| {
        b.iter(|| black_box(renderer.render(black_box(&ctx))))
    });
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
