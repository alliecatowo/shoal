use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use reedline::{PromptEditMode, PromptViMode};
use shoal_prompt::EditMode;

/// Lock-free bridge from Reedline's live edit mode into pure prompt rendering.
#[derive(Clone, Default)]
pub(crate) struct EditModeTracker(Arc<AtomicU8>);

impl EditModeTracker {
    pub(crate) fn observe(&self, mode: &PromptEditMode) {
        let encoded = match mode {
            PromptEditMode::Vi(PromptViMode::Normal) => 1,
            PromptEditMode::Vi(PromptViMode::Insert) => 2,
            PromptEditMode::Vi(PromptViMode::Visual) => 3,
            PromptEditMode::Default | PromptEditMode::Emacs | PromptEditMode::Custom(_) => 0,
        };
        self.0.store(encoded, Ordering::Release);
    }

    pub(crate) fn current(&self) -> EditMode {
        match self.0.load(Ordering::Acquire) {
            1 => EditMode::ViNormal,
            2 => EditMode::ViInsert,
            3 => EditMode::ViVisual,
            _ => EditMode::Emacs,
        }
    }
}
