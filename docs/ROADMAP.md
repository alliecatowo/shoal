# shoal — execution roadmap for the remaining pieces

**Purpose.** A fire-and-forget plan: every unbuilt piece, sequenced into waves that respect the one
hard constraint (below), each with a *locked* design decision so implementation is mechanical, an
ownership partition that avoids collisions, and acceptance criteria. A future session (or an
autonomous `continue` loop) can execute any wave from this doc with zero re-discovery.

**The one hard constraint.** `crates/shoal-eval` is the collision bottleneck — almost every feature
routes through it. **At most one agent edits `shoal-eval` per wave.** Eval-heavy work therefore
serializes; non-eval work parallelizes alongside it.

**Every wave ends the same way** (the pinned loop): `cargo fmt --all --check` + `cargo +stable
clippy --workspace --all-targets --locked -- -D warnings` + `cargo test --workspace --locked` green,
conformance not regressed, then a signed commit (`Co-Authored-By` trailer) → `git push` →
`gh run watch` until all 6 CI jobs (incl. `test (macos-latest)`) are green; fix any macOS-only test
failure test-side (canonicalize temp paths for the `/tmp`→`/private/tmp` alias; gate genuinely
Linux-only behavior) and re-push. macOS is first-class: never a stub, never silently second-class.

---

## Status snapshot (verified against source + the live binary at time of writing)

**Corpus**: 1,218 `[[case]]` entries across 74 files in `spec/cases/` — 1,211 passed, 0 failed, 7
skipped (all host-dependent: a real tool's resolved hash/version, a live PATH-inventory-dependent
binding table). Well past the TDD §12 target of ≥1,000.

**Waves R0–R3 are DONE** (see per-wave notes below for exactly what shipped and how it was
verified). **Wave R4 is mostly done** (hexagonal ports shipped; the big file splits landed; the
builtin-identity registry has landed too — see below — but the broader command-*resolution*
unification into one `resolve.rs` has not). **Wave R5 is in continuous progress** (corpus
target already exceeded; wiki kept current; most small carryovers landed, a few remain — see below).

Broad "done" list, each independently verified against the binary or a targeted grep while writing
this revision (don't take this list on faith either — it's a snapshot, re-verify anything
load-bearing):

- Full language + dispatch + `match`; outcome unification; the R0 REPL fixes (`OutcomeVal.streamed`
  so statement-position builtins render; `exit`/`quit` via a host-level exit flag, not
  `std::process::exit`).
- reef resolution end-to-end, including **project-scope `.reef.toml` walking as the live resolution
  path** (verified: a `.reef.toml` with `[tools] sh = "*"` in a scratch dir changes `which sh`'s
  reported scope/chain to `"reef"` — this was still "landing" as of the previous ROADMAP revision
  and is not anymore), `which`/`with reef:`, the lockfile.
- Hexagonal ports (`Fs`/`Clock`/`Opener`/`SecretPort` in `shoal-value/src/ports.rs`, `Exec` in
  `shoal-eval/src/ports.rs`) — see `docs/CONTRACTS.md` §8. `shoal-eval`'s internals are split across
  ~24 files (one `impl Evaluator` block per file), `shoal-kernel`'s dispatch is split into
  `handlers_*.rs`, similarly for `shoal-value`/`shoal-leash` — the "god file" cleanup from R4 landed
  in the `refactor: modularize kernel/value/leash/eval` commit.
- Reactive streams + in-language `channel()` (R1): sources (`watch`/`tail`/`every`/
  `channel().events()`/`.stream()`), ≥10 combinators, live sinks, bounded/coalescing backpressure on
  every live source (a later hardening pass fixed `every`/`watch`/`tail` from unbounded to bounded
  `sync_channel`s), `.tee(n)` forking a **live** stream with bounded per-fork queues, `.tap`/`.also`.
