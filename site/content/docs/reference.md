+++
title = "Reference inventory and glossary"
description = "The map of Shoal documentation, binaries, files, environment variables, adapter catalog, protocol surfaces, and shared terminology."
weight = 290
template = "docs/page.html"

[extra]
eyebrow = "Reference hub"
group = "Reference"
audience = "All Shoal users and contributors"
status = "Inventory for the current source tree"
toc = true
+++

Use this page to find the authoritative chapter for a symbol or subsystem. It also inventories the shipped binaries, 49 bundled adapter heads, important files/environment variables, and the terms shared by the language, shell, Reef, kernel, and MCP layers.

## Find the right chapter

### Start and orient

| Question | Chapter |
| --- | --- |
| How do I build and run something useful? | [Quickstart](@/docs/quickstart.md) |
| Why is a line parsed as a command or expression? | [Command/expression model](@/docs/mental-model.md) |
| How does the interactive editor/session behave? | [Interactive shell](@/docs/repl.md) |
| What does every `shoal` CLI form do? | [Command-line interface](@/docs/cli.md) |
| How do I translate Bash/zsh/fish/Nushell habits? | [Migrating from traditional shells](@/docs/migration-from-shells.md) |
| Show practical patterns. | [Recipes](@/docs/recipes.md) |

### Language

| Subject | Chapter |
| --- | --- |
| Lexical syntax, literals, comments, names | [Syntax and literals](@/docs/language-syntax.md) |
| Types, equality, access, coercion, methods | [Values, types, and methods](@/docs/language-values.md) |
| Functions, closures, aliases, control flow, modules | [Functions, control flow, and modules](@/docs/language-functions-control.md) |
| Outcomes, raised errors, try/catch, assertions | [Outcomes and errors](@/docs/language-errors-outcomes.md) |
| Lists, records, tables, higher-order transforms | [Collections and tables](@/docs/collections-tables.md) |
| Full grammar, operators, patterns, interpolation | [Grammar reference](@/docs/grammar-reference.md) |
| Every runtime method by receiver | [Value-method reference](@/docs/value-methods-reference.md) |
| Every builtin command | [Builtin reference](@/docs/builtins-reference.md) |
| JSON/YAML/TOML/CSV/math/HTTP/OS/config/env/secret | [Namespace reference](@/docs/namespaces-reference.md) |

### Shell and tools

| Subject | Chapter |
| --- | --- |
| External argv, outcomes, feed, redirects, interpreter blocks | [External commands](@/docs/external-commands.md) |
| Adapter contract, schemas, parsers, effects, full catalog | [Command adapters](@/docs/adapters.md) |
| Files, cwd, tasks, jobs, journal, CAS, undo | [Filesystem, jobs, history, and undo](@/docs/filesystem-jobs-history.md) |
| Pull/live streams, channels, handlers, backpressure | [Streams and channels](@/docs/streams-channels.md) |
| Tool manifests/providers/locks/PATH/hermetic mode | [Reef tool resolution](@/docs/reef.md) |
| Config discovery/precedence/editor/history/render/prompt | [Configuration and prompt](@/docs/configuration-prompt.md) |
| Every configurable key chord/action | [Keybinding reference](@/docs/keybindings-reference.md) |

### Agents and protocol

| Subject | Chapter |
| --- | --- |
| Architecture/setup/sessions/tokens/tool overview | [Agents, kernel, and MCP](@/docs/agents-kernel-mcp.md) |
| Exact schemas/defaults/results for 13 MCP tools | [MCP tool reference](@/docs/mcp-tools-reference.md) |
| Resource URIs/templates/events/cursors/subscriptions | [MCP resources and events](@/docs/mcp-resources-events.md) |
| Raw Unix-socket JSON-RPC methods/wire values/errors | [Kernel protocol](@/docs/kernel-protocol.md) |
| Reliable orchestration patterns | [Agent and MCP workflows](@/docs/mcp-workflows.md) |
| Threat model, Leash, enforcement, P0 defects | [Security and trust boundaries](@/docs/security.md) |
| Kernel/MCP/LSP/token/secret/history/doctor utilities | [Companion CLI reference](@/docs/companion-cli-reference.md) |

### Operate and contribute

| Subject | Chapter |
| --- | --- |
| What works and what does not? | [Current status and limits](@/docs/status-limits.md) |
| What should be built next, in what order? | [Roadmap](@/docs/roadmap.md) |
| Diagnose a symptom. | [Troubleshooting](@/docs/troubleshooting.md) |
| How is the implementation constructed? | [Internal architecture docs](@/internals/_index.md) |

## Binary inventory

