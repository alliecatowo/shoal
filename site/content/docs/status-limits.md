+++
title = "Current status and limits"
description = "A dated, source-verified implementation matrix and an honest account of Shoal's security, protocol, language, platform, and operational limitations."
weight = 240
template = "docs/page.html"

[extra]
eyebrow = "Project status"
group = "Project"
audience = "Evaluators, adopters, operators, and contributors"
status = "Snapshot: 2026-07-16"
toc = true
+++

Shoal is a substantial, working preview—not a production-hardened login shell or multi-tenant agent sandbox. The language, structured shell, adapters, Reef resolver, journal/undo, kernel, MCP tools/resources/events, PTYs, LSP, prompt, and configuration system all execute real code today. The current security model still requires a fully trusted local kernel socket, and several protocol/operational contracts need hardening before consequential unattended deployment.

This page is dated because status prose goes stale. It was checked against the source tree and the 1,310-case conformance corpus on **2026-07-16**.

## Readiness in one table

| Use | Status | Recommendation |
| --- | --- | --- |
| Learn the language / explore interactively | Ready for preview | Keep another shell available; report surprising semantics. |
| Write local `.shl` scripts | Ready for preview | Pin external tools and test on the target OS. |
| Replace brittle text pipelines with typed values | Strong preview | Prefer builtins/adapters; validate schemas. |
| Use as daily login shell | Not recommended yet | Startup/job-control/compatibility surface is not mature enough. |
| Trusted local MCP agent | Usable with guardrails | Private socket, explicit policy, unique trust-group session, supervised kernel. |
| Mutually untrusted agents in one kernel | Unsafe | Use separate OS users/processes/sockets/state; current method/session gaps break isolation. |
| Remote kernel service | Unsupported/unsafe | Do not proxy the Unix socket across trust boundaries. |
| Hermetic build/runtime sandbox | Partial | Reef PATH hermeticity + filesystem enforcement are pieces, not complete hermeticity. |
| Windows | Unsupported | Unix-domain/process/PTY semantics require a deliberate port. |

## Maturity vocabulary

The documentation uses these labels:

| Label | Meaning |
| --- | --- |
| Implemented | A real execution path and tests exist. |
| Preview | Implemented, but compatibility/edge cases/operations can change. |
| Partial | Some advertised workflow exists; named gaps materially matter. |
| Stub | Method/syntax exists but returns unavailable or lacks its promised behavior. |
| Planned | No current behavior should be inferred. |

“Implemented” is not synonymous with “secure against hostile input,” “stable API,” or “identical on every host.”

## Verification baseline

The language contract lives in `spec/cases/*.toml`:

```text
1,310 cases across 77 files
1,306 passed
0 failed
4 skipped
```

Canonical command:

```bash
cargo test -p shoal --test conformance --locked -- --nocapture
```

The four skips are explicit host/harness dependencies:

1. a deliberately deep recursion case overflows the harness thread's native stack before Shoal's clean recursion error (a dedicated large-stack unit test covers the guard);
2. a Node interpreter-block case because Node is not guaranteed on every host;
3. a `jq` feed case because `jq` is not guaranteed on every host;
4. one Reef/`which` inventory case whose resolved path/hash/version/provider depends on installed host tools.

The corpus is broad executable language coverage, not a security proof. Kernel/MCP/platform crates have separate unit/integration tests, and adversarial multi-principal coverage is a specific roadmap priority.

## What is implemented

### Shell host and CLI

- Interactive Reedline-based REPL with multiline parsing, syntax highlighting, completion, configurable keybindings, prompt, persistent line history, and external editor action.
- `-c`/`--command`, script files, and stdin execution.
- `.shl` formatting and `--check` mode.
- bash/zsh/fish completion-script generation.
- prompt explain/print/benchmark developer commands.
- in-process doctor plus `shoal-lsp`/`shoal-mcp` dispatch through `PATH`.
- structured cwd/environment mutation, directory stack, `cd -`, frecency `jump`/`j`, jobs, history, plans, apply, and undo.

### Language

