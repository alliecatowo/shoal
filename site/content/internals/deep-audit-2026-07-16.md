+++
title = "Deep audit — 2026-07-16"
description = "Frozen findings from the 2026-07-16 read-only audit: effect-planning soundness, child-context propagation, port-boundary enforcement, kernel attachment and approval gaps, semantic stubs, and workspace hygiene — each with a stable finding ID."
weight = 123
template = "docs/page.html"

[extra]
group = "Maintenance"
eyebrow = "Audit record"
status = "Findings frozen; remediation tracked in the hardening roadmap"
audience = "Maintainers and reviewers"
wide = true
+++

This page freezes the findings of the 2026-07-16 read-only deep audit so remediation can be
tracked against a stable record. Every finding has an ID (`A1`, `B2`, …). The
[hardening roadmap](@/internals/hardening-roadmap.md) maps **every** ID to at least one atomic
task; completing that roadmap addresses every finding on this page. Do not edit findings here —
if a finding turns out to be wrong, mark it *withdrawn* with a note and adjust the roadmap.

Audit context: repository at the 2026-07-16 state (~65.8k tracked Rust lines, 22 workspace
crates, 394 registry dependencies, 917 test annotations, 1,310 conformance cases). Full
workspace gate (fmt, clippy `-D warnings`, tests, conformance) passed on the audited snapshot.
Overall verdict: an unusually coherent prototype whose feature breadth has grown faster than its
semantic and security invariants; the agent capability/leash system is not yet a trustworthy
boundary.

## A — Effect planning is fail-open (P0)

`crates/shoal-eval/src/plan_derive.rs` does not conservatively cover all effectful language
forms. Plan → approve is therefore unsound for untrusted code. Dynamic probes against the real
binary confirmed each gap: all of the following returned `effects: []`, `reversible: true`,
`spawns: false`.

```text
plan { "x".save("p") }            plan { spawn { "x".save("p") } }
plan { echo hi > p }              plan { parallel(() => "x".save("p")) }
plan { env.AUDIT_ONLY = "y" }     plan { run("echo", "hi") }
plan { path("Cargo.toml").read }  plan { open("Cargo.toml") }
plan { use ./module }             plan { save("x", "p") }
```

- **A1** — `Stmt::Use` falls through with no effects, but module loading reads a file and
  executes every top-level statement (`modules.rs`).
- **A2** — `Stmt::Assign` plans only the RHS; persistent `env.NAME = ...` is not an `EnvWrite`.
- **A3** — Command redirects are not traversed by `plan_call`; `>` and `>>` can write without an
  `FsWrite` plan effect.
- **A4** — Method calls only special-case `http.*`; path/stream `.save`, `.append`, path reads,
  and `.feed` are not classified.
- **A5** — `FnCall` expands only functions declared in the same submitted program. Session
  closures and effectful runtime functions escape static derivation.
- **A6** — Generic external commands become `Opaque`, not a concrete `ProcSpawn`; adapter spawns
  are concrete. The asymmetry hides what actually spawns.
- **A7** — Adapter effect parsing understands only `fs.read`, `fs.write`, `fs.delete`, and
  `net.connect`; declarations such as `proc.spawn(container)` are silently ignored.
- **A8** — Effectful builtins (`run`, `open`, `save`) and `spawn`/`parallel` bodies derive no
  effects (see the probe list above).
- **A9** — `plan { "x".feed(cat) }` reported only `read /tmp` and `spawns: false`: planning
  resolves `cat` as a Shoal builtin while runtime `.feed` bypasses normal command dispatch and
  calls `run_argv` directly — a concrete planner/runtime resolution mismatch.
- **A10** — The required design is fail-closed: every AST form exhaustively classified; an
  unrecognized or unclassifiable form must become an approval-requiring unknown effect, never an
  empty effect set.
- **A11** — There is no negative-coverage suite asserting that each effectful syntax route
  produces its effects; the probes above must become pinned tests.

## B — Child evaluators lose active security/session context (P0)

Fresh evaluators created by `spawn`, `parallel`, `on(channel, handler)`, and `.shl` script
execution copy environment/adapters/effect ports but not the active leash policy/principal, and
omit various reef/config/journal/session settings. Sites: `script.rs` (`spawn_block`,
`run_script_file`), `host.rs` (`builtin_parallel`), `channels.rs` (`builtin_on`), `lib.rs`
(`inherit_ports` copies only ports).

