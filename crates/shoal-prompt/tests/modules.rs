//! One test per prompt module, each building a minimal `PromptContext`. See
//! `site/content/internals/prompt-editor-lsp.md`.
//! fixture and asserting the exact rendered string for default config, a changed
//! config key, and the hidden condition. Rendered with `no_color = true` so the
//! assertions are on plain text, not SGR sequences (styling is covered in
//! style.rs unit tests).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use shoal_prompt::{
    BatterySnapshot, CustomSegment, EditMode, GitSnapshot, JobsSnapshot, LeashSnapshot, LeashTier,
    OutcomeSnapshot, Principal, PromptConfig, PromptContext, ReefBinding, Renderer, RepoState,
    RepoState as RS,
};

fn base_ctx() -> PromptContext {
    let mut ctx = PromptContext::empty(PathBuf::from("/home/dev/develop/shoal"));
    ctx.home = Some(PathBuf::from("/home/dev"));
    ctx.no_color = true;
    ctx.unicode = true;
    ctx.nerd_font = false; // ascii symbols → clean assertions
    ctx.time_local = (14, 3, 9);
    ctx
}

fn git(branch: &str) -> GitSnapshot {
    GitSnapshot {
        repo_root: PathBuf::from("/home/dev/develop/shoal"),
        repo_relative: PathBuf::from("crates"),
        branch: Some(branch.into()),
        detached_at: None,
        state: RepoState::Clean,
        ahead: 0,
        behind: 0,
        staged: 0,
        unstaged: 0,
        untracked: 0,
        conflicted: 0,
        stashed: 0,
        degraded: false,
        age: Duration::ZERO,
    }
}

fn render(cfg: PromptConfig, id: &str, ctx: &PromptContext) -> String {
    let (r, _) = Renderer::new(cfg);
    r.render_placeholder(id, ctx)
}

// -- character --------------------------------------------------------------

#[test]
fn character_success_error_vicmd() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    // no outcome yet → treated as success
    assert_eq!(render(cfg.clone(), "character", &ctx), "❯");
    ctx.last_outcome = Some(OutcomeSnapshot {
        ok: false,
        status: Some(1),
        signal: None,
        dur: Duration::ZERO,
        cmd_head: "x".into(),
    });
    assert_eq!(render(cfg.clone(), "character", &ctx), "❯"); // default error_symbol
    ctx.edit_mode = EditMode::ViNormal;
    assert_eq!(render(cfg, "character", &ctx), "❮");
}

#[test]
fn character_live_edit_mode_override_covers_vi_visual() {
    let mut config = PromptConfig::default();
    config.format.left = "$character".into();
    let (renderer, _) = Renderer::new(config);
    let ctx = base_ctx();
    assert_eq!(
        renderer.render_side_with_edit_mode(shoal_prompt::Side::Left, &ctx, EditMode::ViVisual),
        "❮"
    );
}

#[test]
fn character_ascii_fallback_when_unicode_off() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.unicode = false;
    assert_eq!(render(cfg.clone(), "character", &ctx), ">");
    ctx.edit_mode = EditMode::ViNormal;
    assert_eq!(render(cfg, "character", &ctx), "<");
}

// -- directory --------------------------------------------------------------

#[test]
fn directory_home_collapse_and_truncate() {
    let mut cfg = PromptConfig::default();
    cfg.module.directory.repo_relative = false;
    let ctx = base_ctx();
    // ~/develop/shoal with truncate_to = 3 keeps all three segments
    assert_eq!(render(cfg.clone(), "directory", &ctx), "~/develop/shoal");
    cfg.module.directory.truncate_to = 1;
    assert_eq!(render(cfg, "directory", &ctx), "…/shoal");
}

#[test]
fn directory_truncate_style_selects_the_retained_edges() {
    let mut cfg = PromptConfig::default();
    cfg.module.directory.repo_relative = false;
    cfg.module.directory.truncate_to = 2;
    cfg.module.directory.truncate_style = "end".into();
    assert_eq!(render(cfg.clone(), "directory", &base_ctx()), "~/develop/…");
    cfg.module.directory.truncate_style = "middle".into();
    assert_eq!(render(cfg, "directory", &base_ctx()), "~/…/shoal");
}

