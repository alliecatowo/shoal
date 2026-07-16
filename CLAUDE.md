# CLAUDE.md — operating manual for shoal

shoal is an agent-first structured shell: no pipe, dot-chains over typed values, a modal CMD/EXPR
lexer, fn-as-command, a SQLite journal + blake3 CAS, the `leash` capability engine, and a
`shoal-kernel` + MCP surface for agents. This file is the map for any agent (or human) working ON
shoal's own source. It does not restate the design — it tells you where the truth lives and how to
not break it.

**Read in this order before touching evaluator/wire behavior:**
1. `docs/TDD.md` — the language/semantics contract (decisions, not options).
2. `docs/CONTRACTS.md` — the pinned inter-crate Rust APIs + crate dependency DAG.
3. `docs/{IO,STREAMS,REEF,AGENT-SURFACE,VISION,ROADMAP}.md` — companion normative specs + the
   sequenced work plan.
4. `spec/cases/*.toml` — **the conformance corpus IS the behavioral spec.** ~1,220 cases across 74
   files, corpus-verified 1211 passed / 0 failed / 7 skipped (host-dependent) as of this writing.
   If a case and a doc disagree, the corpus wins (TDD §12: "the corpus decides disputes").

---

## The crate map (dependency DAG, from CONTRACTS.md)

Acyclic, enforced by Cargo. Reproduce with `grep -oE '^shoal-[a-z]+' crates/*/Cargo.toml`.

```
Tier 0 — leaf (no shoal-* deps):
  shoal-ast      canonical AST, serde, desugarer
  shoal-auth     agent bearer tokens (TokenStore)
  shoal-config   shoal.toml layered config (schema + errors)
  shoal-journal  SQLite (WAL) journal + blake3 CAS
  shoal-leash    effects/plans/grants + OS enforcement (Landlock/Seatbelt)
  shoal-proto    JSON-RPC wire types
  shoal-reef     content-addressed tool resolution (replaces PATH)
  shoal-secret   opaque secret store
  shoal-wasm     WASM component-plugin host (not yet wired into eval dispatch)
  shoal-prompt   pure prompt renderer (PromptContext -> string; zero shoal-* deps)

Tier 1 — depend only on Tier 0:
  shoal-value    -> ast            (Value enum, Env, ErrorVal, ports, methods, render)
  shoal-syntax   -> ast            (modal lexer, recursive-descent + Pratt parser; also owns the
                                    canonical builtin command-head registry — `shoal_syntax::
                                    commands::builtin_names()`/`is_builtin`/`is_special_head` —
                                    the ONE list eval dispatch, the shell's completer/highlighter,
                                    and shoal-lsp all consume so they can't drift)
  shoal-exec     -> leash          (NOT a leaf — spawn/PTY/tee/signals + sandbox hooks)
  shoal-history  -> journal
  shoal-lsp      -> syntax         (tower-lsp server; parse/format/diagnostics + the builtin-head
                                    completion vocabulary, all from shoal-syntax — no eval dep)

Tier 2 — depend on Tier 0/1:
  shoal-adapters -> ast, value     (declarative TOML tool schemas)
  shoal-picker   -> value          (fuzzy picker over table/list/stream)

Tier 3 — the domain core:
  shoal-eval -> adapters, ast, exec, journal, leash, picker, reef, secret, syntax, value
    (tree-walk evaluator, scopes, coercion, adapter binding, effects, streams/channels)

Tier 4 — composition roots:
  shoal-doctor -> adapters, journal, leash          (installation diagnostics)
  shoal-kernel -> ast, auth, eval, exec, journal, leash, proto, syntax, value
    (long-lived per-user daemon; sessions, journal, socket, EventBus)

Tier 5 — entrypoints (binaries):
  shoal      -> adapters, ast, config, doctor, eval, prompt, syntax, value   (REPL/script runner)
  shoal-mcp, shoal-lsp — spawned by `shoal` as companion subprocesses (`shoal mcp`/`shoal lsp`),
                         NOT Cargo dependencies of `shoal`. shoal-mcp has zero shoal-* deps in
                         [dependencies] and talks to a running shoal-kernel purely over the wire;
                         shoal-lsp links only shoal-syntax (Tier 1 — parse/format/diagnostics plus
                         the canonical builtin-head registry), so it never pulls in the evaluator.
  shoal-kernel — independent of `shoal` at the Cargo level; `shoal` never depends on or spawns it.
                 One engine (shoal-eval), two hostings: kernel-less (embedded, `shoal -c`/scripts)
                 or kernel-hosted (socket-attached, interactive + agent sessions).
```