- Tagged value system: null, bool, int, float, string, bytes, path, size, duration, datetime/time, list, record, table, range, glob, regex, outcome, error, stream, task, secret, command references, closures.
- Command and expression reading modes, explicit forced external head, dynamic `run`, interpreter blocks, interpolation, safe navigation/coalescing, operators and method chains.
- Immutable/mutable bindings, functions, closures, aliases, modules/exports, typed/default/variadic parameters.
- `if`, `match` with patterns/guards, loops, `try`/`catch`, raised errors and value-position outcomes.
- Higher-order list/table operations, grouping/sorting/zipping/chunking/reductions, structured tables and namespace codecs.
- JSON/YAML/TOML/CSV, math, HTTP, OS, config, secrets, and other namespaces documented in the reference.

### Commands and data exchange

- Structured filesystem builtins including `ls`, `stat`, `cat`, `head`, `tail`, `cp`, `mv`, `rm`, `mkdir`, `touch`, and links.
- External commands return typed `outcome` values with status/signal/stdout/stderr/duration/PID/command.
- Format detection and typed decode for supported output; explicit feed/save/into flows.
- Adapter system with exact schemas/effects/parser strategies and **49 bundled command heads** in the current catalog.
- Real PTY execution for interactive terminal programs through kernel/MCP screen emulation.

### Streams, channels, and tasks

- Pull streams and live sources from process output, watch, tail, timers, and channels.
- Mapping/filtering/take/drop/enumeration/grouping/windowing/merging and sinks such as collect/each/save/feed.
- Single-consumption and cancellation behavior.
- Bounded live-source queues and coalesced dropped summaries.
- In-language `user.*` channels bridged to the kernel event bus.
- Structured background tasks in the evaluator and kernel task resources/events.

### Reef

- User/project/ancestor manifest discovery and scope chaining.
- Tool constraints, provider ranking, resolution, lockfiles, hash cache, and content-addressed view.
- `which`, `reef status`, `reef add`, `reef lock`, and scoped `with reef:` behavior.
- Interactive versus script lock policy and PATH synthesis.
- Hermetic PATH mode (ambient PATH tail removed).

### Journal, CAS, and undo

- SQLite/WAL structured journal with source, AST, effects, status, timing, principal/session, and output descriptors.
- Content-addressed output storage, truncation metadata, pins, TTL/budget garbage collection.
- Typed undo inverses with root confinement, fingerprint/staleness checks, symlink-parent rejection, and idempotent replay status.
- Interactive `history`/`journal` and standalone `shoal-history` inspection.

### Kernel and agents

- Named long-lived evaluator sessions over newline-framed JSON-RPC Unix socket.
- Bearer token store, policy principal attachment, honest enforcement reporting.
- Execution/run/plan/apply, transcript refs, path/slice retrieval, tasks, PTYs, event bus, journal/CAS access, parsing/completion/explanation.
- MCP 2025-06-18 facade with 13 tools, six stable root resources plus dynamic resources, nine URI templates, and resource subscriptions.
- Automatic value elision with shapes/previews/addressable references.
- Journal/transcript durable event replay and bounded subscription queues.

### Editor, configuration, and prompt

- Layered TOML configuration with validation, warnings for unknown keys, prompt/editor/history/reef settings.
- Configurable prompt segments, colors, symbols, left/right/continuation/transient rendering.
- Keybinding chord/action mapping.
- LSP diagnostics, incremental sync, whole-document formatting, scoped completion/hover, document symbols, and local/direct-module goto definition.

## Security boundaries and closed audit findings

The first deep-audit P0s are now closed in code and covered by adversarial tests:

- `journal.query` requires attachment, is principal+Session scoped, and has a hard page cap;
- `cap.request` requires an authenticated approver, denies self-approval by default, durably audits the immutable grant binding, and grants one-shot state;
- plan references carry a full source/AST/effects/Session/principal digest plus a unique object suffix, so same-shape or identical repeated plans cannot overwrite one another;
- every production child evaluator is created through one audited context that propagates principal, policy, Reef, filesystem, and cancellation state;
- public sockets reject asserted local-human authority and default tokenless clients to restricted `agent:mcp`; only the server-selected anonymous private REPL transport is a human trust root;
- evaluator Sessions and their refs/tasks/PTYS/quotas are keyed by principal plus visible Session name.

These changes do not make one kernel process a hard multi-tenant boundary. Same-process principals still share global resources and persisted state files, public transport has no `SO_PEERCRED` binding, tokens load at startup, and arbitrary native code is only constrained along dimensions the OS backend actually enforces. Use separate OS users/processes/state roots for mutually hostile tenants.

