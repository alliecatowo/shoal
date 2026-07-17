+++
title = "Hardening roadmap"
description = "The master atomic task list remediating every finding in the 2026-07-16 deep audit, organized into waves with per-task acceptance criteria and a finding→task traceability matrix."
weight = 124
template = "docs/page.html"

[extra]
group = "Maintenance"
eyebrow = "Execution plan"
status = "Active — wave 1 in flight"
audience = "Maintainers, reviewers, and implementing agents"
wide = true
+++

This is the master remediation plan for the
[2026-07-16 deep audit](@/internals/deep-audit-2026-07-16.md). The contract: **every finding ID
on the audit page maps to at least one task below, and completing every task addresses every
finding.** The traceability matrix at the bottom is the proof; keep it exact when editing.

Rules for implementers:

- Tick a task (`[x]`) only when its acceptance criteria are met on a gated tree
  (fmt + clippy `-D warnings` + full workspace tests + conformance).
- Every language-visible change adds conformance cases; every behavior change updates the
  matching docs page in the same change set.
- Do not weaken a task to make it easier to tick; if scope must change, edit the task text and
  say why in the commit.

## Wave 1 — release-blocking soundness (P0)

### Workstream A — fail-closed effect planning

- [ ] **HR-A1** — `plan` classifies `Stmt::Use`: an `FsRead` of the module path plus effects
  derived from (or an `Opaque` covering) the module's top-level statements. *(A1)*
  <br>Accept: `plan { use ./module }` reports non-empty effects including the module read.
- [ ] **HR-A2** — `plan` emits `EnvWrite` for persistent `env.NAME = …` assignment targets, not
  just RHS effects. *(A2)*
  <br>Accept: `plan { env.X = "y" }` reports an env write naming `X`.
- [ ] **HR-A3** — `plan_call` traverses command redirects; `>` and `>>` derive `FsWrite` with
  the target path (append distinguished from truncate). *(A3)*
  <br>Accept: `plan { echo hi > p }` reports a write to `p`.
- [ ] **HR-A4** — Method-call classification covers path/stream `.save` and `.append`, path
  reads (`.read` and friends), and `.feed`. *(A4)*
  <br>Accept: each of `plan { "x".save("p") }`, `plan { path("f").read }`,
  `plan { "x".feed(cat) }` reports the correct effect kind and target.
- [ ] **HR-A5** — `FnCall` derivation handles session-stored closures/functions: any call that
  cannot be statically expanded derives an approval-requiring unknown effect, never nothing.
  *(A5, A10)*
  <br>Accept: planning a call to a previously-defined session function with effects is not
  reported as effect-free.
- [ ] **HR-A6** — Generic external commands derive a concrete `ProcSpawn` (argv + resolution),
  consistent with adapter spawns. *(A6)*
  <br>Accept: `plan { run("echo","hi") }` and a bare external both report `spawns: true` with
  the argv.
- [ ] **HR-A7** — Adapter effect declarations parse against the full effect vocabulary; an
  unrecognized declaration (e.g. `proc.spawn(container)`) is a load-time error or conservative
  unknown effect — never silently dropped. *(A7)*
  <br>Accept: a fixture adapter with an unknown effect declaration fails loudly or plans
  conservatively; test pins it.
- [ ] **HR-A8** — Effectful builtins (`run`, `open`, `save`, …) and `spawn`/`parallel`/task
  bodies derive the effects of their bodies/arguments. *(A8)*
  <br>Accept: `plan { spawn { "x".save("p") } }` and `plan { parallel(() => "x".save("p")) }`
  report the write.
- [ ] **HR-A9** — The planner resolves command position with the same resolution the runtime
  uses, fixing the `.feed(cat)` builtin-vs-external mismatch. *(A9)*
  <br>Accept: `plan { "x".feed(cat) }` reports a process spawn matching what runtime executes.
- [ ] **HR-A10** — Derivation is structurally exhaustive: a match over every AST node with a
  deny-by-default arm (unknown ⇒ approval-requiring effect). No wildcard that silently returns
  empty. *(A10)*
  <br>Accept: a unit test constructs each AST statement/expression variant and asserts the
  planner returns either concrete effects or the conservative unknown — never silently empty for
  effectful forms.
