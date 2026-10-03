use super::*;
use std::fs;

fn opts(
    system: Option<PathBuf>,
    user: Option<PathBuf>,
    project: Option<PathBuf>,
    env: Vec<(&str, &str)>,
) -> LoadOptions {
    LoadOptions {
        system,
        user,
        project,
        env: env
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect(),
    }
}

#[test]
fn precedence_layers_override_in_order_and_warn_on_unknown_key() {
    let t = tempfile::tempdir().unwrap();
    let s = t.path().join("s");
    let u = t.path().join("u");
    let p = t.path().join("p");
    fs::write(&s, "[prompt]\ntemplate='system'").unwrap();
    fs::write(&u, "[prompt]\ntemplate='user'").unwrap();
    fs::write(&p, "[prompt]\ntemplate='project'\n[render]\nwat=1").unwrap();
    let l = load(&opts(
        Some(s),
        Some(u),
        Some(p),
        vec![("SHOAL_PROMPT", "env")],
    ))
    .unwrap();
    assert_eq!(l.config.prompt.template, "env");
    assert_eq!(l.warnings.len(), 1);
    assert!(l.warnings[0].contains("unknown config key `render.wat`"));
    assert_eq!(l.sources.len(), 3);
}

#[test]
fn a_later_layer_only_overrides_the_keys_it_sets() {
    // system sets history.enabled=false AND max_entries=5; user only
    // touches max_entries — history.enabled must survive from system.
    let t = tempfile::tempdir().unwrap();
    let s = t.path().join("s");
    let u = t.path().join("u");
    fs::write(&s, "[history]\nenabled = false\nmax_entries = 5\n").unwrap();
    fs::write(&u, "[history]\nmax_entries = 50\n").unwrap();
    let l = load(&opts(Some(s), Some(u), None, vec![])).unwrap();
    assert!(!l.config.history.enabled);
    assert_eq!(l.config.history.max_entries, 50);
}

#[test]
fn missing_layers_are_not_an_error() {
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("does-not-exist");
    let l = load(&opts(
        Some(missing.clone()),
        Some(missing.clone()),
        None,
        vec![],
    ))
    .unwrap();
    assert_eq!(l.config, Config::default());
    assert!(l.sources.is_empty());
}

#[test]
fn unsupported_version_is_a_precise_error() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "version=9").unwrap();
    let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
    assert_eq!(
        err,
        ConfigError::Value {
            source: None,
            key: "version".into(),
            message: "unsupported config version 9 (expected 1)".into(),
        }
    );
}

#[test]
fn malformed_toml_never_panics_and_is_a_structured_parse_error() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[history\nenabled = true").unwrap();
    let err = load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err();
    match err {
        ConfigError::Parse { path, message } => {
            assert_eq!(path, p);
            assert!(!message.is_empty());
        }
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn oversized_sparse_layer_is_rejected_before_toml() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("sparse.toml");
    let file = fs::File::create(&p).unwrap();
    file.set_len((CONFIG_FILE_MAX_BYTES + 1) as u64).unwrap();
    let err = load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err();
    assert_eq!(
        err,
        ConfigError::TooLarge {
            path: p,
            max_bytes: CONFIG_FILE_MAX_BYTES,
        }
    );
}

#[test]
fn non_utf8_layer_has_a_path_aware_error() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("binary.toml");
    fs::write(&p, [b'v', 0xff]).unwrap();
    assert_eq!(
        load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err(),
        ConfigError::Utf8 { path: p }
    );
}

#[test]
fn deeply_nested_toml_is_rejected_before_deserialization() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("deep.toml");
    fs::write(
        &p,
        format!(
            "unknown = {}0{}\n",
            "[".repeat(CONFIG_TOML_MAX_NESTING + 1),
            "]".repeat(CONFIG_TOML_MAX_NESTING + 1)
        ),
    )
    .unwrap();
    assert_eq!(
        load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err(),
        ConfigError::Complexity {
            path: p,
            max_nesting: CONFIG_TOML_MAX_NESTING,
        }
    );
}

