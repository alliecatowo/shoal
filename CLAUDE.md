# CLAUDE.md — repository operating manual

Shoal is a structured shell with a modal CMD/EXPR parser, typed values and streams, a tree-walk
evaluator, reproducible tool resolution, a journal/CAS, capability-aware process execution, and a
shared kernel/MCP surface. This file is a contributor runbook. Product and architecture prose live
in the Zola sources; do not grow a second manual here.

## Sources of truth

Read the smallest relevant set before changing behavior:

1. [`spec/cases/*.toml`](spec/cases) is the executable language contract: **1,310 cases across 77
   suites**. If prose and a case disagree, investigate, but do not silently change a case to match a
   regression.
2. [`site/content/internals/language-conformance-contract.md`](site/content/internals/language-conformance-contract.md)
   defines grammar/semantic governance and the corpus schema.
3. [`site/content/internals/intercrate-protocol-contracts.md`](site/content/internals/intercrate-protocol-contracts.md)
   records crate boundaries, shared Rust types, wire types, error registries, and dependency rules.
4. Use the focused atlas page for the subsystem: parser/formatter, evaluator state, values, methods,
   process execution, PTY/job control, streams/channels, Reef resolution, persistence, effects and
   security, kernel protocol, agent/MCP, configuration, or prompt/editor/LSP.
5. [`site/content/internals/implementation-status.md`](site/content/internals/implementation-status.md)
   is the current shipped/partial/deferred ledger. Sequencing belongs in
   [`site/content/internals/roadmap-and-priorities.md`](site/content/internals/roadmap-and-priorities.md).

Public-facing behavior belongs under [`site/content/docs/`](site/content/docs); implementation
reasoning belongs under [`site/content/internals/`](site/content/internals). Keep source comments
short and point to those stable files instead of recreating old numbered design sections.

## Workspace map

The detailed ledger is
[`site/content/internals/crate-ledger.md`](site/content/internals/crate-ledger.md). The short map:

```text
syntax + model
  shoal-ast       canonical AST/desugaring structures
  shoal-syntax    modal lexer/parser, formatter, canonical command registry
  shoal-value     values, errors, methods, rendering, streams, effect ports

execution core
  shoal-eval      scopes, coercion, calls, builtins, modules, streams, effects
  shoal-exec      capture, PTY/tee, process groups, cancellation, sandbox handoff
  shoal-adapters  declarative external-command schemas and output parsing
  shoal-reef      scoped resolution, providers, locks, hashes, runners
  shoal-leash     policies, plans, grants, executable pins, OS enforcement
  shoal-journal   SQLite journal, outputs, undo records, blake3 CAS

hosts and clients
  shoal           CLI, scripts, REPL, editor integration
  shoal-kernel    long-lived sessions and newline-framed JSON-RPC
  shoal-proto     shared wire request/result/error types
  shoal-mcp       13-tool MCP facade, resources, subscriptions, PTY tools
  shoal-lsp       parsing/formatting/diagnostics/completion server
  shoal-prompt    prompt renderer and modules
```

Other focused crates include authentication, secrets, history, picker, doctor, and the prepared WASM
host. Check real `Cargo.toml` edges before changing the tiering; do not infer them from this summary.

`shoal-eval` is intentionally split into focused `impl Evaluator` modules. It remains the collision
magnet, so serialize eval-heavy work. Adapter TOML, corpus cases, docs, and isolated crate work can
usually proceed independently.

## Change discipline

- Add or update conformance cases for every language-visible behavior change. Case names are unique
  repository-wide; search before adding one.
- Each case gets a fresh evaluator and temporary cwd. `it`/`out[n]` are REPL-only. Use `skip` only
  for genuinely host-dependent behavior, never to hide a failure.
- Shared types and public protocols require coordinated updates: implementation, tests, the
  intercrate contract, and relevant manual/atlas pages move together.
- Keep the canonical builtin vocabulary in `shoal-syntax`; evaluator dispatch, completion,
  highlighting, and LSP consume it. Do not create a second name list.
- Preserve hexagonal boundaries. `shoal-value` owns filesystem/time/opener/secret ports;
  `shoal-eval` owns the exec port because it uses `shoal-exec` types.
