//! The speed assertion (site/content/internals/prompt-editor-lsp.md bench gate expressed as a test): the
//! common-path render must be sub-millisecond. We render a realistic fixture
//! many times and assert the *median* stays comfortably inside the p99 budget.
//! This is the tripwire that catches a future change accidentally reintroducing
//! a syscall/subprocess on the render path — it would blow the budget here.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use shoal_prompt::{
    GitSnapshot, OutcomeSnapshot, PromptConfig, PromptContext, ReefBinding, Renderer, RepoState,
};

fn fixture() -> PromptContext {
    let mut ctx = PromptContext::empty(PathBuf::from("/home/dev/develop/shoal"));
    ctx.home = Some(PathBuf::from("/home/dev"));
    ctx.nerd_font = true;
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

#[test]
fn common_path_render_is_sub_millisecond() {
    let (renderer, _) = Renderer::new(PromptConfig::default());
    let ctx = fixture();

    // Warm up (first parse/alloc paths).
    for _ in 0..100 {
        let _ = renderer.render(&ctx);
    }

    let n = 5000;
    let mut samples: Vec<u128> = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        let out = renderer.render(&ctx);
        samples.push(start.elapsed().as_nanos());
        assert!(!out.left.is_empty());
    }
    samples.sort_unstable();
    let median = samples[n / 2];
    let p99 = samples[(n * 99) / 100];

    // Hard contract: p99 well under the 5ms deadline. Median should be tens of
    // µs; we assert a very loose 1ms to stay robust on a loaded CI box while
    // still catching an accidental syscall (which is ~microseconds each, but a
    // subprocess is milliseconds — this test would catch that immediately).
    assert!(
        median < 1_000_000,
        "median render {median}ns exceeded 1ms — a syscall likely crept onto the render path"
    );
    assert!(
        p99 < 5_000_000,
        "p99 render {p99}ns exceeded the 5ms hard deadline"
    );
}

#[test]
fn budget_report_observes_an_impossible_deadline() {
    let mut config = PromptConfig::default();
    config.budget.render_deadline_ms = 0;
    let (renderer, warnings) = Renderer::new(config);
    assert!(warnings.is_empty());

    let report = renderer.budget_report(&PromptContext::empty("/".into()));
    assert!(report.over_budget);
    assert!(report.slowest > Duration::ZERO);
}