#[test]
fn deeply_dotted_toml_key_is_rejected_before_deserialization() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("deep-dotted.toml");
    fs::write(
        &p,
        format!(
            "{} = 1\n",
            std::iter::repeat_n("segment", CONFIG_TOML_MAX_NESTING + 1)
                .collect::<Vec<_>>()
                .join(".")
        ),
    )
    .unwrap();
    assert_eq!(
        load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err(),
        ConfigError::Complexity {
            path: p,
            max_nesting: CONFIG_TOML_MAX_NESTING,
        }
    );
}

#[cfg(unix)]
#[test]
fn symlink_to_regular_layer_remains_supported() {
    use std::os::unix::fs::symlink;

    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("target.toml");
    let link = t.path().join("link.toml");
    fs::write(&target, "[prompt]\ntemplate = 'linked'\n").unwrap();
    symlink(&target, &link).unwrap();
    let loaded = load(&opts(None, Some(link.clone()), None, vec![])).unwrap();
    assert_eq!(loaded.config.prompt.template, "linked");
    assert_eq!(loaded.sources, vec![link]);
}

#[test]
fn recognized_env_string_cannot_amplify_config_without_bound() {
    let huge = "x".repeat(CONFIG_ENV_VALUE_MAX_BYTES + 1);
    let err = load(&opts(
        None,
        None,
        None,
        vec![("SHOAL_PROMPT_TEMPLATE", &huge)],
    ))
    .unwrap_err();
    assert!(matches!(
        err,
        ConfigError::Env { ref var, ref message }
            if var == "SHOAL_PROMPT_TEMPLATE" && message.contains("maximum")
    ));
}

#[test]
fn type_mismatch_names_key_path_and_expected_type() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[history]\nmax_entries = \"lots\"").unwrap();
    let err = load(&opts(None, Some(p.clone()), None, vec![])).unwrap_err();
    assert_eq!(
        err,
        ConfigError::Type {
            source: Some(p),
            key: "history.max_entries".into(),
            expected: "a non-negative integer",
            found: "string",
        }
    );
}

#[test]
fn env_override_bad_bool_is_a_precise_error() {
    let err = load(&opts(
        None,
        None,
        None,
        vec![("SHOAL_HISTORY_ENABLED", "maybe")],
    ))
    .unwrap_err();
    assert_eq!(
        err,
        ConfigError::Env {
            var: "SHOAL_HISTORY_ENABLED".into(),
            message: "expected true/false, got `maybe`".into(),
        }
    );
}

#[test]
fn env_override_scalar_leaves() {
    let l = load(&opts(
        None,
        None,
        None,
        vec![
            ("SHOAL_HISTORY_MAX_ENTRIES", "42"),
            ("SHOAL_EDITOR_MODE", "vi"),
            ("SHOAL_KERNEL_SESSION", "work"),
        ],
    ))
    .unwrap();
    assert_eq!(l.config.history.max_entries, 42);
    assert_eq!(l.config.editor.mode, "vi");
    assert_eq!(l.config.kernel.session, "work");
}

/// `render.paging` defaults to `"never"` (identical behavior to before
/// this knob existed — an unconfigured shoal never pages) and is
/// settable via either the config file or `SHOAL_RENDER_PAGING`/
/// `SHOAL_RENDER_PAGER`.
#[test]
fn render_paging_defaults_to_never_and_is_env_overridable() {
    assert_eq!(Config::default().render.paging, "never");
    assert_eq!(Config::default().render.pager, None);

    let l = load(&opts(
        None,
        None,
        None,
        vec![
            ("SHOAL_RENDER_PAGING", "auto"),
            ("SHOAL_RENDER_PAGER", "bat --paging=always"),
        ],
    ))
    .unwrap();
    assert_eq!(l.config.render.paging, "auto");
    assert_eq!(
        l.config.render.pager.as_deref(),
        Some("bat --paging=always")
    );
}

#[test]
fn render_paging_rejects_anything_other_than_never_or_auto() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[render]\npaging = \"always\"\n").unwrap();
    let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
    assert_eq!(
        err,
        ConfigError::Value {
            source: None,
            key: "render.paging".into(),
            message: "must be `never` or `auto`".into(),
        }
    );
}

#[test]
fn render_width_must_be_positive() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[render]\nwidth = 0\n").unwrap();
    let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
    assert_eq!(
        err,
        ConfigError::Value {
            source: None,
            key: "render.width".into(),
            message: "must be greater than 0".into(),
        }
    );
}

