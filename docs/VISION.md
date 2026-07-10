# shoal — the vision, out of the box

**Normative north star.** Not marketing; the frame every other doc is an instance of. When a design
question isn't answered by TDD/REEF/AGENT-SURFACE/IO/STREAMS, answer it by asking "what does *this*
frame demand?" If a proposed feature only makes sense inside the box (§1), it is wrong here.

## 1. The box we are leaving

The Unix shell is a **text-stream router between processes**. Its ontology, unchanged since the
1970s:

- The unit of data is the **byte stream**. Structure is a convention re-guessed at every boundary
  (whitespace, columns, NUL, JSON-if-you're-lucky). Every tool re-parses what the last tool
  un-parsed.
- Composition is the **pipe**: a byte hose between two processes, unidirectional, untyped, lazy by
  accident of buffering.
- Naming is **PATH**: a flat mutable string of directories, first match wins, mutated as ambient
  state. Version managers, shims, and activation hooks are all scar tissue over this one wound
  (see REEF §0).
- State lives in **the filesystem and the environment**: coordination is done by writing files and
  watching them (`tail -f`, lockfiles, sentinels), or by exporting variables a child inherits
  invisibly.
- Feeding input is **stdin plumbing**: heredocs (`<< EOF … EOF`), `-c "…"` string-smuggling, and
  quoting hell — a program is smuggled as a byte payload through a delimiter dance.
- History is a **line of text you typed**. What actually *happened* — what resolved, what ran, what
  changed — is unrecorded and unrecoverable.
- Every consumer, human or machine, gets the **same wall of bytes** and must scrape it. For an LLM
  agent this is catastrophic: the bash tool dumps text into context and every decision downstream
  is regex archaeology.

Every one of these is a *position* — a choice that made sense on a teletype in 1975 and is now a
poison pill. shoal exists to rip them out at the root, not to shim them.

## 2. The box we are building: a typed value graph over one session kernel

shoal is a **session kernel that evaluates a program over a graph of typed values**. There are four
kinds of node, and everything in the system is one of them:

1. **Values** — immutable, structural, content-addressable (`val:blake3:…`), the TDD §4 model.
   A number, a table, a path, an outcome, a stream. Data has a *type*, always; structure is never
   re-guessed because it is never lost.
2. **Processes** — spawn nodes that consume values (args, stdin-as-value, env) and produce an
   **outcome** value (status, stdout bytes, structured `.out`, timings). A process is not a text
   hose; it is a function from values to a value. exec owns this (TDD §1.2 position rule; §13.6
   signal honesty).
3. **Resources** — browsable typed trees the session sits over: the filesystem (typed paths, not
   strings), tools (reef resolution, not PATH), session state (cwd/env/reef views), the journal.
   Nouns you address, not text you cat.
4. **Events / streams** — time-varying values: file watches, live process output, timers,
   channels. First-class, subscribable, composed with the same combinators as any collection
   (STREAMS). The honest replacement for `tail -f | grep` and for file-watching coordination.

Composition across nodes is the **dot-chain over typed values** (`.where .map .sort .feed …`), not
the pipe. Laziness and backpressure — the pipe's only real virtues — are properties of the
`stream` type, not of a syntax character. The pipe is deleted; its physics are kept.

## 3. One kernel, three surfaces

The same kernel evaluating the same graph is driven by three surfaces, none privileged, none an
afterthought:

- **Human (TUI):** the colorized, direct-manipulation surface. Tables render, streams live-update,
  `.pick()` opens a fuzzy overlay, diagnostics are caret boxes with hints. The tty is a *rich
  display*, not a byte wire. This is where the bells and whistles live and where nothing ever makes
  you miss oh-my-zsh.
- **Agent (MCP/JSON-RPC):** resources (nouns), tools (verbs), events (push). Values are `$`-tagged
  and ref-addressed; large payloads elide automatically (AGENT-SURFACE §3). **The agent never
  parses text it didn't explicitly ask to see raw.** The bash tool is the anti-pattern this surface
  exists to end.
- **Script (.shl):** deterministic value programs. No `it`/`out` (REPL-only), errors on unlocked
  reef constraints, watchdogs off unless set. A committed artifact whose behavior is a pure
  function of its inputs and its lockfile.

The surfaces differ only in *rendering and affordance*, never in *semantics*. A command means the
same thing in all three; that is why pair-shelling (a human and agents in one live session, §
AGENT-SURFACE 7) is free rather than a feature.

## 4. The load-bearing inversions

Each is specified precisely elsewhere; collected here so the shape is visible at once.

| The box (Unix) | Out of the box (shoal) | Spec |
|---|---|---|
| Byte streams, structure re-guessed | Typed values, structure never lost | TDD §4 |
| Pipe (untyped byte hose) | Dot-chain over typed values; laziness is the `stream` type | TDD §1.4, STREAMS |
| PATH (flat mutable global) | reef: scoped, content-addressed, locked resolution; PATH is an *output* | REEF |
| Heredoc / `-c "…"` stdin smuggling | Values as stdin (`.feed`); interpreter **blocks** (`python { … }`) | IO |
| `tail -f` / file-watch coordination | Reactive streams + kernel channels; subscribe, never poll | STREAMS, AGENT-SURFACE §4–7 |
| History = text you typed | Journal = what happened (AST, effects, resolution, output hashes) | TDD §9 |
| Same wall of bytes for everyone | Refs + shapes; payload pulled/pushed on purpose, per surface | AGENT-SURFACE |
| Ambient env mutation (`export`) | Session state + lexical `with`; no invisible inheritance | TDD §4.6 |
| "Which binary?" is forensics | `which` returns the full resolution chain as a value | REEF §6 |
| Permission = who can read the FS | leash: capability over the *semantic call* (name→hash→grant) | TDD §8 |

## 5. What this makes trivial that bash makes miserable

- **Reproducibility:** "which node built this three weeks ago" is a journal query, because
  resolution and spawn are recorded, not re-guessed.
- **Safety:** plan → inspect effects → apply; undo from recorded inverses; a binary that changed
  since you locked it refuses to run.
- **Agent ergonomics:** an agent drives a 40k-row result by ref and field-path, spending tens of
  tokens, not tens of thousands — and cannot flood its own context (elision is wire-level).
- **Coordination:** a human and an agent signal each other with structured channel events, not
  lockfiles and `tail`.
- **Extension:** adding a tool is a TOML adapter; adding a runner is one `[runners]` line; adding a
  command is writing a `fn`. No new ontology, ever.

## 6. The discipline

1. **No text-scraping of our own output, anywhere, by anyone.** Not in the agent surface, not in a
   builtin, not in a completion. If a decision requires parsing lines/columns of shoal's own
   rendering, the design is wrong — the structured value already exists upstream; reach for it.
2. **The tty is a wire for humans and a display, never a data interface.** Machines use the socket.
3. **Ambient state is the enemy.** cwd/env/reef are explicit session state with lexical `with`
   scoping; nothing is inherited invisibly, nothing is mutated by a hook.
4. **Every value is addressable and every action is journaled.** If you can't point at it later or
   ask what it did, it isn't finished.
5. **Do the hard thing at the root.** When tempted to shim a Unix position (a PATH tweak, a stdin
   hack, a text parse), stop — that temptation is the box reasserting itself. Rip it out instead.

*shoal VISION v0.1 — the frame decides scope disputes; a feature that only lives inside the box is
out of scope.*
