+++
title = "Evaluator decomposition (HR-J2)"
description = "Implementation record for splitting the Evaluator god-context into three cohesive sub-contexts behind the audited child constructor, preserving the pre-change field census and invariants."
weight = 125
template = "docs/page.html"

[extra]
group = "Maintenance"
eyebrow = "Implementation record"
status = "Implemented — three-context evaluator and unified child construction"
audience = "Runtime and security contributors"
wide = true
+++

This page began as the design deliverable for hardening task **HR-J2** on the
[hardening roadmap](@/internals/hardening-roadmap.md), promoted from wave 3 to wave 2 on
2026-07-16 because it is the structural root cause behind the audit's worst findings. The
[2026-07-16 deep audit](@/internals/deep-audit-2026-07-16.md) records it as **H1**: `Evaluator`
recorded a large mutable god-context (environment, cwd, adapters, jobs, reef, journal, security,
modules, event bus, ports, config, …), where manual child setup *directly caused* the B-findings.
That pre-change diagnosis is preserved below; the decomposition and unified child constructor are
now implemented.

`Evaluator` now contains exactly `Arc<HostServices>`, `SessionCtx`, and `ExecState`.
`Evaluator::child_context()` captures those three ownership domains and
`ChildContext::build(ChildKind, CancelToken)` is the only production child construction path for
spawn, scripts, parallel calls, channel handlers, and stream pumps. The constructor destructures the
captured context so omitting an already-captured capability is a compile error; a source-inventory
test rejects new manual `Evaluator::new` child sites.

The authoritative live description is [evaluator state and control flow](@/internals/evaluator-state.md).
The field census and schedule below are retained as implementation rationale, not as claims that
the old flat layout or authority escape still exists.

## Why decompose

Before HR-J2, `Evaluator` held **40 fields** on one flat object. Four production paths built a
child evaluator by hand — `script.rs::spawn_block`, `script.rs::run_script_file`,
`host.rs::builtin_parallel`, `channels.rs::builtin_on` — and each manually copies a *different
subset* of those 40 fields. Every field a site forgets is a capability a child silently loses.
The audit's B-findings are exactly this: a restricted command becomes unrestricted the moment it
is wrapped in `spawn` or `parallel`, because the leash policy is one of the forgotten fields.

No amount of "remember to copy the field" discipline fixes a 40-field manual copy repeated at
four sites. The fix is structural: group the 40 fields into a small number of cohesive
sub-contexts whose ownership and child-inheritance rules are encoded in *types*, so the compiler —
not a reviewer — enforces that a child inherits its parent's authority and resolution inputs.

## Pre-implementation field census

Every `Evaluator` field, grouped by lifetime and annotated with the modules that mutate and
consume it. Evidence is module names (stable) rather than line numbers (which rot). "Mutated by"
lists the modules that assign, push, insert, take, or increment the field; "read by" lists the
consuming modules. Setters and construction in `lib.rs` are elided from "mutated by" unless
`lib.rs` is the *only* writer (i.e. the field is set once at host setup and never touched by an
eval path).

### Class 1 — host services: set once at setup, never mutated by an eval path

These are installed by the host before evaluation and are read-only for the rest of the session.
The census confirms it: each is "mutated in `lib.rs`" only — that is a setter or `inherit_ports`,
never a statement/expression path. All are `Arc<dyn …>`, an `Arc` handle, or a catalog set by a
single setter.