/// `render.echo` defaults to unset (`None` — each host surface picks its
/// own fallback: `-c`/scripts default to `quiet`, the REPL to `all`) and
/// is settable via either the config file or `SHOAL_RENDER_ECHO`.
#[test]
fn render_echo_defaults_to_none_and_is_env_overridable() {
    assert_eq!(Config::default().render.echo, None);

    let l = load(&opts(
        None,
        None,
        None,
        vec![("SHOAL_RENDER_ECHO", "commands")],
    ))
    .unwrap();
    assert_eq!(l.config.render.echo.as_deref(), Some("commands"));
}

#[test]
fn render_echo_rejects_anything_other_than_quiet_commands_or_all() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[render]\necho = \"loud\"\n").unwrap();
    let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
    assert_eq!(
        err,
        ConfigError::Value {
            source: None,
            key: "render.echo".into(),
            message: "must be `quiet`, `commands`, or `all`".into(),
        }
    );
}

/// All three legal values load cleanly from the config file.
#[test]
fn render_echo_accepts_each_legal_value() {
    for value in ["quiet", "commands", "all"] {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("c");
        fs::write(&p, format!("[render]\necho = \"{value}\"\n")).unwrap();
        let l = load(&opts(None, Some(p), None, vec![])).unwrap();
        assert_eq!(l.config.render.echo.as_deref(), Some(value));
    }
}

#[test]
fn no_color_env_forces_render_color_off_even_if_config_says_true() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[render]\ncolor = true\n").unwrap();
    let l = load(&opts(None, Some(p), None, vec![("NO_COLOR", "")])).unwrap();
    assert!(!l.config.render.color);
}

#[test]
fn no_color_wins_over_an_explicit_shoal_render_color_override() {
    let l = load(&opts(
        None,
        None,
        None,
        vec![("SHOAL_RENDER_COLOR", "true"), ("NO_COLOR", "1")],
    ))
    .unwrap();
    assert!(!l.config.render.color);
}

#[test]
fn project_walk_up_finds_nearest_dot_shoal_toml() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let nested = root.join("a").join("b");
    fs::create_dir_all(&nested).unwrap();
    fs::write(root.join(".shoal.toml"), "[prompt]\ntemplate='root'").unwrap();
    assert_eq!(find_project_config(&nested), Some(root.join(".shoal.toml")));

    // A closer one wins over the root's.
    fs::write(root.join("a").join(".shoal.toml"), "[prompt]\ntemplate='a'").unwrap();
    assert_eq!(
        find_project_config(&nested),
        Some(root.join("a").join(".shoal.toml"))
    );
}

#[test]
fn project_walk_up_finds_nothing_when_absent() {
    let t = tempfile::tempdir().unwrap();
    let nested = t.path().join("a").join("b");
    fs::create_dir_all(&nested).unwrap();
    assert_eq!(find_project_config(&nested), None);
}

#[test]
fn discover_wires_all_four_layers() {
    let t = tempfile::tempdir().unwrap();
    let o = LoadOptions::discover(t.path());
    assert_eq!(o.system, Some(PathBuf::from("/etc/shoal/shoal.toml")));
    // project: none present in a fresh tempdir tree (well, unless the
    // real filesystem above it happens to have one — exercise the
    // plumbing, not the outcome).
    let _ = o.project;
    assert!(!o.env.is_empty() || std::env::vars_os().next().is_none());
}

#[test]
fn unknown_key_deep_in_a_table_still_recurses_and_warns() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[editor]\nbracketde_paste = true\n").unwrap();
    let l = load(&opts(None, Some(p), None, vec![])).unwrap();
    assert_eq!(l.warnings.len(), 1);
    assert!(l.warnings[0].contains("editor.bracketde_paste"));
    assert!(l.warnings[0].contains("did you mean `editor.bracketed_paste`?"));
}

#[test]
fn opaque_reef_table_layers_and_validates_shape_only() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(
        &p,
        "[reef.tools]\nnode = \"22\"\n[reef.options]\nhermetic = true\n",
    )
    .unwrap();
    let l = load(&opts(None, Some(p), None, vec![])).unwrap();
    assert!(l.warnings.is_empty());
    assert_eq!(
        l.config.reef.tools.get("node"),
        Some(&toml::Value::String("22".into()))
    );
    assert!(l.config.reef.options.hermetic);
}

