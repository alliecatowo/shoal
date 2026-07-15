//! Config-driven keybinding parsing (docs/CONFIG.md `[editor.keybindings]`,
//! `chord -> action`, e.g. `"ctrl-r" = "history_search_backward"`).
//!
//! reedline's own keybinding tables are always typed Rust values
//! (`KeyModifiers`/`KeyCode`/`ReedlineEvent`) — there is no string-based
//! config format built into the crate — so a `shoal.toml` string needs a
//! small parser on both sides of the arrow. This module owns both: `chord`
//! parsing (`"ctrl-alt-x"`, `"f5"`, `"tab"`, a bare character, …) and `action`
//! parsing (a curated, documented subset of `ReedlineEvent`/`EditCommand`
//! variants — reedline exposes dozens of parameterized edit commands that
//! have no plain-string form; only the ones meaningful as a *whole* keybind
//! target are supported here).
//!
//! Neither side ever hard-fails: an unrecognized chord or action is reported
//! back as one warning string per bad entry and simply skipped — a typo in
//! `shoal.toml` must not make the shell unusable.

use std::collections::BTreeMap;

use reedline::{EditCommand, KeyCode, KeyModifiers, ReedlineEvent};

/// One successfully parsed `chord -> action` entry, ready for
/// `Keybindings::add_binding`.
pub(crate) struct ParsedBinding {
    pub modifiers: KeyModifiers,
    pub code: KeyCode,
    pub event: ReedlineEvent,
}

/// Parse every `[editor.keybindings]` entry. Returns the bindings that
/// parsed cleanly, plus one human-readable warning per entry that didn't
/// (unrecognized chord syntax or unrecognized action name) — never a hard
/// error.
pub(crate) fn parse_bindings(
    table: &BTreeMap<String, String>,
) -> (Vec<ParsedBinding>, Vec<String>) {
    let mut bindings = Vec::new();
    let mut warnings = Vec::new();
    for (chord, action) in table {
        let Some((modifiers, code)) = parse_chord(chord) else {
            warnings.push(format!(
                "editor.keybindings: unrecognized key chord `{chord}`"
            ));
            continue;
        };
        let Some(event) = parse_action(action) else {
            warnings.push(format!(
                "editor.keybindings.{chord}: unrecognized action `{action}`"
            ));
            continue;
        };
        bindings.push(ParsedBinding {
            modifiers,
            code,
            event,
        });
    }
    (bindings, warnings)
}

/// Parse a chord string (`"ctrl-r"`, `"ctrl-alt-x"`, `"f5"`, `"tab"`, a bare
/// character, …), case-insensitively. A chord is zero or more `-`-joined
/// modifier names followed by a key name; a literal trailing `-` (e.g.
/// `"ctrl--"`, or a bare `"-"`) names the dash key itself — the *last* dash
/// is the key, not a modifier separator, so it is stripped before splitting
/// the modifier list rather than appearing as an extra empty segment.
fn parse_chord(chord: &str) -> Option<(KeyModifiers, KeyCode)> {
    if chord.is_empty() {
        return None;
    }
    let lower = chord.to_ascii_lowercase();
    let (mods_str, key): (&str, &str) = match lower.strip_suffix('-') {
        Some(before_key) => (before_key.strip_suffix('-').unwrap_or(before_key), "-"),
        None => match lower.rsplit_once('-') {
            Some((rest, k)) => (rest, k),
            None => ("", lower.as_str()),
        },
    };
    let mods: Vec<&str> = if mods_str.is_empty() {
        Vec::new()
    } else {
        mods_str.split('-').collect()
    };
    let mut modifiers = KeyModifiers::NONE;
    for m in &mods {
        modifiers |= match *m {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "option" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" | "cmd" | "command" | "meta" => KeyModifiers::SUPER,
            _ => return None,
        };
    }
    let code = match key {
        "tab" => KeyCode::Tab,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "space" => KeyCode::Char(' '),
        _ if key.len() == 1 => KeyCode::Char(key.chars().next()?),
        _ if key.starts_with('f') && key.len() <= 3 => KeyCode::F(key[1..].parse().ok()?),
        _ => return None,
    };
    Some((modifiers, code))
}