| Field | Type | Read by | Writer | Lifetime |
|---|---|---|---|---|
| `fs` | `Arc<dyn Fs>` | streams, host, expr_access, script, frecency, journal, command, channels, reef_builtins | `set_fs` / `inherit_ports` | session-long, immutable after setup |
| `exec` | `Arc<dyn Exec>` | channels, host, script, command | `set_exec` / `inherit_ports` | session-long, immutable |
| `clock` | `Arc<dyn Clock>` | channels, expr, host, expr_access, frecency, journal, script | `set_clock` / `inherit_ports` | session-long, immutable |
| `opener` | `Arc<dyn Opener>` | channels, host, script | `set_opener` / `inherit_ports` | session-long, immutable |
| `secrets` | `Arc<dyn SecretPort>` | channels, host, script | `set_secrets` / `inherit_ports` | session-long, immutable |
| `config` | `Arc<dyn ConfigPort>` | namespaces (via accessor), lib | `set_config` / `inherit_ports` | session-long, immutable |
| `adapters` | `AdapterCatalog` | host, channels, script, command, plan_derive | `set_adapters` / `load_bundled_adapters` | session-long, immutable |
| `bus` | `Arc<EventBus>` | script, channels | `set_bus` (Arc, already shared) | session-long, shared |
| `reef_resolver` | `Option<Arc<Resolver>>` | reef_resolve, reef_builtins | reef_resolve (lazy build), `set_reef_resolver` | session-long resolution input |
| `reef_user_manifest` | `Option<PathBuf>` | reef_resolve | `set_reef_user_manifest` | session-long resolution input |

`reef_resolver` and `reef_user_manifest` sit here because they are **resolution inputs** — the
provider stack and the user-scope manifest that decide *how a tool name resolves*. They are built
lazily but never change per-statement, and both the runtime and the planner must consult the same
ones (see invariant I2).

### Class 2 — session identity, authority, and presentation

These define *who is acting*, *what they may do*, and *how output is presented*. They change only
via explicit host setup or a kernel re-attach, never per statement (except the journal
attribution transients, split out into Class 3).

| Field | Type | Read by | Writer | Lifetime |
|---|---|---|---|---|
| `principal` | `String` | journal | journal setup / `load_leash_policy` | session identity |
| `session_id` | `String` | journal | journal setup | session identity |
| `leash` | `Option<(LeashPolicy, String)>` | command, lib (`resolve_sandbox`) | `set_leash_policy` / `load_leash_policy` | session authority |
| `journal` | `Option<Journal>` | journal, command | `set_journal` (root-only; not `Sync`) | durable, root-attached only |
| `echo_mode` | `EchoMode` | stmt | `set_echo_mode` | session presentation policy |
| `interactive` | `bool` | reef_resolve, host, command | host setter | session presentation policy |
| `sink` | `Option<StatementSink>` | reef_resolve, lib (`emit`) | `set_statement_sink` | root-only host output binding |

`journal` is the live SQLite handle. It is deliberately **not** shared with thread-spawned
children (single-handle, not `Sync`). The *identity* it records (`principal`, `session_id`) is
what children carry; the handle itself stays on the host-attached root. `sink` is the one host
output that cannot be `Arc`-shared (a `Box<dyn FnMut>`); competing mutable renderers are unsafe,
so children never inherit it and return values through their task result channel instead.

### Class 3 — the mutable execution core (per-eval / per-run / per-statement)

The genuinely mutable state that advances as a program runs. Sub-grouped by finer lifetime.

**Lexical and ambient context** (per-eval; inherited or fresh depending on child kind):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `env` | `Env` | pattern, call, modules, expr, stmt | lexical scope; closures capture the handle |
| `cwd` | `PathBuf` | modules, script | logical working directory |
| `process_env` | `Vec<(OsString, OsString)>` | stmt, script | child process environment snapshot |
| `oldpwd` | `Option<PathBuf>` | command | `cd -` target |
| `dir_stack` | `Vec<PathBuf>` | command | `pushd`/`popd`/`dirs` |

**Control and recursion** (per-eval; fresh in children):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `cancel` | `CancelToken` | lib (`reset_cancel`) | cooperative cancellation epoch |
| `call_depth` | `usize` | call (inc/dec around every callable) | recursion guard; maximum depth 128 |
| `in_fn_body` | `usize` | call (inc/dec), modules (save/restore) | gates ambient `cd` / `env.NAME =` |
| `pending_exit` | `Option<i32>` | command, lib (`take_exit`) | host-honored exit request |
| `it` | `Value` | stmt | last top-level value |

