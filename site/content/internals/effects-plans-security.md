+++
title = "Effects, plans, ports, and authority"
description = "How Shoal describes side effects, derives stable plans, evaluates principal policy, lowers sandboxes, and keeps testable capabilities at the runtime edge."
weight = 50
template = "docs/page.html"

[extra]
group = "Execution & security"
eyebrow = "Authority model"
status = "Policy plus best-available enforcement"
audience = "Security, evaluator, and kernel contributors"
wide = true
+++

Shoal separates four questions that are easy to conflate:

1. **What might this program do?** Static plan derivation answers with semantic effects.
2. **May this principal do it unattended?** Leash policy returns allow, deny, or approval required.
3. **What can the OS enforce for this spawn?** Sandbox lowering and the execution backend report it.
4. **How does evaluator code reach the outside world?** Ports make those capabilities explicit and
   replaceable in tests.

Policy is authority. Sandboxing is defense in depth and must report partial enforcement honestly.

## Effect vocabulary

Plans contain concrete effects from a closed enum:

| Effect | Meaning |
|---|---|
| `FsRead`, `FsWrite`, `FsDelete` | access to named path sets |
| `ProcSpawn` | spawn identified by executable hash and `argv0` |
| `NetConnect`, `NetListen` | outbound host/port or inbound port |
| `EnvRead`, `EnvWrite` | session/process environment names |
| `SecretUse` | named secret access |
| `SessionWrite` | mutation of session state |
| `JournalRead` | durable history access |
| `Time` | clock observation |
| `Opaque` | behavior cannot be bounded by the static derivation |

Every `Plan` also carries `Reversibility` (`Reversible`, `Irreversible`, or `Unknown`), optional byte
and item estimates, and a stable `plan_ref` derived from canonical serialized contents.

```mermaid
flowchart LR
accTitle: Effect derivation and plan model
accDescr: Source structure, builtin effect knowledge, and current evaluator state combine into a plan containing effects, estimates, and reversibility.
  Source["Program AST"] --> Walk["plan derivation"]
  Registry["builtin + adapter effect registry"] --> Walk
  State["cwd + environment + principal state"] --> Walk
  Walk --> Effects["fs / proc / net / env / secret / session / journal / time / opaque"]
  Effects --> Plan["Plan: plan_ref + ordered effects"]
  Walk --> Estimate["bytes / items estimates"]
  Walk --> Reverse["reversible / irreversible / unknown"]
  Estimate --> Plan
  Reverse --> Plan
```

