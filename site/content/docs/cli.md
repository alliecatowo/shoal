+++
title = "Command-line interface"
description = "Every Shoal entry point: interactive use, one-liners, scripts, stdin, formatting, diagnostics, completions, prompt tools, LSP, and MCP."
weight = 30
template = "docs/page.html"

[extra]
eyebrow = "Run Shoal"
group = "Shell & tools"
audience = "Users and automation authors"
status = "Current implementation"
toc = true
+++

The `shoal` executable is both the language runner and the dispatcher for developer integrations. Its default is intentionally context-sensitive: no arguments starts the REPL when stdin is a terminal, but reads source from stdin when input is piped.

## Synopsis

```text
shoal [OPTIONS] [SCRIPT [ARG ...]]

shoal -c SOURCE
shoal fmt [--check] [FILE ...]
shoal doctor [--json]
shoal completions <bash|zsh|fish>
shoal prompt <explain|bench|print> [--side SIDE] [--n N]
shoal lsp
shoal mcp
```

Global help and version:

```bash
shoal --help
shoal --version
```

## Dispatch rules


## Evaluate a one-liner

Use `-c` or `--command` when the source is part of the invocation:

```bash
shoal -c 'json.parse("{\"ready\":true}").ready'
shoal --command '(ls .).where(.type == "file").map(.name)'
```

Your invoking shell still interprets its own quoting before Shoal sees the string. Single-quote the outer source in Bash, Zsh, and similar shells when it contains Shoal double strings.

The non-interactive default is quiet: bare command output is shown, intermediate pure expression results are suppressed, and the final value is rendered. Set `render.echo = "commands"` to suppress even a final expression, or `"all"` to render every top-level result.

## Run a script

Pass a `.shl` file as the first non-option argument:

```bash
shoal scripts/release.shl staging --dry-run
```

The remaining arguments become a language-level `args` list of path-like values. Use normal value methods rather than expecting POSIX positional variables:

```text
assert(!args.is_empty(), "usage: release.shl ENV")

let environment = args.first().str()
```

`--` ends host option parsing when a script name or script argument could be mistaken for an option:

```bash
shoal -- ./-audit.shl --literal-argument
```

For runner-dispatched non-Shoal files inside the language, use `run ./tool.py`; see [Reef environments](@/docs/reef.md).

## Read source from stdin

With no script and non-terminal stdin, Shoal reads the entire input as language source:

```bash
printf 'let x = 20\nx + 22\n' | shoal
```

This is source input, not data input to a program running inside Shoal. To give data to an external program, write a Shoal expression using `feed`.

## Exit status

Shoal uses a small host-level convention:

| Condition | Exit status |
|---|---:|
| Successful evaluation | `0` |
| Uncaught external `cmd_failed` with child status `1..=255` | that child status |
| Other uncaught evaluation/runtime error, or child status absent/out of range | `1` |
| Parse error | `2` |
| `exit N` | `N` |

An external process's non-zero status raises `cmd_failed` in command-statement position. The non-interactive host propagates an attached status from `1` through `255`; a signal-only failure, absent status, or out-of-range status falls back to `1`. Capture an outcome when you need to inspect or deliberately transform that status before exiting.

```text
let result = (^some-command)
if !result.ok { exit (result.status) }
```

## Format source

`shoal fmt` parses and formats Shoal source. With file arguments it preflights every input before
making changes, then replaces each changed file atomically:

```bash
shoal fmt src/main.shl scripts/release.shl
```

With no files it reads source from stdin and writes formatted source to stdout:

```bash
printf 'let   x=1\nx+1\n' | shoal fmt
```

Use check mode in CI. It makes no edits and exits `1` if any input would change:

```bash
shoal fmt --check scripts/*.shl
```

The current AST does not retain free comments or shebangs. The formatter therefore leaves a parsed
source unchanged when its token-aware safety pass finds either one; it never silently deletes them.
A semantic `#` inside a quoted value, record key, `use` path, or raw command word is not mistaken for
a comment and does not block formatting.

File rewrites refuse symbolic links instead of replacing the link itself. On Unix they preserve the
complete permission mode and refuse files owned by another user or carrying extended metadata that
atomic replacement would discard (including Linux ACL xattrs and macOS ACLs). Linux security labels
are accepted only when a replacement created in the same directory receives an identical label.
Shoal also refuses duplicate input aliases and, on Unix, any multiply-linked inode: replacing one
hard-link name would silently detach it from the other names. Identity is established for every
input before the first rewrite. Shoal syncs both the replacement contents and its parent directory.
These checks are intentionally stricter than script execution; pass a singly-linked regular target
path explicitly when a link should be formatted. A failure during a later write can still leave
already committed earlier files changed, because there is no portable transaction spanning multiple
directory entries. Filesystem replacement races after identity preflight are detected where the
platform exposes stable metadata, but this formatter is not a general hostile-directory transaction.