Full impact/mitigation: [Security and trust boundaries](@/docs/security.md).

## Protocol and agent limitations

| Limitation | Current behavior | Impact/workaround |
| --- | --- | --- |
| Raw retrieval throughput | `value.get format=raw` returns at most 8 KiB of decoded content per page. | Follow `page.next_offset`; string offsets are Unicode scalars and byte/CAS offsets are octets. |
| `blob.get` throughput | Byte `offset`/`length` pages are capped at 8 KiB after exact owner authorization. | Follow `page.next_offset`; many pages require repeated verified decompression because CAS files are compressed. |
| MCP subscription cost | One kernel connection and OS thread per resource subscription. | Bound subscriptions; consolidate channels. |
| MCP cwd resource stale | `shoal://session/cwd` is cached at attach. | Execute `pwd` or reconnect after `cd`. |
| Task output not streaming | `/task/{id}/out` resolves whole result only after capture. | Use lifecycle events; no incremental byte cursor yet. |
| Streams on wire | Wire stream contains only label; no chunk-pull method. | Collect/bound in language or use tasks/resources. |
| Timeout semantics | Converts unfinished execution into task; does not terminate. | Cancel and observe terminal state explicitly. |
| Task await | Raw method can block a connection indefinitely. | Subscribe/poll task resource. |
| Kernel task suspend/resume | Task records advertise current controls; raw methods control process-backed tasks only. | The snapshot is advisory; evaluator-only work is never advertised as suspendable, and MCP exposes cancel, not pause/resume. |
| PTY subscriptions | PTYs are poll-read only. | Bounded delayed polling + deadline + close. |
| PTY output | Current rendered grid, no raw ANSI or durable scrollback stream. | Use ordinary exec for audit capture. |
| Resource/session lifetime | Plans/tasks/PTys/transcript refs disappear on kernel restart. | Reconcile journal/artifacts and recreate. |
| Event retention | Only journal/transcript durable; other channels retain 1,024. | Detect gaps and reconcile authoritative state. |
| Partial quotas | Connections, retained Sessions, active tasks, PTYs, subscriptions, transcript/cursor retention, plans, and frame sizes are bounded; CPU, memory, and descendant process trees are not comprehensively metered. | Apply OS service/cgroup limits for hostile workloads. |

## Token and policy limitations

- `shoal-token` profile and `--cap` entries are attachment metadata, not authorization grants; Leash evaluates only the principal's policy entry.
- The daemon reads `tokens.json` at startup and does not reload it. Create/revoke requires kernel restart; expiry is checked live.
- `SHOAL_TOKEN_STORE` affects the CLI but not the kernel, which always uses `<state-dir>/tokens.json`.
- The default no-`--policy` durable kernel gives tokenless public clients the restricted `agent:mcp` identity; the private embedded-human REPL remains a distinct trusted surface.
- Leash effect analysis describes understood behavior; arbitrary native programs can do more unless an OS boundary prevents it.
- Filesystem sandboxing can be active on Linux Landlock/macOS Seatbelt. Network enforcement is absent; spawn hash checking has a pre-exec TOCTOU window.
- `caps_enforced` is a useful but coarse bool: inspect individual dimensions and nested propagation.
- `hermetic=true` is not full container/build hermeticity.

## Language and runtime limitations

### Not a POSIX shell dialect

Shoal does not promise Bash/POSIX parsing or expansion. There is no `$var` expansion, backtick substitution, implicit word splitting, globbing identical to Bash, shell function syntax, or drop-in startup-file compatibility. Use `sh { ... }` for an explicit legacy block and migrate deliberately.

### Command resolution is distributed

Builtin identity has a canonical registry used by evaluator/completion/highlighting/LSP, but full resolution across lexical functions, aliases, Reef, adapters, PATH, and interpreter runners still spans multiple evaluator paths. Edge-case precedence/forced-head behavior needs regression tests when changed.

### Lexical environments are bounded

A session retains at most 4,096 live lexical names and 16 MiB of measured binding state across root,
block, script, and module scopes. One name is at most 256 UTF-8 bytes; one materialized value is at
most 1 MiB, depth 64, and 16,384 nodes. Replacing an existing binding is still allowed when the
identity cap is full, and temporary scope charges are reclaimed when that scope is dropped.