- Do not hand-edit the workspace dependency graph casually. Prefer
  `cargo add -p <crate> <dependency>` for a crate dependency, then inspect the diff.
- Existing worktree changes belong to their author. Never discard unrelated edits or use a
  destructive reset to make a task easier.

The case schema is:

```toml
[[case]]
name = "globally-unique-kebab-name"
src = "let x = 2 + 3\nx * 2"
value = "10"
# or: error = "type_error"
# or: parse_error = true
# optional: error_contains / parse_error_contains / fixture / skip
```

## Verification

Use a unique target directory when another agent or terminal may be building concurrently:

```sh
CARGO_TARGET_DIR=target-<name> cargo build --workspace --locked
CARGO_TARGET_DIR=target-<name> cargo test -p <crate> --locked
```

The repository gate is:

```sh
cargo fmt --all --check
cargo +stable clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

For language/evaluator work, isolate the corpus while iterating and quote its final counts:

```sh
cargo test -p shoal --test conformance --locked -- --nocapture
```

For kernel, MCP, resources, events, CAS refs, or PTY changes, also run:

```sh
CARGO_TARGET_DIR=target-mcp cargo test -p shoal-mcp --test live_kernel --locked
```

That test starts a real kernel/socket stack and exercises attach, structured exec, elision and
drill-down, resources, subscriptions, channels, content refs, and interactive PTYs across the actual
facade boundary.

## Dogfooding

```sh
cargo run -p shoal
cargo run -p shoal -- -c $'let answer = 6 * 7\nanswer'
cargo run -p shoal -- examples/example.shl
```

Exercise workflows rather than isolated arithmetic: structured external output, aliases, Reef,
interpreter blocks, `.feed`, a stream/channel pipeline, failure capture versus raise, journal/undo,
and a real PTY program. The retained [dogfooder runbook](.claude/agents/dogfooder.md) has a concrete
checklist.

## Kernel and MCP facts that are easy to stale

- `shoal-mcp` exposes **13 tools**, including `shoal_pty_open/send/read/resize/close/list`, plus MCP
  resource listing/templates/reads/subscriptions.
- `shoal-mcp` autostarts a detached kernel when the selected socket has no listener. A non-empty
  `SHOAL_NO_AUTOSTART` disables this for supervised deployments.
- MCP `shoal_exec` defaults to value position. Earlier statements use statement semantics; a final
  expression uses value semantics and captures a failed external outcome. Language errors raise in
  both positions and still receive an addressable transcript ref.
- Spawned outcomes carry invocation spans. Synthesized or reconstructed outcomes may honestly omit
  the optional span.
- Spawn-hash enforcement activates only for a principal with a non-empty `proc_spawn` allowlist.
  The unconfigured/default path remains permissive. The gate covers ordinary and PTY spawns.
- Filesystem sandboxes are conditional and platform-specific; network enforcement is still reported
  separately as unavailable. Never collapse “policy allowed,” “filesystem enforced,” and “network
  enforced” into one boolean claim.

Read [`site/content/internals/agent-mcp.md`](site/content/internals/agent-mcp.md),
[`site/content/internals/kernel-protocol.md`](site/content/internals/kernel-protocol.md), and
[`site/content/internals/security-threat-model.md`](site/content/internals/security-threat-model.md)
before changing these paths.

## Retained specialist runbooks

- [adapter-author](.claude/agents/adapter-author.md): declarative adapter work
- [conformance-author](.claude/agents/conformance-author.md): corpus additions and validation
- [crate-auditor](.claude/agents/crate-auditor.md): evidence-backed single-crate audits
- [dogfooder](.claude/agents/dogfooder.md): multi-step product workflows

These are operational checklists, not product documentation. Update their stable atlas links when a
subsystem moves; do not point them at deleted root docs or private scratch notes.

## When uncertain

- Language behavior: search the corpus, then the language contract and implementation.
- Wire/MCP behavior: inspect the protocol/agent pages, source schemas, and live-kernel tests.
- Current status: inspect the implementation-status page and verify the source or binary.
- Priorities: follow the roadmap package and exit tests; do not resurrect old R-wave shorthand.
- Documentation placement: follow
  [`site/content/internals/documentation-governance.md`](site/content/internals/documentation-governance.md).