#[test]
fn invalid_truncate_style_warns_and_uses_middle() {
    let mut cfg = PromptConfig::default();
    cfg.module.directory.repo_relative = false;
    cfg.module.directory.truncate_to = 2;
    cfg.module.directory.truncate_style = "mystery".into();
    let (renderer, warnings) = Renderer::new(cfg);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("truncate_style"))
    );
    assert_eq!(
        renderer.render_placeholder("directory", &base_ctx()),
        "~/…/shoal"
    );
}

#[test]
fn directory_repo_relative() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.git = Some(git("main"));
    assert_eq!(render(cfg, "directory", &ctx), "shoal/crates");
}

#[test]
fn directory_read_only_marker() {
    let mut cfg = PromptConfig::default();
    cfg.module.directory.repo_relative = false;
    cfg.module.directory.read_only_symbol = " RO".into();
    let mut ctx = base_ctx();
    ctx.read_only = true;
    assert!(render(cfg, "directory", &ctx).ends_with(" RO"));
}

// -- git_branch -------------------------------------------------------------

#[test]
fn git_branch_hidden_without_repo() {
    let ctx = base_ctx();
    assert_eq!(render(PromptConfig::default(), "git_branch", &ctx), "");
}

#[test]
fn git_branch_ascii_symbol_and_truncation() {
    let mut cfg = PromptConfig::default();
    cfg.module.git_branch.truncate_to = 4;
    let mut ctx = base_ctx();
    ctx.git = Some(git("feature-xyz"));
    assert_eq!(render(cfg, "git_branch", &ctx), "git:feat…");
}

#[test]
fn git_branch_detached_shows_sha() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    let mut g = git("main");
    g.branch = None;
    g.detached_at = Some("a1b2c3d".into());
    ctx.git = Some(g);
    assert_eq!(render(cfg, "git_branch", &ctx), "git:a1b2c3d");
}

// -- git_status -------------------------------------------------------------

#[test]
fn git_status_counts_and_order() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    let mut g = git("main");
    g.staged = 2;
    g.unstaged = 1;
    g.untracked = 3;
    g.ahead = 4;
    ctx.git = Some(g);
    // order: staged, unstaged, untracked, conflicted, stashed, then ahead/behind
    assert_eq!(render(cfg, "git_status", &ctx), "+2!1?3⇡4");
}

#[test]
fn git_status_diverged() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    let mut g = git("main");
    g.ahead = 2;
    g.behind = 3;
    ctx.git = Some(g);
    assert_eq!(render(cfg, "git_status", &ctx), "⇕⇡2⇣3");
}

#[test]
fn git_status_clean_is_hidden_but_degraded_shows_stale() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.git = Some(git("main"));
    assert_eq!(render(cfg.clone(), "git_status", &ctx), "");
    let mut g = git("main");
    g.degraded = true;
    ctx.git = Some(g);
    assert_eq!(render(cfg, "git_status", &ctx), "…");
}

// -- git_state --------------------------------------------------------------

#[test]
fn git_state_maps_operations() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    let mut g = git("main");
    g.state = RS::Merging;
    ctx.git = Some(g);
    assert_eq!(render(cfg.clone(), "git_state", &ctx), "MERGING");
    let mut g = git("main");
    g.state = RS::Clean;
    ctx.git = Some(g);
    assert_eq!(render(cfg, "git_state", &ctx), "");
}

// -- cmd_duration -----------------------------------------------------------

#[test]
fn cmd_duration_respects_min_ms() {
    let cfg = PromptConfig::default(); // min_ms = 500
    let mut ctx = base_ctx();
    ctx.last_outcome = Some(OutcomeSnapshot {
        ok: true,
        status: Some(0),
        signal: None,
        dur: Duration::from_millis(200),
        cmd_head: "x".into(),
    });
    assert_eq!(render(cfg.clone(), "cmd_duration", &ctx), "");
    ctx.last_outcome.as_mut().unwrap().dur = Duration::from_millis(1500);
    assert_eq!(render(cfg, "cmd_duration", &ctx), "1s500ms");
}

// -- exit_status ------------------------------------------------------------