- [ ] **HR-A11** — Negative-coverage suite: every dynamic probe from the audit becomes a pinned
  test/conformance case asserting non-empty, correct effects. *(A8, A11)*
  <br>Accept: all ten audit probes are cases; corpus passes.

### Workstream B — one child-evaluator constructor

- [x] **HR-B1** — Introduce a single authoritative child-context constructor that necessarily
  propagates: leash policy/principal, all effect ports, config, reef overrides/locks,
  journal/session identity, event bus, and cancellation. *(B1, B2, B3, H1)*
  <br>Accept: constructor exists in `shoal-eval`; a compile-visible seam (not ad-hoc field
  copies) is the only way to build a child evaluator.
- [x] **HR-B2** — `spawn_block` uses the constructor. *(B1)*
- [x] **HR-B3** — `run_script_file` uses the constructor. *(B1)*
- [x] **HR-B4** — `builtin_parallel` uses the constructor. *(B1)*
- [x] **HR-B5** — `builtin_on` (channel handlers) uses the constructor. *(B1)*
- [x] **HR-B6** — Remove or privatize `inherit_ports`-style partial copying so future call sites
  cannot under-inherit. *(B1, B3)*
  <br>Accept: no public API constructs a child evaluator without the full-context constructor.
- [x] **HR-B7** — Tests: a restrictive leash policy observably constrains work run via `spawn`,
  `parallel`, an `on` handler, and a `.shl` script exactly as it does foreground; reef/config/
  journal settings propagate identically; the config port reaches `spawn` blocks (B5); parent
  cancellation reaches `parallel` children and `.shl` script children (B6). *(B4, B5, B6)*

### Workstream C — effects through enforceable ports

- [x] **HR-C1** — `path.save`/`path.append` route through the Fs effect port. *(C1)*
- [x] **HR-C2** — Stream `.save`/`.append` (and any spill-to-file write in value methods) route
  through the Fs effect port. *(C1)*
- [x] **HR-C3** — Inventory every direct `std::fs`/`OpenOptions` use in `shoal-value` and
  `shoal-eval` value/method paths; route each through a port or record a justified exemption in
  the effects/security internals page. *(C2)*
  <br>Accept: the inventory list is committed; non-exempt sites are ported.
- [x] **HR-C4** — Tests: with a recording/denying Fs port installed, covered in-process writes
  are observed and deniable; no covered route writes directly. *(C3)*

### Workstream D — attachment, approval, identity

- [ ] **HR-D1** — `cap.request` requires an authenticated attachment and receives the caller
  principal. *(D1)*
- [ ] **HR-D2** — Approval records bind requester, plan hash, approver principal, scope, and the
  execution that consumes the approval; the binding is journal-auditable. *(D3)*
- [ ] **HR-D3** — Approver identity must differ from the requester unless policy explicitly
  permits self-acknowledgement; default policy separates them. Document the chosen model in the
  security threat model page. *(D2, D4)*
- [ ] **HR-D4** — `journal.query` requires attachment, matching the documented rule. *(F1)*
- [ ] **HR-D5** — `journal.query` limits are bounded: `limit: 0` returns zero rows and a
  server-side maximum caps page size. *(F2)*
- [ ] **HR-D6** — Zero-config MCP attach lands on a restricted agent principal rather than
  `local-human`; permissive attach becomes explicit opt-in. Update autostart, attach handling,
  and the agent/MCP + threat-model docs together. *(E1, E2)*
- [ ] **HR-D7** — Session identity model made explicit: statement-level journal attribution
  follows the current actor (matching kernel exec entries), and task/PTY cross-principal access
  rules are documented and enforced — or the shared pair-shell model is documented as
  intentional with its token-isolation consequences. *(G1, G2, G3)*
- [ ] **HR-D8** — `live_kernel` integration tests cover: unattached `journal.query` rejected;
  unattached `cap.request` rejected; zero-config attach is restricted; cross-principal task/PTY
  access follows the documented rule. *(D1, E2, F1, G3)*