- **B1** — Child construction is manual field copying at several call sites; each new execution
  route must remember dozens of fields, and some do not.
- **B2** — The active leash policy/principal is not propagated to children.
- **B3** — Reef overrides/locks, config, journal/session settings propagate inconsistently.
- **B4** — Consequences: runtime spawn-hash gating and OS sandbox selection can disappear inside
  child evaluation; background/parallel/handler/script work can behave differently from
  foreground work; a restricted command can become unrestricted when wrapped in `spawn` or
  `parallel`. Release-blocking for the agent security story.

## C — In-process filesystem effects bypass ports and sandbox (P0/P1)

- **C1** — `shoal-value/src/methods/path.rs::save` and stream save code use
  `std::fs::OpenOptions` directly; these effects happen in the kernel process, not the sandboxed
  external child, and bypass the Fs effect port.
- **C2** — The stated hexagonal/effect-port boundary is therefore incomplete; direct `std::fs`
  use in value methods has not been inventoried.
- **C3** — Combined with the A-findings, an in-process method can write a file without being
  planned, approved, port-mediated, or child-sandboxed.

## D — Approval is not an independent boundary (P0/P1)

- **D1** — `cap.request` is callable without attachment and receives no caller
  attachment/principal.
- **D2** — It looks up a plan ref, checks the stored plan principal's policy is not `Deny`, and
  sets `approved = true`; the MCP surface exposes this to the same agent that requested the
  plan — self-approval.
- **D3** — There is no auditable binding between requester, plan contents/hash, approver,
  approval scope, and the execution consuming the approval.
- **D4** — Docs describe a human/supervisor in the approval channel, but code does not require a
  distinct authorized approver. Either that model must be implemented or the intended
  self-acknowledgement model documented explicitly.

## E — Zero-config MCP defaults to the unrestricted local principal (P1)

- **E1** — MCP autostarts a kernel and obtains a token only from `SHOAL_TOKEN`. With no token,
  `session.attach` assigns the same-UID `local-human` principal and the permissive policy. The
  socket is mode 0600 (same-user boundary), but an MCP agent normally runs as that user.
- **E2** — Capability isolation is opt-in rather than the zero-config default. A zero-config
  MCP-launched kernel should attach agents as a restricted agent principal by default.

## F — `journal.query` bypasses documented attachment (P1)

- **F1** — Dispatch passes no attachment to `handle_journal_query`, though documentation says
  every method except attach/parse/complete/cap.request requires attachment. Confirmed live: a
  fresh socket connection that never called `session.attach` read stored journal entries.
- **F2** — `limit: 0` returned the full history rather than zero rows — a surprising unbounded
  limit edge case.

## G — Session identity and attribution are inconsistent (P1)

- **G1** — A named kernel session creates its evaluator/journal using only the first attaching
  principal; later principals in the same session inherit that evaluator journal identity.
- **G2** — Kernel-level exec entries use the current actor while evaluator statement-level
  entries may be attributed to the first principal.
- **G3** — Tasks and PTYs are session-scoped, not principal-scoped; another principal attaching
  to the same named session can control them. Possibly intentional pair-shell sharing, but it
  weakens token isolation and needs an explicit model.

## H — Architecture and hygiene (P1/P2)

- **H1** — `Evaluator` is a large mutable god-context (environment, cwd, adapters, jobs, reef,
  journal, security, modules, event bus, ports, config, …); manual child setup directly caused
  the B-findings.
- **H2** — Command resolution is a long hand-ordered precedence chain across bindings, builtins,
  adapters, externals, special commands, module functions, and command references; centralized
  resolution remains incomplete and shadowing interactions are high-risk.
- **H3** — The kernel uses thread-per-connection/task/subscription with a per-session evaluator
  mutex; long evals serialize a session and unbounded connections/tasks can exhaust resources.
- **H4** — Many daemon locks use `.lock().unwrap()`; a panic can poison shared locks and cascade
  failures.
- **H5** — Sessions retain transcript values, tasks, and plans with little or no garbage
  collection; long-lived agent sessions grow indefinitely.
- **H6** — The newline frame size limit is checked only after `read_line` allocates the full
  line (`shoal-proto`, MCP), so the 16 MiB cap does not prevent allocation-based resource
  exhaustion.
