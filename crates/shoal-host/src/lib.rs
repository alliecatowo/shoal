//! Common evaluator composition for every Shoal host surface.
//!
//! A parser/evaluator is only one layer of a working shell session. This
//! crate applies the resolved config snapshot, aliases, environment, Reef
//! scope, adapters, and optional leash policy in one ordered composition.
//! Interactive init files are an explicit second phase used only by the REPL.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use shoal_adapters::AdapterCatalog;
use shoal_config::{Config, Loaded};
use shoal_eval::{EchoMode, Evaluator};
use shoal_syntax::parse;
use shoal_value::{ConfigSnapshot, Record, Value, json_to_value};

/// Host behavior that intentionally differs by execution surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    NonInteractive,
    Interactive,
    Kernel,
}

impl Surface {
    fn is_interactive(self) -> bool {
        matches!(self, Self::Interactive)
    }

    fn default_echo(self) -> EchoMode {
        match self {
            Self::Interactive => EchoMode::All,
            Self::NonInteractive | Self::Kernel => EchoMode::Quiet,
        }
    }

    const fn runs_init(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// Fully resolved config plus discovery diagnostics shared by all hosts.
#[derive(Debug)]
pub struct SessionBootstrap {
    loaded: Loaded,
    user_manifest: PathBuf,
}

/// Non-fatal composition diagnostics and adapter metadata needed by REPL
/// completion. Fatal config/policy errors are returned from `apply`; interactive
/// init errors are returned separately by [`SessionBootstrap::run_init`].
#[derive(Debug, Default)]
pub struct BootstrapReport {
    pub warnings: Vec<String>,
    pub adapter_catalogs: Vec<AdapterCatalog>,
    pub adapter_dirs: Vec<PathBuf>,
}

impl SessionBootstrap {
    pub fn discover(cwd: &Path) -> Result<Self, shoal_config::ConfigError> {
        let loaded = shoal_config::load(&shoal_config::LoadOptions::discover(cwd))?;
        Ok(Self::from_loaded(loaded))
    }

    pub fn from_loaded(loaded: Loaded) -> Self {
        let user_manifest = shoal_paths::ShoalPaths::discover()
            .config_dir()
            .join("shoal.toml");
        Self {
            loaded,
            user_manifest,
        }
    }

    pub fn config(&self) -> &Config {
        &self.loaded.config
    }

    pub fn config_warnings(&self) -> &[String] {
        &self.loaded.warnings
    }

    pub fn config_sources(&self) -> &[PathBuf] {
        &self.loaded.sources
    }

    /// Apply the common host layers in deterministic order. `principal` is
    /// used only when an explicit `[leash].policy` is configured; kernel
    /// execution later replaces that policy with its authenticated policy at
    /// the request boundary.
    pub fn apply(
        &self,
        evaluator: &mut Evaluator,
        surface: Surface,
        principal: &str,
    ) -> Result<BootstrapReport, String> {
        evaluator.set_interactive(surface.is_interactive());
        evaluator.set_echo_mode(resolve_echo_mode(
            self.loaded.config.render.echo.as_deref(),
            surface.default_echo(),
        ));
        evaluator.set_config(Arc::new(ConfigSnapshot::new(config_snapshot_value(
            &self.loaded.config,
        ))));
        evaluator.set_reef_user_manifest(self.user_manifest.clone());

        if let Some(path) = &self.loaded.config.leash.policy {
            let policy = shoal_leash::Policy::load(path)
                .map_err(|error| format!("leash policy {}: {error}", path.display()))?;
            evaluator.set_leash_policy(policy, principal);
        }

        let mut report = BootstrapReport::default();
        let plugin_errors = evaluator
            .load_wasm_plugins(&self.loaded.config.plugins.dirs)
            .map_err(|error| format!("plugin registry: {error}"))?;
        report.warnings.extend(
            plugin_errors
                .into_iter()
                .map(|error| format!("plugin: {error}")),
        );
        seed_config_bindings(evaluator, &self.loaded.config, &mut report.warnings);
        load_adapters(evaluator, &self.loaded.config, &mut report);

        Ok(report)
    }

    /// Run configured session init files after the host has installed its
    /// surface-specific journal, output sink, and event forwarding. The
    /// profile is authoritative: non-interactive and kernel calls are no-ops,
    /// so an agent host cannot accidentally execute terminal startup code.
    pub fn run_init(&self, evaluator: &mut Evaluator, surface: Surface) -> Result<(), String> {
        if !surface.runs_init() {
            return Ok(());
        }
        for init in &self.loaded.config.init.files {
            evaluator
                .eval_source_file(init)
                .map_err(|error| format!("init {}: {error}", init.display()))?;
        }
        Ok(())
    }
}

pub fn bundled_adapter_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters")
}

pub fn adapter_dirs(config: &Config) -> Vec<PathBuf> {
    std::iter::once(bundled_adapter_dir())
        .chain(config.adapters.dirs.iter().cloned())
        .collect()
}

fn load_adapters(evaluator: &mut Evaluator, config: &Config, report: &mut BootstrapReport) {
    let mut active = AdapterCatalog::empty();
    for dir in adapter_dirs(config) {
        let (catalog, warnings) = AdapterCatalog::load_dir(&dir);
        report.warnings.extend(
            warnings
                .into_iter()
                .map(|warning| format!("adapter: {warning}")),
        );
        for name in active.overlay(&catalog) {
            report.warnings.push(format!(
                "adapter: {}: command {name} overrides an earlier adapter directory",
                dir.display()
            ));
        }
        report.adapter_dirs.push(dir);
        report.adapter_catalogs.push(catalog);
    }
    evaluator.set_adapters(active);
}

fn seed_config_bindings(evaluator: &mut Evaluator, config: &Config, warnings: &mut Vec<String>) {
    for (name, target) in &config.aliases {
        let src = format!("alias {name} = {target}\n");
        if let Err(message) = eval_seed_statement(evaluator, &src) {
            warnings.push(format!("aliases.{name}: {message}"));
        }
    }
    for (name, value) in &config.env {
        let src = format!("env.{name} = {}\n", quote_shoal_string(value));
        if let Err(message) = eval_seed_statement(evaluator, &src) {
            warnings.push(format!("env.{name}: {message}"));
        }
    }
}

fn eval_seed_statement(evaluator: &mut Evaluator, src: &str) -> Result<(), String> {
    let program = parse(src).map_err(|error| error.msg)?;
    evaluator
        .eval_program(&program)
        .map(|_| ())
        .map_err(|error| error.msg)
}

pub fn quote_shoal_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

pub fn resolve_echo_mode(echo: Option<&str>, default: EchoMode) -> EchoMode {
    match echo {
        Some("quiet") => EchoMode::Quiet,
        Some("commands") => EchoMode::Commands,
        Some("all") => EchoMode::All,
        _ => default,
    }
}

pub fn config_snapshot_value(config: &Config) -> Value {
    match serde_json::to_value(config) {
        Ok(json) => json_to_value(&json),
        Err(_) => Value::Record(Record::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_round_trips_control_and_interpolation_characters() {
        for value in ["plain", "quote \" slash \\", "{x}", "line\nnext\t"] {
            let program = parse(&quote_shoal_string(value)).unwrap();
            let shoal_ast::Stmt::Expr {
                expr: shoal_ast::Expr::Str { value: parsed, .. },
                ..
            } = &program.stmts[0]
            else {
                panic!("expected string expression")
            };
            assert_eq!(parsed, value);
        }
    }

    #[test]
    fn bootstrap_seeds_config_and_init_is_an_explicit_second_phase() {
        fn assert_common_bindings(evaluator: &mut Evaluator) {
            assert_eq!(
                evaluator
                    .eval_program(&parse("env.FROM_CONFIG").unwrap())
                    .unwrap(),
                Value::Str("value {safe}".into())
            );
            let alias = evaluator.eval_program(&parse("hi").unwrap()).unwrap();
            assert!(
                matches!(alias, Value::Outcome(ref outcome)
                    if outcome.ok && outcome.stdout.as_ref() == b"hello\n"),
                "configured alias must be identical across profiles: {alias:?}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.shoal");
        std::fs::write(&init, "env.FROM_INIT = \"yes\"\n").unwrap();
        let mut config = Config::default();
        config.aliases.insert("hi".into(), "echo hello".into());
        config
            .env
            .insert("FROM_CONFIG".into(), "value {safe}".into());
        config.init.files.push(init);
        let bootstrap = SessionBootstrap::from_loaded(Loaded {
            config,
            warnings: Vec::new(),
            sources: Vec::new(),
        });

        let mut script = Evaluator::new(dir.path().to_path_buf());
        bootstrap
            .apply(&mut script, Surface::NonInteractive, "human")
            .unwrap();
        bootstrap
            .run_init(&mut script, Surface::NonInteractive)
            .unwrap();
        assert_common_bindings(&mut script);
        assert!(
            script
                .eval_program(&parse("env.FROM_INIT").unwrap())
                .is_err()
        );

        let mut kernel = Evaluator::new(dir.path().to_path_buf());
        bootstrap
            .apply(&mut kernel, Surface::Kernel, "agent:test")
            .unwrap();
        bootstrap.run_init(&mut kernel, Surface::Kernel).unwrap();
        assert_common_bindings(&mut kernel);
        assert!(
            kernel
                .eval_program(&parse("env.FROM_INIT").unwrap())
                .is_err()
        );

        let mut interactive = Evaluator::new(dir.path().to_path_buf());
        bootstrap
            .apply(&mut interactive, Surface::Interactive, "human")
            .unwrap();
        bootstrap
            .run_init(&mut interactive, Surface::Interactive)
            .unwrap();
        assert_common_bindings(&mut interactive);
        assert_eq!(
            interactive
                .eval_program(&parse("env.FROM_INIT").unwrap())
                .unwrap(),
            Value::Str("yes".into())
        );
    }

    #[test]
    fn hostile_init_is_typed_path_aware_and_does_not_break_retry() {
        let dir = tempfile::tempdir().unwrap();
        let init = dir.path().join("init.shl");
        let mut config = Config::default();
        config.init.files.push(init.clone());
        let bootstrap = SessionBootstrap::from_loaded(Loaded {
            config,
            warnings: Vec::new(),
            sources: Vec::new(),
        });
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());

        std::fs::write(&init, [0xff]).unwrap();
        // Non-interactive profiles never touch even a malformed init path.
        bootstrap
            .run_init(&mut evaluator, Surface::NonInteractive)
            .unwrap();
        bootstrap.run_init(&mut evaluator, Surface::Kernel).unwrap();

        let error = bootstrap
            .run_init(&mut evaluator, Surface::Interactive)
            .unwrap_err();
        assert!(error.contains("source_utf8"), "{error}");
        assert!(error.contains(&init.display().to_string()), "{error}");

        let file = std::fs::File::create(&init).unwrap();
        file.set_len((shoal_syntax::MAX_SOURCE_BYTES + 1) as u64)
            .unwrap();
        let error = bootstrap
            .run_init(&mut evaluator, Surface::Interactive)
            .unwrap_err();
        assert!(error.contains("source_too_large"), "{error}");

        std::fs::write(&init, "env.INIT_RECOVERED = 'yes'\n").unwrap();
        bootstrap
            .run_init(&mut evaluator, Surface::Interactive)
            .unwrap();
        assert_eq!(
            evaluator
                .eval_program(&parse("env.INIT_RECOVERED").unwrap())
                .unwrap(),
            Value::Str("yes".into())
        );
    }

    #[test]
    fn init_bootstrap_cannot_bypass_the_evaluator_source_reader() {
        let production = include_str!("lib.rs").split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("std::fs::read_to_string"));
        assert!(production.contains("eval_source_file(init)"));
    }

    #[test]
    fn configured_adapter_directories_are_all_active_not_just_the_last() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(
            first.join("first.toml"),
            "[cmd.onlyfirst]\nbin = \"true\"\nclass = \"cli\"\n",
        )
        .unwrap();
        std::fs::write(
            second.join("second.toml"),
            "[cmd.onlysecond]\nbin = \"true\"\nclass = \"cli\"\n",
        )
        .unwrap();
        let mut config = Config::default();
        config.adapters.dirs = vec![first, second];
        config.aliases.insert("9bad".into(), "echo no".into());
        let bootstrap = SessionBootstrap::from_loaded(Loaded {
            config,
            warnings: Vec::new(),
            sources: Vec::new(),
        });
        for surface in [
            Surface::NonInteractive,
            Surface::Interactive,
            Surface::Kernel,
        ] {
            let mut evaluator = Evaluator::new(dir.path().to_path_buf());
            let report = bootstrap.apply(&mut evaluator, surface, "human").unwrap();
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains("9bad"))
            );
            for command in ["onlyfirst", "onlysecond"] {
                let value = evaluator.eval_program(&parse(command).unwrap()).unwrap();
                assert!(
                    matches!(value, Value::Outcome(ref outcome) if outcome.ok),
                    "{command} should dispatch through its configured adapter on {surface:?}: \
                     {value:?}"
                );
            }
        }
    }
}