**Reef dynamic overlay and per-cwd cache** (per-eval; cwd/manifest-derived):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `reef_overrides` | `Vec<ScopeEntry>` | reef_resolve | dynamic `with reef:` stack, innermost-first |
| `reef_chain` | `Option<(PathBuf, ScopeChain)>` | reef_resolve, reef_builtins | cache keyed on the cwd it was discovered for |
| `reef_lock` | `Lockfile` | reef_builtins, reef_resolve | in-memory lock beside nearest manifest |
| `reef_lock_path` | `Option<PathBuf>` | reef_resolve | lock persistence target |

**Job and task registry** (per-eval; fresh in children):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `jobs` | `Vec<TaskVal>` | channels, script, lib | live task table |
| `external_jobs` | `HashMap<u64, u32>` | lib | task id → stopped child pid |
| `pending_stop` | `Option<(u64, String)>` | lib | newest stopped foreground command |

**Module memoization and derived plans** (session cache; root-scoped):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `modules` | `HashMap<PathBuf, Value>` | modules | once-per-evaluator export cache |
| `module_stack` | `Vec<PathBuf>` | modules | circular-import detection |
| `plans` | `Vec<Program>` | plan | derived-but-unapplied `plan { … }` store |

**Journal attribution transients** (per-run / per-statement; fresh in children):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `source` | `Option<String>` | journal | current program source, for statement slicing |
| `current_entry` | `Option<i64>` | journal | open top-level entry id for undo attachment |

**Persistence path** (root-only):

| Field | Type | Mutated by | Lifetime |
|---|---|---|---|
| `jump_store` | `Option<PathBuf>` | frecency | frecency persistence target; `None` disables writes |

### What the four child sites copied before HR-J2

The census made the drift concrete in the pre-change `spawn_block`, `run_script_file`,
`builtin_parallel`, and `builtin_on` implementations:

| Capability | `spawn` | `.shl` script | `parallel` | `on` handler |
|---|:--:|:--:|:--:|:--:|
| effect ports (fs/exec/clock/opener/secrets) | copied | via `inherit_ports` | copied | copied |
| **`config` port** | **dropped** | via `inherit_ports` | **dropped** | **dropped** |
| `adapters` | copied | copied | copied | copied |
| event `bus` | copied | copied | **dropped** | copied |
| lexical `env` | shared | fresh root (intended) | shared | shared |
| `cwd` / `process_env` | copied | copied | copied | copied |
| **`leash` policy/principal** | **dropped** | **dropped** | **dropped** | **dropped** |
| **reef inputs + overlay/cache** | **dropped** | **dropped** | **dropped** | **dropped** |
| `cancel` wiring | fresh, linked to task | **fresh, unlinked** | **fresh, unwired** | fresh, linked to task |
| journal handle | not inherited (rule) | not inherited (rule) | not inherited (rule) | not inherited (rule) |

Two omissions were worth flagging beyond the audit's B-table (which listed spawn's losses as only
leash + reef): **`spawn_block` also drops the `config` port** (it copies the five other ports by
hand but not `config`; only `run_script_file` keeps config, because only it calls
`inherit_ports`), and **cancellation propagation is silently inconsistent** — `run_script_file`
gives its child a fresh `CancelToken` that is never linked to the parent, and `builtin_parallel`
neither links a token nor installs an `on_cancel` hook, so **parallel children are effectively
uncancellable**. These belong on the audit page as refinements of B3 and as a cancellation-scope
finding. They motivated the typed child seed and cancellation rules now in `child_context.rs` and
`exec_state.rs`.

## Implemented shape

The implementation collapses the 40 flat fields into a three-field façade:

```rust
pub struct Evaluator {
    host: Arc<HostServices>, // Class 1: shared, immutable, unconditionally inherited
    session: SessionCtx,     // Class 2: identity + authority + presentation
    exec: ExecState,         // Class 3: the mutable per-eval core
}
```

`Evaluator::child_context()` captures `Arc<HostServices>`, `SessionCtx`, and an explicit
`ChildExecSeed`; `ChildContext::build` consumes them and constructs a fresh/inherited `ExecState`
according to `ChildKind` (`Spawn`, `Script`, `Parallel`, `OnHandler`, or `StreamPump`). Because the
captured fields are required and destructured, a child that omits the parent's captured authority or
resolution inputs **does not compile**.

