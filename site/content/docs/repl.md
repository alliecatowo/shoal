+++
title = "Interactive shell"
description = "Use the local REPL confidently: editing, completion, transcript values, jobs, history, paging, init files, and recovery."
weight = 40
template = "docs/page.html"

[extra]
eyebrow = "Interactive use"
group = "Shell & tools"
audience = "People using Shoal at a terminal"
status = "Current local REPL behavior"
toc = true
+++

Running `shoal` on a terminal starts the local REPL. It combines the language evaluator with a line editor, syntax highlighting, completion, persistent history, a prompt snapshot, local process-group job control, journaling, and transcript values.

This page describes the **local REPL**. A kernel-hosted agent session shares language semantics but has different task and PTY control; see [Agents, kernel, and MCP](@/docs/agents-kernel-mcp.md).

## The interactive loop


Open delimiters, trailing operators or commas, a trailing backslash, and a next line beginning with `.`, `catch`, or `else` keep the input open. This lets blocks and method chains span lines without a special multiline command.

```text
let report = (ls .)
  .where(.type == "file")
  .sort_by(.size)
  .reverse()

if report.is_empty() {
  "no files"
} else {
  report
}
```

## Transcript values: `it` and `out`

After each submitted evaluation, the REPL retains result values:

- `it` is the most recent result.
- `out[n]` addresses a prior transcript value.

```text
(ls .).where(.type == "file")
it.map(.name)
out[0]
```

These names are REPL-only. The parser rejects them in ordinary scripts so a file cannot accidentally depend on an invisible interactive transcript. Use explicit bindings in reusable code.

`undo out[n]` maps the transcript entry back to its journal entry when possible. This is especially useful after an interactive filesystem operation.

## Editing and completion

The editor supports Emacs and Vi modes, bracketed paste, syntax highlighting, history navigation, and a completion menu. Completion candidates come from the active context:

- language constructs and builtins;
- variables, functions, aliases, and methods;
- bundled and custom adapters;
- executables on `PATH`;
- relevant filesystem paths.

Configure matching:

```toml
[completion]
fuzzy = true
case_insensitive = true
max_results = 100
menu = true

[editor]
mode = "emacs" # or "vi"
bracketed_paste = true
```

When `completion.menu = false`, the current line-editor integration approximates menu-free completion by inserting unique matches or a shared prefix. If several candidates have no common prefix, the popup can still appear because the editor has no separate cycle-only completion path.

### Custom keybindings

Keybindings map a chord string to a curated action name:

```toml
[editor.keybindings]
"ctrl-r" = "history_search_backward"
"ctrl-l" = "clear_screen"
"alt-e" = "open_editor"
"ctrl-space" = "completion_menu"
```

Modifiers include `ctrl`/`control`, `alt`/`option`, `shift`, and `super`/`cmd`/`command`/`meta`. Named keys include arrows, tab, enter, escape, backspace, delete, home/end, page keys, insert, space, and function keys; a single character is also valid.

Recognized actions include history search and navigation, cursor directions, clear screen/scrollback, completion-menu navigation, open editor, submit, delete/backspace, clear line, word cuts, complete, undo, and redo. The line editor exposes more parameterized motions than Shoal's string schema can represent. Invalid chords or action names produce startup warnings instead of aborting the session.

## Interrupt and end-of-input

`Ctrl-C` is context-sensitive:

- while editing, it clears/cancels the current input line;
- while a foreground external runs, it signals the foreground process tree without terminating Shoal itself.

`Ctrl-D` on an empty input exits the REPL. An explicit `exit` or `quit` does the same; `exit N` selects a status.

## Foreground and background jobs

Append `&` to run work as a task, or use `spawn { ... }` for a language block:

```text
sleep 30s &
let build = spawn { cargo test }
jobs
build.is_done()
build.await()
```

Task methods include `await`/`wait`, `cancel`, `is_done`, `suspend`, `resume`, and `is_suspended`. For a local foreground external, `Ctrl-Z` stops its process group and places it in the job table:

```text
jobs
bg %1
fg %1
```

