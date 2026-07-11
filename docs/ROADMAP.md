# shoal — execution roadmap for the remaining pieces

**Purpose.** A fire-and-forget plan: every unbuilt piece, sequenced into waves that respect the one
hard constraint (below), each with a *locked* design decision so implementation is mechanical, an
ownership partition that avoids collisions, and acceptance criteria. A future session (or the
autonomous `continue` loop) can execute any wave from this doc with zero re-discovery.

**The one hard constraint.** `crates/shoal-eval` is the collision bottleneck — almost every feature
routes through it. **At most one agent edits `shoal-eval` per wave.** Eval-heavy work therefore
serializes; non-eval work parallelizes alongside it.

**Every wave ends the same way** (the pinned loop): `cargo fmt --all --check` + `cargo +stable
clippy --workspace --all-targets --locked -- -D warnings` + `cargo test --workspace` green,
conformance not regressed, then a signed commit (`Co-Authored-By` trailer) → `git push` →
`gh run watch` until all 6 CI jobs (incl. `test (macos-latest)`) are green; fix any macOS-only test
failure test-side (canonicalize temp paths for the `/tmp`→`/private/tmp` alias; gate genuinely
Linux-only behavior) and re-push. macOS is first-class: never a stub, never silently second-class.

---

## Status snapshot (on `main`, CI-green Linux+macOS, ~342 conformance cases)

Done: full language + dispatch + match; outcome unification; reef resolution + `which`/`with reef:`;
agent surface (elision, `resources/*`, events/channels/subscriptions, MCP tools) + Claude plugin;
leash enforcement **active** (Landlock/Seatbelt, honest tier, proven denial); `shoal-prompt`
(~8µs); 23 adapters; interpreter blocks (`python { }.out`→structured) + `.feed`; journal-in-eval +
`undo` + `journal`/`history`; README+logo+demo; GPG-signed Verified history.

Not yet built — the subject of this roadmap.

---

## Wave R0 — Interactive ergonomics (do FIRST · eval+bin · S)

Two dealbreakers in the interactive REPL, both with a locked root cause. Fix before anything else.

**Bug 1 — statement-position builtins don't print.** `echo hello` / `ls` at the prompt render
nothing until you pull `.out`. Root cause: `crates/shoal/src/main.rs:511` calls
`render_result(&value, true)` unconditionally, and `render_result` (`main.rs:562`) skips rendering
*any* `Value::Outcome` when `pty_was_live` — on the assumption the PTY already streamed it. True for
**external** commands in statement position (real PTY passthrough — output is already on screen),
**false for builtins** (echo/ls/cat/etc.) and any Capture-mode outcome, which stream nothing and so
render nothing.
**Locked fix.** An outcome must know whether its bytes actually went to the terminal. Add
`OutcomeVal.streamed: bool` (default `false`; set `true` *only* in the `ExecMode::PtyTee` spawn path
in `shoal-eval`, where bytes hit the real tty). Change `render_result` to skip re-rendering only
when `matches!(value, Value::Outcome(o)) && o.streamed`. Then builtins and captured outcomes render
their `.out`/stdout as they should; PtyTee externals still don't double-print. (Note: `render_block`'s
Outcome arm already returns the `.out` string correctly — the value never reached it.) Add a REPL
test (drive a PTY: `echo hello` prints `hello` immediately) and confirm an external like
`ls --color=auto` still shows once, not twice.

**Bug 2 — no `exit`.** Only Ctrl-D quits. Add an `exit [code: int = 0]` builtin (alias `quit`):
in the REPL it ends the loop cleanly (mirror the Ctrl-D path); in a script / `-c` it exits the
process with `code`. Register `exit`/`quit` as command heads. Cleanest wiring: the builtin returns a
distinct `Flow::Exit(code)` (or sets an evaluator exit flag) that `eval_program`/the REPL loop
detects and honors — do not `std::process::exit` from inside eval (breaks the kernel/embedded host);
surface it as a value the host acts on.