- **The language-channel ↔ kernel-bus bridge** (the gap the previous ROADMAP/AGENT-SURFACE revision
  flagged as the last pair-shelling blocker): `channel("x").emit(v)` inside evaluated source now
  round-trips onto the kernel's wire `EventBus` and back, `user.*`-scoped only (kernel-owned
  channels like `journal`/`approval`/`session.transcript` can't be spoofed from language code) —
  landed in `feat: bridge in-language channels to the kernel wire bus (one substrate)`, proven by a
  real end-to-end test in `crates/shoal-mcp/tests/live_kernel.rs`.
- Data namespaces + remaining builtins (R2): `json`/`yaml`/`toml`/`csv` (`.parse`/`.stringify`),
  `math`, `http` (get/post/put/delete), `os`, `config`; `tail`/`head`/`ln`/`explain` builtins.
  `jump`/`j` (frecency-ranked `cd`, plus a `pushd`/`popd`/`dirs` directory stack and `cd -`/`OLDPWD`)
  have since landed too — see the R2 wave note below.
- Modules, task lifecycle, plan/apply, undo `out[n]` (R3): `use ./lib/x` (+ `as` alias, `export`)
  binds a module's exports and its `fn`s run as commands (verified live); `task.suspend()`/
  `.resume()` are wired for evaluator-owned processes (kernel-spawned/non-evaluator processes return
  an honest "unavailable" error rather than a silent no-op — this is intentional honesty, not a bug,
  see §"still open" below for the narrower remaining gap); `plan { … }` / `plan <stmt>` derives and
  renders an effect plan without spawning (verified live); `undo out[n]` resolves via a REPL-side
  `out[n] → journal entry id` rewrite (`crates/shoal/src/repl.rs`, `resolve_out_undo`).
- Agent surface: elision (wire-level, automatic), `resources/list|read|subscribe|unsubscribe`
  dispatched, `events.subscribe`/MCP resource push wired, real (non-hardcoded) plan `reversibility`
  derived from effects, seven MCP tools including `shoal_cancel`, the Claude Code plugin.
- leash **filesystem** enforcement active for direct spawns (Landlock/seccomp on Linux, Seatbelt on
  macOS — real `sandbox_init`, not a stub), honest tier reporting at `session.attach`
  (`caps_enforced` reflects whether a real backend actually confined the call). **Not yet wired**:
  spawn-*identity* pinning (a policy's `proc_spawn = ["<hash>"]` against reef's locked hash) — see
  open item #1 below, this is a real, specific, currently-unenforced gap, not enforcement in
  general being fake.
