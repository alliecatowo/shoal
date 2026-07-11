# Reactive values: sources, combinators, sinks; the honest `tail -f`

**Status:** substantially implemented. Working: `channel(name)` with `.emit`/`.events()`/`.latest()`;
`every(dur)`; stream pipelines (`.map`, `.scan(init, f)`, `.take(n)`, `.collect()`); the
`stream_unbounded` guard on collecting an unbounded stream; `watch(...)`/`tail(...)`. Verify a
specific combinator (`.debounce`/`.throttle`/`.window`/`.merge`/`.buffer`/`.dedupe`/`.distinct`) or
sink (`.into(channel)`, `on channel(...) { }`) against source before relying on it.

**Normative. The corpus/frame decides disputes.** Companion to `docs/TDD.md` (esp. §1.9, §4.5,
§4.7, §13.7), `docs/VISION.md` §2, `docs/AGENT-SURFACE.md` §4–7, `docs/REEF.md`; supersedes them
where they conflict on stream semantics. Everything here is a decision; the corpus
(`spec/cases/*.toml`) decides disputes.

## 0. Thesis

`tail -f app.log | grep ERROR`, `fswatch . | xargs -n1 ./rebuild.sh`, and lockfile-polling loops
(`while [ ! -f .done ]; do sleep 1; done`) are box-era coordination (VISION §1: "state lives in the
filesystem… coordination is done by writing files and watching them"). shoal models every
time-varying source of data as a first-class `stream<T>` value, composed with **the same dot-chain
combinators used on any collection** (VISION §2: "composition across nodes is the dot-chain over
typed values… laziness and backpressure are properties of the `stream` type, not of a syntax
character"). A `stream<T>` is single-consumption (TDD §1.9) exactly like every other stream already
is in this language — `ls` (a `list<record>`/`table`) and `watch(".")` (a `stream<event>`) differ
in *time-varying-ness*, not in the vocabulary you drive them with. **Subscribe, never poll**: the
word "poll" is disallowed vocabulary in shoal design (AGENT-SURFACE §6 already states this for the
agent wire; this document extends the same discipline to every stream, human-typed or agent-driven,
anywhere in the language).

---

## 1. The `stream<T>` value, restated precisely

`stream<T>` (TDD §4.1 type list) is:

- **Lazy**: constructing a stream (`watch(...)`, `tail(...)`, `every(...)`, a command's live
  stdout) does no work and consumes no OS resources (no inotify handle opened, no file opened, no
  timer armed) until the stream is **driven** — subscribed to by a sink (§3) or by another
  combinator's own downstream sink. A stream sitting in a variable, unconsumed, is inert.
- **Single-consumption** (TDD §1.9): a `stream<T>` value may be driven to a sink exactly once.
  Attempting to attach a second sink (calling `.each`, `.collect`, rendering it a second time, etc.
  on the *same* stream value after it has already been driven) is a runtime error, code
  `stream_consumed`, with the standing fix-it: *"collect first, or `.tee(2)`"* (`.tee(n)` — TDD's
  existing method, §5's builtin surface list — forks a stream into `n` independently-drivable
  streams, each replaying every item to its own sink; forking is the sanctioned way to have two
  consumers of one source).
- **Backpressured, pull-based** (TDD §4.5: "Streams: pull-based, bounded buffers, backpressure;
  cancellation propagates down-chain"): a sink pulls; a source only produces the next item once the
  previous one has been consumed downstream (or a bounded buffer — never unbounded — has room).
  Nothing in a stream pipeline can run a producer arbitrarily ahead of a slow consumer and exhaust
  memory; where true unboundedness would otherwise happen (e.g. a fast filesystem-event source and
  a slow `.each` sink), the standing rule is the same coalesce-and-drop discipline
  AGENT-SURFACE §6 specifies for event channels (§7 below ties these together explicitly).
- **Typed**: `stream<str>`, `stream<event>`, `stream<record>`, etc. — the element type is part of
  the value's type, checked at combinator-chain construction time (a `.where(pred)` on a
  `stream<event>` type-checks `pred`'s parameter against `event`'s record shape, same as `.where`
  on a `table`).
- **Cancellable down-chain**: cancelling a sink (task cancellation, `Ctrl-C` on a foreground stream
  render, an agent's `shoal_cancel`) propagates upstream through every combinator to the source,
  which releases its OS resource (closes the inotify watch, stops tailing, cancels the timer,
  kills/detaches the streaming child) — TDD §4.7's "cancellation propagates down-chain" wording
  refers to the *cancellation signal* traveling from sink toward source (against the data-flow
  direction, hence "down" the dependency chain from consumer to producer); this document uses the
  unambiguous phrase **sink-to-source** for the propagation direction throughout.

Equality/comparison on streams is an error (TDD §4.1: "comparing streams is an error") —
identity-only, and even identity comparison has no sanctioned use in v1 (there is no `stream ==
stream` case in the corpus).

---

## 2. Sources

Each source below is a **function or method returning a `stream<T>`**, inert until driven (§1).
Every source is specified as: signature, element type, what makes it "live" (the OS/kernel
primitive backing it), and its backpressure story.

### 2.1 `watch(path | glob) → stream<event>`

```
watch(target: path | glob, recursive: bool = true) -> stream<event>
```

- **Backing primitive**: inotify (Linux) / kqueue (BSD/macOS/FSEvents), tier-honest per platform
  exactly like leash's enforcement tiers (TDD §8) — `watch` on a tier without a native
  file-event API degrades to poll-under-the-hood **inside the kernel's own implementation only**
  (never exposed as a poll to the language — the *user* never sees a poll loop; the runtime may
  fall back to one internally on an unsupported platform, surfaced honestly via
  `session.attach`-style capability reporting, mirroring TDD §8's `caps.enforced: false` pattern:
  a stream sourced this way reports `degraded: true` in its metadata if introspected).
- **Element type — `event`**: `{path: path, kind: "created" | "modified" | "removed", ts:
  datetime}`. `kind` is a closed enum rendered as a tagged string; matching on it uses ordinary
  `match`/`.where` against the string, or the sugar `.where(.kind == "modified")`.
  A rename is reported as `removed` (old path) then `created` (new path) — no synthetic "renamed"
  kind exists in v1 (open item; see REEF-style non-goals framing — not resolved here, filed as a
  future addition, not a silent gap: a rename today is two events, correctly typed, just not
  correlated).
- `target` a `glob` (TDD §2.2/§4.3 glob semantics apply): the watch covers every path the glob's
  *directory prefix* could ever match, filtering events by the compiled pattern — so
  `watch("src/**/*.rs")` opens watches rooted at `src/` (recursive) and yields only events whose
  path matches `**/*.rs`, not every filesystem event under `src/`.
- `recursive`: when `target` is a directory path (not a glob) and `recursive: true` (default),
  subdirectories created *after* the watch starts are automatically added — the stream's coverage
  is not fixed at construction time; when `false`, only the named directory's direct children.
- **Backpressure**: bounded ring buffer sized to the kernel's underlying event-queue limit
  (platform-dependent, documented per-tier); on overflow (a burst of file events faster than the
  consumer drains), the source coalesces to a single synthetic `{path: target, kind: "modified",
  ts, coalesced: true}` summary event rather than blocking the producer (the OS's own inotify queue
  has the identical overflow behavior — shoal surfaces it structurally, `coalesced: true`, instead
  of silently losing events with no signal, which is what a raw inotify overflow does today).

### 2.2 `tail(file) / path.tail() → stream<str>`

```
tail(file: path, from_start: bool = false) -> stream<str>
path.tail(from_start: bool = false) -> stream<str>          # method form, identical semantics
```

- **Backing primitive**: opens the file, seeks to EOF (`from_start: false`, the default — matching
  `tail -f`'s default) or to byte 0 (`from_start: true`, matching `tail -f` with no seek /
  `tail -f -n +1`), then watches for appends via the same inotify/kqueue primitive as `watch` (not
  a poll loop) and reads newly-appended bytes as they land.
- **Element type**: `str`, one element per completed line (a trailing partial line — bytes written
  since the last `\n` — is buffered and not yielded until its `\n` arrives; TDD §13.10's CRLF rule
  applies: `\r?\n` is the line terminator, stripped, `bytes` never translated elsewhere in the
  system but `tail`'s whole contract is line-oriented text so this is the one source that commits
  to text rather than bytes).
- **Truncation/rotation**: if the file shrinks (rotated, truncated) the stream detects the file
  size has decreased since last read, re-opens, and continues from the new EOF (matching GNU
  `tail -f --retry`-class robustness) — surfaced as no special event in v1 (open item: a future
  `kind: "rotated"` synthetic event is a candidate v0.3 addition, not specified further here).
- **Backpressure**: bounded line buffer (default cap documented alongside `watch`'s ring buffer
  size); an unread backlog beyond the cap coalesces to a `dropped: n` line-count marker element
  rather than unbounded growth — same discipline as AGENT-SURFACE §6's channel overflow, applied
  to file-tailing specifically.

### 2.3 A running task's live output: `task.output()` / `shoal://task/{id}/out`

```
task.output() -> stream<bytes>            # or stream<str> if the task's adapter declares text output
```

- A `task` (TDD §4.7: `spawn { }` → `task`) exposes its live stdout as a stream via `.output()`.
  This is the in-language mirror of the wire resource `shoal://task/{id}/out` (AGENT-SURFACE §1):
  same underlying subscription, two entry points — a script/REPL consumer calls `.output()`
  directly on a `task` value it holds; an MCP/JSON-RPC client subscribes to the URI (or
  `events.subscribe {channel: "task.{id}"}`, AGENT-SURFACE §4). Both observe the same
  kernel-native pub/sub channel; neither re-reads a file.
- **Element type**: `bytes` by default (the raw stream), narrowing to `str` when the task's
  resolved adapter/runner declares text output (e.g. most CLI tools) — mirrors `outcome.out`'s own
  lazy-structural-parse story (TDD §1.2) but incrementally: each chunk is offered as it's captured,
  not buffered to completion first.
- **Backpressure**: exactly AGENT-SURFACE §6's task channel semantics — bounded per-subscriber
  queue; overflow coalesces to `{dropped: n, latest_seq}`; the stream-side consumer sees this as a
  synthetic element carrying the same shape, so `.each` written against `task.output()` and a wire
  subscriber written against `task.{id}` see structurally the same overflow signal.
- **Cancellation**: cancelling the stream (sink-to-source, §1) does **not** cancel the task itself
  — a stream is a *read* of the task's output, not a handle on its lifecycle; cancel the `task`
  value directly (`.cancel()`, TDD §4.7) to actually stop it. Ending the output stream just stops
  that particular subscription; the task keeps running (and buffering per-task per TDD §4.7,
  "background task output buffered per-task, rendered as discrete blocks") for any other consumer
  or later re-subscription from the journal-backed replay if the channel is journal-backed for
  that task (task channels are ring-buffered per AGENT-SURFACE §4's default, not journal-backed,
  unless the task is journaled as an entry — its *outcome* is journaled at completion regardless).

### 2.4 `every(duration) → stream<datetime>`

```
every(interval: duration) -> stream<datetime>
```

- **Backing primitive**: a single kernel timer (not a sleep loop in language-space); ticks yield
  the tick's `datetime` (wall-clock time of firing, not a monotonic counter — for a monotonic tick
  count, `.enumerate()` the stream, TDD/AGENT-SURFACE's standard "compose, don't add a new
  primitive" instinct).
- **Element type**: `datetime`.
- **Drift/backpressure**: ticks are **not queued** — if a consumer is still processing tick *n*
  when tick *n+1*'s wall-clock moment arrives, tick *n+1* is coalesced away (only the latest
  missed tick's timestamp is delivered once the consumer is ready) rather than buffering a queue of
  stale ticks; this is a timer, not an event log, and infinite catch-up buffering would be actively
  wrong for a timer's semantics. Bounded memory: O(1) always.

### 2.5 `channel(name).events() → stream<event>`

Specified fully in AGENT-SURFACE §7; restated here for the stream-combinator contract: returns a
`stream<event>` of `{channel, seq, ts, payload}` records (the same shape as any wire event,
AGENT-SURFACE §4), lazy and single-consumption like every other stream in this document — it is
not a special case, it slots into every combinator in §3 identically to `watch(...)` or `tail(...)`.
Backpressure is AGENT-SURFACE §6's channel backpressure verbatim (bounded per-subscriber queue,
coalesced-summary overflow) — §7 below states the tie-in once for all channel-backed streams
rather than repeating it per source.

### 2.6 A command's streaming stdout in value position

A command call in value position (TDD §1.2) that is the operand of a stream-consuming combinator
or explicitly requested as streaming — `cmd args…` used where a `stream<bytes|str>` is expected, or
via an explicit `cmd args….stream()` (a value method promoting a normally-buffered outcome-in-
progress into a live stream of its stdout as it's produced, mirroring `task.output()` for
foreground rather than background execution) — yields `stream<bytes>` (narrowing to `stream<str>`
per adapter output-type declaration, same rule as §2.3). Backpressure: the child's stdout pipe
itself provides OS-level backpressure (a full pipe buffer blocks the child's writes, exactly the
Unix pipe's one honest virtue, VISION §2: "Laziness and backpressure — the pipe's only real
virtues — are properties of the `stream` type... The pipe is deleted; its physics are kept") —
here is the literal place that sentence is implemented: the underlying OS pipe still exists and
still backpressures; it is simply wrapped in the typed `stream` value instead of being a bare fd
plumbed by number (IO.md §4 forbids the fd-number spelling; this is the fd's honest replacement).

---

## 3. Combinators

All combinators below are **methods on `stream<T>`**, returning a new `stream<U>` (lazy — none of
them drive the upstream source; driving only happens at a sink, §4) unless marked otherwise. Every
combinator is single-consumption of its *input* stream (attaching one combinator to a stream
consumes that stream value; the combinator's *output* is a fresh, not-yet-consumed stream).

| Combinator | Signature | Semantics | Bounded memory? |
|---|---|---|---|
| `.where(pred)` | `stream<T> -> stream<T>` | yields only elements where `pred(item)` is truthy (bool or outcome-success, TDD §1.10); pred can use `.field <op> val` implicit-lambda sugar (TDD §3.4) same as `list.where` | yes, O(1) — no buffering, pure filter |
| `.map(f)` | `stream<T> -> stream<U>` | transforms each element through `f`; errors raised by `f` propagate as a stream-level error at that position, terminating the stream (does not silently drop) | yes, O(1) |
| `.scan(init, f)` | `stream<T> -> stream<U>`, `f: (U, T) -> U` | running fold — emits the accumulator's *new* value after each input item (unlike `.reduce`/`.sum` which are collection-only terminal ops, `.scan` is a combinator: it emits one output per input, not one final value) | yes, O(1) extra beyond the accumulator's own size |
| `.window(n)` | `stream<T> -> stream<list<T>>` | emits a `list<T>` of the last `n` items each time a new item arrives, once at least `n` have arrived (a sliding count-window; first `n-1` items produce no output) | yes, O(n) |
| `.window(duration)` | `stream<T> -> stream<list<T>>` | emits the list of items received within the trailing `duration`, re-emitted on every new arrival (a sliding time-window; requires items to carry or be assigned arrival timestamps internally — items themselves are unmodified `T`, timestamps are bookkeeping only) | yes, bounded by (rate × duration) — a caller windowing an unbounded-rate source over a long duration is responsible for that product being sane, same caveat as any sliding-window implementation |
| `.debounce(dur)` | `stream<T> -> stream<T>` | suppresses items until `dur` has elapsed with **no new item arriving**, then emits only the *last* item of the suppressed burst — the classic "wait for quiet" semantics (`watch(...).debounce(200ms)` — don't fire on every intermediate write during a save-storm, fire once quiet returns) | yes, O(1) (holds at most one pending item) |
| `.throttle(dur)` | `stream<T> -> stream<T>` | emits the *first* item immediately, then suppresses further items until `dur` has elapsed, then lets the next item through and restarts the window — rate-limiting, distinct from debounce (throttle guarantees a leading emission and a periodic cadence under sustained input; debounce guarantees a trailing emission only after quiet) | yes, O(1) |
| `.dedupe` / `.distinct` | `stream<T> -> stream<T>` | `.dedupe`: suppresses an item equal (structural equality, TDD §4.1) to the **immediately preceding** emitted item only — consecutive-duplicate suppression, O(1) memory. `.distinct`: suppresses any item structurally equal to **any previously emitted** item — full-history suppression | `.dedupe` yes O(1); `.distinct` **no** — unbounded memory in the general case (must remember every distinct value seen); documented as the one combinator in this table that is not bounded-memory, matching TDD §1.9's `.collect()`-style "infinite needs a bound" caveat — use `.distinct` only on streams known to have small distinct-value cardinality, or compose `.take(n)`/`.take_until` first |
| `.merge(other)` | `stream<T>, stream<T> -> stream<T>` | interleaves items from both streams in arrival order (first-ready-wins); ends when **both** upstreams have ended; cancelling the merged stream cancels both upstreams sink-to-source | yes, O(1) beyond each upstream's own state |
| `.zip(other)` | `stream<T>, stream<U> -> stream<(T,U)>` | pairs the *n*-th item of each stream positionally, holding the faster stream's items in a bounded buffer until the slower stream produces its *n*-th; ends when **either** upstream ends (no partial trailing pair) | yes, bounded by the two upstreams' rate skew — documented as "bounded but not O(1)"; a persistently-skewed pair is a design smell, not a shoal bug |
| `.take(n)` | `stream<T> -> stream<T>` | yields the first `n` items then ends the stream (cancels upstream sink-to-source once `n` is reached — does not wait for a natural upstream end) | yes, O(1) |
| `.take_until(pred)` | `stream<T> -> stream<T>`, `pred: T -> bool` | yields items up to and **not including** the first item for which `pred` is true, then ends (cancels upstream) | yes, O(1) |
| `.take_until(other: stream<_>)` | `stream<T>, stream<_> -> stream<T>` | yields items from the primary stream until `other` produces its first item (any type), then ends both (cancels both sink-to-source) — the standard "run until signalled" idiom, e.g. `watch(...).take_until(channel("stop").events())` | yes, O(1) |
| `.buffer(n)` | `stream<T> -> stream<T>` | inserts an explicit bounded buffer of size `n` between upstream and downstream, decoupling their pacing (upstream can run up to `n` items ahead of a momentarily-slow downstream instead of blocking immediately) — this is the one combinator that exists *purely* to relax backpressure locally, never to remove it: at `n` full, upstream still blocks | yes, O(n), explicitly bounded by the caller's own choice of `n` |
| `.flat_map(f)` | `stream<T> -> stream<U>`, `f: T -> stream<U> \| list<U>` | maps each item to a sub-stream/list and flattens the results into one output stream, interleaving sub-streams as they produce (not sequentially exhausting one sub-stream before starting the next) — the stream analog of `list.flat_map` (TDD §5 method list) | yes if each `f(item)`'s output is itself bounded per the same rules; the combinator adds no unboundedness of its own beyond what any concurrently-open sub-stream requires |

Every combinator preserves single-consumption and sink-to-source cancellation propagation (§1) —
none of these are exceptions; none buffer the *entire* upstream by default (the ones that buffer at
all — `.window`, `.zip`, `.buffer`, `.distinct`'s history — are called out explicitly above with
their bound, and `.distinct` is the sole unbounded one).

---

## 4. Sinks

A **sink** is a terminal operation: it drives the stream (§1 — this is the moment the source
actually becomes live and starts producing). Every stream must eventually reach exactly one sink
(or be discarded unconsumed, which is legal — an inert, never-driven stream simply never opens its
underlying resource and is dropped, no error).

- **Live render** — the default when a `stream<T>` value is the result of a **statement position**
  (TDD §1.2) expression in the TUI: the REPL/TUI renders the stream as a **live-updating view**
  rather than the one-shot value render every other type gets. Concretely: `watch(".")` typed bare
  at the prompt opens a live-scrolling structured view of events as they arrive (colorized by
  `kind`, one line per event, in the same table-rendering machinery as any other table — TDD's
  `render_block`, CONTRACTS §3 — refreshed incrementally rather than printed once) and returns
  control to the prompt only on `Ctrl-C` (which cancels the stream, sink-to-source, exactly like
  cancelling any other foreground task, TDD §4.7) or the stream's natural end (`.take(n)`-bounded
  streams, etc.). **At value position**, a stream is never live-rendered automatically — value
  position never renders side effects on its own; the value is just a `stream<T>` handed to
  whatever consumes the expression (an assignment, an argument, a further combinator). Explicitly
  requesting a live render of a value-position stream is `.render()` (a sink like any other,
  callable anywhere, useful inside a `fn` body that wants the live-view behavior without relying on
  statement-position magic).
- `.each(f)` — calls `f(item)` for every item, side-effecting, driving the stream to natural
  completion or cancellation; returns `null`. The general-purpose sink for "do something per
  event," used for both finite and infinite streams (an infinite stream's `.each` simply runs
  until cancelled — this is the *sanctioned* way to consume an infinite stream, unlike `.collect`).
- `.collect()` — gathers all items into a `list<T>`. **Finite streams only.** Calling `.collect()`
  on a stream with no natural end (a `watch(...)`, `every(...)`, a channel's `.events()` with no
  `.take`/`.take_until` upstream of the collect) is a runtime error, code `stream_unbounded`
  (extends CONTRACTS §4's error table alongside `stream_consumed` — both are stream-discipline
  errors, kept as separate codes since one is a reuse violation and the other is a boundedness
  violation), with fix-it *"this stream has no natural end — bound it first: `.take(n)` or
  `.take_until(...)`, or use `.each(f)` if you don't need a final list."* A stream that *is*
  naturally finite (e.g. `channel(...).events()` composed with `.take_until(other)`, or a
  `tail(file, from_start: true)` that the caller has separately arranged to end — rare, since tail
  is inherently unbounded-forward by design) collects normally.
- `.into(channel(name))` — republishes every item of the stream as an event on the named channel
  (`emit`-equivalent per item, AGENT-SURFACE §7): `some_stream.into(channel("progress"))` turns any
  stream — a file tail, a debounced watch, a mapped task output — into something **other
  principals can subscribe to** rather than only the current session's own combinator chain. This
  is the sanctioned bridge from "a stream I built" to "a stream anyone with the right subscription
  can see," and is how a human's derived/filtered view becomes an agent-consumable channel without
  either party touching a file.
- `.save(path)` — writes each item to `path`, **append mode**, exactly matching the "live" version
  of TDD §1.3's `>>` sugar: each arriving item is appended as it arrives (not buffered to the end
  and written once) — the honest live-logging idiom, replacing "redirect a long-running process to
  a file and tail the file to watch it" with "the stream both renders live and is durably appended,
  simultaneously, because `.save` is just another sink you can chain alongside `.each`/render via
  `.tee(2)` (§1) if you want both."
- `.feed(cmd)` — feeds each item to a freshly-relevant destination per IO.md §1's serialization
  rules, applied per-item as they arrive (for a `stream<str>`, this means: each string item is fed,
  newline-joined per IO.md's `list<str>` rule, but incrementally — the child process receives a
  continuous line-oriented stdin as the source produces, rather than shoal buffering the entire
  stream before spawning the child). This is IO.md §1.2's `stream<T>` row, restated here as a sink
  because from the stream side of the contract it *is* a sink (terminal — it drives the stream to
  completion or cancellation, same as any other sink here) even though from IO.md's side it's
  documented as a serialization case of `.feed`. Cross-reference, not duplication: IO.md owns
  *what bytes* get fed; this document owns *that feeding is a valid stream terminus*.

---

## 5. The honest replacements, worked

```shoal
# box era: tail -f app.log | grep ERROR
tail("app.log").where(.contains("ERROR")).each(render)

# box era: fswatch -r src | xargs -n1 -I{} cargo test  (debounced by hand with sleep hacks)
watch("src/**/*.rs").debounce(200ms).each(_ => cargo test)

# box era: watch -n1 'ps aux | grep myapp'   (a poll loop with a fixed interval)
every(1s).map(_ => ps.where(.name == "myapp")).each(render)
#   ^ NOT a poll: `every` is one kernel timer; the *ps* call runs on tick, but nothing in this
#     chain is a `while sleep 1; do …; done` shell loop — the timer is subscribed once.

# a live table that updates: channel-sourced counter, rendered as it changes
channel("build.progress").events()
    .map(ev => ev.payload)          # {step: str, pct: int}
    .into(channel("dashboard"))      # republish for any other subscriber
# ...and, separately, at the prompt, statement position drives the live render directly:
channel("build.progress").events().map(ev => ev.payload)
#   ^ typed bare at the REPL: live-updating record view, one row rewritten per event, per §4.

# box era: while [ ! -f .deploy-done ]; do sleep 1; done; cat result.json
channel("deploy").take()            # blocks for the next value, no file, no poll (AGENT-SURFACE §7)

# combining a source with `.feed` as the sink, matching IO.md's stream row directly:
tail("access.log", from_start: true).feed(awk { { print $1 } }).out.lines().uniq().len()
```

Every replacement above is a dot-chain over a typed value: no `|`, no `>>`/`.done` sentinel file, no
`sleep`-loop, and every intermediate value is inspectable (`it` at statement position; a bound
`let` anywhere) rather than a byte wall that must be scraped.

---

## 6. Agent surface tie-in

Any stream **sink** can target a channel (`.into(channel(name))`, §4), and any channel's
`.events()` is itself a stream **source** (§2.5) — the two directions compose, which is precisely
how a stream a human built interactively becomes something an agent subscribes to, and how an
agent's own derived stream becomes something a human's live TUI view renders. Concretely:

- Agents consume via **subscription** exactly as AGENT-SURFACE §4–6 specify: `resources/subscribe`
  on a `shoal://events/{channel}` or `shoal://task/{id}/out` URI (MCP), or `events.subscribe
  {channel, since?}` (native JSON-RPC) — never by tailing a file, never by re-reading a rendered
  tty transcript, never by polling `shoal_get` in a loop hoping for change (AGENT-SURFACE §6: "A
  correct agent never calls the same read twice hoping for change — it subscribes.").
- A stream defined and combinated **in-language** (§2–§3) and a wire-level event channel
  (AGENT-SURFACE §4) are **the same underlying primitive** viewed from two surfaces — there is no
  separate "agent streams" system. `channel("deploy").events()` inside a `.shl` script and
  `events.subscribe {channel: "deploy"}` from an MCP client are subscribing to the identical
  kernel-native pub/sub channel; a human's derived, filtered, debounced stream reaching `.into(a
  channel)` is precisely how it becomes that same subscribable thing for an agent, with zero
  additional plumbing.

### 6.1 Cancellation and backpressure — exact references

- **Cancellation**: sink-to-source propagation (§1) is the general rule; for a *task's* output
  stream specifically, cancelling the stream does not cancel the task (§2.3) — the task's own
  lifecycle is cancelled via `task.cancel()` (TDD §4.7) or, from the agent surface,
  `shoal_cancel {task}` (AGENT-SURFACE §5). For every other source (`watch`, `tail`, `every`,
  `channel(...).events()`), cancelling the stream *does* release the source's underlying resource
  immediately (closes the inotify watch, stops tailing, disarms the timer, drops the channel
  subscription) — there is no separate handle to cancel because there is no separate lifecycle;
  the stream *is* the subscription.
- **Backpressure**: every source in §2 documents its own bound; the unifying rule across all of
  them, restated once: a slow consumer never causes unbounded producer-side buffering or a blocked
  producer holding a lock/fd indefinitely — it causes a **coalesced summary** (a `dropped: n` /
  `coalesced: true` marker element, or, for `.merge`/`.zip`/`.buffer`, an explicitly bounded local
  buffer with a caller-chosen size) exactly matching AGENT-SURFACE §6's channel overflow contract
  (`{dropped: n, latest_seq}`) — "liveness beats completeness on a saturated channel; completeness
  is always recoverable from the journal" applies verbatim to in-language streams, not just the
  wire, because they are the same channels.
- **The SIGPIPE analog** (TDD §13.7: "downstream cancellation closes pipe; producer's early exit
  not an error when caused by intentional cancel"): a stream sink cancelling upstream (e.g. `.take
  (10)` reaching its 10th item and cancelling a `tail(...)` source) is not surfaced as an error on
  either side — the source's early stop is expected, recorded (if journaled at all) as a normal
  cancellation, not a fault, mirroring exactly how a `cmd | head -10`'s upstream SIGPIPE is not
  treated as `cmd` having failed. Contrast with an *unrequested* upstream failure (the watched file
  is deleted out from under a `tail`, the filesystem being watched is unmounted) — that **is** a
  stream-level error, delivered as an error element / raised at the sink, because nothing
  downstream asked for the stream to end.

---

## 7. Cursor/replay and at-least-once — ring buffer vs. journal-backed

Mirroring AGENT-SURFACE §4 exactly (restated here as the stream-level contract rather than the wire
contract, since they govern the same channels):

- **Delivery is at-least-once**, per-channel monotonic `seq`. A stream sourced from a channel
  (`channel(name).events()`) carries each event's `seq`; a consumer that needs to resume after a
  gap (a crashed `.each` handler, a re-subscribed script) does so by re-deriving the stream with a
  cursor: `channel(name).events(since: last_seq)` — the `since` parameter is the in-language mirror
  of the wire's `?since={seq}` query parameter (AGENT-SURFACE §1, §6). Consumers **dedup by `seq`**
  if delivery-exactly-once matters to them; the stream itself does not silently deduplicate (an
  at-least-once channel replayed with an overlapping `since` window will, correctly, re-yield
  events in the overlap — this is not a bug to paper over in the combinator layer, it is the
  contract, and `.dedupe`/`.distinct` (§3) are the caller's tool if per-event dedup is wanted, keyed
  on `seq` via `.dedupe_by(.seq)`-style field projection through the same `.field` implicit-lambda
  sugar as `.where`).
- **Ring buffer vs. journal-backed**, exactly AGENT-SURFACE §4's split: `session.transcript` and
  `journal` channels are journal-backed (replayable from *any* historical `seq`, unbounded lookback
  — the journal is durable per TDD §9); every other channel (task output, user-defined
  `channel(name)`, filesystem-watch-derived streams `.into`'d to a channel) is **ring-buffered**
  (≥1024 events per AGENT-SURFACE §4's stated minimum) — replay is bounded to whatever's still in
  the ring; older history for a ring-buffered channel is gone once evicted, by design (an unbounded
  ring would just be the journal wearing a disguise, and the journal already exists for the
  durable case). A stream built over a ring-buffered channel that needs durability should
  explicitly `.save(path)` (§4) or route through a journaled channel — there is no implicit
  promotion from ring-buffered to durable.
- **A stream's cursor is not automatically persisted** across process restarts — `since: last_seq`
  must be supplied by the caller (read back from wherever the caller last recorded it: a saved
  file, a journal query, an agent's own state). shoal does not invent a hidden checkpoint file for
  this (that would reintroduce exactly the sentinel-file coordination this document exists to
  delete) — the `seq` value itself, returned/observable on every event, *is* the durable handle;
  what the caller does with it is the caller's business, same as any other value.

---

*shoal STREAMS v0.1 — the corpus decides disputes. Cross-refs: VISION §2 (typed value graph,
events/streams as first-class node kind), §4 (inversions table — `tail -f` row); TDD §1.9
(single-consumption), §4.1 (`stream` type, equality), §4.5 (evaluation/execution model — pull,
backpressure, cancellation, sinks), §4.7 (concurrency/task model, `Ctrl-C`), §13.7 (SIGPIPE
analog); AGENT-SURFACE §4 (channels, cursors, ring vs. journal), §5 (tools — `shoal_cancel`), §6
(subscriptions, backpressure, "never poll"), §7 (in-language channel API); IO.md §1 (`.feed`
serialization, the `stream<T>` row) — this document owns stream semantics, IO.md owns byte
serialization; REEF.md is not load-bearing for this document (no tool-resolution content here) and
is cited only where AGENT-SURFACE itself cites it.*
