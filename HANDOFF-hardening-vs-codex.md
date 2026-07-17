# Handoff: `hardening/deep-audit-2026-07` (Claude, PR #8) ↔ `codex/deep-audit-continuation-2026-07-16`

Both branches independently attacked the **same** 2026-07-16 deep audit, forking from `main` at
`a370b97`. They must be reconciled before either fully merges. This doc is the map.

- **Claude branch**: `hardening/deep-audit-2026-07` → **PR #8** (open). ~60 substantive commits.
  `main` (with dependabot bumps) is already merged in; full gate green
  (fmt, clippy `-D warnings`, `cargo test --workspace`, conformance **1327/1331**, live_kernel **20/20**).
- **Codex branch**: `codex/deep-audit-continuation-2026-07-16` (local only, not on origin at handoff
  time). ~170 substantive commits.

## Method contrast (matters for reconciliation)

- **Claude** worked from an explicit written plan committed to the repo:
  `site/content/internals/deep-audit-2026-07-16.md` (findings frozen as stable IDs **A1–J4**) and
  `site/content/internals/hardening-roadmap.md` (atomic **HR-*** tasks with a finding→task
  traceability matrix). Each fix cites its finding ID and ticks a checkbox. If we keep Claude's
  branch as the base, that matrix is the audit's completeness proof.
- **Codex** worked commit-by-commit without that scaffold; its commits are individually excellent
  but not mapped to finding IDs, so overlap has to be judged per-subsystem (below).

## What Claude did (by audit finding)