Source: [`shoal-leash/src/effects.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal-leash/src/effects.rs).

## Plan derivation

The evaluator walks AST and command metadata without executing it. Known builtins and adapter specs
can contribute concrete effect templates. Paths and arguments are resolved as far as the AST and
current evaluator state allow. Dynamic calls, unknown external behavior, or constructs that cannot
be bounded become `Opaque` and normally make reversibility unknown.

Adapter effect declarations parse against the **full** effect vocabulary — `fs.read`, `fs.write`,
`fs.delete`, `proc.spawn`, `net.connect`, `net.listen`, `env.read`, `env.write`, `secret.use`,
`session.write`, `journal.read`, and `time` — in both parenthesized (`proc.spawn(container)`) and
bare (`session.write`) forms. A declaration whose kind is outside the vocabulary is **never silently
dropped**: it plans as `Opaque` so an unrecognized adapter effect forces approval rather than
vanishing from the plan.

Derivation is intentionally conservative and fail-closed. The AST walk is structurally exhaustive —
a match over every statement and expression node with **no wildcard arm**, so a new syntax form
cannot be added without classifying its effects. Concretely the walk covers: `use` (module `FsRead`
plus `Opaque` for the module body), persistent `env.NAME = …` (`EnvWrite`), command redirects
(`>`/`>>` → `FsWrite`, `< file` → `FsRead`), the method sinks `.save`/`.append` and the path reads
`.read`/`.lines`/…, `.feed` (an **external** spawn of the fed command, matching the runtime's
`run_argv` path rather than builtin dispatch), the effectful builtins `run`/`open`/`save` and the
bodies of `spawn`/`parallel`/lambda arguments, and generic external commands (a concrete `ProcSpawn`
with a resolved binary hash, consistent with adapter spawns). A call that cannot be statically
expanded — a session-stored closure, or any unrecognized name — becomes an approval-requiring
`Opaque` effect; an empty effect set is emitted only for forms that are provably effect-free
(literals, pure constructors, and control-flow scaffolding). It is safer to require approval for an
opaque program than to manufacture a precise-looking plan that omits a dynamic effect.

Sources: [`plan_derive.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal-eval/src/plan_derive.rs)
and [`plan_effects.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal-eval/src/plan_effects.rs).

## Policy evaluation

Policy is keyed by principal. Each principal can grant path globs, executable names/hashes, network
destinations, environment names, secrets, session/journal/time access, an opaque mode, hermetic
intent, and an automatic-apply rule.


Denial dominates approval, which dominates allow. Unknown principals deny at this evaluator. Local
human operation ordinarily installs a built-in permissive principal policy to preserve normal shell
behavior.

### Spawn pinning exception

An empty `proc_spawn` list means spawn pinning is inactive, not “deny every ordinary command.” The
spawn path first checks `spawn_pinning_active`; only principals that opted into a non-empty allowlist
pay the hash-and-match gate. This exception is explicit in the policy API and must remain covered by
tests if plan evaluation is refactored.

### Approval lifecycle in the kernel

```mermaid
sequenceDiagram
accTitle: Approval lifecycle in the kernel
accDescr: Shows the components and relationships described in Approval lifecycle in the kernel.
  participant C as client
  participant K as kernel
  participant P as policy
  C->>K: exec(mode="plan", src)
  K->>P: derive + evaluate
  K-->>C: plan_ref, effects, verdict
  C->>K: cap.request(plan_ref)
  K->>K: mark stored plan approved
  C->>K: exec(mode="approved", plan_ref, same src)
  K->>K: verify session + principal + source + approval/current allow
  K->>P: re-derive and evaluate current plan
  K-->>C: result or leash error
```

`plan.apply` and approved `exec` re-check the currently stored session, principal, source, and
approval state. That execution-side check is real. The approval mutation itself is currently unsafe:
`cap.request` is routed without an attachment, receives no caller principal, and marks a global
stored plan approved by ref. A direct socket caller therefore does not prove approver authority.

Plan refs are also not unique stored-object identities. `Plan::new` hashes only effects,
reversibility, and estimates (first 16 hex characters), while the kernel map is keyed solely by that
ref. Two plans with equal effect shape but different source/session/principal overwrite the same
entry. Treat the current ref as a short content-shape fingerprint, not a capability or stable object
ID, until the [P0 authority work](../roadmap-and-priorities/)
lands.

## Ports

The evaluator holds ports for filesystem, execution, clock, opener, secrets, configuration, and
CAS-byte loading. Default ports perform real host actions; tests can inject deterministic fakes.
This is an improving-but-incomplete seam. Every **write** effect the language exposes now crosses
the `Fs` port (the ledger below is the proof); the residue is a set of read-only `Path::exists`,
`is_dir`, `is_file`, and `canonicalize` *observations* and OS watcher setup that still call the
filesystem directly. See the filesystem-boundary ledger in the evaluator-state chapter.

### In-process filesystem-effect ledger (HR-C3, 2026-07-16)

Inventory of every `std::fs`/`OpenOptions` use in `shoal-value` and in `shoal-eval`'s value/method
paths. Each site is either **routed** through the `Fs` port or an **exempt** read-only observation
with a stated reason. Kept exact; a new effectful filesystem call adds a row.

**Routed write/read effects** — mediated by the `Fs` port, so a fake can observe or deny them and a
sandbox can enforce them:

| Site | Effect | Port route |
|---|---|---|
| `shoal-value` `methods/path.rs::save` — value `.save`/`.append`, `save(path, value)` builtin | file write / append | `CallCtx::fs().write` / `.append` (HR-C1) |
| `shoal-value` `methods/stream.rs::stream_save` — stream `.save`/`.append` | open-once incremental append | `CallCtx::fs().open_append` (HR-C2) |
| `shoal-value` `ports.rs::StdFs` | every `std::fs` syscall | the port adapter itself — the boundary, not a bypass |
| `shoal-eval` `expr_access.rs::path_fs_method` — path `.read`/`.read_bytes`/`.lines`/`.exists`/`.is_dir`/`.is_file`/`.size`/`.modified` | file read / stat | `self.fs.read` / `.metadata` |
| `shoal-eval` `command.rs` redirects `>` and `>>` | file write / append | `self.fs.write` / `.append` |
| `shoal-eval` `builtins.rs` — `cat`/`ls`/`mkdir`/`touch`/`mv`/`cp`/`rm`/`trash`/`ln` | read / write / dir / rename / link | `self.fs.*` |
| `shoal-eval` `frecency.rs` dir-jump store load/save | read / write / rename | `self.fs.*` |
| `shoal-eval` `journal.rs` undo snapshot + restore | read | `self.fs.read` |
| `shoal-eval` `reef_builtins.rs` manifest read | stat / read | `self.fs.is_file` / `.read_to_string` |

**Exempt read-only observations** — non-mutating existence/type/canonicalization probes that still
call `Path::*` directly. They neither write, delete, nor spawn, so they are not the "in-process
write that escapes plan/approval/sandbox" the C-findings target; several already read from
port-fetched `Metadata`. Routing them through an `Fs` stat/exists/canonicalize method is a read-side
follow-up, not part of lane C's write-effect mandate:

| Site | Call | Why exempt |
|---|---|---|
| `builtins.rs` `root.is_dir()` (ls), `dest.is_dir()` (cp/mv) | `Path::is_dir` | type probe guarding a *ported* `fs.read_dir`/`fs.copy`/`fs.rename` |
| `command.rs::cd` `joined.canonicalize()` | `Path::canonicalize` | cwd resolution; `cd` is a session-state change, not an `FsWrite` |
| `modules.rs` module resolution | `Path::is_file` / `canonicalize` | read-only discovery; the module read/exec is planned as `FsRead` (HR-A1) |
| `script.rs` `resolved.exists()` | `Path::exists` | dispatch probe before `run_script_file` (itself a read) |
| `streams.rs` watch/tail `root.exists()` / `path.exists()` | `Path::exists` | source-existence guard; the source read uses `self.fs.open_read` |
| `journal.rs` undo target `is_file`/`exists`/`is_dir` | `Path::*` | read-only guards choosing the undo inverse; the mutation is ported |

The `Fs` port also mediates the `CallCtx::fs()` seam value methods reach through. Its default is the
real filesystem (`StdFs`) so a portless context is byte-identical to the pre-port `OpenOptions`
code. The evaluator's `impl CallCtx` overrides `fs()` to return its injected `Arc<dyn Fs>`
(`set_fs`), so value-method writes consult the session's actual port; a denying injected adapter
blocks `"x".save(...)` end to end (`value_method_saves_go_through_the_injected_fs_port`).

Child evaluators created by `spawn_block`, `.shl` `run_script_file`, `parallel`, and `on` inherit
the parent's Leash policy/principal, all ports (including `ConfigPort`), Reef state, event bus, and
cancellation through the single `ChildContext` constructor (HR-B1–B6); cross-route propagation is
pinned by `child_context_propagation.rs`. Leash is transitive across child construction by
compile-enforced design rather than per-site convention.

```mermaid
flowchart LR
accTitle: Ports
accDescr: Shows the components and relationships described in Ports.
  Builtin["evaluator builtin / method"] --> Trait["port trait"]
  Trait --> Std["standard host implementation"]
  Trait --> Fake["recording / in-memory test port"]
  Std --> OS["filesystem / child / clock / desktop / secret store"]
  Fake --> Assert["assert requested capability and arguments"]
```

This boundary is not only for test convenience. It prevents value methods and language semantics
from acquiring accidental ambient authority. A new side-effecting builtin should define its effect,
policy behavior, port method, standard implementation, and fake-port test together.

## Lowering grants to an OS sandbox

Filesystem globs are reduced to their longest concrete leading roots. Nonexistent roots are dropped;
an unrestricted root grant or a grant set that yields no useful existing roots produces no sandbox.
The plan verdict remains the authority in those cases.

```mermaid
flowchart TD
accTitle: Lowering grants to an OS sandbox
accDescr: Shows the components and relationships described in Lowering grants to an OS sandbox.
  Grants["principal fs glob grants"] --> Root["extract concrete roots"]
  Root --> Existing["retain existing paths"]
  Existing --> Useful{"restricted, non-empty roots?"}
  Useful -->|no| None["no OS wrapper; policy still applies"]
  Useful -->|yes| Sandbox["SandboxPolicy"]
  Sandbox --> Linux["Linux Landlock"]
  Sandbox --> Mac["macOS Seatbelt"]
  Sandbox --> Other["other platform: advisory"]
```

The concrete request records filesystem scopes, a coarse network policy, optional spawn hash, and a
`hermetic` flag. A hermetic request must fail the spawn if any requested dimension cannot be
enforced; non-hermetic execution uses the strongest available backend and returns an
`EnforcementStatus` describing what actually happened.

### Enforcement truth table

| Dimension | Current status |
|---|---|
| filesystem on supported Linux | Landlock backend |
| filesystem on macOS | Seatbelt backend |
| executable identity | preflight BLAKE3 check; no exec-time pin |
| network | plan/policy gate only; no seccomp/network-namespace backend |
| unsupported OS | advisory policy, no strong OS sandbox |

The spawn hash has a documented time-of-check/time-of-use gap: the path is hashed before `exec`, so
the file can theoretically change between those events. Enforcement reports this rather than
claiming an exec-time guarantee.

Source: [`shoal-leash/src/enforce.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal-leash/src/enforce.rs).