/// Parse an action name into the `ReedlineEvent` it names. Covers common
/// shell-editing actions (history search/navigation, screen clearing, the
/// completion menu, a handful of unparameterized `EditCommand`s); reedline's
/// many `select`/`MotionTarget`-parameterized edit commands have no plain
/// string form and are not represented here (docs/CONFIG.md's "read-today vs
/// schema-only" note).
fn parse_action(action: &str) -> Option<ReedlineEvent> {
    Some(match action {
        "history_search_backward" | "search_history" => ReedlineEvent::SearchHistory,
        "history_prev" | "previous_history" | "up_history" => ReedlineEvent::PreviousHistory,
        "history_next" | "next_history" | "down_history" => ReedlineEvent::NextHistory,
        "up" => ReedlineEvent::Up,
        "down" => ReedlineEvent::Down,
        "left" => ReedlineEvent::Left,
        "right" => ReedlineEvent::Right,
        "clear_screen" => ReedlineEvent::ClearScreen,
        "clear_scrollback" => ReedlineEvent::ClearScrollback,
        "menu" | "completion_menu" => ReedlineEvent::Menu("completion_menu".to_string()),
        "menu_next" => ReedlineEvent::MenuNext,
        "menu_previous" => ReedlineEvent::MenuPrevious,
        "open_editor" => ReedlineEvent::OpenEditor,
        "enter" => ReedlineEvent::Enter,
        "submit" => ReedlineEvent::Submit,
        "none" => ReedlineEvent::None,
        "backspace" => ReedlineEvent::Edit(vec![EditCommand::Backspace]),
        "delete" => ReedlineEvent::Edit(vec![EditCommand::Delete]),
        "clear" | "clear_line" => ReedlineEvent::Edit(vec![EditCommand::Clear]),
        "cut_word_left" => ReedlineEvent::Edit(vec![EditCommand::CutWordLeft]),
        "cut_word_right" => ReedlineEvent::Edit(vec![EditCommand::CutWordRight]),
        "complete" => ReedlineEvent::Edit(vec![EditCommand::Complete]),
        "undo" => ReedlineEvent::Edit(vec![EditCommand::Undo]),
        "redo" => ReedlineEvent::Edit(vec![EditCommand::Redo]),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_modifier_chords() {
        assert_eq!(
            parse_chord("ctrl-r"),
            Some((KeyModifiers::CONTROL, KeyCode::Char('r')))
        );
        assert_eq!(
            parse_chord("CTRL-R"),
            Some((KeyModifiers::CONTROL, KeyCode::Char('r'))),
            "chord parsing is case-insensitive"
        );
        assert_eq!(
            parse_chord("ctrl-alt-x"),
            Some((
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                KeyCode::Char('x')
            ))
        );
        assert_eq!(parse_chord("f5"), Some((KeyModifiers::NONE, KeyCode::F(5))));
        assert_eq!(parse_chord("tab"), Some((KeyModifiers::NONE, KeyCode::Tab)));
        assert_eq!(
            parse_chord("shift-tab"),
            Some((KeyModifiers::SHIFT, KeyCode::Tab))
        );
    }

    #[test]
    fn trailing_dash_is_the_dash_key() {
        assert_eq!(
            parse_chord("ctrl--"),
            Some((KeyModifiers::CONTROL, KeyCode::Char('-')))
        );
    }

    #[test]
    fn unknown_modifier_or_empty_chord_is_rejected() {
        assert_eq!(parse_chord("hyper-r"), None);
        assert_eq!(parse_chord(""), None);
    }

    #[test]
    fn parses_documented_actions() {
        assert_eq!(
            parse_action("history_search_backward"),
            Some(ReedlineEvent::SearchHistory)
        );
        assert_eq!(parse_action("menu_next"), Some(ReedlineEvent::MenuNext));
        assert_eq!(
            parse_action("backspace"),
            Some(ReedlineEvent::Edit(vec![EditCommand::Backspace]))
        );
        assert_eq!(parse_action("not_a_real_action"), None);
    }

    #[test]
    fn parse_bindings_reports_one_warning_per_bad_entry_and_keeps_good_ones() {
        let mut table = BTreeMap::new();
        table.insert("ctrl-r".to_string(), "history_search_backward".to_string());
        table.insert("bogus-chord-!!".to_string(), "up".to_string());
        table.insert("ctrl-g".to_string(), "not_a_real_action".to_string());
        let (bindings, warnings) = parse_bindings(&table);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].modifiers, KeyModifiers::CONTROL);
        assert_eq!(bindings[0].code, KeyCode::Char('r'));
        assert_eq!(warnings.len(), 2);
    }
}