- `shoal-prompt` (~8µs render), **42 adapters** shipped under `adapters/` (git, cargo, rg, docker,
  kubectl, jq, curl, tar, fd, du, df, stat, npm, pnpm, bun, yarn, deno, node, python, pip, uv, ruby,
  go, rustup, terraform, helm, gcloud, aws, gh, jj, sqlite3, systemctl, systemd-analyze, ip, ss, ps,
  bash, brew, podman, unzip, zip, yq — `ls adapters/` for the current, growing list), journal-in-eval
  + `undo` + `journal`/`history`, README+logo+demo, GPG-signed commit history. `du`/`df` now emit a
  real, comparable `Value::Size` (a `size_kb`-typed column scaling the pinned `-k`/`-kP` block
  counts ×1024) rather than bare kb ints, and `du.toml`/`stat.toml`'s previous `tsv-headerless`
  parser-load failure (see the old open-items #10 below) is fixed — both load and parse cleanly now.

---

## Wave R0 — Interactive ergonomics — **DONE**

Both dealbreakers fixed and verified live: `OutcomeVal.streamed: bool` (set only on the real
`PtyTee` spawn path) means `render_result` only skips re-rendering an outcome that actually already
hit the terminal — builtins (`echo hello`, `ls`, `cat`) render correctly now, external PTY commands
still don't double-print. `exit [code: int = 0]` / `quit` are registered command heads that set a
host-level exit flag (`Flow`) rather than calling `std::process::exit` from inside eval, so the same
code path works identically in the REPL, a script, or an embedded kernel session.

Verify: `echo hello | head` at an interactive prompt prints `hello` immediately (no need to pull
`.out`); `target-*/debug/shoal -c 'exit 0'` exits 0 without a stack of panics.

---

## Wave R1 — Reactive streams + in-language `channel()` — **DONE**

`docs/STREAMS.md` is real, not aspirational: `channel(name)` (user-populated) and
`watch`/`tail`/`every`/process-stdout (system-populated) are one substrate, all driven by the same
`stream<T>` combinators. Sources, ≥10 combinators, live/`.each`/`.collect`/`.into`/`.save`/`.feed`
sinks, single-consumption enforcement, sink-to-source cancellation, and per-source bounded
backpressure (coalesced-summary overflow, never unbounded buffering) are all shipped and covered by
`spec/cases/streams*.toml` plus `shoal-eval`'s own unit tests for the non-deterministic sources
(`watch`/`tail`/`every`'s real OS/timer backing). The kernel-bus bridge (see status snapshot above)
closed the one gap this wave's original acceptance criteria didn't yet cover.

---

## Wave R2 — Data namespaces + remaining structured builtins — **DONE**

`json`/`yaml`/`toml`/`csv`/`math`/`http`/`os`/`config` all exist as namespace values with the
methods specified in the original mini-spec (verified live: `json.parse("[1,2]")`,
`math.sqrt(2)`, `os.platform()`). `tail`/`head`/`ln`/`explain` are structured builtins, not raw
passthrough.

**`jump`/`j` (frecency-ranked `cd`) has landed**: a small frecency store backs `jump`/`j`, verified
live (`jump "x"` ranks and cds by recency+frequency); `feat(eval): jump/j frecency-ranked directory
jumping`. A `pushd`/`popd`/`dirs` directory stack and `cd -`/`OLDPWD` landed alongside it in a later
wave (`feat(shell): cd -/pushd/popd/dirs`) — all route through one `change_cwd` choke point so
directory history keeps recording, all session-scoped and top-level-only like `cd` itself
(**corpus** `spec/cases/dir-stack.toml`, `reef.toml:jump-inside-fn-body-is-illegal`).

---

## Wave R3 — Modules + task lifecycle + plan/apply + undo `out[n]` — **DONE**

`use ./lib/deploy` (+ `as alias`, `export`) loads and memoizes a module, binding its exports under
the file-stem namespace; a module's exported `fn`s run as commands (the §1.6 unification extends
across module boundaries) — verified live. `task.suspend()`/`.resume()` are wired for
evaluator-owned processes; the kernel's `task.suspend`/`task.resume` wire methods return an honest,
explicit "unavailable for evaluator-owned processes" error for kernel-spawned tasks rather than
silently no-opping (this is a deliberate honesty boundary per the project's own tier-honesty
discipline, not an unfinished feature — see "still open" for the one place this could still be
tightened). `plan { … }` / `plan <statement>` derives and renders an effect plan without spawning
(verified live: `plan rm "x"` reports `effects`/`reversible`/`spawns` and doesn't touch the
filesystem). `undo out[n]` resolves via a REPL-side rewrite that maps `out[n]` to its journal entry
id before delegating to the existing `undo <id>` path (`crates/shoal/src/repl.rs`,
`resolve_out_undo`) — the mapping lives host-side, not in the evaluator, by design (`out` itself is
a REPL-side transcript list with no evaluator-side notion of journal entry ids).

---

## Wave R4 — Hexagonal ports + modularization round 2 — **mostly DONE**

**Done**: the `Fs`/`Exec`/`Clock`/`Opener`/`SecretPort` ports (see `docs/CONTRACTS.md` §8) and the
god-file splits (`shoal-eval` internals across ~24 files; `shoal-kernel`'s dispatch split into
`handlers_*.rs`; `shoal-value` and `shoal-leash` similarly modularized) landed in the
`refactor: hexagonal ports + god-file splits + lint tightening + corpus growth` and
`refactor: modularize kernel/value/leash/eval + grow conformance corpus` commits. `[workspace.lints]`
tightening continued incrementally (see root `Cargo.toml`'s `[workspace.metadata.lints]` for the
live-violation-tracked remainder — `use_self`, `unused_qualifications`, etc. — each with a documented
reason it isn't enabled yet).

**Done (partial): the builtin-identity half.** `crates/shoal-eval/src/builtins.rs` now holds one
canonical registry (`NAMES` + `SPECIAL_HEADS`, exposed via `pub fn builtin_names()`) that `is_builtin`/
`is_special_head`/`is_command_name` all derive from, instead of hand-copied lists drifting across
dispatch sites. The completer, syntax highlighter, and `shoal-lsp` (which gained a `shoal-eval`
dependency for this, moving it from Tier 1 to Tier 4 — see `docs/CONTRACTS.md`'s crate-dependency
DAG) now consume that same list, so every real builtin tab-completes/highlights consistently and a
new head can't silently skip one of the four consumers (`refactor: one canonical builtin-command
registry + method-name completion`).

**Not done**: the broader command-*resolution* unification — collapsing fn/alias/reef/adapter/PATH
lookup into one `resolve.rs` returning `enum { Builtin, Adapter, External, Interpreter }`. Grep
confirms it still doesn't exist (`crates/shoal-eval/src` has no `resolve.rs`); dispatch across those
sources is still the separate hand-written paths in `command.rs`/`call.rs`. This is a narrower
remaining slice than the original item (the builtin-identity drift it also targeted is fixed), still
a real cleanup opportunity, still eval-heavy — serialize with any other `shoal-eval` work per the one
hard constraint.

**Acceptance for the remaining slice**: one `resolve.rs` unifying fn/alias/reef/adapter/PATH lookup;
conformance corpus unchanged; `cargo clippy --workspace --all-targets --locked -- -D warnings` still
green.

---

## Wave R5 — Corpus growth + docs/wiki refresh + polish — **corpus target exceeded; polish ongoing**

**Corpus**: 1,218 cases (target was ≥1,000) — done, and growing incrementally is still welcome for
any newly-landed behavior (every behavior change should add/adjust a case; see `CLAUDE.md`).

**Wiki**: kept current in the sister `shoal.wiki` repo (agent surface, leash, adapters, prompt,
interpreter blocks, journal/undo, the plugin, streams/channels, the reef bridge) — refreshed
alongside this revision; re-check stale figures (case counts, adapter counts) whenever either
changes materially, since they're restated as prose in multiple wiki pages rather than computed.

**Small carryovers — status, individually re-checked**:
- Production undo-when-cwd-under-a-symlinked-path (macOS TOCTOU-vs-alias tension) — **still open**,
  no evidence of a fix in `shoal-eval`/`shoal-kernel` source.
- Adapter `class = "interpreter"` (adapter-extensible interpreter blocks, not a hardcoded parser
  const) — **done**: `docs/IO.md` §2.2's mechanism is real; adapters declare `class = "interpreter"`
  and the shipped pack includes interpreter-class entries.
- Feeding a bare `outcome` to `.feed` — **done** (verified live: `(echo hi).feed(sort).out` works
  per IO.md §1.2's outcome row).
- The `Outcome` wire `span` (spec'd but hardcoded) — **still open**: `crates/shoal-kernel/src/
  wire.rs`'s `Value::Outcome => WireValue::Outcome { .. span: None }` is still a literal `None`,
  never threaded from the spawning call's span.
- User-scope `[reef]` auto-discovery — **appears to have landed** (`shoal-config` explicitly parses
  `[reef]` out of `shoal.toml` for user scope per `REEF.md` §1; re-verify the ambient-shadow
  did-you-mean specifically before relying on it — that narrower piece wasn't independently
  confirmed in this pass).
- Prompt async/deferred segments + git status via `notify` instead of once-per-command subprocess —
  **still open**, no evidence found in `shoal-prompt`/`shoal`'s prompt wiring.

**Ownership.** Fully parallelizable (corpus, wiki, and each carryover are independent). Mostly
Sonnet.

---

## What's genuinely still open (the honest punch list)

Pulled together from the per-wave notes above, plus fresh findings from this revision's
verification pass:

1. **Binary-content-hash spawn pinning — WIRED (was the single most security-relevant gap).**
   `shoal-eval`'s spawn path now consults `shoal-leash`'s effect evaluator before exec. In
   `crates/shoal-eval/src/command.rs`, `run_argv` calls a new `spawn_gate` for every external
   spawn: when the active principal declares a non-empty `proc_spawn` allowlist
   (`Policy::spawn_pinning_active`), the resolved binary's blake3 content hash is checked against it
   via `Policy::evaluate_effect(ProcSpawn{bin_hash, argv0})`; a miss returns a `spawn_denied` error
   *before* the child is launched. The hash is reef's own `Resolution::hash` when reef resolved the
   head (reused verbatim — same blake3-hex `reef_apply` now returns), else it is computed from the
   resolved binary's bytes via `shoal_reef::hashcache::hash_bytes`, so a pin an author copies from
   `reef`/`which` output compares equal either way. `plan_derive.rs` likewise now emits a real
   `bin_hash` instead of `String::new()`. **No default-deny regression:** the gate is a strict
   no-op unless `proc_spawn` is non-empty — an empty/absent allowlist means "unrestricted spawns",
   guarded explicitly by `spawn_pinning_active` (an empty allowlist would otherwise evaluate every
   `ProcSpawn` as `Deny`) and pinned by unit + end-to-end tests
   (`crates/shoal-eval/tests/leash_activation.rs`, `crates/shoal-leash/src/lib.rs`). Residual
   caveat: the hash is a pre-exec preflight, so a TOCTOU window remains between check and exec until
   an exec-time BPF-LSM/`spawn_hash` pin lands — the same caveat `preflight_spawn` already
   documents; the OS `SandboxPolicy.spawn_hash` pin (exec-layer `verify_pin`) remains available for
   the fs-scoped path.
2. **Command-resolution `resolve.rs` unification — REMAINING SLICE ONLY.** The builtin-*identity*
   half (one registry backing dispatch/completer/highlighter/LSP) shipped — see Wave R4 above. What's
   left is collapsing fn/alias/reef/adapter/PATH lookup itself into one `resolve.rs`; architectural
   cleanup, eval-heavy.
3. **`jump`/`j` frecency-ranked `cd` — DONE** (R2), along with a `pushd`/`popd`/`dirs` directory stack
   and `cd -`/`OLDPWD` that landed in the same stretch of work — see Wave R2 above.
4. **`Outcome` wire `span`** always `None` over the kernel wire (R5 carryover) — small, `shoal-kernel`.
5. **`shoal_cap_request`'s grant response enforcement honesty — WIRED.**
   `crates/shoal-kernel/src/handlers_task.rs`'s `handle_cap_request` calls the same
   `Kernel::caps_enforced_for` helper `session.attach`'s `caps_enforced` uses, so a `cap.request`
   grant reports the SAME honest enforcement truth an agent already learned at attach time — it does
   NOT hardcode `"enforced": false` (a previous revision of this doc said otherwise; that was
   stale). Pinned by `cap_request_reports_the_same_enforcement_truth_attach_does` and
   `cap_request_reports_false_for_the_default_permissive_principal` (`shoal-kernel/src/lib.rs`).
6. **Bare-path-head runner ergonomics** (`./script.py` with no `run`) work for `.shl` only — other
   extensions need the explicit `run script.py` spelling. `docs/REEF.md` §5 / wiki Reef §5 carry the
   corrected account; wiring the general case is a `shoal-eval` command-head-resolution change.
7. **Real OS-level sandbox enforcement wired end-to-end through the kernel/MCP surface** — the
   pieces exist (leash filesystem enforcement at spawn, honest tier reporting at attach) but a full
   trace of "an MCP `shoal_exec` call that should be denied/confined by policy actually gets
   denied/confined, not just reported as such" is worth re-verifying with a live kernel + a real
   restrictive policy file, not assumed from the pieces being individually real — and per #1 above,
   the *spawn-identity* half of that story (as opposed to filesystem/network confinement) isn't
   wired at all yet.
8. **macOS cwd-under-a-symlink undo edge case** (R5 carryover) — still open.
9. **Prompt async/deferred git-status segments** (R5 carryover) — still open; today's prompt does a
   once-per-render subprocess-based git status, not an event-driven one.
10. ~~`adapters/du.toml`/`adapters/stat.toml` fail to load (`unknown output parser
    "tsv-headerless"`)~~ — **FIXED**. `shoal-adapters` now implements the `tsv-headerless` strategy
    (`parse_tsv_headerless` in `crates/shoal-adapters/src/lib.rs`); both load and parse cleanly with
    no startup warning (verified live: `du`/`stat` render structured tables, not a passthrough
    fallback). `du`/`df` additionally moved off bare kb ints onto a real, comparable `Value::Size`
    (`size_kb`-typed columns, `fix(adapters): du/df emit a real, comparable Value::Size...`).
11. **Windows** — resolution semantics, ConPTY, ports — entirely deferred, `docs/TDD.md` §14.
12. **Config hardening** — in flight under separate ownership (`docs/CONFIG.md`); not detailed here.
13. **More adapters** — 42 shipped as of this revision (`ls adapters/` for the current, growing
    list); the ecosystem is large and this is perpetually "in flight" by nature, not a blocking gap.

---

## Suggested order & rationale

Given R0–R3 are done and R4/R5 are the only waves with open work:

1. **#1, wire spawn-identity pinning end to end** — the highest-priority item precisely because it's
   a security doc/reality gap, not a missing feature: thread a real blake3 hash from reef through
   `plan_derive.rs`'s `ProcSpawn` effect and have the real spawn path actually consult
   `shoal-leash`'s evaluator with it before exec. Eval-heavy (touches `shoal-eval`'s spawn path);
   serialize with any other `shoal-eval` work per the one hard constraint.
2. **Close the other small, cheap items** (#4 `Outcome` span — #3 `jump`/`j` and #5 `cap_request`
   enforcement honesty are already closed, see above) — self-contained, low-risk, removes a specific
   documented gap.
3. **The remaining `resolve.rs` command-resolution slice** (#2) — do this once no other eval-heavy
   work is in flight (the one-hard-constraint serialization applies), since it touches command
   dispatch broadly and benefits from a quiet tree. **#6** (bare-path runner ergonomics) touches the
   same command-head-resolution machinery — worth doing in the same pass.
4. **#7, the end-to-end sandbox-enforcement trace** — security-relevant, worth a dedicated
   verification pass (ideally by an agent that writes a real restrictive policy file and a live
   kernel test, not just reads source) before trusting it either way, once #1 is closed.
5. **The remaining R5 carryovers** (#8 symlink undo, #9 prompt async segments) — non-eval, fully
   parallelizable, pick up whenever convenient.
6. **Corpus growth, adapters (including the #10 `du`/`stat` adapter parser bug), wiki upkeep** —
   continuous, non-eval, run alongside anything else.

*shoal ROADMAP — the corpus decides disputes; this doc sequences the work to get there.*