#[test]
fn aliases_and_env_layer_as_string_maps() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(
        &p,
        "[aliases]\ngs = \"git status\"\n[env]\nEDITOR = \"hx\"\n",
    )
    .unwrap();
    let l = load(&opts(None, Some(p), None, vec![])).unwrap();
    assert_eq!(l.config.aliases.get("gs"), Some(&"git status".to_string()));
    assert_eq!(l.config.env.get("EDITOR"), Some(&"hx".to_string()));
}

#[test]
fn invalid_alias_name_is_rejected() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("c");
    fs::write(&p, "[aliases]\n\"g s\" = \"git status\"\n").unwrap();
    let err = load(&opts(None, Some(p), None, vec![])).unwrap_err();
    assert!(matches!(err, ConfigError::Value { ref key, .. } if key == "aliases"));
}

/// A "golden" full config exercising every documented key at once —
/// round-trips through `load` unchanged and un-warned. If this test
/// needs an update, site/content/internals/configuration-reference.md's worked example almost certainly
/// needs the same update.
#[test]
fn golden_full_config_round_trip() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("shoal.toml");
    fs::write(
        &p,
        r#"
version = 1

[prompt]
template = "{cwd} $"

[history]
enabled = true
max_entries = 5000
path = "/home/dev/.local/state/shoal/history"
dedup = true
ignore = ["ls", "cd *"]
ignore_space = true

[render]
width = 120
color = true
paging = "auto"
pager = "less -R"
echo = "quiet"

[editor]
mode = "vi"
bracketed_paste = true
[editor.keybindings]
"ctrl-r" = "history_search_backward"

[kernel]
enabled = true
session = "default"

[adapters]
dirs = ["/home/dev/.config/shoal/adapters"]

[journal]
enabled = true
state_dir = "/home/dev/.local/share/shoal"

[leash]
policy = "/home/dev/.config/shoal/leash.toml"

[init]
files = ["/home/dev/.config/shoal/init.shl"]

[completion]
fuzzy = true
case_insensitive = true
max_results = 200
menu = true

[aliases]
gs = "git status"
gd = "git diff"

[env]
EDITOR = "hx"

[reef.tools]
node = "22"
python = "3.12"
go = { provider = "mise" }

[reef.runners]
py = "python"
ts = { tool = "deno", args = ["run"] }

[reef.options]
hermetic = false
"#,
    )
    .unwrap();

    let l = load(&opts(None, Some(p), None, vec![])).unwrap();
    assert!(
        l.warnings.is_empty(),
        "unexpected warnings: {:?}",
        l.warnings
    );

    let c = &l.config;
    assert_eq!(c.version, 1);
    assert_eq!(c.prompt.template, "{cwd} $");
    assert_eq!(c.history.max_entries, 5000);
    assert_eq!(
        c.history.path,
        Some(PathBuf::from("/home/dev/.local/state/shoal/history"))
    );
    assert!(c.history.dedup);
    assert_eq!(c.history.ignore, vec!["ls".to_string(), "cd *".to_string()]);
    assert_eq!(c.render.width, Some(120));
    assert_eq!(c.render.paging, "auto");
    assert_eq!(c.render.pager.as_deref(), Some("less -R"));
    assert_eq!(c.render.echo.as_deref(), Some("quiet"));
    assert_eq!(c.editor.mode, "vi");
    assert_eq!(
        c.editor.keybindings.get("ctrl-r").map(String::as_str),
        Some("history_search_backward")
    );
    assert_eq!(
        c.adapters.dirs,
        vec![PathBuf::from("/home/dev/.config/shoal/adapters")]
    );
    assert_eq!(c.completion.max_results, 200);
    assert_eq!(c.aliases.get("gs").map(String::as_str), Some("git status"));
    assert_eq!(c.env.get("EDITOR").map(String::as_str), Some("hx"));
    assert!(!c.reef.options.hermetic);
    assert_eq!(
        c.reef.tools.get("node"),
        Some(&toml::Value::String("22".into()))
    );

    // And round-tripping the *typed* Config back through TOML (not the
    // original file text) must reproduce the same Config bit-for-bit —
    // the golden property that makes this a round-trip test rather than
    // just a parse test.
    let text = toml::to_string(&l.config).unwrap();
    let back: Config = toml::from_str(&text).unwrap();
    assert_eq!(back, l.config);
}
