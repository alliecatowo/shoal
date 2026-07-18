use std::sync::Arc;

use reedline::ExternalPrinter;
use shoal_prompt::{PromptContext, Renderer};

use crate::maybe_strip;

/// Samples pure prompt rendering once per refreshed snapshot and reports slow
/// renders through Reedline's bounded, nonblocking notice queue.
pub(crate) struct PromptBudgetWarnings {
    renderer: Arc<Renderer>,
    printer: ExternalPrinter<String>,
    suppressed: usize,
}

impl PromptBudgetWarnings {
    pub(crate) fn new(renderer: Arc<Renderer>, printer: ExternalPrinter<String>) -> Self {
        Self {
            renderer,
            printer,
            suppressed: 0,
        }
    }

    pub(crate) fn observe(&mut self, context: &PromptContext) {
        if !self.renderer.config().budget.warn_on_exceed {
            return;
        }
        let report = self.renderer.budget_report(context);
        if !report.over_budget {
            return;
        }
        let suffix = if self.suppressed == 0 {
            String::new()
        } else {
            format!("; {} earlier warning(s) suppressed", self.suppressed)
        };
        let warning = maybe_strip(format!(
            "\x1b[33;1mwarning:\x1b[0m prompt render took {}us (budget {}ms){suffix}",
            report.slowest.as_micros(),
            self.renderer.config().budget.render_deadline_ms,
        ));
        if self.printer.sender().try_send(warning).is_ok() {
            self.suppressed = 0;
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reporter(capacity: usize, warn: bool) -> (PromptBudgetWarnings, ExternalPrinter<String>) {
        let mut config = shoal_prompt::PromptConfig::default();
        config.budget.render_deadline_ms = 0;
        config.budget.warn_on_exceed = warn;
        let (renderer, _) = Renderer::new(config);
        let printer = ExternalPrinter::new(capacity);
        (
            PromptBudgetWarnings::new(Arc::new(renderer), printer.clone()),
            printer,
        )
    }

    #[test]
    fn warning_is_bounded_and_carries_suppression_count() {
        let (mut reporter, printer) = reporter(1, true);
        printer.sender().try_send("occupied".into()).unwrap();
        reporter.observe(&PromptContext::empty("/".into()));
        assert_eq!(printer.get_line().unwrap(), "occupied");

        reporter.observe(&PromptContext::empty("/".into()));
        let warning = printer.get_line().unwrap();
        assert!(warning.contains("prompt render took"));
        assert!(warning.contains("1 earlier warning(s) suppressed"));
    }

    #[test]
    fn disabled_warning_does_not_sample_or_enqueue() {
        let (mut reporter, printer) = reporter(1, false);
        reporter.observe(&PromptContext::empty("/".into()));
        assert!(printer.get_line().is_none());
    }
}