### Workstream E — protocol and daemon robustness

- [ ] **HR-E1** — Frame length is enforced during read (bounded reader) in `shoal-proto` and
  `shoal-mcp`; oversize input cannot allocate past the cap. *(H6)*
  <br>Accept: a test feeds an over-limit frame and observes bounded memory + a protocol error.
- [ ] **HR-E2** — Shared daemon state stops relying on `.lock().unwrap()`: a poison-tolerant
  locking pattern (recover-or-shutdown helper) replaces bare unwraps so one panicking connection
  cannot cascade. *(H4)*
- [ ] **HR-E3** — Quotas with clear errors: max concurrent connections, and per-session caps for
  tasks, PTYs, and subscriptions. *(H3, H5)*
- [ ] **HR-E4** — Session lifecycle GC: bounded transcript retention, plan expiry,
  completed-task reaping; limits configurable, defaults documented. *(H5)*

### Workstream F — workspace hygiene

- [x] **HR-F1** — Every member crate inherits the root `[workspace.lints]` policy
  (`lints.workspace = true`), making the staged policy actually active. *(H7)*
- [x] **HR-F2** — `rust-toolchain.toml` pins the stable toolchain CI uses. *(H11)*
- [x] **HR-F3** — Syntax-highlighter tests force their color environment explicitly; the suite
  passes with and without `NO_COLOR=1`. *(H13)*
- [x] **HR-F4** — A scheduled CI job runs the fuzz targets on a short nightly budget and
  surfaces failures. *(H12)*
- [x] **HR-F5** — Benchmark honesty: delete or implement the prompt benchmark's per-PR/p99
  claims; make `table_1m_where_sort` exercise the real evaluator/table methods or rename/remove
  it. *(I12)*
- [ ] **HR-F6** — Supply-chain advisories checked in CI (`cargo audit` or `cargo deny`) with a
  documented allowlist. *(H9)*
- [ ] **HR-F7** — Unix-only support stated explicitly in README/docs; Windows recorded as out of
  scope for now. *(H8)*
- [ ] **HR-F8** — Decide and document how CI pays (or stops paying) the wasmtime compile cost
  for the unwired `shoal-wasm` crate: keep, feature-gate, or a separate job. *(H10)*

## Wave 2 — semantic truth (P2)

### Workstream G — stream and channel semantics

- [ ] **HR-G1** — `StreamVal::buffer(n)` becomes a real bounded decoupling buffer, or is removed
  and its docs state the unimplemented status. *(I2)*
- [ ] **HR-G2** — `flat_map` interleaves substreams as documented, or the docs are corrected to
  sequential semantics; a test pins whichever behavior ships. *(I3)*
- [ ] **HR-G3** — In-language subscriber queues are bounded with a defined overflow policy that
  matches the documented backpressure story. *(I4)*
- [ ] **HR-G4** — Cancelling an `on(channel, handler)` task interrupts a blocking `recv`
  (timeout, close, or wakeup token); no permanently stuck handler threads. *(I5)*
- [ ] **HR-G5** — `distinct` uses hashing (amortized O(1) membership); its memory behavior on
  unbounded streams is documented. *(I13)*
- [ ] **HR-G6** — `zip`/`merge` rate-skew and backpressure semantics are documented precisely
  with tests. *(I14)*
- [ ] **HR-G7** — Incremental stream `.feed` is implemented, or its explicit unimplemented error
  and docs status are kept accurate and linked from the streams page. *(I6)*

### Workstream H — truthful surface statuses

- [ ] **HR-H1** — Per-feature status labels (implemented / partial / experimental / planned) for
  WASM dispatch, task suspend/resume, wire stream chunking, LSP scope, and network leash
  enforcement, recorded on the implementation-status page and the relevant feature pages.
  *(I7, I8, I9, I10)*
- [ ] **HR-H2** — REPL↔kernel decision recorded as an ADR: either the REPL attaches to
  `shoal-kernel` (design + tracked implementation plan) or the one-kernel/three-surfaces claim
  is narrowed everywhere it appears. *(I1)*