Limit failures are catchable language errors: `binding_name_limit`, `binding_identity_limit`,
`binding_value_limit`, or `binding_aggregate_limit`. Runtime handles such as closures, tasks, and
streams receive a conservative fixed charge here and remain subject to their own subsystem quotas;
this is accounting protection, not a complete process-memory meter. Use an OS memory limit for
mutually hostile workloads.

### Bare path runner is narrow

A bare `./script.shl` path has language runner support. Other interpreter extensions generally require explicit `run path` or an interpreter block even when an adapter declares a runner.

### Method metadata is not the sole truth

The metadata registry used by method discovery/completion has drift from actual dispatch: it advertises some `get` combinations not dispatched (notably table/range) and omits some valid bool display/string methods. The public method reference follows implementation/tests, not metadata alone. Completion can therefore omit a valid method or suggest an invalid receiver combination.

### Stream caveats

- `buffer(n)` creates a bounded asynchronous pump, but each pump consumes a thread and the evaluator
  admits at most 64 concurrent stream pumps. Drop or consume buffered streams promptly.
- `.distinct()` retains all previously seen distinct values and can grow without bound. Use a finite/taken stream or `.dedupe()` for adjacent suppression.
- live `.tee(n)` uses 64-entry per-fork queues; overflow drops values and inserts a `{dropped:n}` marker rather than raising.
- collecting/sorting/grouping an infinite stream without a bound never completes and can exhaust memory.
- live timing/filesystem sources remain host-dependent despite deterministic core combinators.

### Cancellation and process trees

Cancellation is cooperative through evaluator tokens and process handles. It is not a universal transaction or guaranteed descendant-process-tree cleanup on every platform. Side effects completed before cancellation remain.

### Module/session cache lifetime

Loaded module/evaluator state lives in the process/session and can become stale relative to files. Restart/reload semantics are not a hot-reload system.

### Non-UTF-8 boundaries

Wire paths preserve raw Unix bytes alongside lossy display strings. Some other views—such as session environment enumeration—omit entries that cannot convert to UTF-8. Source/config/JSON-facing APIs remain UTF-8.

## Adapter limitations

- Bundled adapters cover many common versions, not every CLI release/locale/platform output.
- Parser strategies are schema-driven but still depend on upstream formatting; a changed column/header can produce parse failure or incorrect mapping.
- Structured adapter effect declarations are metadata, not confinement proof.
- `SHOAL_ADAPTER_PATH` is replacement semantics: setting it stops automatic bundled/user default search rather than merely appending.
- Custom directories with a matching command head override earlier definitions; inspect discovery order.
- Locale/color/pager settings can break text parsers unless the adapter pins flags/environment.
- `^command` bypasses an external adapter for diagnosis; `run(name, ...)` forces dynamic external execution.
- Adapter count is not API compatibility: validate the specific head/flags/schema your script uses.

## Reef limitations

- Manifests activate only tool resolution; there are no shell activation hooks.
- The evaluator caches parsed scope discovery behind a candidate/lock metadata fingerprint including device/inode/mtime/ctime/length; same-cwd create/edit/repair/remove changes invalidate it. Interactive discovery warns and remains best-effort, while noninteractive external execution refuses retained discovery errors. Metadata-to-open identity races remain a filesystem-port limitation.
- An unlocked constrained tool is tolerated/locked interactively but rejected under script policy; host-installed versions/providers make first resolution host-dependent.
- Hermetic mode removes ambient PATH tail; it does not sandbox filesystem/network/environment/syscalls.
- Provider availability and install commands depend on host managers; offline/missing providers remain honest resolution errors.
- Hash pins protect selected content identity at resolution, but runtime spawn pinning remains preflight/TOCTOU-prone.
- Child evaluators inherit policy, principal, Reef/config inputs, filesystem/watch ports, and cancellation
  through the audited child-context constructor; divergence is a security regression.
- Windows provider/path semantics are deferred.

## Journal, history, undo, and secret limitations