- **H7** — Root workspace lint policy is staged but member crates do not inherit it; manifest
  lint policy is not actually active per-crate.
- **H8** — Unix assumptions are pervasive; CI/release cover Linux/macOS only. Windows is not
  supported (and should be stated as out of scope, not left implicit).
- **H9** — ~394 registry dependencies: large supply-chain, compile-time, binary, and maintenance
  surface, with no advisory audit in CI.
- **H10** — Wasmtime/Cranelift lives in the separate `shoal-wasm` crate (good for shipped binary
  scope), but `--workspace --all-targets` CI still pays its compile/storage cost for an
  integration not wired into language dispatch.
- **H11** — Stable Rust is unpinned; compiler drift can break builds.
- **H12** — Fuzz targets exist but nothing ever runs them on a schedule.
- **H13** — Five syntax-highlighter tests assume color is enabled and fail under `NO_COLOR=1`;
  the product honors `NO_COLOR` correctly — a test-isolation defect.

## I — Semantic stubs and docs-reality gaps (P2)

- **I1** — The vision says one kernel / three surfaces, but the shipped human REPL owns an
  embedded evaluator and never attaches to `shoal-kernel` (acknowledged in code comments).
  Human–agent pair-shelling is not the default real architecture yet.
- **I2** — `StreamVal::buffer(n)` is an identity function despite docs implying a bounded
  decoupling buffer (`stream/mod.rs`).
- **I3** — `flat_map` sequentially drains substreams rather than interleaving as documented.
- **I4** — In-language channel subscribers use unbounded `std::sync::mpsc::channel` despite
  bounded/backpressure language in docs (the channel ring is bounded; live subscriber queues are
  not).
- **I5** — Cancelling an `on(channel, handler)` task cannot interrupt a blocking channel `recv`;
  cancelled handler threads can remain stuck.
- **I6** — Incremental stream `.feed` is explicitly unimplemented.
- **I7** — WASM validation/registry code exists but WASM commands are not integrated into
  evaluator dispatch.
- **I8** — Kernel task suspend/resume returns unavailable.
- **I9** — Wire stream values are labels/refs; chunk streaming is deferred.
- **I10** — The LSP is basic lexical/editor assistance and should be described as such, not as a
  mature semantic IDE stack.
- **I11** — Docs/roadmap counts and open items drift rapidly from current source.
- **I12** — Benchmark coverage is thin and unenforced. The prompt benchmark's source comment
  claims per-PR runs and a protected p99 contract that CI never executes; the
  `table_1m_where_sort` benchmark hand-filters a Rust `Vec` and never invokes Shoal table
  methods or the evaluator, providing no evidence for language-level million-row performance.
- **I13** — `distinct` uses a `Vec`: O(n) membership per item, O(n²) total, unbounded memory on
  an unbounded stream.
- **I14** — `zip`/`merge` are synchronous pull operators; rate-skew/backpressure claims need
  precise documentation and tests.

## J — Secrets and token storage (P2)

- **J1** — The secret-store AES key is stored beside the ciphertext in the same protected
  directory; OS permissions are the real boundary, and copying the directory yields both key and
  ciphertext. This must be documented honestly or improved (OS keyring).
- **J2** — Secret values are ordinary `Arc<str>` and are not zeroized; environment injection
  creates additional copies.
- **J3** — Whole-map read/modify/write of the secret store has no interprocess lock; concurrent
  updates can be lost.
- **J4** — Token hashing is reasonable (high-entropy bearer tokens), but token-store updates
  need the same concurrency review as J3.

## Priority direction recorded by the audit

1. Make effect derivation exhaustive and fail-closed (A).
2. One authoritative child-evaluator/context constructor (B).
3. Route every filesystem/process/network effect through enforceable ports (C).
4. Define the threat model; zero-config agents attach restricted (E).
5. Authenticate approval; separate requester from approver (D).
6. Require attachment/authorization for journal and session operations (F, G).
7. Decide whether the REPL truly joins the kernel (I1).
8. Quotas and lifecycle cleanup for sessions, tasks, plans, transcripts, PTYs, connections,
   subscribers; no allocation before frame-size enforcement (H3–H6).
9. Label incomplete semantics explicitly (I2–I10).
10. Add negative-coverage and cross-principal integration testing; actually run fuzzers (A11,
    D/E/F tests, H12).