`shoal-eval`'s internals are split across many files under `crates/shoal-eval/src/` (one
`impl Evaluator` block per file: `args, builtins, call, channels, coerce, command, expr*, helpers,
host, journal, modules, namespaces, pattern, plan*, ports, reef*, script, stmt, streams`) —
internal organization only, no separate crates.

**Hexagonal ports (done):** `shoal-value/src/ports.rs` defines `Fs`/`Clock`/`Opener`/`SecretPort`;
`shoal-eval/src/ports.rs` defines `Exec` (needs `shoal-exec` types, so it can't live in the leaf
`shoal-value` crate). Each has a `Std*` default adapter that is byte-identical to the pre-ports
inline `std::fs`/`std::process`/`std::time` calls. This is how the domain core stays testable
without touching a real filesystem/process/clock.

**Ownership map** (CONTRACTS.md, don't relitigate in a random PR):
- Integrator-owned core: `shoal-ast`, `shoal-value` (core types), `shoal-syntax`, `shoal-eval`, `shoal` (binary).
- Delegated modules (build to the pinned contract, don't renegotiate it silently): `shoal-exec`,
  `shoal-journal`, `shoal-adapters`, `shoal-reef`, `shoal-value/src/methods.rs` + `render.rs`,
  `spec/cases/*.toml`.
- `docs/CONFIG.md` and `shoal-config` internals are out of scope for this file's authors —
  check current ownership before editing.

---

## THE PRE-COMMIT GATE — always run before committing

```sh
cargo fmt --all --check
cargo +stable clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

All three, every time, no exceptions. **The CI `fmt + clippy` job fails on formatting alone** —
a clean clippy run with one misplaced brace still goes red. Run `cargo fmt --all` (not `--check`)
locally first if you're not sure, then re-run `--check` to confirm.

Also run the conformance corpus explicitly when touching anything language/eval-shaped (it's part
of `cargo test --workspace` but worth isolating for fast iteration and `--nocapture` pass/fail
counts):

```sh
cargo test -p shoal --test conformance --locked -- --nocapture
```

**Avoid lock contention on parallel/concurrent work**: use a scratch target dir per agent/session
instead of the shared `target/`:

```sh
CARGO_TARGET_DIR=target-<name> cargo test --workspace --locked
CARGO_TARGET_DIR=target-<name> cargo build --bin shoal
```

(The repo root has many stale `target-*` scratch dirs from past sessions — pick a fresh, unique
name; don't fight another agent for one that's still warm.)

Never touch the workspace root `Cargo.toml` casually — if you need a new dependency for your crate,
`cargo add -p <your-crate> <dep>`, don't hand-edit the root manifest or `[workspace.lints]`.

---

## The conformance corpus IS the spec

`spec/cases/*.toml` (74 files, ~1,220 `[[case]]` entries) is normative (TDD §12): a case encodes
*correct* behavior per `docs/TDD.md`/`docs/CONTRACTS.md`, not necessarily what today's code does.
**Add or adjust a case for every behavior change** — it's the sharpest regression test and doubles
as documentation. Schema (CONTRACTS §5):

```toml
[[case]]
name = "unique-kebab-name"     # globally unique across every file — harness fails loudly on collision
src  = "let x = 2 + 3\nx * 2"
value = "10"                    # render_inline of the final statement's value
# OR error = "type_error"       # + optional error_contains = "substr"
# OR parse_error = true         # + optional parse_error_contains = "substr"
fixture = ["a.txt", "sub/b.log"]  # optional: empty files created under a fresh temp cwd first
skip    = "reason"                # optional: use for genuinely host-dependent behavior only
```

Each case runs in a **fresh** `Evaluator` rooted at a fresh temp-dir cwd — no cross-case state.
Don't `skip` a case just because it's currently failing; `skip` is for host-dependent
non-determinism (a real tool's resolved hash/version, wall-clock timing), not a way to hide a bug.

---

## Parallel work discipline

**`shoal-eval` is the collision magnet** — almost every language feature routes through it. At most
one agent/session should be editing `crates/shoal-eval` at a time; serialize eval-heavy work.
Non-eval work (adapters, docs, corpus growth, a delegated crate) parallelizes freely alongside it.
`docs/ROADMAP.md`'s wave plan encodes this: each wave states its ownership partition up front so
concurrent agents don't collide.

If you're touching a delegated crate (`shoal-exec`, `shoal-journal`, `shoal-adapters`, `shoal-reef`,
`shoal-value/src/methods.rs`+`render.rs`), build to the pinned signatures in `docs/CONTRACTS.md` and
update that file in the same change if a signature genuinely has to move — other in-flight work
depends on it staying put.

---

## Running the binary / dogfooding

```sh
cargo run -p shoal                                            # interactive REPL
cargo run -p shoal -- -c 'let answer = 6 * 7\nanswer'          # one-shot source string
cargo run -p shoal -- path/to/script.shl                       # run a .shl script
CARGO_TARGET_DIR=target-docs cargo build --bin shoal && target-docs/debug/shoal -c '…'   # fast iteration
```

Dogfood it like you'd dogfood any shell: drive real workflows (`ls.where(.size > 1mb)`, `git
status()`, a `python { … }.out` interpreter block, `.feed`, `watch`/`tail`/`channel()`), not just
unit-test snippets. The `dogfooder` subagent (`.claude/agents/dogfooder.md`) exists specifically for
this — use it to find papercuts before they ship.

---

## The kernel + MCP surface

`shoal-kernel` is the long-lived daemon; `shoal-mcp` is a standalone MCP-facade binary that talks to
a running kernel purely over the JSON-RPC wire (zero `shoal-*` Cargo deps). The end-to-end proof
that this stack actually works — not just each piece in isolation — lives at
`crates/shoal-mcp/tests/live_kernel.rs`: it spins up a **real** `shoal-kernel` on a real Unix socket
in a background thread, drives it both through the `shoal-mcp` `Facade` and via raw JSON-RPC, and
asserts the AGENT-SURFACE doctrine holds across the whole stack (elision at the MCP boundary, an
elided value's ref being a live resource, events round-tripping on a user channel, the
language-channel <-> kernel-bus bridge). If you change anything in `shoal-kernel`, `shoal-mcp`, or
the `channel()`/`EventBus` plumbing in `shoal-eval`, run this test — it catches integration bugs
unit tests on either side individually cannot see.

```sh
CARGO_TARGET_DIR=target-mcp cargo test -p shoal-mcp --test live_kernel --locked
```

---

## Commit-trailer convention

Every commit in this repo's history carries a two-line trailer identifying the human and the model
that paired on it:

```
Co-Authored-By: Allie <alliecatowo@users.noreply.github.com>
Co-Authored-By: Claude <model-name> <noreply@anthropic.com>
```

Commits are GPG-signed. Keep commit messages substantive (why, not just what) — the history is
meant to be read later as a design log, not just a diff index; see `git log` for the established
tone (e.g. `feat: bridge in-language channels to the kernel wire bus (one substrate)`).

---

## When in doubt

- Behavior question about the language → check `spec/cases/*.toml` first, then `docs/TDD.md`.
- Wire/agent-surface question → `docs/AGENT-SURFACE.md`, then `crates/shoal-mcp/tests/live_kernel.rs`.
- "Is X actually implemented?" → don't trust a doc's status line at face value; grep the source and
  run the binary (`cargo run -p shoal -- -c '…'`) — it's cheap and this codebase moves fast enough
  that status lines lag reality in both directions.
- Sequencing / what to build next → `docs/ROADMAP.md`.
