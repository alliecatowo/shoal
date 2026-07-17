+++
title = "Keybinding reference"
description = "Configure Emacs or Vi editing, parse key chords, map every supported action, and diagnose terminal/keybinding conflicts."
weight = 155
template = "docs/page.html"

[extra]
eyebrow = "Interactive shell"
group = "Reference"
audience = "Interactive Shoal users"
status = "Current Reedline mapping"
toc = true
+++

Shoal's interactive editor uses Reedline's Emacs or Vi defaults and layers user mappings from `[editor.keybindings]` on top.

```toml
[editor]
mode = "emacs"
bracketed_paste = true

[editor.keybindings]
"ctrl-r" = "history_search_backward"
"ctrl-l" = "clear_screen"
"ctrl-alt-e" = "open_editor"
"shift-tab" = "menu_previous"
```

Bad entries warn and are skipped; they do not prevent the shell from starting or discard valid siblings.

## Editing modes

```toml
[editor]
mode = "emacs" # default
```

or:

```toml
[editor]
mode = "vi"
```

The mode selects Reedline's default binding table:

- Emacs mode: custom bindings are added to the default Emacs table.
- Vi mode: each custom binding is added to **both** insert and normal tables; the current config cannot target only one Vi mode.

Shoal explicitly maps unmodified Tab to the completion menu/next behavior before applying custom mappings. A user mapping for `"tab"` is added afterward and can replace that default.

Changing config requires a new interactive process; there is no live keymap reload.

## Chord grammar

A chord is:

```text
[modifier-]...[modifier-]key
```

Examples:

```text
r
ctrl-r
ctrl-alt-x
shift-tab
cmd-k
f5
ctrl--
```

Chord parsing is ASCII case-insensitive, so `CTRL-R` and `ctrl-r` are equivalent. Action names are case-sensitive.

### Modifiers

| Canonical | Accepted spellings |
| --- | --- |
| Control | `ctrl`, `control` |
| Alt | `alt`, `option` |
| Shift | `shift` |
| Super | `super`, `cmd`, `command`, `meta` |

Multiple modifiers are ORed:

```toml
"ctrl-alt-shift-x" = "clear_line"
```

Unknown modifiers reject the chord. `hyper-r`, for example, warns and is skipped.

### Named keys

| Key | Accepted spellings |
| --- | --- |
| Tab | `tab` |
| Enter | `enter`, `return` |
| Escape | `esc`, `escape` |
| Backspace | `backspace` |
| Delete | `delete`, `del` |
| Arrows | `left`, `right`, `up`, `down` |
| Line/document movement | `home`, `end`, `pageup`, `pagedown` |
| Insert | `insert` |
| Space | `space` |
| Function key | `f0` through `f99` are syntactically accepted; actual terminal support is narrower. |
| Character | Any single-byte character, such as `a`, `/`, `+`, or `,`. |

Only a one-byte character is accepted by the current parser. A multibyte Unicode key name is not a valid chord even if a terminal could emit it.

### Binding the dash key

Because `-` separates modifiers, a trailing dash has a special rule:

```toml
"-" = "none"
"ctrl--" = "cut_word_left"
```

The final dash is the character key. `ctrl--` means Ctrl plus `-`, not an empty key segment.

### Terminal normalization caveats

Configuration describes the event Shoal expects, not what every terminal sends:

- many terminals cannot distinguish some Ctrl-letter/control-byte combinations;
- Shift with punctuation may arrive as the shifted character without a Shift modifier;
- Super/Cmd is often consumed by the desktop/terminal and never reaches the application;
- Alt may arrive as Escape-prefixed input depending on terminal settings;
- tmux/screen can translate or reserve chords;
- function keys beyond the physical set are unlikely to be emitted.

If a syntactically valid mapping does nothing, test whether the terminal/tmux receives/forwards it before changing Shoal.

## Supported actions

### History

| Canonical action | Aliases | Reedline event |
| --- | --- | --- |
| `history_search_backward` | `search_history` | Search history. |
| `history_prev` | `previous_history`, `up_history` | Previous history entry. |
| `history_next` | `next_history`, `down_history` | Next history entry. |

Example:

```toml
[editor.keybindings]
"ctrl-r" = "history_search_backward"
"ctrl-p" = "history_prev"
"ctrl-n" = "history_next"
```

Line history is the editor's command recall store, distinct from the structured execution journal.

### Directional editor events

| Action | Event |
| --- | --- |
| `up` | Generic editor Up. |
| `down` | Generic editor Down. |
| `left` | Generic editor Left. |
| `right` | Generic editor Right. |

These are Reedline events rather than fixed cursor-edit commands; their behavior can depend on menu/multiline/editor state.

### Screen

| Action | Meaning |
| --- | --- |
| `clear_screen` | Clear/redraw the visible screen event. |
| `clear_scrollback` | Clear terminal scrollback event. |

```toml
"ctrl-l" = "clear_screen"
"ctrl-alt-l" = "clear_scrollback"
```

Terminal support for clearing scrollback varies.

### Completion menu