### `HostServices` — `Arc`, shared, unconditionally inherited

Holds Class 1. Wrapped in a single `Arc<HostServices>`; a child clones the `Arc` (one refcount
bump), never the contents. `adapters` moved behind the `Arc` too (it was deep-cloned at each
child site — the `Arc` makes that free). Setters (`set_fs`, `set_adapters`,
`set_reef_resolver`, …) run only during host setup, before any child exists; at runtime
`HostServices` is read-only.

| Field | Child rule | Why |
|---|---|---|
| fs, exec, clock, opener, secrets, config | inherit (Arc) | a fake/host port must behave identically in a child; **config inheritance closes the B3 config-drop by construction** |
| adapters | inherit (Arc) | command resolution must not diverge in a child |
| bus | inherit (Arc) | session channels are cross-task coordination; parallel's dropped bus is fixed here |
| reef_resolver, reef_user_manifest | inherit | children must resolve tools against the same providers and user scope |

### `SessionCtx` — identity, authority, presentation; inherited with identity

Holds Class 2. Cloned into a child (`String`s and an `Option<(LeashPolicy, String)>` are cheap;
the `LeashPolicy` can be `Arc`-wrapped if profiling warrants). The kernel swaps a *new*
`SessionCtx` when a different principal attaches to a named session, while `HostServices` stays
shared — that is exactly what makes attribution follow the current actor (invariant I3).

| Field | Child rule | Why |
|---|---|---|
| principal, session_id | inherit | journal attribution must name the acting principal, not "the first attacher" |
| leash | **inherit (the core fix)** | a child must never escape the parent's confinement — the B2/B4 root cause |
| echo_mode | inherit | a `Quiet` non-interactive session's children stay quiet |
| interactive | **fresh = false** | a spawned/parallel/handler task has no controlling terminal |
| journal (handle) | **fresh = None** | the `Journal` is not `Sync`; children carry identity, not the live handle |
| sink | **fresh = None** | one mutable renderer per session; children return via their result channel |

### `ExecState` — the mutable per-eval core

Holds Class 3. The constructor builds it per `ChildKind`: a closure-capturing child (`spawn`,
`parallel`, `on`) inherits `env`/`cwd`/`process_env`/`oldpwd`/`dir_stack` and the reef overlay;
a `.shl` script child takes a **fresh root `env`** (intended isolation) but still inherits
`cwd`/`process_env` and the reef overlay. Everything else starts fresh.

| Field group | Child rule | Why |
|---|---|---|
| env | inherit for closure children; fresh root for scripts | closures need capture; scripts must not leak `let`s back to the parent |
| cwd, process_env, oldpwd, dir_stack | inherit (snapshot) | commands need the caller's dynamic context |
| reef_overrides, reef_chain, reef_lock, reef_lock_path | **inherit** | a `spawn` inside `with reef:` must honor the override and the locked versions; the pre-change children dropped all four |
| cancel | fresh, **linked to the child's task** | fixes the unwired-cancel gap in script/parallel |
| call_depth, in_fn_body | fresh (0) | recursion / `fn`-body nesting is per-eval |
| it, pending_exit, pending_stop | fresh | top-level result and exit/stop notices are per-run and host-consumed |
| jobs, external_jobs | fresh (empty) | a child has its own task table |
| modules, module_stack | fresh | a child re-memoizes its own imports |
| plans | fresh (root-only in practice) | `plan { … }`/`apply` is a REPL/root verb |
| source, current_entry | fresh | set at the child's own `eval_program` / per-statement |
| jump_store | **fresh = None** | never write frecency from a background thread; avoids a file race on inherited `cwd` |

## Invariants the split must enforce

The decomposition is only worth doing if it makes these true **by construction**:

- **I1 — children cannot lose policy or identity.** `Evaluator::child` takes
  `Arc<HostServices>` and `SessionCtx` as required, typed parameters. There is no
  `Evaluator::new`-plus-manual-copy path reachable from a child site once HR-B6 removes
  `inherit_ports` and the four hand-copies. Omitting the leash, config, reef inputs, or principal
  from the already-captured child context becomes a compile error. Adding evaluator state still requires an explicit
  inheritance review. (Retires the route-by-route portion of H1 → B1–B4.)
