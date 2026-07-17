//! End-to-end coverage that config keys documented as "read today"
//! (site/content/internals/configuration-reference.md) actually change the `shoal` binary's behavior, driven
//! through the deterministic `-c` path (no PTY needed) against a real
//! `shoal.toml` written to a temp `XDG_CONFIG_HOME` — the same user-layer
//! resolution `shoal_config::LoadOptions::discover` and
//! `reef_user_manifest_path` both use.

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_shoal");

/// Write `shoal.toml` under `<home>/.config/shoal/` (the XDG user layer) and
/// run `shoal -c <src>` with `HOME`/`XDG_CONFIG_HOME`/`XDG_STATE_HOME` all
/// pointed at `home` — fully isolated from the real user's config.
fn run_with_config(home: &Path, config_toml: &str, src: &str) -> Output {
    let config_dir = home.join(".config").join("shoal");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("shoal.toml"), config_toml).unwrap();

    Command::new(BIN)
        .args(["-c", src])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home)
        .env_remove("NO_COLOR")
        .env_remove("SHOAL_RENDER_COLOR")
        .current_dir(home)
        .output()
        .expect("spawn shoal -c")
}

#[test]
fn aliases_from_config_file_expand_at_startup() {
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(
        home.path(),
        "version = 1\n[aliases]\nmygreet = \"echo hi from config alias\"\n",
        "mygreet",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("hi from config alias"),
        "alias from shoal.toml should have expanded; stdout was {stdout:?}, stderr was {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn env_from_config_file_is_readable_via_env_namespace() {
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(
        home.path(),
        "version = 1\n[env]\nMY_SHOAL_CFG_VAR = \"configured-value\"\n",
        "env.MY_SHOAL_CFG_VAR",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("configured-value"),
        "env.NAME should read the config-declared value; stdout was {stdout:?}, stderr was {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn env_value_with_braces_and_quotes_round_trips_through_seeding() {
    // Regression coverage for the seed-statement quoting: a value containing
    // characters the shoal string lexer treats specially (`{`, `"`) must
    // still come back byte-for-byte, not be misinterpreted as interpolation.
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(
        home.path(),
        "version = 1\n[env]\nMY_SHOAL_CFG_VAR = \"has {braces} and \\\"quotes\\\"\"\n",
        "env.MY_SHOAL_CFG_VAR",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("has {braces} and \"quotes\""),
        "stdout was {stdout:?}, stderr was {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn render_color_false_in_config_suppresses_ansi_without_no_color_env() {
    // A parse error's diagnostic is always colorized unless suppressed
    // (`main.rs::format_diagnostic`); trigger one with a config that sets
    // `render.color = false` and NO `NO_COLOR` env var, and confirm no ANSI
    // escape reached stderr.
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(home.path(), "version = 1\n[render]\ncolor = false\n", "1 +");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.is_empty(),
        "expected a parse-error diagnostic on stderr"
    );
    assert!(
        !stderr.contains('\x1b'),
        "render.color = false should suppress ANSI even without NO_COLOR; stderr was {stderr:?}"
    );

    // Control: the same source with color left at its default (true) DOES
    // colorize — proving the suppression above came from config, not from
    // some other reason the binary never colorizes this diagnostic.
    let home2 = tempfile::tempdir().unwrap();
    let out2 = run_with_config(home2.path(), "version = 1\n", "1 +");
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr2.contains('\x1b'),
        "control run without render.color = false should still colorize; stderr was {stderr2:?}"
    );
}

/// `render.echo` (site/content/internals/configuration-reference.md): a non-interactive `-c`/script run
/// defaults to `quiet` — an intermediate bare command's output shows, the
/// FINAL statement's value shows, but intermediate pure expressions do NOT
/// auto-print. No `render.echo` key is set here, so this exercises the default.
#[test]
fn render_echo_quiet_is_the_default_for_scripts() {
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(home.path(), "version = 1\n", "echo CMDOUT\n100 + 1\n303");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CMDOUT"),
        "an intermediate bare command's output must still show; stdout was {stdout:?}"
    );
    assert!(
        stdout.contains("303"),
        "the final statement's value must show; stdout was {stdout:?}"
    );
    assert!(
        !stdout.contains("101"),
        "an intermediate pure expression (100 + 1) must NOT auto-print in quiet; stdout was {stdout:?}"
    );
}

/// `render.echo = "all"` restores the legacy echo-every-statement behavior:
/// the intermediate `100 + 1` prints too.
#[test]
fn render_echo_all_restores_echo_every_statement() {
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(
        home.path(),
        "version = 1\n[render]\necho = \"all\"\n",
        "echo CMDOUT\n100 + 1\n303",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in ["CMDOUT", "101", "303"] {
        assert!(
            stdout.contains(needle),
            "render.echo = all must echo every statement (missing {needle:?}); stdout was {stdout:?}"
        );
    }
}

/// `render.echo = "commands"`: only bare-command output shows — not even the
/// final pure expression.
#[test]
fn render_echo_commands_suppresses_even_the_final_expression() {
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(
        home.path(),
        "version = 1\n[render]\necho = \"commands\"\n",
        "echo CMDOUT\n100 + 1\n999",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CMDOUT"),
        "a bare command's output must show in commands mode; stdout was {stdout:?}"
    );
    assert!(
        !stdout.contains("101"),
        "an intermediate pure expression must not print; stdout was {stdout:?}"
    );
    assert!(
        !stdout.contains("999"),
        "commands mode must suppress even the final pure expression; stdout was {stdout:?}"
    );
}

/// Decision 2 end-to-end: the in-language `config` namespace reflects the
/// SAME layered/validated config the binary applied to itself (not a raw
/// `shoal.toml` walk) — a value set in the user-layer config file is visible
/// via `config.get(...)`/`config.all()`.
#[test]
fn config_namespace_reflects_the_host_applied_config() {
    let home = tempfile::tempdir().unwrap();
    let out = run_with_config(
        home.path(),
        "version = 1\n[history]\nmax_entries = 4242\n",
        "config.all().history.max_entries",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("4242"),
        "config.all() should expose the host-applied resolved config; stdout was {stdout:?}, stderr {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}
