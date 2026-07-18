//! Criterion bench for the common-path render (site/content/internals/prompt-editor-lsp.md).
//!
//! What this measures: `Renderer::render_side`/`render` over a fixed, hand-built `PromptContext`
//! fixture with no IO — pure formatting/lookup cost. On typical development hardware this reads
//! low tens of microseconds.
//!
//! What this does NOT measure or enforce: this bench is not run in CI (see
//! site/content/internals/tooling-and-quality.md's "Performance review gates" section for what CI
//! actually runs) and there is no automated p99 budget gate wired to it. `cargo bench -p
//! shoal-prompt --bench render` is a manual/local review tool for a human comparing before/after a
//! change to the render path; treat any number here as a local data point, not a release
//! guarantee, until a dedicated CI job with a pinned baseline exists. The pure-render regression
//! test that IS enforced in CI lives at `crates/shoal-prompt/tests/speed.rs`.

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
