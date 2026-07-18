//! `shoal prompt` introspection, printing, and benchmark CLI.

use super::*;

// ---------------------------------------------------------------------------
// `shoal prompt` dev/introspection surface (site/content/internals/prompt-editor-lsp.md)
// ---------------------------------------------------------------------------

/// `shoal prompt <explain|bench|print>` (site/content/internals/prompt-editor-lsp.md).
#[derive(Debug, Clone)]
pub enum PromptAction {
    Explain { side: Side },
    Bench { n: usize, side: Side },
    Print { side: Side },
}

/// Parse `prompt` subcommand arguments into a [`PromptAction`].
pub fn parse_action(mut args: impl Iterator<Item = String>) -> Result<PromptAction, String> {
    let sub = args.next().unwrap_or_else(|| "explain".into());
    let mut side = Side::Left;
    let mut n = 10_000usize;
    let mut rest = args.peekable();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--side" => {
                let v = rest.next().ok_or("--side requires a value")?;
                side = parse_side(&v)?;
            }
            "--n" => {
                let v = rest.next().ok_or("--n requires a value")?;
                n = v.parse().map_err(|_| "--n expects a number".to_string())?;
            }
            other => return Err(format!("unknown prompt argument `{other}`")),
        }
    }
    match sub.as_str() {
        "explain" => Ok(PromptAction::Explain { side }),
        "bench" => Ok(PromptAction::Bench { n, side }),
        "print" => Ok(PromptAction::Print { side }),
        other => Err(format!(
            "unknown prompt subcommand `{other}`; expected explain, bench, or print"
        )),
    }
}

fn parse_side(s: &str) -> Result<Side, String> {
    match s {
        "left" => Ok(Side::Left),
        "right" => Ok(Side::Right),
        "continuation" => Ok(Side::Continuation),
        "transient" => Ok(Side::Transient),
        _ => Err(format!(
            "unknown side `{s}`; expected left, right, continuation, or transient"
        )),
    }
}

fn side_format(cfg: &PromptConfig, side: Side) -> &str {
    match side {
        Side::Left => &cfg.format.left,
        Side::Right => &cfg.format.right,
        Side::Continuation => &cfg.format.continuation,
        Side::Transient => &cfg.format.transient,
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Continuation => "continuation",
        Side::Transient => "transient",
    }
}

/// Run a `shoal prompt` subcommand.
pub fn run(action: PromptAction) -> Result<i32, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine cwd: {e}"))?;
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let (config, warnings) = load_prompt_config(&cwd);
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    let facts = StaticFacts::resolve(&config, no_color);
    let deadline_ms = config.budget.render_deadline_ms;
    let (renderer, more_warnings) = Renderer::new(config);
    for w in &more_warnings {
        eprintln!("warning: {w}");
    }

    let mut ev = Evaluator::new(cwd.clone());
    let mut live = build_context(&mut ev, &facts, 80);
    if !matches!(&action, PromptAction::Bench { .. }) {
        let mut custom_warnings = Vec::new();
        let mut custom = CustomScheduler::new(renderer.config(), &mut custom_warnings);
        for warning in custom_warnings {
            eprintln!("warning: {warning}");
        }
        live.custom = custom.refresh_until(&cwd, ev.env_vars(), CUSTOM_ONE_SHOT_WAIT);
    }

    match action {
        PromptAction::Print { side } => {
            print!("{}", renderer.render_side(side, &live));
            Ok(0)
        }
        PromptAction::Explain { side } => {
            let src = side_format(renderer.config(), side).to_string();
            let tokens = shoal_prompt::parse_format(&src);
            println!("prompt {} — {}", side_name(side), src);
            let mut total = Duration::ZERO;
            for tok in &tokens {
                if let shoal_prompt::FormatToken::Placeholder(id) = tok {
                    let start = std::time::Instant::now();
                    let rendered = renderer.render_placeholder(id, &live);
                    let elapsed = start.elapsed();
                    total += elapsed;
                    let plain = crate::strip_ansi(&rendered);
                    println!("  {id:<16} ‹{plain}›  {}µs", elapsed.as_micros());
                }
            }
            let ok = total < Duration::from_millis(deadline_ms);
            println!(
                "  total: {}µs (budget: {}ms) — {}",
                total.as_micros(),
                deadline_ms,
                if ok { "OK" } else { "OVER" }
            );
            Ok(0)
        }
        PromptAction::Bench { n, side } => {
            let ctx = bench_fixture(&facts);
            // warm up
            for _ in 0..100 {
                let _ = renderer.render_side(side, &ctx);
            }
            let mut samples: Vec<u128> = Vec::with_capacity(n);
            for _ in 0..n {
                let start = std::time::Instant::now();
                let out = renderer.render_side(side, &ctx);
                samples.push(start.elapsed().as_nanos());
                std::hint::black_box(out);
            }
            samples.sort_unstable();
            let p = |q: usize| samples[(n.saturating_sub(1) * q) / 100];
            let p50 = p(50);
            let p99 = p(99);
            let max = *samples.last().unwrap_or(&0);
            println!(
                "prompt {} bench (n={n}): p50={:.1}µs p99={:.1}µs max={:.1}µs (budget {}ms)",
                side_name(side),
                p50 as f64 / 1000.0,
                p99 as f64 / 1000.0,
                max as f64 / 1000.0,
                deadline_ms
            );
            // CI regression gate (site/content/internals/prompt-editor-lsp.md): exit 1 if p99 exceeds the deadline.
            if p99 > (deadline_ms as u128) * 1_000_000 {
                eprintln!("error: p99 exceeded the render deadline");
                return Ok(1);
            }
            Ok(0)
        }
    }
}

/// A fixed, reproducible fixture for `shoal prompt bench` (site/content/internals/prompt-editor-lsp.md) — not live state.
fn bench_fixture(facts: &StaticFacts) -> PromptContext {
    let mut ctx = PromptContext::empty(PathBuf::from("/home/dev/develop/shoal"));
    ctx.home = facts.home.clone();
    ctx.no_color = facts.no_color;
    ctx.nerd_font = facts.nerd_font;
    ctx.unicode = facts.unicode;
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
    ctx
}