- [ ] **HR-H3** — Docs drift pass: counts and open items across internals pages match source at
  a stated commit. *(I11)*

### Workstream I — secrets and token store

- [ ] **HR-I1** — Secret-store boundary documented honestly (key beside ciphertext ⇒ OS
  permissions are the boundary), or key material moves to the OS keyring where available. *(J1)*
- [ ] **HR-I2** — Secret material zeroized where practical; env-injection copies noted in docs.
  *(J2)*
- [ ] **HR-I3** — Interprocess locking (file lock or equivalent) around secret-store and
  token-store read-modify-write. *(J3, J4)*

### Workstream J — structural debt (promoted into this wave)

- [ ] **HR-J2** — Evaluator decomposition plan: split the god-context into cohesive sub-contexts
  behind the HR-B1 seam; recorded as a design page with staged extraction steps. *(H1)*
  — promoted 2026-07-16; design: evaluator-decomposition page.

## Wave 3 — structural debt (sequenced after waves 1–2)

- [ ] **HR-J1** — Centralized command resolution: one resolution function/table with an explicit
  precedence order consumed by the evaluator, planner, completion, and LSP. *(H2; closes the
  root cause behind A9)*
- [ ] **HR-J3** — Kernel concurrency model review: documented limits of
  thread-per-connection + per-session mutex; bounded executor or explicit caps where needed
  beyond HR-E3. *(H3)*

## Traceability matrix

Every audit finding → the task(s) that retire it.

| Finding | Task(s) | | Finding | Task(s) |
|---|---|---|---|---|
| A1 | HR-A1 | | H1 | HR-B1, HR-J2 |
| A2 | HR-A2 | | H2 | HR-J1 |
| A3 | HR-A3 | | H3 | HR-E3, HR-J3 |
| A4 | HR-A4 | | H4 | HR-E2 |
| A5 | HR-A5 | | H5 | HR-E3, HR-E4 |
| A6 | HR-A6 | | H6 | HR-E1 |
| A7 | HR-A7 | | H7 | HR-F1 |
| A8 | HR-A8, HR-A11 | | H8 | HR-F7 |
| A9 | HR-A9, HR-J1 | | H9 | HR-F6 |
| A10 | HR-A5, HR-A10 | | H10 | HR-F8 |
| A11 | HR-A11 | | H11 | HR-F2 |
| B1 | HR-B1–B6 | | H12 | HR-F4 |
| B2 | HR-B1 | | H13 | HR-F3 |
| B3 | HR-B1, HR-B6 | | I1 | HR-H2 |
| B4 | HR-B7 | | I2 | HR-G1 |
| B5 | HR-B1, HR-B7 | | | |
| B6 | HR-B1, HR-B7 | | | |
| C1 | HR-C1, HR-C2 | | I3 | HR-G2 |
| C2 | HR-C3 | | I4 | HR-G3 |
| C3 | HR-C4 | | I5 | HR-G4 |
| D1 | HR-D1, HR-D8 | | I6 | HR-G7 |
| D2 | HR-D3 | | I7 | HR-H1 |
| D3 | HR-D2 | | I8 | HR-H1 |
| D4 | HR-D3 | | I9 | HR-H1 |
| E1 | HR-D6 | | I10 | HR-H1 |
| E2 | HR-D6, HR-D8 | | I11 | HR-H3 |
| F1 | HR-D4, HR-D8 | | I12 | HR-F5 |
| F2 | HR-D5 | | I13 | HR-G5 |
| G1 | HR-D7 | | I14 | HR-G6 |
| G2 | HR-D7 | | J1 | HR-I1 |
| G3 | HR-D7, HR-D8 | | J2 | HR-I2 |
| | | | J3 | HR-I3 |
| | | | J4 | HR-I3 |

## Verification gate (unchanged, mandatory)

```sh
cargo fmt --all --check
cargo +stable clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test -p shoal --test conformance --locked -- --nocapture   # quote final counts
CARGO_TARGET_DIR=target-mcp cargo test -p shoal-mcp --test live_kernel --locked   # kernel/MCP work
```