- Shell, history, and doctor share the canonical XDG state default and layered `journal.state_dir`. Pass history `--state-dir` for a durable kernel launched with a different explicit root or to bypass malformed config.
- GC may age out output blobs that journal entries still reference unless pinned; metadata remains but bytes become unavailable.
- Undo covers only operations that recorded a typed inverse; it is not arbitrary command rollback.
- Undo depends on surviving CAS bytes/current fingerprint and refuses stale/symlink-escaped targets.
- A macOS/Unix leading-symlink alias is handled defensively, but filesystem races/special mounts warrant platform testing.
- `shoal-secret` has no print/get command, bounds encrypted/decrypted store admission, and leaves
  invalid snapshots intact; its key is still stored beside ciphertext, so OS permissions are the
  true at-rest boundary.
- Evaluator honors `SHOAL_SECRET_DIR`; CLI does not, so stores can diverge.
- An authorized child can print a typed secret into captured output; redacted wire encoding cannot prevent downstream exfiltration.

## Configuration, prompt, and editor limits

- Configuration is loaded at process/session startup; there is no general live reload protocol.
- Unknown-key warnings protect typos but do not migrate renamed semantics automatically.
- Keybinding action names are a fixed mapping to Reedline; unknown chords/actions warn/skip.
- Prompt snapshots avoid arbitrary scripting in the render path; customization is structured rather than Starship/Bash-code compatible.
- LSP is parser/local-document oriented with true incremental sync, scope-aware local symbols, document symbols, and goto-definition for local bindings plus exported members of directly used file modules. It still has no references, rename, workspace index, semantic tokens, code actions, signature help, or project/manifest graph.
- LSP completion and definitions use parser-derived lexical scopes, not a type-aware or workspace-indexed semantic model.
- `shoal lsp`/`shoal mcp` search `PATH`; they do not search beside the main binary. The sandbox helper has the opposite packaging constraint (searched beside executable).

## Platform support

Shoal is explicitly Unix-only for now. The kernel/CLI/MCP stack is built directly on Unix-domain
sockets, POSIX process groups/signals, `/dev/ptmx` PTYs, and (where available) Linux Landlock or
macOS Seatbelt sandboxing — none of which have a Windows equivalent wired up. Windows support is
recorded as **out of scope for now**, not silently deferred; CI only builds/tests on Ubuntu and
macOS (see [Tooling and quality](@/internals/tooling-and-quality.md)).

| Platform | Status |
| --- | --- |
| Linux | Primary development target; Unix socket/PTTY, Landlock when kernel supports it. |
| macOS | First-class intended target; Unix socket/PTTY, Seatbelt filesystem backend, `/tmp` aliases need tests. |
| Other Unix | May compile partially; enforcement tier D and behavior not promised. |
| Windows | Out of scope for now: Unix paths/sockets/PTY/process semantics and enforcement need a deliberate design/port that has not been scheduled. |

External tool behavior remains host-specific even on supported OSes. A script that depends on GNU flags may fail against BSD tools. Adapters and Reef can pin/normalize some of this, but scripts should state tool/version requirements.

## Compatibility policy

All workspace packages are currently `0.1.0`. There is no promised stable language grammar, config schema, adapter schema, kernel RPC, MCP resource shape, journal schema, or CLI output compatibility across arbitrary preview commits.

Practical guidance:

- pin a Git commit/release for scripts and integrations;
- commit exact Reef constraints and custom adapters; materialize the host-local `reef.lock` after installing tools;
- test `cargo test -p shoal --test conformance --locked` when contributing;
- use tagged fields/numeric error codes rather than display prose;
- ignore unknown additive JSON fields;
- do not persist ephemeral `out:`, `task:`, `plan:`, or `pty:` refs as durable IDs;
- read changelogs/diffs before upgrading consequential automation.

## What “production ready” would require

At minimum:

1. decide whether public deployments need mandatory bearer mode and/or peer-credential binding;
2. add live token revocation/reload;
3. close remaining raw/blob resource-exhaustion gaps and add CPU/memory/process-tree controls;
4. provide stronger network/process enforcement and preserve per-dimension enforcement truth;
5. continue long-duration task/PTY/subscription/process-tree lifecycle testing;
6. stabilize/version wire/config/journal contracts;
7. run adversarial cross-principal and long-duration platform testing.

The ordered work is in [Roadmap](@/docs/roadmap.md). For practical current deployment, use [Security](@/docs/security.md), [Agent workflows](@/docs/mcp-workflows.md), and [Troubleshooting](@/docs/troubleshooting.md).