- **I2 — planner and runtime consult the same resolution inputs.** `adapters`,
  `reef_resolver`, and `reef_user_manifest` live in the single `Arc<HostServices>` that both the
  `eval_*` and `plan_*` paths borrow. There is no second resolution table for the planner to
  drift from, which is the structural precondition for centralized command resolution
  ([HR-J1](@/internals/hardening-roadmap.md)) and removes the class of bug behind **A9** (the
  `.feed(cat)` planner-vs-runtime mismatch).
- **I3 — journal attribution follows the current actor.** `principal`/`session_id` live in
  `SessionCtx`; when a second principal attaches to a named kernel session, the kernel installs a
  new `SessionCtx` over the shared `HostServices`, so statement-level attribution matches the
  kernel's exec-entry attribution. The live `Journal` handle stays root-only (not `Sync`), so
  children carry identity but write nothing durable — today's rule, now explicit. (Supports
  [HR-D7](@/internals/hardening-roadmap.md); addresses **G1/G2**, recorded also in the inter-crate
  contract's session-identity risk note.)
- **I4 — `HostServices` is immutable after construction.** All Class-1 setters run during host
  setup before any child exists; at runtime the bundle is read-only, so a child sharing the `Arc`
  can never observe a half-updated host. This is why the census shows every Class-1 field
  "mutated in `lib.rs` only."
- **I5 — no observable behavior change.** The three-context evaluator holds the identical field
  set, merely regrouped. Every one of the **1,310 conformance cases** and every unit test passes
  unchanged; the census is the proof that no field is added, dropped, or re-typed in a
  behavior-visible way.

## Historical staged extraction plan

The following is the individually gateable sequence used to land the implementation. It is retained
to explain why the broad field moves followed the security seam and characterization tests; the
steps are complete, and the old conflict notes are historical.

**Step 0 (completed prerequisite).** HR-B1 landed the single child-context constructor. Every
production child route now uses that seam.

**Step 1 — Decomposition characterization tests (completed).** Before any struct moves,
pin the target invariants: assert that a child built via each `ChildKind` (`spawn`, `parallel`,
`on`, `.shl`) observes the parent's leash policy, reef resolution inputs, config port, and
journal identity. Test-module-only; conflicts with nobody; turns every later step into a provable
no-op. *Overlaps HR-B7* — coordinate so the two suites do not duplicate; this suite pins the
decomposition's field-level invariants specifically.
*Size: ~1 agent-day. Conflicts: none (test module).*

**Step 2 — Extract `SessionCtx` (completed).** Move `principal`, `session_id`, `leash`, `echo_mode`,
`interactive`, `journal`, `sink` into a `SessionCtx` struct; access via `self.session.…`. Make
the child constructor take it by value. This step alone retires the H1 → B security core: after
it, a child cannot be built without a `SessionCtx`.
*Size: ~1 agent-day. Conflicts: **Workstream D (HR-D1–D7)** heavily edits `journal.rs`,
`principal`/attribution, and `command.rs` leash reads. Schedule **after** the D-wave lands or
pair with the D agent; this is the one high-overlap step.*

**Step 3 — Group the reef overlay + cache into the child-inherited set (completed).** Bundle
`reef_overrides`, `reef_chain`, `reef_lock`, `reef_lock_path` into a `ReefState` sub-struct inside
`ExecState`, and make the constructor clone it into children. Closes the reef half of finding B
(the pre-change children dropped all four).
*Size: ~1 agent-day. Conflicts: **HR-J1** (centralized resolution) and any reef work touching
`reef_resolve.rs`/`reef_builtins.rs`. Coordinate with the resolution refactor.*

