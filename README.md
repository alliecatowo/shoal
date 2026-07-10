<div align="center">

<img src="assets/logo.png" alt="shoal" width="180" />

# shoal

**A structured shell for humans and agents.**
The pipe becomes a dot‑chain over typed values. `PATH` becomes content‑addressed resolution.
Ambient capability becomes an explicit sandbox. And every session speaks a structured wire
protocol, so an AI agent never has to scrape a wall of bytes to know what happened.

<!-- badges: CI + license + edition. Update the CI badge path if the workflow file is renamed. -->
[![CI](https://github.com/alliecatowo/shoal/actions/workflows/ci.yml/badge.svg)](https://github.com/alliecatowo/shoal/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Rust edition 2024](https://img.shields.io/badge/rust-edition%202024-orange)](#)
[![Platforms: Linux · macOS](https://img.shields.io/badge/platforms-Linux%20%C2%B7%20macOS-success)](#)

[Quick start](#quick-start) · [The language](#the-language-in-five-minutes) · [For agents](#for-agents) · [Wiki](https://github.com/alliecatowo/shoal/wiki) · [Design docs](docs/)

</div>

---

## See it

<div align="center">

<img src="assets/demo.gif" alt="A real shoal terminal session: the colorized starship-style prompt, ls.where(.size &gt; 1b) rendering a typed table, a [3,1,2].sort().map(x =&gt; x * 10) dot-chain, 1.5gb + 500mb unit arithmetic, a fn defined and called like a command, and the no-pipe teaching diagnostic on ls | grep foo." width="720" />

<sub>Real terminal output, captured over a PTY — nothing here is mocked up. The prompt is
<code>$directory · $git_branch · $git_status · $reef · $character</code>; <code>!14?3</code> means
14 unstaged files and 3 untracked.</sub>

</div>

---

Shoal keeps ordinary terminal programs alive on a **real PTY** at the prompt — colors, progress
bars, password prompts, and full‑screen TUIs behave exactly like they do in bash — but the moment
a command's result participates in an expression, it becomes a **typed value** you can filter,
sort, and reshape without ever parsing text. It is a shell built for the age where a human and
several agents share one session.

> **Status:** early and honest. The language, REPL, command runner, reef resolver, journal/CAS,
> adapter catalog, capability engine, and kernel/MCP protocol are real and tested
> (300+ conformance cases, green on Linux **and** macOS). It is not ready to be your login shell
> yet — but it already *feels* like the thing. See [Status & roadmap](#status--roadmap).

<details open>
<summary><b>Why another shell?</b></summary>

<br>

The Unix shell is a **text‑stream router** whose ontology hasn't changed since the 1970s: bytes
between processes, structure re‑guessed at every boundary, `PATH` as a flat mutable global, and the
same wall of text for every consumer — human or machine. For an LLM agent that's catastrophic: the
"bash tool" dumps text into context and every downstream decision becomes regex archaeology.

Shoal keeps everything that made the shell great — brevity, direct filesystem access, running any
binary — and removes the 1970s poison:

| The box (Unix) | Out of the box (shoal) |
|---|---|
| Byte streams, structure re‑guessed | **Typed values**, structure never lost |
| The pipe (untyped byte hose) | **Dot‑chains** over typed values (`ls.where(.size > 1mb)`) |
| `PATH` (flat mutable global) | **reef**: scoped, content‑addressed, locked resolution |
| Heredocs / `-c "…"` stdin smuggling | Values as stdin; interpreter blocks |
| `tail -f` / lockfile coordination | Reactive streams + kernel channels |
| History = text you typed | **Journal** = what happened (AST, effects, output hashes) |
| Same wall of bytes for everyone | **Refs + shapes**; agents pull payloads on purpose |
| Ambient `export` | Session state + lexical `with` — nothing inherited invisibly |
| "Which binary?" is forensics | `which` returns the full resolution chain, as a value |
| Permission = who can read the disk | **leash**: capability over the *semantic call* |

The pipe's only real virtue was laziness — and laziness is a property of the `stream` *type*, not a
syntax character. So the pipe is gone, but its physics are kept.

</details>

---

## Quick start

```sh
# Run the interactive shell
cargo run -p shoal

# One-shot a script string
cargo run -p shoal -- -c $'let answer = 6 * 7\nanswer'

# Run a .shl script
cargo run -p shoal -- examples/example.shl
```

<details>
<summary><b>What you get at the prompt</b></summary>

<br>

Statement‑position commands run on a PTY, so `vim`, `git rebase -i`, `htop`, and a colored
`cargo build` all behave normally. Expressions and assignments capture clean structured output
instead. The editor has persistent history, multiline input, context‑aware tab completion,
live syntax highlighting, and a fast cwd/Git/reef‑aware prompt.

```text
~/dev/shoal  main ▲  rust 1.97          # the prompt already knows — zero subprocesses
```

</details>

---

## The language in five minutes

```text
# Compose with dots, not pipes — every step knows the type it carries
["ada", "grace", "linus"].where(.len() > 3).map(.upper())
# → ["ADA", "GRACE", "LINUS"]

# Real arithmetic on typed units — not stringly-typed mush
1.5gb + 500mb          # → 2gb
90s > 1m               # → true

# Functions ARE commands: call them like a program, flags and all
fn deploy(env: str, dry: bool = false) {
    if dry { "would deploy to {env}" } else { "deployed to {env}" }
}
deploy staging --dry   # → "would deploy to staging"

# Structured output from real tools, via adapters — filter it like data
git.status().where(.status == "modified").map(.path)

# Pattern-match on shape, not string-munging
match request {
    { status: 200, body }        => body,
    { status: s } if s >= 500    => "server error {s}",
    _                            => "unhandled",
}
```

<details>
<summary><b>The pipe is a teaching error, on purpose</b></summary>

<br>

Type a bash pipe and shoal doesn't run it — it teaches you the better form:

```text
> ls | grep foo
error: shoal has no pipe operator
  hint: data composes with `.` (try `ls.where(.name.contains("foo"))`);
        raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`
```

Same for the other 1970s reflexes — each with the *why* and the correct alternative:

```text
> echo $HOME     → "shoal variables have no sigil; environment variables are env.NAME"
> if 1 { }       → "shoal has no truthiness — try .is_empty(), .is_some(), or != null"
```

</details>

<details>
<summary><b>Escape hatches — nothing is a cage</b></summary>

<br>

- `sh { … }` runs verbatim POSIX when you truly want it.
- Any external binary just runs (T0): `docker compose up` works with zero configuration.
- `^cmd` forces command interpretation past a shadowing variable.
- `with cwd: p, env: {…} { … }` scopes ambient state lexically — no `cd` hangover, no `export` leak.

</details>

---

## For agents

Shoal exposes every session over a structured wire protocol (JSON‑RPC + an **MCP facade**), built
on one doctrine: **an agent never parses text it didn't explicitly ask to see raw.**

<details open>
<summary><b>The anti–bash‑tool contract</b></summary>

<br>

- Every value the shell produces is **addressable by a stable ref**. The agent's context holds refs
  and small structured summaries; payloads are pulled surgically (by field path or slice) — a
  40k‑row result costs tens of tokens, not tens of thousands.
- **Large values elide automatically** at the wire level: the agent gets shape + schema + a small
  preview + a ref, never the payload, until it asks.
- **State is browsable** (resources), **actions are verbs** (tools), **changes are pushed** (events).
  Polling is a bug. Text‑matching shoal's own output is a bug.
- Diagnostics are structured (`code`/`msg`/`span`/`hint`). No agent shall parse a caret box.
- The multi‑principal kernel makes **pair‑shelling** free: a human and their agents share one live
  session, signalling each other through structured channels — no sentinel files, no `tail`.

A Claude Code plugin (MCP server + a gotcha‑free "language card" skill) is in the works so an agent
can drive shoal flawlessly. See [`docs/AGENT-SURFACE.md`](docs/AGENT-SURFACE.md).

</details>

---

<details>
<summary><b>reef — tool resolution, ripped out of <code>PATH</code></b></summary>

<br>

Every version manager — mise, asdf, nvm, pyenv, direnv — is a workaround for one fossil: `PATH`, a
flat ordered mutable string where first‑match wins and version selection means mutating invisible
global state. shoal deletes the fossil.

- **Names resolve through scopes, not directories**: session fns → project `.reef.toml` → user →
  system → ambient (demoted to a labeled last resort). No hooks, no activation, no env mutation;
  `cd` just re‑scopes.
- A **blake3 lockfile** chains resolution into the sandbox: name → version → content hash → grant.
  Binary changed since you locked it? Hard error, both hashes named.
- **`which node`** returns the full resolution chain *as a value*. "Which node built this artifact
  three weeks ago" is a journal query.
- mise is *interop, not a dependency* — a provider that reads its install tree directly, no shims.
- The poly‑runner question ("how do I run `./x.py`") dissolves into a configurable `[runners]`
  table — resolution keyed on content‑type instead of name.

See [`docs/REEF.md`](docs/REEF.md).

</details>

<details>
<summary><b>leash — capability sandboxing, first‑class on Linux <i>and</i> macOS</b></summary>

<br>

leash answers *may this run*; reef answers *what does this name denote*. Enforcement is evaluated on
the **semantic call** (binary content hash, typed args, declared effects) and enforced by the
strongest available OS mechanism — **Landlock + seccomp on Linux**, **Seatbelt on macOS** (real
`sandbox_init`, not a stub) — with **honest tier reporting**: shoal never claims "enforced" when it
isn't. `plan → inspect effects → apply`, journaled inverses, and content‑hash spawn pinning.
See [`docs/TDD.md`](docs/TDD.md) §8.

</details>

<details>
<summary><b>Architecture — the workspace</b></summary>

<br>

A tree‑walk interpreter over a value model, hosted two ways: embedded (for `shoal -c` and scripts)
and long‑lived (`shoal-kernel`, for interactive + agent sessions). One engine, two hostings.

| Crate | Responsibility |
|---|---|
| `shoal-syntax`, `shoal-ast` | modal lexer, canonical AST, formatter |
| `shoal-value`, `shoal-eval` | typed values, method stdlib, tree‑walk evaluator |
| `shoal-exec` | capture + PTY execution, process‑tree cancellation |
| `shoal-reef` | content‑addressed tool resolution |
| `shoal-adapters` | declarative external‑command schemas (T2) |
| `shoal-journal` | SQLite journal + blake3 content‑addressed output store |
| `shoal-leash` | effects, plans, grants, OS enforcement (Landlock / Seatbelt) |
| `shoal-proto`, `shoal-kernel` | JSON‑RPC protocol + shared multi‑principal sessions |
| `shoal-mcp` | MCP facade for agent harnesses |
| `shoal-prompt` | the fast, config‑driven prompt |
| `shoal` | script runner + interactive terminal client |

Rust edition 2024. The pinned inter‑crate contracts live in
[`docs/CONTRACTS.md`](docs/CONTRACTS.md).

</details>

<details>
<summary><b>What is an "adapter"?</b></summary>

<br>

An adapter is a small **declarative TOML file** that teaches shoal to treat an existing Unix tool as
a native, typed command — *without modifying the tool*. It declares how to invoke it (typed
flags/params), how to parse its output into structured values, and its effects/success‑codes. So
`git status` returns a **table you can `.where`/`.sort`**, not text.

Three tiers: **T0** raw passthrough (any binary works like bash), **T1** output sniffing (auto‑
structure JSON‑ish output), **T2** a declared adapter (precise, typed, community‑shippable, no code).
Adapters wrap the entire existing ecosystem into shoal's typed world — no boil‑the‑ocean rewrite.

</details>

---

## Status & roadmap

<details open>
<summary><b>What works today</b></summary>

<br>

- The full language: literals + typed units, strings/interpolation, the coercion matrix, control
  flow, functions‑as‑commands, lambdas + implicit `.field` forms, dot‑chain composition, `match`
  with every pattern kind, `try`/`catch`, and the teaching diagnostics — **300+ conformance cases,
  green on Linux and macOS.**
- Interactive REPL: PTY passthrough, tab completion, syntax highlighting, `it`/`out[n]`, Ctrl‑C that
  cancels the job (not the shell).
- reef resolution (`which`‑chain, `with reef:`, lockfile), the SQLite journal + CAS, the adapter
  catalog, the kernel JSON‑RPC + MCP facade, and OS sandbox backends.

</details>

<details>
<summary><b>In active construction</b></summary>

<br>

- Interpreter blocks (`python { … }.out` → structured data) and `.feed` values‑as‑stdin.
- Activating leash enforcement on the live spawn path; journal‑backed `undo out[n]`.
- The reactive streams subsystem (`watch` / `tail` / `every` / channels + combinators).
- Events/channels/subscriptions for full pair‑shelling; the Claude Code plugin.
- A broader adapter pack and the remaining structured builtins.

Honest gaps are tracked in the code and design docs — nothing here is vaporware‑by‑omission.

</details>

---

## Building & testing

```sh
cargo test --workspace --all-targets            # unit + integration + the conformance corpus
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The normative language corpus lives in [`spec/cases`](spec/cases) — it *is* the spec; a wrong case
is a bug in the case. CI runs the whole matrix on Linux and macOS on every push.

<details>
<summary><b>Design docs & further reading</b></summary>

<br>

- [`docs/VISION.md`](docs/VISION.md) — the north‑star frame (the typed value graph; one kernel, three surfaces)
- [`docs/TDD.md`](docs/TDD.md) — the language & semantics contract
- [`docs/REEF.md`](docs/REEF.md) — tool resolution
- [`docs/IO.md`](docs/IO.md) — values‑as‑stdin & interpreter blocks
- [`docs/STREAMS.md`](docs/STREAMS.md) — the reactive model
- [`docs/AGENT-SURFACE.md`](docs/AGENT-SURFACE.md) — the agent wire contract
- [`docs/CONTRACTS.md`](docs/CONTRACTS.md) — inter‑crate APIs & error codes
- The [**project wiki**](https://github.com/alliecatowo/shoal/wiki) — narrative guides to the language, reef, agent surface, and security

</details>

---

## License

Dual‑licensed under either of [MIT](LICENSE-MIT) or [Apache‑2.0](LICENSE-APACHE), at your option.

<div align="center">
<sub>A shoal is a school of fish moving as one — and the shallow water where careless vessels run aground. Both are the point.</sub>
</div>