## Authentication, capabilities, and policy are distinct

Kernel bearer tokens authenticate a principal. Token records also carry advertised capability
metadata, but Leash authorization is evaluated against principal policy. Do not describe the token's
capability list as if it directly grants an effect.


The auth store persists a keyed hash, expiry, revocation state, and the keyed-hash secret in the same
mode-restricted store. It does not persist the original bearer token. Verification uses a
constant-time comparison. File permissions are part of the threat model; this is local same-user
infrastructure, not a hardware-backed identity service.

## Secret storage

`shoal-secret` stores a name/value map encrypted with AES-256-GCM. It validates restrictive directory
and file modes and rewrites the encrypted map on mutation. The master key resides alongside the
store under the same user-level permission boundary. This protects accidental disclosure and
detects ciphertext tampering; it does not protect against a process already running as the same
compromised user. Values of type `Secret` are deliberately not generally renderable/feedable.

## WASM boundary: validated, not integrated

`shoal-wasm` validates component/manifests, rejects ambient imports, and represents fuel, memory,
table, and instance ceilings. Its `Limits` type has **no wall-clock timeout**. It currently has no
evaluator or command-dispatch dependency and no invocation
API wired into normal Shoal execution. Treat it as a prepared isolation component, not a supported
plugin runtime. Integration must start with an explicit host-capability interface and effects—not by
calling it directly from a builtin.

## Fail-open local policy is a conscious risk

`Policy::load_user_or_permissive` falls back to a permissive policy if the per-user policy is missing
**or malformed**, so a syntax error does not brick an interactive shell. That is convenient for local
humans and dangerous if callers assume malformed policy fails closed. Kernel startup with an
explicit policy path uses the fallible loader instead. Any new agent host should choose and document
its loader deliberately.

## Review checklist for a new effect

- Is the effect concrete enough to evaluate without executing?
- What makes a path/name/hash comparison canonical and cross-platform?
- Does denial dominate every alternate dispatch route, including adapters and scripts?
- Is approval bound to principal, session, exact source, and current plan contents?
- Which port performs the action, and can a test observe it without real IO?
- Which OS dimensions are actually enforced, and what does hermetic mode do when unavailable?
- Is undo evidence sufficient to call the mutation reversible?
- Can a secret or raw path escape through rendering, errors, events, journal rows, or wire values?