**Ownership.** P1 (Opus): `crates/shoal-eval` (the `streamed` flag on the PtyTee path, `exit`
builtin/Flow) + `crates/shoal-value` (`OutcomeVal.streamed` field) + `crates/shoal/src/main.rs`
(render_result condition, honor exit). Verify: the two PTY repros above + zero regression (336/0/6).
This is small and unblocks the daily-driver feel — ship it first.

---

## Wave R1 — Reactive streams + in-language `channel()`  [eval-heavy · L/XL]

**Goal.** Make STREAMS.md real: time-varying data as first-class streams composed with the same
dot-chain combinators as collections. The honest replacement for `tail -f | grep` and file-watch
coordination. Contract: `docs/STREAMS.md` (read it — it's the normative spec).

**Locked decisions.**
- **Everything time-varying is a channel.** This is the unification (per Allie): there is ONE
  substrate — event-streams — and sources differ only in *who populates them*. `channel(name)` is
  **user-populated** (`.emit`); `watch`/`tail`/`every`/process-stdout/journal are **system-populated**
  channels the kernel feeds from OS file events, timers, pipes. All are consumed by the *same* stream
  combinators. `tail`/`watch` are therefore ergonomic *constructors for system channels*, not
  file-polling hacks — the anti-pattern being killed is **coordination by watching files**
  (lockfiles, sentinels, `tail`-a-file-to-know-another-process-finished): that is *always* a channel,
  never a file. Following a genuinely external log you don't control is the legitimate residue, and
  it rides the same substrate (event-driven via `notify`, not polling).
- `Value::Stream` already exists as a single-consumption pull iterator (TDD §1.9). Extend it, do not
  replace it. A stream is a consumer of a channel (system- or user-populated); combinators are lazy
  adapters over the iterator; single-consumption is enforced by `StreamVal::take` (already present).
- **Sources** (all yield a `stream<T>` over a channel): `watch(path | glob)` → `stream<{path, kind}>`
  via the `notify` crate (inotify/kqueue — cross-platform, mac first-class); `tail(file,
  from_start: bool=false)` → `stream<str>` (follows external log appends, event-driven); `every(dur)`
  → `stream<datetime>` timer ticks; `channel(name).events()` → `stream<event>`; a command's streaming
  stdout in value position (the `spawn_capture` path already streams — wrap it as `stream<str>` of
  lines). Prefer `channel()` for any coordination; reserve `watch`/`tail` for external files.
- **Combinators** (methods on `stream`, all lazy + bounded-memory unless noted): `.where .map
  .scan(init, f) .window(n | duration) .debounce(dur) .throttle(dur) .dedupe .distinct .merge(other)
  .zip(other) .take(n) .take_until(pred | stream) .buffer(n) .flat_map`. `.window`/`.buffer` are the
  only bounded-buffer ones; document memory for each.
- **Sinks** (terminate a stream): live render (REPL shows a live-updating view), `.each(f)`,
  `.collect()` (finite only — infinite errors, needs `.take`), `.into(channel(name))` (republish as
  events → agents subscribe), `.save(path)` (append mode for live), `.feed(cmd)`.
- **in-language `channel()`** — the kernel EventBus substrate already exists (`events.publish/
  subscribe/read` landed in the agent-surface wave). Wire the eval binding to it:
  `channel("x").emit(v)` → kernel publish on `user.x`; `.events()` → subscribe stream; `.latest()`
  → last value or null (no wait); `.take()` (with `timeout: duration`) → block for next; and the
  sugar `on channel("x") { ev => … }` → `channel("x").events().each(...)` in a spawned task.
- Backpressure & cancellation reference TDD §4.5/§4.7 and the SIGPIPE-analog §13.7 — a satisfied
  `.take(10)` closes the pipe upstream; downstream cancel propagates.

**Ownership.** P1 (Opus): `crates/shoal-eval` + `crates/shoal-value` (stream methods) + a new
`notify` dependency on shoal-eval for `watch`. P2 (Sonnet, parallel non-collide): the streams
conformance/spec cases in `spec/cases/streams.toml` (deterministic ones — `every`/`watch` are
timing-dependent → unit-test in eval, keep corpus host-safe) + a wiki page. Verify (Opus): live
repros — `tail(f).where(.contains("ERROR")).each(render)`, `watch("src/**/*.rs").debounce(200ms)`,
`channel("x").emit(1)` then `channel("x").latest()`.

**Acceptance.** `tail`/`watch`/`every`/`channel` sources exist and compose with ≥10 combinators;
live sinks work; in-language channels roundtrip through the kernel EventBus; single-consumption
enforced; zero regression; macOS `watch` uses kqueue and is CI-green.

---

## Wave R2 — Data namespaces + remaining structured builtins  [eval-heavy · L]

**Goal.** Make TDD §5's namespaces first-class values and finish the structured builtins. Mini-spec
below (this piece was under-specified — treat this as the contract).

**Locked decisions — namespaces** (each is a value in the root env exposing methods/fns):
- `json` — `json.parse(str) -> value` (decode; error `arg_error` on invalid), `json.stringify(value,
  pretty: bool=false) -> str` (encode; the existing `.json` method delegates here). Round-trips via
  the existing `json_to_value`/`value_to_json`.
- `yaml` / `toml` / `csv` — `.parse(str) -> value` and `.stringify(value) -> str`. yaml via
  `serde_yaml` (or `serde_norway`), toml via `toml`, csv via `csv` (headers → `table`). Decoders are
  the priority (encoders exist partially).
- `math` — `math.pi math.e`, `math.sqrt/sin/cos/tan/ln/log10/log2/exp/floor/ceil/round/abs/pow(x,y)/
  min(a,b)/max(a,b)/hypot`, `math.clamp(x, lo, hi)`. Pure `f64`.
- `http` — `http.get(url, headers: record?) -> outcome-like {status: int, ok: bool, body: str,
  json(): value, headers: record}`; `http.post(url, body: value|str, headers?)`; PUT/DELETE. Typed
  responses; `body.json()` parses. Deps: `ureq` (blocking, small) or `reqwest` blocking. Declares
  `net.connect(host)` effects for leash. Timeouts + a size cap.
- `os` — `os.platform() os.arch() os.hostname() os.username() os.pid() os.env() (-> record of the
  session env names→values, secrets as names) os.cpus() os.uptime()`.
- `config` — a typed view over `shoal.toml` (read); `history` is an alias for the `journal` table
  view (already built).

**Locked decisions — remaining §5 builtins** (structured, not raw passthrough): `tail(file, n:
int=10, follow: bool=false)` (follow → a `stream<str>`, ties into R1), `head(file, n: int=10) ->
list<str>`, `ln(target, link, symbolic: bool=false)`, `watch(cmd, interval: duration=2s)` (re-run a
command on a timer, live-render diffs — or defer to R1's `every`), `jump`/`j` (frecency-ranked cd —
needs a small frecency store in the journal/state dir), `explain(src)` → a structured explanation of
what a statement will do (reuse the kernel `explain` method's logic). `pick`/`interact`/`open`
already exist.

**Ownership.** P1 (Opus): `crates/shoal-eval` (namespace registration + builtins) + `shoal-value`
(namespace value type if needed) + new deps (`ureq`/`serde_yaml`/`csv`). P2 (Sonnet): `spec/cases`
for the pure ones (json/yaml/toml/csv/math are deterministic → lots of corpus cases; http/os are
environment-dependent → skip/gate). Verify (Sonnet).

**Acceptance.** `json.parse('{"a":1}').a == 1`; `math.sqrt(2)`; `yaml.parse`/`toml.parse`/`csv.parse`
round-trip; `http.get(url).status` typed (gated in CI); `os.platform()` correct on both OSes;
`tail`/`head`/`ln` structured; corpus grows by ≥40 deterministic cases.

---

## Wave R3 — Modules (`use`) + task lifecycle + plan/apply + undo `out[n]`  [eval-heavy · M]

**Goal.** Close the remaining language/session gaps. Mini-specs below.

**Locked decisions — modules (`use`)** (TDD §4.6 says "modules are files"; currently errors "not
implemented"):
- `use ./lib/deploy` loads the file `./lib/deploy.shl` (resolve against cwd; `.shl` optional),
  evaluates it in a fresh module scope, and binds its `export`ed decls under `deploy.` (the file
  stem). `use ./lib/deploy as d` binds under `d.`. `export fn`/`export let`/`export alias` mark a
  decl public; non-exported decls are module-private.
- Caching: a module evaluates once per session (memoized by canonical path); circular `use` is an
  error naming the cycle. Modules cannot perform ambient mutation at import (no top-level `cd`/side
  effects beyond `export`s — or run them but document). A module's `fn`s are commands too (the §1.6
  unification, namespaced as `deploy.<name>`).

**Locked decisions — task lifecycle** (TDD §4.7 — currently only `.await/.cancel/.is_done`):
- `task.suspend()` (SIGTSTP the process group), `task.resume()` (SIGCONT), `fg <task>` (re-front a
  background task with PTY in the REPL). `jobs` already lists the task table. The kernel has a
  `task.suspend` wire method already — add `task.resume` and the eval methods.

**Locked decisions — plan/apply from the REPL** (currently `plan_program` exists, no verb):
- `plan { … }` (or `plan <statement>`) derives and renders the effect plan without spawning (the
  pure-prefix eval → effects + reversibility + estimates). `apply <plan-ref>` executes a previously
  derived plan. This mirrors the MCP `shoal_plan`/`shoal_apply` on the human side.

**Locked decisions — undo `out[n]`:** wire the REPL/kernel `out[n]`→journal-entry-id map so `undo
out[n]` resolves (today only bare `undo` and `undo <id>` work — the eval has no `out` map; it lives
host/kernel-side). Add the mapping in the `shoal` binary + kernel session and pass the entry id to
the existing `undo <id>` path.

**Ownership.** P1 (Opus): `crates/shoal-eval` (modules, task methods, plan/apply verb). P2 (Sonnet):
`crates/shoal` + `crates/shoal-kernel` (out[n]→id map, `fg`, task.resume wire) — non-collide with
eval. Verify (Sonnet).

**Acceptance.** `use ./mod` binds exports; a module `fn` runs as a command; circular use errors;
`spawn`ed task suspend/resume/fg work; `plan rm x` shows effects without deleting; `undo out[3]`
resolves.

---

## Wave R4 — Hexagonal ports + modularization round 2  [mostly non-eval · L]

**Goal.** `scratch/audit-arch.md` Waves 2–3. Make the domain core pure; kill the remaining god-files.
Behavior-preserving, guarded by the conformance corpus.

**Locked decisions.**
- **Ports** (traits in `shoal-value`, `Std*` adapters default to today's calls, held as `Box<dyn
  Port>` on `Evaluator`): `Fs` (read/write/metadata/remove — eval makes 20+ direct `std::fs` calls
  today), `Exec` (spawn — wrap `shoal-exec`), `Clock` (now — for journal ts + deterministic tests),
  `Opener`/`SecretPort`. This makes eval testable without touching the real FS and is the last
  hexagonal-purity gap.
- **File splits** (each behavior-preserving, ≤~500 code-LOC): `parser.rs` (1488 → stmt/command/
  expr/pattern/block modules via multi-file `impl Parser`), `shoal-journal/lib.rs` (split schema/cas/
  undo/gc/query), the `Kernel::dispatch` ~515-line match (one `handle_*` fn per arm), `main.rs`,
  `shoal-value/methods.rs`.
- **One builtin REGISTRY** table replacing the 3 hardcoded sources of builtin identity (dispatch /
  `is_command_name` / `builtin_effects`); collapse command resolution (fn/alias/reef/adapter/PATH)
  into one `resolve.rs` returning `enum { Builtin, Adapter, External, Interpreter }`.
- Then **tighten `[workspace.lints]`** now that the tree is clean (add clippy::pedantic selectively,
  missing_docs on public API where reasonable) — only lints that pass workspace-wide.

**Ownership.** Can partly parallelize by crate since it's mechanical: P1 (Opus) eval ports + eval
splits; P2 (Sonnet) parser/journal/kernel splits + registry; P3 (Sonnet) lint tightening + DAG.
Each must keep the conformance corpus green after every split. Verify (Opus).

**Acceptance.** No `crates/**/*.rs` over ~600 code-LOC; eval domain makes zero direct
`std::fs`/`std::process` calls (all via ports); one builtin registry; conformance unchanged;
lints tightened and green.

---

## Wave R5 — Corpus growth + docs/wiki refresh + polish  [non-eval · M]

**Goal.** Reach toward the TDD §12 target (≥1000 conformance cases, currently ~342), refresh the
narrative docs, and clear the small carryovers.

**Tasks.**
- Grow `spec/cases/` toward 1000: exhaustive coverage of every §3.4 desugar row, every §4.2 coercion
  cell, §13 edge rulings, all string/list/table/record/path methods, all match pattern kinds,
  interpreter blocks, `.feed`, the namespaces (R2). Property tests: `parse(format(ast))==ast`,
  format idempotence, glob order-stability, codec roundtrip incl. non-UTF-8. Fuzz targets stay green.
- **Wiki re-refresh** (separate repo `shoal.wiki`, safe to parallelize any time): update for the
  agent surface, leash activation, adapters (23), the prompt, interpreter blocks, journal/undo, the
  plugin.
- Small carryovers: production undo-when-cwd-under-a-symlinked-path (macOS) — resolve the
  TOCTOU-vs-alias tension properly (canonicalize only the leading system-symlink prefix, not
  intra-scope symlinks; test on both OSes); adapter `class = "interpreter"` (make interpreter blocks
  adapter-extensible, not just the static parser const); feeding a bare `outcome` to `.feed`; the
  `Outcome` wire `span` (currently always None — thread the spawning span through); user-scope
  `[reef]` auto-discovery + the ambient-shadow did-you-mean; prompt async/deferred segments + git
  status via `notify` instead of once-per-command subprocess.

**Ownership.** Fully parallelizable (corpus, wiki, and each carryover are independent). Mostly Sonnet.

---

## Suggested order & rationale

1. **R1 streams** — the last big VISION inversion; unlocks the reactive/pair-shelling story and the
   `channel()` binding the agent-surface wave left as a stub. Highest thesis value.
2. **R2 namespaces/builtins** — broad daily-driver utility (json/http/math), makes the shell feel
   complete for real work.
3. **R3 modules/tasks/plan** — closes the language/session gaps; enables larger shoal programs.
4. **R4 hexagonal/refactor** — do once the feature surface has stabilized so the port boundaries are
   drawn around the real shape, not a moving target. Behavior-preserving, low risk, high durability.
5. **R5 corpus/docs/polish** — continuous; run pieces of it in parallel with any of the above (it's
   non-eval), and finish it last to lock the spec.

R4 and R5 are non-eval and can run *alongside* R1–R3 where they don't touch the same crates (e.g.
the wiki refresh, corpus growth, parser/journal/kernel splits, lint tightening). R1–R3 serialize
because each owns `shoal-eval`.

*shoal ROADMAP — the corpus decides disputes; this doc sequences the work to get there.*