| Canonical action | Alias | Meaning |
| --- | --- | --- |
| `menu` | `completion_menu` | Open/select the menu named `completion_menu`. |
| `menu_next` | — | Advance menu selection. |
| `menu_previous` | — | Move to previous menu selection. |
| `complete` | — | Reedline's unparameterized Complete edit command. |

`menu` targets Shoal's configured `completion_menu`. `complete` is an edit command and is not identical to explicitly opening/navigating the named popup.

Example:

```toml
"tab" = "menu_next"
"shift-tab" = "menu_previous"
"ctrl-space" = "menu"
```

If `[completion].menu = false`, Reedline is configured for quicker/partial completion where possible, but a popup can still appear when multiple candidates have no shared prefix.

### Submission and neutral action

| Action | Meaning |
| --- | --- |
| `enter` | Reedline Enter event (works with Shoal's multiline validator). |
| `submit` | Reedline Submit event. |
| `none` | Consume/map to no editor action. |

Use `enter` for the normal return-key behavior unless you specifically understand Reedline's Submit distinction and Shoal multiline validation.

### External editor

| Action | Meaning |
| --- | --- |
| `open_editor` | Open the current buffer in the configured/external editor flow. |

```toml
"ctrl-x" = "open_editor"
```

The host editor environment/config still determines whether an editor can launch.

### Editing commands

| Canonical action | Aliases | Underlying edit command |
| --- | --- | --- |
| `backspace` | — | Backspace. |
| `delete` | — | Delete. |
| `clear` | `clear_line` | Clear buffer. |
| `cut_word_left` | — | Cut word left. |
| `cut_word_right` | — | Cut word right. |
| `complete` | — | Complete. |
| `undo` | — | Undo edit. |
| `redo` | — | Redo edit. |

Only this curated unparameterized subset is configurable through strings. Reedline exposes many motion/selection/parameterized edit commands that Shoal's config parser does not map today.

## Complete action-name list

For copy/paste/reference:

```text
history_search_backward  search_history
history_prev             previous_history  up_history
history_next             next_history      down_history
up                       down              left             right
clear_screen             clear_scrollback
menu                     completion_menu
menu_next                menu_previous
open_editor
enter                    submit            none
backspace                delete
clear                    clear_line
cut_word_left            cut_word_right
complete                 undo              redo
```

Whitespace/case is not normalized for action values. Use the lowercase exact spellings.

## Example: compact Emacs-style customizations

```toml
[editor]
mode = "emacs"

[editor.keybindings]
"ctrl-r" = "history_search_backward"
"ctrl-p" = "history_prev"
"ctrl-n" = "history_next"
"ctrl-l" = "clear_screen"
"ctrl-w" = "cut_word_left"
"alt-d" = "cut_word_right"
"ctrl-_" = "undo"
"ctrl-alt-_" = "redo"
"ctrl-x" = "open_editor"
"shift-tab" = "menu_previous"
```

Whether `ctrl-_` is distinguishable depends on terminal encoding; choose another chord if not.

## Example: Vi mode with shared custom chords

```toml
[editor]
mode = "vi"

[editor.keybindings]
"ctrl-r" = "history_search_backward"
"ctrl-l" = "clear_screen"
"ctrl-alt-e" = "open_editor"
```

These three custom chords are installed in both Vi insert and normal tables. There is currently no config such as `[editor.keybindings.insert]`/`normal`.

## Overrides and conflicts

Custom bindings are added after defaults. A matching `(modifiers, key)` replaces/overrides the prior table entry according to Reedline's keybinding map.

Potential conflicts:

1. a custom Tab replaces Shoal's explicit completion Tab mapping;
2. Vi custom chords apply to both modes and can replace a valuable normal-mode command;
3. two textual chords that normalize to the same event (`ctrl-r` and `control-r`) produce two additions in sorted config-map iteration; avoid aliases for the same chord because the final winner is not a useful contract to rely on;
4. terminal-level shortcuts can mask the application mapping;
5. action aliases map to exactly the same Reedline event and add no distinct semantics.

Keep one canonical spelling per chord.

## Warnings and recovery

Invalid chord warning:

```text
warning: editor.keybindings: unrecognized key chord `hyper-r`
```

Invalid action warning:

```text
warning: editor.keybindings.ctrl-g: unrecognized action `cancel_everything`
```

The entry is skipped. To recover from a broken/unusable valid mapping:

1. exit the shell from another available chord or terminal signal;
2. edit `shoal.toml` with another shell/editor;
3. remove/comment the custom table or launch with a clean temporary config environment;
4. restart Shoal.

Because invalid mappings never hard-fail, a typo can look like “the default still works.” Read startup warnings.

## Debug checklist

1. Confirm `[editor.keybindings]` is nested under `[editor]` correctly.
2. Quote TOML keys containing dashes.
3. Use a lowercase exact action name.
4. Reduce to a simple mapping such as `"ctrl-l" = "clear_screen"`.
5. Restart the interactive process after editing.
6. Check startup warnings and the active config layer.
7. Test outside tmux/screen.
8. Check terminal/OS shortcut settings.
9. In Vi mode, test both insert and normal behavior.
10. Restore defaults by removing the custom entry/table.

Configuration discovery and editor/history/completion options are in [Configuration and prompt](@/docs/configuration-prompt.md).