#[test]
fn exit_status_off_by_default_and_shows_code_when_enabled() {
    let mut cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.last_outcome = Some(OutcomeSnapshot {
        ok: false,
        status: Some(127),
        signal: None,
        dur: Duration::ZERO,
        cmd_head: "x".into(),
    });
    // default disabled
    assert_eq!(render(cfg.clone(), "exit_status", &ctx), "");
    cfg.module.exit_status.enabled = true;
    assert_eq!(render(cfg.clone(), "exit_status", &ctx), "[127]");
    // signal death shows the signal name, never 128+n
    ctx.last_outcome.as_mut().unwrap().signal = Some("SIGSEGV".into());
    assert_eq!(render(cfg, "exit_status", &ctx), "[SIGSEGV]");
}

// -- jobs -------------------------------------------------------------------

#[test]
fn jobs_threshold_and_format() {
    let cfg = PromptConfig::default(); // threshold 1, symbol ✦
    let mut ctx = base_ctx();
    ctx.jobs = JobsSnapshot {
        running: 0,
        suspended: 0,
        total: 0,
        completed: 0,
    };
    assert_eq!(render(cfg.clone(), "jobs", &ctx), "");
    ctx.jobs.total = 2;
    assert_eq!(render(cfg, "jobs", &ctx), "✦2");
}

#[test]
fn jobs_format_can_distinguish_active_work_from_completed_history() {
    let mut cfg = PromptConfig::default();
    cfg.module.jobs.format = "${running}/${suspended} (+${completed})".into();
    let mut ctx = base_ctx();
    ctx.jobs = JobsSnapshot {
        running: 2,
        suspended: 1,
        total: 3,
        completed: 8,
    };
    assert_eq!(render(cfg, "jobs", &ctx), "2/1 (+8)");
}

#[test]
fn completed_history_alone_does_not_keep_the_jobs_module_visible() {
    let mut cfg = PromptConfig::default();
    cfg.module.jobs.format = "${total} (+${completed})".into();
    let mut ctx = base_ctx();
    ctx.jobs.completed = 8;
    assert_eq!(render(cfg, "jobs", &ctx), "");
}

// -- time -------------------------------------------------------------------

#[test]
fn time_strftime() {
    let cfg = PromptConfig::default();
    let ctx = base_ctx(); // (14, 3, 9)
    assert_eq!(render(cfg, "time", &ctx), "14:03:09");
}

// -- username / hostname ----------------------------------------------------

#[test]
fn username_only_when_ssh_or_root_or_always() {
    let mut cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.session.user = "dev".into();
    assert_eq!(render(cfg.clone(), "username", &ctx), "");
    ctx.session.is_root = true;
    assert_eq!(render(cfg.clone(), "username", &ctx), "dev");
    ctx.session.is_root = false;
    cfg.module.username.show_always = true;
    assert_eq!(render(cfg, "username", &ctx), "dev");
}

#[test]
fn hostname_only_when_ssh() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.session.host = "server".into();
    assert_eq!(render(cfg.clone(), "hostname", &ctx), "");
    ctx.session.is_ssh = true;
    assert_eq!(render(cfg, "hostname", &ctx), "@server");
}

// -- reef -------------------------------------------------------------------

#[test]
fn reef_lists_constrained_tools() {
    let cfg = PromptConfig::default();
    let mut ctx = base_ctx();
    ctx.reef = vec![
        ReefBinding {
            tool: "node".into(),
            version: Some("22.1.0".into()),
            provider: None,
            scope: None,
            constrained: true,
        },
        ReefBinding {
            tool: "python".into(),
            version: Some("3.12.1".into()),
            provider: None,
            scope: None,
            constrained: false, // ambient, hidden by default
        },
    ];
    assert_eq!(render(cfg, "reef", &ctx), "reef:node22.1");
}

#[test]
fn reef_hidden_when_empty() {
    let cfg = PromptConfig::default();
    let ctx = base_ctx();
    assert_eq!(render(cfg, "reef", &ctx), "");
}

// -- language ---------------------------------------------------------------

