//! Reedline, completion, prompt, history, and paging assembly.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use reedline::{ColumnarMenu, DefaultHinter, ExternalPrinter, MenuBuilder, Reedline, ReedlineMenu};
use shoal_adapters::AdapterCatalog;
use shoal_eval::Evaluator;

use super::editor::{FilteredHistory, ShoalValidator, build_edit_mode, history_path, open_history};
use super::rendering::PagerContext;
use crate::completer::ShoalCompleter;
use crate::highlight::ShoalHighlighter;
use crate::{maybe_strip, no_color, prompt};

/// Cohesive terminal UI state. Evaluator, protocol, transcript, and job
/// identities deliberately remain outside this boundary.
pub(super) struct ReplUi {
    pub(super) editor: Reedline,
    pub(super) prompt: prompt::ShoalPrompt,
    pub(super) shared_ctx: prompt::SharedCtx,
    pub(super) static_facts: prompt::StaticFacts,
    custom: prompt::CustomScheduler,
    battery: prompt::BatterySampler,
    budget_warnings: prompt::PromptBudgetWarnings,
    pub(super) pager: PagerContext,
}

impl ReplUi {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build(
        config: &shoal_config::Config,
        cwd: &Path,
        evaluator: &Evaluator,
        catalogs: Vec<AdapterCatalog>,
        adapter_names: Vec<String>,
        cwd_cell: Arc<Mutex<PathBuf>>,
        completion_path_dirs: Arc<Mutex<Option<Vec<PathBuf>>>>,
    ) -> (Self, ExternalPrinter<String>) {
        let completer =
            ShoalCompleter::new(evaluator.env().clone(), cwd_cell, catalogs, adapter_names)
                .with_path_dirs(completion_path_dirs)
                .configure(
                    config.completion.fuzzy,
                    config.completion.case_insensitive,
                    config.completion.max_results,
                );
        let (custom_bindings, keybinding_warnings) =
            crate::keybindings::parse_bindings(&config.editor.keybindings);
        warn_each(&keybinding_warnings);
        let edit_mode = build_edit_mode(config, &custom_bindings);

        let (prompt_config, prompt_warnings) = prompt::load_prompt_config(cwd);
        warn_each(&prompt_warnings);
        let static_facts = prompt::StaticFacts::resolve(&prompt_config, no_color());
        let transient_enabled = prompt_config.transient.enabled;
        let mut battery_warnings = Vec::new();
        let battery = prompt::BatterySampler::new(&prompt_config, &mut battery_warnings);
        warn_each(&battery_warnings);
        let (renderer, renderer_warnings) = shoal_prompt::Renderer::new(prompt_config);
        warn_each(&renderer_warnings);
        let mut custom_warnings = Vec::new();
        let custom = prompt::CustomScheduler::new(renderer.config(), &mut custom_warnings);
        warn_each(&custom_warnings);
        let renderer = Arc::new(renderer);
        let shared_ctx: prompt::SharedCtx = Arc::new(RwLock::new(Arc::new(
            shoal_prompt::PromptContext::empty(cwd.to_path_buf()),
        )));
        let prompt = prompt::ShoalPrompt::new(renderer.clone(), shared_ctx.clone(), false);

        let background_printer = ExternalPrinter::new(64);
        let mut editor = Reedline::create()
            .with_external_printer(background_printer.clone())
            .with_poll_interval(Duration::from_millis(50))
            .use_bracketed_paste(config.editor.bracketed_paste)
            .with_validator(Box::new(ShoalValidator))
            .with_completer(Box::new(completer))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                ColumnarMenu::default().with_name("completion_menu"),
            )))
            .with_quick_completions(!config.completion.menu)
            .with_partial_completions(!config.completion.menu)
            .with_edit_mode(edit_mode)
            .with_highlighter(Box::new(ShoalHighlighter::with_env(
                evaluator.env().clone(),
            )))
            .with_hinter(Box::new(DefaultHinter::default()))
            .with_history_exclusion_prefix(config.history.ignore_space.then(|| " ".to_string()));
        if transient_enabled {
            editor = editor.with_transient_prompt(Box::new(prompt::ShoalPrompt::new(
                renderer.clone(),
                shared_ctx.clone(),
                true,
            )));
        }
        if config.history.enabled
            && let Some(path) = config.history.path.clone().or_else(history_path)
        {
            match open_history(config.history.max_entries, &path) {
                Ok(history) => {
                    editor = editor.with_history(Box::new(FilteredHistory::new(
                        Box::new(history),
                        config.history.dedup,
                        config.history.ignore.clone(),
                    )));
                }
                Err(error) => eprintln!(
                    "{}",
                    maybe_strip(format!(
                        "\x1b[33;1mwarning:\x1b[0m history unavailable ({}): {error}",
                        path.display()
                    ))
                ),
            }
        }
        (
            Self {
                editor,
                prompt,
                shared_ctx,
                static_facts,
                custom,
                battery,
                budget_warnings: prompt::PromptBudgetWarnings::new(
                    renderer,
                    background_printer.clone(),
                ),
                pager: PagerContext {
                    enabled: config.render.paging == "auto",
                    pager: config.render.pager.clone(),
                    configured_width: config.render.width,
                },
            },
            background_printer,
        )
    }

    pub(super) fn refresh_prompt(
        &mut self,
        evaluator: &mut Evaluator,
        snapshot: Option<&crate::repl_state::ProtocolSnapshot>,
    ) {
        let width = u16::try_from(self.pager.width()).unwrap_or(u16::MAX);
        let mut context = match snapshot {
            Some(snapshot) => {
                prompt::build_context_from_protocol(snapshot, &self.static_facts, width)
            }
            None => prompt::build_context(evaluator, &self.static_facts, width),
        };
        context.battery = self.battery.sample();
        context.custom = self.custom.refresh(&context.cwd, evaluator.env_vars());
        self.budget_warnings.observe(&context);
        if let Ok(mut cell) = self.shared_ctx.write() {
            *cell = Arc::new(context);
        }
    }
}

fn warn_each(warnings: &[String]) {
    for warning in warnings {
        eprintln!(
            "{}",
            maybe_strip(format!("\x1b[33;1mwarning:\x1b[0m {warning}"))
        );
    }
}