`fg <task-variable>` is rewritten to resume and await that task. Local process-group job control depends on Unix terminal facilities. A known preview limitation is that a stopped external resumed with `bg` may remain displayed as running after it exits until it is foregrounded again or the session ends; background completion tracking is not yet fully event-driven.

Kernel tasks are different: raw kernel suspend/resume controls process-backed tasks through their
owned process groups, while evaluator-only work returns `TASK_CONTROL_UNAVAILABLE`. MCP does not
expose those raw controls. This is not the local REPL's terminal-reattachment/`Ctrl-Z` model.

## History and journal

Line-editing history and the structured command journal are related but distinct:


History defaults to a file under `$XDG_STATE_HOME/shoal`, falling back to `~/.local/state/shoal`. It can deduplicate adjacent identical entries, ignore leading-space commands, and ignore configured patterns.

```toml
[history]
enabled = true
max_entries = 10000
dedup = true
ignore_space = true
ignore = ["*secret*", "exit"]
```

Inspect structured entries with `history` or `journal`. Rows include entry ID, timestamp, principal, source, success/status, and recorded effects. Common filters include `--head`, `--principal`, and `--limit`.

Use `undo` to reverse the latest eligible effect or `undo <entry-id>` for a specific one. Shoal checks fingerprints before applying an inverse so it refuses stale undo rather than overwriting unrelated later changes. [Filesystem, jobs, history, and undo](@/docs/filesystem-jobs-history.md) documents what is and is not reversible.

## Directory stack and jumping

The session owns its current directory:

```text
pwd
cd ./crates
pushd ../site
dirs
popd
cd -
```

`j`/`jump` use an interactive frecency database stored under the Shoal state directory. This is a REPL convenience rather than a portable script primitive.

## Paging

Paging is opt-in:

```toml
[render]
paging = "auto"
pager = "less -R"
```

When auto paging is enabled, stdout is a real terminal, and the **final result of a submitted REPL line** would exceed the terminal height (including wrapped lines), Shoal pipes that rendering through the configured pager. It falls back to `$PAGER`, then `less -R`. A missing or failing pager falls back to ordinary printing so output is not lost.

Paging does not apply to scripts, `-c`, piped source, intermediate values in a multi-statement submission, or an independently running live stream.

## Streams in the REPL

A bare stream renders as a stream descriptor; it does not begin an implicit live UI. Drive it with an explicit sink:

```text
every(1s).take(5).render()
watch(./src).take(10).collect()
```

This is an important difference from older design notes that described automatic live rendering. Explicit sinks make lifecycle and cancellation visible. See [Streams and channels](@/docs/streams-channels.md).

## Init files, aliases, and environment

Interactive init files run in order at session startup:

```toml
[init]
files = ["~/.config/shoal/init.shl"]

[aliases]
gs = "git status"

[env]
EDITOR = "hx"
RUST_BACKTRACE = "1"
```

Aliases and environment entries are also seeded in non-interactive evaluators. An alias is parsed into Shoal command structure rather than expanded as text. Keep init files deterministic and fast: a failure is surfaced during startup, and hidden state makes scripts harder to reproduce.

## Prompt snapshots

Before each read, Shoal collects a frozen context containing directory, last outcome, jobs, Reef state, principal/leash context, time, and Git information where applicable. Reedline can redraw from that snapshot on every keystroke without performing filesystem I/O or spawning Git each time. Git status collection occurs at most once per command in a repository, not per keypress.

Use `shoal prompt explain` to see the rendered modules and their timing. Prompt syntax and themes are in [Configuration and prompt](@/docs/configuration-prompt.md).

## Recovery habits

When an interactive command surprises you:

1. Inspect `it` or the relevant `out[n]` before rerunning it.
2. Check `.ok`, `.status`, `.stderr`, and `.out` on an outcome.
3. Run `journal --head=rm` (or another concrete command head) to identify recorded effects.
4. Use `undo <id>` only after checking the target entry.
5. Prefix an adapted command with `^` if you intentionally need raw argv behavior.
6. Keep another shell open while Shoal remains a preview.

For symptom-driven diagnosis, use [Troubleshooting](@/docs/troubleshooting.md).