**Step 4 — Extract `HostServices` behind `Arc` (completed).** Move the ten Class-1 fields into
`HostServices`, hold `Arc<HostServices>`, rewrite `self.fs` → `self.host.fs` (and siblings)
across ~15 modules, and replace `inherit_ports` with `host: parent.host.clone()`. Mechanically
simple (these fields are never eval-mutated) but the **broadest textual rename** in the plan.
*Size: ~1 agent-day. Conflicts: **Workstream C (HR-C1–C3)** adds `self.fs` call sites;
**HR-A9** reads `adapters`/reef inputs. Schedule in a low-contention window **after** C and A9
settle; the rename is mergeable but noisy.*

**Step 5 — Extract `ExecState` and collapse `Evaluator` to the three-field façade (completed).** Move the
remaining Class-3 fields into `ExecState`; `Evaluator` becomes `{ host, session, exec }`. Field
moves within `stmt.rs`/`command.rs`/`expr*.rs`/`call.rs`/`modules.rs`. Broad but mechanical.
*Size: ~1–2 agent-days. Conflicts: touches the "hot" eval-heavy modules every language change
edits — schedule when eval edit pressure is low; individually gateable because it is a pure field
move.*

**Step 6 — Delete `inherit_ports` and every manual child field-copy (completed HR-B6).** With the
three contexts in place, `Evaluator::child` is the only way to build a child. Remove
`inherit_ports`, the hand-copies in `spawn_block`/`run_script_file`/`builtin_parallel`/
`builtin_on`, and privatize `Evaluator::new` field access so no site can under-inherit again.
Verify no `Evaluator::new`-plus-manual-copy survives outside the test module.
*Size: ~1 agent-day. Conflicts: `script.rs`/`host.rs`/`channels.rs` child sites — coordinate with
anyone editing those concurrency paths.*

### Historical conflict schedule at a glance

| Step | Primary files | Conflicts with | Scheduling note |
|---|---|---|---|
| 1 | test module | — | anytime |
| 2 | journal.rs, command.rs, lib.rs | HR-D1–D7 | after / with the D-wave |
| 3 | reef_resolve.rs, reef_builtins.rs, lib.rs | HR-J1, reef work | with the resolution refactor |
| 4 | ~15 modules (`self.fs`→`self.host.fs`) | HR-C1–C3, HR-A9 | low-contention window, after C/A9 |
| 5 | stmt/command/expr/call/modules | eval-heavy work | low eval-edit-pressure window |
| 6 | script.rs, host.rs, channels.rs | concurrency-path edits | coordinate on child sites |

Steps 1–3 proceeded ahead of the broad renames; steps 4–6 completed the three-context façade and
removed manual child copies. Future fields must still be classified deliberately as shared,
inherited/root-only, or fresh execution state and covered by child-inheritance tests.

## Non-goals

This refactor deliberately does **not**:

- **Change any language semantics.** Command dispatch order, the echo/`Position`/`Outcome.streamed`
  presentation machine, scope transitions, coercion, and flow control are untouched. **Zero
  conformance case edits are expected** — the 1,310 cases must pass byte-identically, and any case
  change is a signal the refactor altered behavior and must be reverted.
- **Change the wire or MCP protocol.** `shoal-proto` types, kernel framing, and the MCP facade
  are unaffected; the split is entirely internal to `shoal-eval`. No shared/wire type moves.
- **Centralize command resolution.** That is [HR-J1](@/internals/hardening-roadmap.md).
  Decomposition only *enables* it by giving resolution inputs one home (`HostServices`, invariant
  I2); it does not rewrite the dispatch precedence chain.
- **Make effect derivation fail-closed.** That is workstream A (HR-A1–A11). The split guarantees
  planner and runtime *share* resolution inputs but does not change what `plan_derive` classifies.
- **Close the filesystem-port gaps.** Direct `std::fs`/`OpenOptions` sites in value methods and
  the evaluator's direct path observations are workstream C (HR-C1–C4). The split neither adds nor
  removes port coverage.
- **Share the live journal handle with children.** The `Journal` remains root-only and not
  `Sync`; children carry identity, not the handle. Any change to that ownership is a separate
  decision under HR-D7.
- **Add quotas or lifecycle GC.** Session/task/plan/transcript limits are workstream E
  (HR-E3/E4). Decomposition changes ownership, not retention.