| Finding(s) | What landed | Key files |
|---|---|---|
| **A1–A11** | Fail-closed **exhaustive plan derivation**: no-wildcard AST walk, unknown⇒approval-requiring Opaque, `use`/env-writes/redirects/method-sinks/path-reads/`.feed`/effectful-builtins/spawn-parallel-bodies classified, externals⇒concrete ProcSpawn, adapter effect vocabulary complete; 10 audit probes pinned as conformance cases | `shoal-eval/src/plan_derive.rs`, `plan_effects.rs`, `spec/cases/plan-effects.toml` |
| **B1–B6, H1** | One compile-enforced **`ChildContext`** constructor; spawn/parallel/on/.shl inherit leash+ports+config+reef+bus+cancellation. Found+fixed B5 (spawn dropped config port) and B6 (parallel children uncancellable) | `shoal-eval/src/child_context.rs`, `lib.rs`, `script.rs`, `host.rs`, `channels.rs` |
| **C1–C3** | fs writes through the **Fs port**; evaluator `CallCtx::fs()` returns its injected port (denying adapter blocks `.save` end-to-end); committed inventory of every direct `std::fs` site | `shoal-value/src/methods/path.rs`, `stream.rs`, `ports.rs`, `shoal-eval/src/lib.rs` |
| **D1–D8, E1(auth), F1(journal), G(session)** | `cap.request` needs attachment; approval binds requester/approver/plan-hash/scope/execution, **self-approval denied by default** (`SHOAL_ALLOW_SELF_ACK` opt-in); `journal.query` needs attachment + bounded limits; **zero-config MCP attaches as restricted `agent:mcp`** (`SHOAL_MCP_PERMISSIVE`/token opt-in); pair-shell session identity model documented | `shoal-kernel/src/dispatch.rs`, `handlers_session.rs`, `session.rs`, `handlers_exec.rs`, `shoal-mcp` |
| **H6** | 16 MiB frame cap enforced **during read** (bounded reader, no pre-alloc) | `shoal-proto/src/lib.rs`, `shoal-mcp/src/lib.rs` |
| **H4** | Poison-tolerant **`lock_recover()`** across the whole daemon incl. all handler sites | `shoal-kernel/src/lib.rs` + handlers |
| **H3, H5** | Quotas: connections/tasks/PTYs/subscriptions/**sessions** (`QUOTA_EXCEEDED`, CLI-overridable); session GC (transcript/finished-tasks/plan-TTL) | `shoal-kernel/src/lib.rs`, `session.rs`, `eventbus.rs` |
| **H7–H13, I12** | Workspace lints active in all 22 crates; `rust-toolchain.toml` pin (fuzz jobs forced nightly); NO_COLOR test isolation; nightly bounded fuzz CI; honest benchmarks; **cargo-audit CI** | crate `Cargo.toml`s, `.github/workflows/*`, benches |
| **I2–I6, I13, I14** | Real bounded `.buffer(n)`; `flat_map` pinned sequential; bounded subscriber queues + drop markers; cancellable `on`-handler recv; hashed `distinct`; zip/merge pinned | `shoal-value/src/stream/`, `shoal-eval/src/channels.rs`, `streams.rs` |
| **J1–J4 (secrets)** | Secret/token stores: fd-lock interprocess RMW locking, zeroized plaintext, honest key-beside-ciphertext docs (OS keyring deferred) | `shoal-secret/src/lib.rs`, `shoal-auth/src/lib.rs` |
| **H9 (extra)** | **wasmtime 37 → 46.0.1**, clearing all 15 RUSTSEC advisories (2 critical); allowlist now empty | `shoal-wasm/Cargo.toml` |
| **H1 / HR-J2** | **Evaluator decomposition**: full 6-step extraction to the designed three-field façade — `Arc<HostServices>` + `SessionCtx` + `ExecState`, every field private, children only via `ChildContext`; zero behavior change | `shoal-eval/src/lib.rs`, `child_context.rs`, design page `evaluator-decomposition.md` |
| **H3 / HR-J3** | Kernel concurrency-model review page + `max_sessions` cap + last lock sites | `shoal-kernel/src/lib.rs`, `kernel-protocol.md` |

## What Claude did NOT do (still open on the plan)

- **HR-J1 — centralized command resolution** (one resolution table shared by evaluator/planner/
  completion/LSP). Not started. **Codex appears to have done this** (`refactor: split command
  execution and resolution policy`, `f10b922`) — see overlap below.
- **HR-H1 / HR-H2 / HR-H3** (Workstream H) — deliberately **held**: per-feature status labels, the
  REPL↔kernel ADR/decision, and the docs-drift pass. Held precisely *because* Codex was building
  REPL-through-kernel; that work should decide HR-H2 rather than Claude guessing.
- Documented residual debt: read-side fs probes still bypass the port (exemptions listed in the
  ports page); token revocation latency until kernel restart; stream `.feed` incremental (blocked on
  a `shoal-exec::StdinSpec` streaming variant); the old module-split polish item.

## Overlap map (where the two branches collide)

Judged from Codex commit subjects vs Claude's finding fixes. **These are the reconciliation
hot-spots — same subsystem touched by both:**

| Subsystem | Claude | Codex | Assessment |
|---|---|---|---|
| **Poison-tolerant locking** | HR-E2 `lock_recover()` sweep + `ci: reject raw kernel lock unwraps` equivalent | large "quarantine poisoned {session,plan,event,PTY,task,journal} state" series + `ci: reject raw kernel lock unwraps` | **Direct overlap.** Codex went deeper (per-subsystem quarantine + recovery semantics, not just recover-the-guard). Likely **prefer Codex** here; verify Claude's HR-E2 tests still pass on top. |
| **REPL ↔ kernel** | *nothing* (HR-H2 held) | `repl: route interactive sessions through protocol`, `drive interactive execution over kernel tasks`, `add trusted embedded repl transport` | **No conflict, Codex-only.** This is the "one kernel/three surfaces" finding (I1). **Take Codex.** It resolves Claude's held HR-H2. |
| **Command resolution** | HR-J1 *not done* | `refactor: split command execution and resolution policy` | **Codex-only, take it.** But it overlaps Claude's evaluator façade (HR-J2) in `shoal-eval` — see merge risk below. |
| **wasm dispatch** | bumped wasmtime to 46; did **not** wire wasm into eval (I7 left as "not wired") | `execute validated plugin commands`, `install configured wasm plugins`, `isolate wasm {manifest,registry,value envelope}` | **Codex wired wasm into dispatch** (closes I7). **Take Codex's wiring**, but it must be rebased onto **wasmtime 46** (Codex was on 37 — the 15 RUSTSEC advisories). This is the single most important merge action: Codex's wasm code on 46's API. |
| **Quotas / bounds** | HR-E3/E4 connection/task/PTY/subscription/session quotas + GC | `bound CAS decompression/retrieval`, `bound recursion`, `bound completed job history`, `bound reversible trash lifecycle` | **Complementary, not conflicting** — different resources. **Take both**; they compose. |
| **Redirects** | HR-A3 planner classifies `>`/`>>`/`<` as fs effects | `make process redirects a capture boundary`, `reject ambiguous redirect channels`, `preserve specific redirect diagnostics` | **Adjacent** — Claude = *planner* view, Codex = *runtime* semantics. Likely both needed; check the planner still matches Codex's runtime redirect behavior (Claude's HR-A9 principle: planner must resolve like runtime). |
| **Leash pinning** | HR-A plan effects; existing bin_hash spawn pinning | `align plan and spawn pinning semantics` | **Overlap in leash/plan.** Reconcile carefully; both touch plan derivation ↔ spawn gate. |
| **Module splits / isolation** | HR-J2 evaluator façade (`shoal-eval` internal restructure) | `isolate lsp/adapter/repl/prompt/channel-integration`, `split prompt cli/git` | **Both restructure heavily.** Highest textual-conflict risk in `shoal-eval` and host crates. |
| **Kernel autostart** | `ensure_kernel` (already on main) | `handshake embedded startup readiness` | Codex refined it; **take Codex**. |
| **Fuzz/bench** | HR-F4 nightly fuzz CI, HR-F5 honest benches | `broaden fuzz and pipeline benchmarks` | **Complementary**; merge both fuzz sets. |

## Recommended reconciliation path

1. **Merge Claude's PR #8 into `main` first.** It is self-contained, green, and carries the audit
   traceability matrix (the completeness proof). This makes `main` the hardened base.
2. **Rebase Codex's branch onto the new `main`.** Expect real conflicts in: `shoal-eval` (façade vs
   command-resolution split + channel isolation), `shoal-kernel` (lock recovery vs quarantine
   series), `shoal-secret` (already resolved once here — rand 0.10 `try_fill_bytes` + fd-lock),
   `shoal-wasm` (**Codex's wiring must move to wasmtime 46's API**), redirects, leash pinning.
3. **Per hot-spot, the default calls** (verify, don't assume): locking → Codex's deeper quarantine;
   REPL↔kernel → Codex (closes held HR-H2); command resolution → Codex (closes HR-J1); wasm dispatch
   → Codex's logic on Claude's wasmtime 46; quotas/bounds/redirects/fuzz → keep both.
4. **After rebase, re-run the full gate** and confirm the HR-* matrix still holds — every finding
   that was ticked stays ticked, and Codex's additions get folded into the matrix (esp. I1/HR-H2 and
   HR-J1, which Codex resolves).

## Where to look
- Plan + findings: `site/content/internals/deep-audit-2026-07-16.md`,
  `site/content/internals/hardening-roadmap.md` (traceability matrix at the bottom).
- Design pages: `evaluator-decomposition.md`, `kernel-protocol.md` (concurrency + limits sections),
  `effects-plans-security.md`, `security-threat-model.md`.
- Claude's branch fork point: `main@a370b97`. Codex's: same.