| Executable | Primary interface |
| --- | --- |
| `shoal` | REPL, scripts/stdin/`-c`, `fmt`, `doctor`, `lsp`, `mcp`, completions, prompt tools. |
| `shoal-kernel` | Long-lived Unix-socket JSON-RPC evaluator/session host. |
| `shoal-mcp` | MCP stdio facade. |
| `shoal-lsp` | LSP stdio server. |
| `shoal-token` | Token create/list/revoke. |
| `shoal-secret` | Secret set/list/delete. |
| `shoal-history` | Journal query/show/pin/unpin/GC/undo. |
| `shoal-doctor` | Installation diagnostics. |
| `shoal-sandbox-exec` | Internal filesystem-sandbox child launcher. |
| `shoal-landlock-helper` | Low-level enforcement test/helper. |

The packages are separate. See [install commands](@/docs/companion-cli-reference.md#build-or-install-the-complete-set).

## File and directory inventory

| Name | Location/default | Role |
| --- | --- | --- |
| Shoal script | `*.shl` | Native source/module/runner file. |
| User config | `$XDG_CONFIG_HOME/shoal/shoal.toml` or `~/.config/shoal/shoal.toml` | User layer, prompt/editor/history/reef/etc. |
| Project config | project-discovered `shoal.toml` according to config rules | Project layer. |
| Reef manifest | `.reef.toml` | Native project tool constraints/runners/hermetic intent. |
| Reef lock | `reef.lock` beside nearest native scope | Resolved tool/provider/path/hash bindings. |
| Leash policy | usually `$XDG_CONFIG_HOME/shoal/leash.toml` | Principal grants/approval/sandbox intent. |
| Built-in adapters | repository/package `adapters/*.toml` | Shipped command schemas. |
| User adapters | `$XDG_CONFIG_HOME/shoal/adapters` | Custom command schemas. |
| Journal/CAS | `$XDG_STATE_HOME/shoal` or `~/.local/state/shoal` for main/kernel | SQLite/WAL metadata and content blobs. |
| Token store | kernel `<state-dir>/tokens.json` | Keyed token digests/metadata. |
| Line history | configurable; normally XDG state | Reedline recall store, distinct from journal. |
| Secret directory | XDG data `shoal/secrets` (or evaluator override) | `master.key` + authenticated encrypted envelope. |
| Kernel socket | XDG runtime or UID-qualified temp fallback | Local IPC boundary. |

Do not assume all companion defaults agree: `shoal-history`/doctor use XDG data in places where shell/kernel use XDG state. The [path matrix](@/docs/companion-cli-reference.md#xdg-path-matrix) is authoritative.

## Environment-variable inventory

### Standard/XDG

| Variable | Role |
| --- | --- |
| `HOME` | Fallback anchor for config/state/data and `~/` grants. |
| `XDG_CONFIG_HOME` | User config/policy/adapters. |
| `XDG_STATE_HOME` | Main shell/kernel state/journal/token default. |
| `XDG_DATA_HOME` | Secret store and current history/doctor companion defaults. |
| `XDG_RUNTIME_DIR` | Preferred kernel socket root. |
| `TMPDIR` | UID-qualified socket fallback and temporary trash/files. |
| `NO_COLOR` | Disable color where the host honors it. |
| `EDITOR` / `VISUAL` | External editor behavior. |

### Kernel and MCP

| Variable | Role |
| --- | --- |
| `SHOAL_SOCKET` | Explicit MCP/kernel-client socket selection. |
| `SHOAL_SESSION` | MCP attachment session. |
| `SHOAL_TOKEN` | MCP bearer token—secret, never log. |
| `SHOAL_NO_AUTOSTART` | Nonempty disables MCP's detached kernel startup. |
| `SHOAL_TOKEN_STORE` | `shoal-token` CLI store override; kernel ignores it. |
| `SHOAL_KERNEL` | Main config/env compatibility setting for kernel enablement. |
| `SHOAL_KERNEL_SESSION` | Main config/env compatibility setting for kernel session. |
| `SHOAL_LEASH_POLICY` | Main configuration override for policy path where supported. |
| `SHOAL_JOURNAL_ENABLED` | Main configuration override. |

### Capture, adapters, and secrets

| Variable | Role |
| --- | --- |
| `SHOAL_CAPTURE_CAP_BYTES` | Resident process-output capture cap (default 64 MiB). |
| `SHOAL_CAPTURE_SPILL_CAP_BYTES` | CAS spill cap (default 1 GiB). |
| `SHOAL_ADAPTER_PATH` | Replacement custom adapter search path; not simple append. |
| `SHOAL_SECRET_DIR` | Evaluator secret-store override; `shoal-secret` CLI ignores it. |

### Configuration overrides

The configuration loader also recognizes environment overrides including:

```text
SHOAL_EDITOR_MODE
SHOAL_EDITOR_BRACKETED_PASTE
SHOAL_COMPLETION_MENU
SHOAL_COMPLETION_FUZZY
SHOAL_COMPLETION_CASE_INSENSITIVE
SHOAL_COMPLETION_MAX_RESULTS
SHOAL_HISTORY
SHOAL_HISTORY_MAX_ENTRIES
SHOAL_HISTORY_FILE
SHOAL_HISTORY_DEDUP
SHOAL_RENDER_WIDTH
SHOAL_RENDER_COLOR
SHOAL_RENDER_PAGING
SHOAL_RENDER_PAGER
SHOAL_RENDER_ECHO
SHOAL_PROMPT
SHOAL_PROMPT_TEMPLATE
SHOAL_NERD_FONT
```

Use [Configuration and prompt](@/docs/configuration-prompt.md) for value parsing and precedence; an inventory alone is not sufficient to configure them correctly.

## Bundled adapter inventory (49 heads)

An adapter head is the Shoal command name selected before subcommand parsing. Availability still depends on the external executable/platform.

### Source control and development

```text
cargo  git  gh  go  jj  rg  rustup
```

### Language runtimes and interpreters

```text
bash  deno  jq  node  python  ruby  yq
```

### JavaScript/Python/package tools

```text
brew  bun  npm  pip  pnpm  uv  yarn
```

### Containers, orchestration, and infrastructure

```text
docker  helm  kubectl  podman  terraform
```

### Cloud/project services

```text
aws  gcloud
```

### Filesystem, archive, and data

```text
curl  df  du  env  fd  findmnt  sqlite3  stat  tar  unzip  zip
```

### Linux/system inspection

```text
ip  journalctl  lsblk  lscpu  ps  ss  systemctl  systemd-analyze  vmstat  who
```

Count check:

```text
7 + 7 + 7 + 5 + 2 + 11 + 10 = 49
```

The [adapter catalog](@/docs/adapters.md#bundled-catalog-at-a-glance) documents each head's class, structured subcommands/output, and caveats. A listed adapter does not mean the tool is installed or every subcommand is structured.

## Adapter parser inventory

```text
json
ndjson
csv
tsv
z-records
porcelain-v2
cols
cols2
tsv-headerless
lines
kv
none
```

See [Structured output parsers](@/docs/adapters.md#structured-output-parsers) for exact behavior.

## MCP inventory

### Tools (13)

```text
shoal_exec
shoal_plan
shoal_apply
shoal_get
shoal_journal
shoal_cancel
shoal_cap_request
shoal_pty_open
shoal_pty_send
shoal_pty_read
shoal_pty_resize
shoal_pty_close
shoal_pty_list
```

### Stable resource roots (6)

```text
shoal://journal
shoal://jobs
shoal://session/cwd
shoal://session/env
shoal://session/reef
shoal://pty
```

Dynamic resources add tasks, plans, and PTYs. Transcript/content values are reached from returned refs/templates.

### Static event channels (4)

```text
session.transcript
journal
approval
render
```

Dynamic forms are `task.{id}` and `user.{name}`. There is no currently advertised Reef channel.

## Kernel method inventory

```text
session.attach  session.env  session.reef
parse           complete     explain
exec            value.get    blob.get
journal.query
task.list       task.get     task.await
task.cancel     task.suspend task.resume
plan.get        plan.list    plan.apply
cap.request
pty.open        pty.send     pty.read
pty.resize      pty.close    pty.list
events.read     events.publish
events.subscribe events.unsubscribe
```

`journal.query` and `cap.request` currently lack the required attachment gate; treat that as a security defect, not a pre-auth API. See [Kernel method index](@/docs/kernel-protocol.md#method-index).

## Short-reference inventory

| Form | Meaning | Lifetime/scope |
| --- | --- | --- |
| `out:N` | Transcript value/error | Named session, live kernel/evaluator. |
| `task:N` | Background/timed task | Named session, live kernel. |
| `pty:N` | Interactive PTY | Named session, live kernel. |
| `plan:FULL_DIGEST:OBJECT_ID` | Stored plan | Live kernel; immutable caller/content-bound object, lost on restart. |
| `val:blake3:HASH` | Content-addressed value/blob | State-store/CAS retention. |

Equivalent resource URIs use `shoal://out/N`, `shoal://task/N`, etc. Never persist the first four as durable business IDs.

## Error-code namespaces

Shoal has two distinct error namespaces:

1. kernel/MCP JSON-RPC numeric codes (`-32602`, `-32011`, etc.);
2. language error-value string codes (`type_error`, `cmd_failed`, etc.).

Do not compare a language code to an RPC number. [Kernel errors](@/docs/kernel-protocol.md#error-codes) and [language errors](@/docs/language-errors-outcomes.md) document their control paths.

## Glossary

### Adapter

A TOML declaration that gives a command head typed parameters, invocation rewriting, output parser/schema, accepted statuses, class, and planning effects. It wraps an external executable; it is not the executable or an OS sandbox.

### Addressable value

A value stored behind a short ref/resource URI so a client can retrieve a field or slice without re-executing source.

### AST

Abstract syntax tree produced by Shoal parsing. Kernel attach/parse currently reports AST vocabulary version 2.

### CAS

Content-addressed store. Journal outputs/large bytes can be stored by BLAKE3 hash and later retrieved while retained. A hash does not imply authorization or permanence.

### Channel

A named event conduit. Language/wire clients may publish only `user.*`; kernel owns semantic channels. A channel event has per-channel sequence/timestamp/payload.

### Command head

The first command-shaped word used for resolution, such as `git` in `git status`. It can resolve to a lexical function/alias, builtin, Reef tool, adapter, external program, or interpreter path according to current precedence.

### Command mode

Parser/evaluator reading where an unbound statement-head word is a command and subsequent words are arguments. It coexists with expression mode; it is not a separate shell language.

### Effect

Planner description such as filesystem read/write/delete, process spawn, network connect/listen, environment/secret/session/journal/time, or opaque. Effects are analysis/policy data, not proof of native program behavior.

### Elision

Replacement of a large wire value with type/count/schema/preview/render-head plus URI, preserving addressability while bounding context.

### Evaluator

The runtime object holding lexical/session state, cwd/env, Reef resolver, ports, journal, cancellation, event bus, and command execution. The local REPL and each named kernel session own separate evaluators.

### Expression mode

Evaluation of names/literals/operators/calls/methods. Parenthesizing a command places it at a value boundary inside an expression.

### Feed

Explicit serialization of a finite value to a child process's stdin. It is the byte-boundary bridge, not a typed pipeline operator.

### Hermetic

Context-specific. Reef hermetic mode removes ambient PATH tail. Leash `hermetic=true` requests fail-closed sandbox behavior for represented dimensions. Neither alone is a full hermetic container.

### Journal

SQLite/WAL structured execution record with source/AST/effects/principal/status/output descriptors and optional undo inverses. Distinct from editor line history.

### Leash

Principal policy/effect evaluator plus OS sandbox selection. It can allow/deny/request approval and apply filesystem confinement where supported.

### Outcome

External/builtin process result containing status, success, signal, semantic `out`, stdout/stderr, duration, PID, command, and optional source span. Statement/value position controls whether non-ok raises.

### Plan

Derived effects/reversibility/estimates/verdict for source before spawn. Stored plans can be approved/applied. Current short identity/approval security gaps are documented prominently.

### Principal

Identity attached to a kernel connection: token-supplied agent string or tokenless local `uid:<euid>`. Leash policy keys by principal.

### PTY

Pseudoterminal session for interactive programs. MCP exposes a rendered emulator screen and semantic key input, not raw ANSI history.

### Reef

Tool constraint/resolution/lock/provider/PATH subsystem. It selects what executable/version/hash a command should use; Leash governs what behavior is allowed.

### Reference (`ref`)

A compact handle such as `out:17` or `task:9`. Meaning depends on kind and lifetime; it is not automatically durable, secret, collision-proof, or an authorization token.

### Session

A named kernel evaluator namespace sharing bindings/cwd/env/transcript/tasks/PTys/Reef state. It is currently a collaboration boundary, not principal isolation.

### Statement position

Top-level command/control context where a non-ok outcome normally raises `cmd_failed` and aborts subsequent statements.

### Stream

Lazy/single-consumption sequence, potentially live. Live sources have bounded/coalescing behavior; streams currently do not chunk over the kernel wire or feed a process incrementally.

### Task

Background evaluator/kernel execution with lifecycle/cancellation/result reference. Timeout can return ongoing work as a task rather than terminate it.

### Value position

Expression/capture context where a failed external outcome remains inspectable as a value.

### Wire value

JSON object tagged by `$` that represents a Shoal value across the kernel/MCP boundary. Secrets carry only names; large values can become `$:"ref"`.

## Snapshot notes

- Workspace/package version is currently `0.1.0`; contracts are preview.
- Conformance corpus: 1,310 cases, 1,306 pass, 4 explicit host/harness skips as of 2026-07-16.
- Bundled adapter heads: 49.
- MCP tools: 13.
- MCP protocol initialization version: `2025-06-18`.
- Kernel serialized AST version: 2.
- Supported deployment platforms: Linux/macOS preview; Windows unsupported.

For changes since this inventory was written, prefer the source registries/tests and update this page in the same change.
