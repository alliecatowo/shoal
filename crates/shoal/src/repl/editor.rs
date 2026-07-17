//! Reedline edit-mode, history filtering, and multiline validation policy.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use reedline::{
    EditMode, Emacs, FileBackedHistory, History, HistoryItem, HistoryItemId, HistorySessionId,
    KeyCode, KeyModifiers, ReedlineEvent, SearchDirection, SearchQuery, ValidationResult,
    Validator, Vi, default_emacs_keybindings, default_vi_insert_keybindings,
    default_vi_normal_keybindings,
};

pub(super) fn build_edit_mode(
    config: &shoal_config::Config,
    custom: &[crate::keybindings::ParsedBinding],
) -> Box<dyn EditMode> {
    let tab_event = ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu("completion_menu".to_string()),
        ReedlineEvent::MenuNext,
    ]);
    if config.editor.mode == "vi" {
        let mut insert = default_vi_insert_keybindings();
        let mut normal = default_vi_normal_keybindings();
        insert.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_event);
        for binding in custom {
            insert.add_binding(binding.modifiers, binding.code, binding.event.clone());
            normal.add_binding(binding.modifiers, binding.code, binding.event.clone());
        }
        Box::new(Vi::new(insert, normal))
    } else {
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, tab_event);
        for binding in custom {
            keybindings.add_binding(binding.modifiers, binding.code, binding.event.clone());
        }
        Box::new(Emacs::new(keybindings))
    }
}

/// History backend decorator implementing Shoal's dedup/ignore policies.
pub(super) struct FilteredHistory {
    inner: Box<dyn History>,
    dedup: bool,
    ignore: Vec<String>,
    last_recorded: Option<String>,
}

impl FilteredHistory {
    pub(super) fn new(inner: Box<dyn History>, dedup: bool, ignore: Vec<String>) -> Self {
        let last_recorded = inner
            .search(SearchQuery::everything(SearchDirection::Backward, None))
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|item| item.command_line);
        Self {
            inner,
            dedup,
            ignore,
            last_recorded,
        }
    }

    fn should_skip(&self, line: &str) -> bool {
        if self.dedup && self.last_recorded.as_deref() == Some(line) {
            return true;
        }
        self.ignore.iter().any(|pattern| glob_match(pattern, line))
    }

    fn refresh_last_recorded(&mut self) {
        self.last_recorded = self
            .inner
            .search(SearchQuery::everything(SearchDirection::Backward, None))
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|item| item.command_line);
    }
}

impl History for FilteredHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        if self.should_skip(&item.command_line) {
            return Ok(item);
        }
        let command_line = item.command_line.clone();
        let saved = self.inner.save(item)?;
        self.last_recorded = Some(command_line);
        Ok(saved)
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.inner.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.inner.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.inner.search(query)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.inner.update(id, updater)?;
        self.refresh_last_recorded();
        Ok(())
    }

    fn clear(&mut self) -> reedline::Result<()> {
        self.inner.clear()?;
        self.last_recorded = None;
        Ok(())
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.inner.delete(id)?;
        self.refresh_last_recorded();
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.inner.sync()
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.inner.session()
    }
}

pub(super) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let text = text.chars().collect::<Vec<_>>();
    let mut matches = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    matches[0][0] = true;
    for index in 1..=pattern.len() {
        if pattern[index - 1] == '*' {
            matches[index][0] = matches[index - 1][0];
        }
    }
    for pattern_index in 1..=pattern.len() {
        for text_index in 1..=text.len() {
            matches[pattern_index][text_index] = match pattern[pattern_index - 1] {
                '*' => {
                    matches[pattern_index - 1][text_index] || matches[pattern_index][text_index - 1]
                }
                '?' => matches[pattern_index - 1][text_index - 1],
                character => {
                    matches[pattern_index - 1][text_index - 1] && character == text[text_index - 1]
                }
            };
        }
    }
    matches[pattern.len()][text.len()]
}

pub(super) fn history_path() -> Option<PathBuf> {
    Some(
        shoal_paths::ShoalPaths::discover()
            .state_dir()
            .join("history.txt"),
    )
}

pub(super) fn open_history(max_entries: usize, path: &Path) -> Result<FileBackedHistory, String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create history directory: {error}"))?;
    }
    FileBackedHistory::with_file(max_entries, path.to_path_buf())
        .map_err(|error| format!("cannot open history file: {error}"))
}

pub(super) struct ShoalValidator;

impl Validator for ShoalValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if input_is_incomplete(line) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

pub(super) fn input_is_incomplete(source: &str) -> bool {
    let mut stack = Vec::new();
    let chars = source.chars().collect::<Vec<_>>();
    let mut quote: Option<(char, bool)> = None;
    let mut escaped = false;
    let mut comment = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if comment {
            if character == '\n' {
                comment = false;
            }
            index += 1;
            continue;
        }
        if let Some((delimiter, triple)) = quote {
            if triple
                && character == delimiter
                && chars.get(index + 1) == Some(&delimiter)
                && chars.get(index + 2) == Some(&delimiter)
            {
                quote = None;
                index += 3;
                continue;
            }
            if !triple && delimiter == '"' && character == '\\' && !escaped {
                escaped = true;
                index += 1;
                continue;
            }
            if !triple && character == delimiter && !escaped {
                quote = None;
            }
            escaped = false;
            index += 1;
            continue;
        }
        match character {
            '#' => comment = true,
            '\'' | '"' => {
                let triple = chars.get(index + 1) == Some(&character)
                    && chars.get(index + 2) == Some(&character);
                quote = Some((character, triple));
                if triple {
                    index += 2;
                }
            }
            '(' | '[' | '{' => stack.push(character),
            ')' if stack.last() == Some(&'(') => {
                stack.pop();
            }
            ']' if stack.last() == Some(&'[') => {
                stack.pop();
            }
            '}' if stack.last() == Some(&'{') => {
                stack.pop();
            }
            _ => {}
        }
        index += 1;
    }
    if quote.is_some() || !stack.is_empty() {
        return true;
    }
    let tail = source.trim_end();
    tail.ends_with('\\')
        || ["&&", "||", "??", "+", "-", "*", "/", "%", "=", ",", "."]
            .iter()
            .any(|operator| tail.ends_with(operator))
}