## Run diagnostics

`shoal doctor` reports host and Shoal integration health:

```bash
shoal doctor
shoal doctor --json
```

The JSON form is intended for automation. Reef has a separate scope-aware health check, `reef doctor`, which checks lock drift, orphaned entries, and ambient shadowing.

## Generate shell completions

Generate completion source for the supported host shell, then install it using that shell's normal mechanism:

```bash
shoal completions bash > ~/.local/share/bash-completion/completions/shoal
shoal completions zsh > ~/.zfunc/_shoal
shoal completions fish > ~/.config/fish/completions/shoal.fish
```

These generated scripts share one checked vocabulary with the CLI help surfaces, including every
top-level command, root option, kernel/prompt action, and command-specific option. Bash, Zsh, and
Fish syntax is smoke-tested when the corresponding shell is installed. These completions invoke the
`shoal` executable from another shell; Shoal's own REPL completion engine is configured under
`[completion]` and understands language names, builtins, adapters, methods, variables, and `PATH`
programs.

## Inspect the prompt

The prompt dispatcher can render a side, explain its modules and timing, or benchmark it:

```bash
shoal prompt print --side left
shoal prompt explain --side right
shoal prompt bench --side left --n 10000
```

Valid sides are `left`, `right`, `continuation`, and `transient`. `--n` applies to `bench`. Prompt rendering uses a frozen session snapshot so per-keystroke redraws perform no I/O; context collection happens between commands. See [Configuration and prompt](@/docs/configuration-prompt.md).

## Start the language server

```bash
shoal lsp
```

This launches the `shoal-lsp` stdio server. Configure an editor to start `shoal lsp` for Shoal source files. The subcommand looks up the companion executable through `PATH`, so `shoal-lsp` must be installed and discoverable there. The same rule applies to `shoal mcp` and its `shoal-mcp` companion.

## Start the MCP bridge

```bash
shoal mcp
```

This launches the stdio `shoal-mcp` bridge. It connects to a named kernel session and currently auto-starts `shoal-kernel` when the socket is unavailable. Set a non-empty `SHOAL_NO_AUTOSTART` to require an already-running kernel instead. Session, socket, and token options are available on the companion `shoal-mcp` binary and through `SHOAL_SESSION`, `SHOAL_SOCKET`, and `SHOAL_TOKEN`.

The MCP server is not simply a remote `-c`: it adds session-scoped references, plans and approvals, journal and task resources, notifications, and rendered PTY tools. Read [Agents, kernel, and MCP](@/docs/agents-kernel-mcp.md) before granting an agent access.

## Companion binaries

The workspace currently builds these public or operational executables:

| Binary | Role |
|---|---|
| `shoal` | REPL, language runner, and developer-command dispatcher |
| `shoal-doctor` | standalone diagnostics |
| `shoal-history` | history/journal utility |
| `shoal-kernel` | persistent multi-principal session host |
| `shoal-mcp` | stdio MCP-to-kernel bridge |
| `shoal-lsp` | language server |
| `shoal-secret` | secret-handling helper |
| `shoal-token` | token utility for authenticated sessions |
| `shoal-landlock-helper` | Linux Landlock sandbox helper |
| `shoal-sandbox-exec` | macOS sandbox-exec helper |

The helpers are not all required for a basic local REPL. Install the complete workspace set when exercising kernel policy, agents, or platform sandboxing.

## Environment used by the host

Common host-level variables include:

| Variable | Meaning |
|---|---|
| `XDG_CONFIG_HOME` | user configuration root; fallback `~/.config` |
| `XDG_STATE_HOME` | history, journal, and jump-state root; fallback `~/.local/state` |
| `XDG_RUNTIME_DIR` | preferred kernel socket root |
| `NO_COLOR` | disable colored rendering by presence |
| `PAGER` | pager fallback when `render.paging = "auto"` |
| `SHOAL_NO_AUTOSTART` | disable MCP kernel auto-start when non-empty |
| `SHOAL_CAPTURE_CAP_BYTES` | in-memory process capture cap |
| `SHOAL_CAPTURE_SPILL_CAP_BYTES` | journal/CAS spill cap |

Configuration-specific `SHOAL_*` overrides are listed in [Configuration and prompt](@/docs/configuration-prompt.md). Treat any variable not documented there as an operational interface that may still move during the preview.