#[test]
fn language_module_reads_reef_binding() {
    // default theme is not loaded here, so add a language module explicitly
    let mut cfg = PromptConfig::default();
    cfg.module.language.insert(
        "rust".into(),
        shoal_prompt::LanguageModule {
            tool: "rust".into(),
            ascii_symbol: "rs:".into(),
            ..Default::default()
        },
    );
    let mut ctx = base_ctx();
    ctx.reef = vec![ReefBinding {
        tool: "rust".into(),
        version: Some("1.97.0".into()),
        provider: None,
        scope: None,
        constrained: true,
    }];
    assert_eq!(render(cfg.clone(), "language_rust", &ctx), "rs:1.97.0");
    // hidden when the tool isn't constrained (default `when`)
    ctx.reef[0].constrained = false;
    assert_eq!(render(cfg, "language_rust", &ctx), "");
}

#[test]
fn language_resolved_visibility_does_not_require_a_constraint() {
    let mut cfg = PromptConfig::default();
    cfg.module.language.insert(
        "rust".into(),
        shoal_prompt::LanguageModule {
            tool: "rust".into(),
            when: "resolved".into(),
            ..Default::default()
        },
    );
    let mut ctx = base_ctx();
    ctx.reef = vec![ReefBinding {
        tool: "rust".into(),
        version: Some("1.97.0".into()),
        provider: None,
        scope: None,
        constrained: false,
    }];
    assert_eq!(render(cfg, "language_rust", &ctx), "1.97.0");
}

#[test]
fn invalid_language_visibility_warns_and_fails_back_to_constrained() {
    let mut cfg = PromptConfig::default();
    cfg.module.language.insert(
        "rust".into(),
        shoal_prompt::LanguageModule {
            tool: "rust".into(),
            when: "probe".into(),
            ..Default::default()
        },
    );
    let (renderer, warnings) = Renderer::new(cfg);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("language.rust.when"))
    );
    let mut ctx = base_ctx();
    ctx.reef = vec![ReefBinding {
        tool: "rust".into(),
        version: Some("1.97.0".into()),
        provider: None,
        scope: None,
        constrained: false,
    }];
    assert_eq!(renderer.render_placeholder("language_rust", &ctx), "");
}

// -- principal --------------------------------------------------------------

#[test]
fn principal_human_empty_agent_named() {
    let mut cfg = PromptConfig::default();
    cfg.module.principal.enabled = true;
    let mut ctx = base_ctx();
    ctx.principal = Principal::Human;
    assert_eq!(render(cfg.clone(), "principal", &ctx), "");
    ctx.principal = Principal::Agent("claude".into());
    assert_eq!(render(cfg, "principal", &ctx), "🤖 claude");
}

// -- leash ------------------------------------------------------------------

#[test]
fn leash_badge_by_tier() {
    let mut cfg = PromptConfig::default();
    cfg.module.leash.enabled = true;
    let mut ctx = base_ctx();
    ctx.leash = LeashSnapshot {
        tier: LeashTier::D,
        enforced: false,
    };
    assert_eq!(render(cfg.clone(), "leash", &ctx), "⚠");
    // hide_when_enforced
    cfg.module.leash.hide_when_enforced = true;
    ctx.leash.enforced = true;
    assert_eq!(render(cfg, "leash", &ctx), "");
}

// -- battery ----------------------------------------------------------------

#[test]
fn battery_low_and_charging() {
    let mut cfg = PromptConfig::default();
    cfg.module.battery.enabled = true;
    let mut ctx = base_ctx();
    assert_eq!(render(cfg.clone(), "battery", &ctx), ""); // no battery snapshot
    ctx.battery = Some(BatterySnapshot {
        pct: 12,
        charging: true,
    });
    assert_eq!(render(cfg, "battery", &ctx), "⚡12%");
}

// -- custom -----------------------------------------------------------------

#[test]
fn custom_ready_and_pending() {
    let mut cfg = PromptConfig::default();
    cfg.module.custom.insert(
        "k8s".into(),
        shoal_prompt::CustomModule {
            command: "kubectl".into(),
            format: "☁ ${output}".into(),
            ..Default::default()
        },
    );
    let mut ctx = base_ctx();
    let mut custom = BTreeMap::new();
    custom.insert("k8s".to_string(), CustomSegment::Ready("prod".into()));
    ctx.custom = custom;
    assert_eq!(render(cfg.clone(), "custom_k8s", &ctx), "☁ prod");
    // pending renders empty
    ctx.custom.insert("k8s".to_string(), CustomSegment::Pending);
    assert_eq!(render(cfg, "custom_k8s", &ctx), "");
}
