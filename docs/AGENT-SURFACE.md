# The agent surface — wire contract v2

**Status:** wire encoding partial.

**Normative.** Companion to TDD §7; supersedes it where they conflict. Implements the doctrine
below across `shoal-proto`, `shoal-kernel`, `shoal-mcp`.

## 0. Doctrine — the anti-bash-tool

The bash tool is the failure mode this shell exists to end: an agent runs a command, a wall of
text lands in its context, and every downstream decision is regex archaeology. **In shoal, an
agent never parses text it didn't explicitly ask to see raw.**

1. Every value the shell produces is **addressable by a stable ref**. The agent's context holds
   refs and small structured summaries; payloads are fetched surgically (field paths, slices) or
   subscribed to. Re-reading costs zero re-execution and near-zero tokens.
2. **Large values never enter context uninvited** — the elision rule (§3) is wire-level and
   automatic, not agent etiquette.
3. **State is browsable (resources), actions are verbs (tools), changes are pushed (events).**
   Polling is a bug. Text-matching shoal's own output is a bug. The tty is a wire, not an
   interface.
4. Diagnostics are structured (`code`/`msg`/`span`/`hint`). No agent shall parse the caret box.

## 1. Refs and URIs

Short refs (in values, journal, renders): `out:12`, `val:blake3:<hex>`, `task:7`, `plan:<hex16>`,
`ch:<name>`. Every short ref has a URI form; MCP resources use URIs:

```
shoal://out/{n}                    transcript value n (session-scoped)
shoal://val/{blake3}               immutable content-addressed value
shoal://task/{id}                  task record: status, desc, exit, timings
shoal://task/{id}/out              task's live output (subscribable, cursor-read)
shoal://jobs                       the task table
shoal://journal                    query root (template: ?since,head,principal,ok,limit)
shoal://journal/entry/{id}         one entry: src, canonical AST, effects, outputs (hashes)
shoal://plan/{ref}                 a derived plan: effects, reversibility, verdict
shoal://session/cwd|env|reef       session state views (env NAMES only unless granted)
shoal://events/{channel}           event channel, cursor-read (?since=seq)
```

All value-bearing resources accept `?path=<fieldpath>&slice={a}..{b}&format=json|render|raw`.
Field-path grammar (same as `value.get`): `.field`, `[n]`, `[a..b]` — e.g.
`shoal://out/12?path=.rows[3].name`. `format=raw` returns original bytes (outcome stdout etc.);
`render` returns the human render; default `json` returns the `$`-tagged encoding.

## 2. Value encoding

`$`-tagged JSON for every type (no fallthrough-to-string, ever):
`null/bool/int/float` native; `{"$":"str","v":…}` only when tagging is needed (top level values are
always tagged); `{"$":"path","v":…,"raw":base64?}` (raw present iff non-UTF-8);
`{"$":"size","v":bytes}`; `{"$":"duration","v":ns}`; `{"$":"datetime","v":rfc3339}`;
`{"$":"time","v":"HH:MM:SS"}`; `{"$":"regex","v":src}`; `{"$":"glob","v":pattern}`;
`{"$":"range","start","end","inclusive"}`; `{"$":"list","v":[…]}`;
`{"$":"record","v":{k:…}}`; **table columnar**: `{"$":"table","cols":{name:[…],…},"n":N}`;
`{"$":"outcome","status","signal","ok","out":<tagged>,"err":str,"dur_ns","pid","cmd","span"}`;
`{"$":"error","code","msg","span","hint","stderr"}`; `{"$":"task","id","done","desc"}`;
`{"$":"stream","label","uri"}` (never inline payload); `{"$":"secret","name"}` (never material);
`{"$":"bytes","len","v":base64?}` (v elided per §3); `{"$":"closure","repr"}`.

## 3. The elision rule (wire-level, automatic)

**Status: elision: implemented.**

Any value whose encoded JSON exceeds **8 KiB**, any table over **100 rows**, any bytes over
**4 KiB**, any list over **500 items**: the wire carries an **elided form** instead:

```json
{"$":"ref", "uri":"shoal://out/12", "of":"table", "n":8214,
 "cols":{"name":"str","size":"size","modified":"datetime"},
 "preview":{"$":"table","cols":{…first 5 rows…},"n":5},
 "render_head":"name  size  modified\n────…(first 10 lines)"}
```

Shape always travels (type, counts, schema, small preview); payload never does until asked.
Callers may tighten/loosen per call (`elide:{max_bytes,max_rows}` on exec/read), never disable
above a hard cap (64 KiB) — a misbehaving agent cannot flood itself.

## 4. Events — channels, cursors, push

Kernel-native pub/sub over the same socket. Every event: `{channel, seq, ts, payload}` — `seq`
monotonic **per channel**; ring-buffered (≥1024 events; `session.transcript`/`journal` channels
are journal-backed, replayable from any seq). Read = `shoal://events/{ch}?since={seq}`;
push = subscription (§6). At-least-once; consumers dedup by seq.

Channels (payloads are `$`-tagged):
```
session.transcript   {n, ref, summary:{type, ok?, cmd?, n?}}      every new out[n]
task.{id}            {state:"started"|"output"|"suspended"|"exited", chunk_ref?, exit?}
journal              {entry_id, head, ok, principal}
approval             {plan_ref, effects, principal, expires}       plan awaiting approval
render               {ref, render}                                 UI clients
reef                 {tool, event:"locked"|"drift"|"fetched", hash}
user.{name}          arbitrary $-tagged value                      in-language channel(name)
```

`user.*` channels are the cross-principal primitive: a human's session and its agents signal each
other structurally (pair-shelling; no file-watching, no sentinel strings in ttys).

## 5. Tools (verbs) — MCP `tools/*` and kernel JSON-RPC

Small verb set; nouns live in §1. All results carry `structuredContent` (the `$`-tagged value or
its elided form) **and** a `resource_link` to the ref — text content is a render string only.

```
shoal_exec   {src|ast, mode:"run"|"plan", position:"stmt"|"value", background:bool,
              timeout_ms?, elide?}
  → run:  {ref, uri, value|elided, render, ok, events:"session.transcript"}
  → run+background or timeout hit: {task:{id,uri}, events:"task.{id}"}   (never blocks context)
  → plan: {plan_ref, uri, effects, reversibility, verdict}
shoal_apply  {plan_ref}          → as exec-run   (refs are unique per plan — collision = bug)
shoal_get    {ref|uri, path?, slice?, format?, elide?}
shoal_journal{since?, head?, principal?, ok?, limit?}
shoal_cancel {task}
shoal_cap_request {effects:[…]}  → granted | denied{why} | approval_pending{ref}
```

Kernel JSON-RPC keeps the TDD §7 method set (`session.attach`, `parse`, `exec`, `plan.apply`,
`value.get`, `task.*`, `journal.query`, `complete`, `explain`, `cap.request`) plus
`events.read {channel, since, limit}` and `events.publish {channel, payload}` (user channels
only). `session.attach` result gains `{caps_enforced: bool, ast_version, elide_defaults,
channels: [names]}` so a client learns, at attach time, whether the wall is real (TDD §8 tier
honesty) and what it may subscribe to.

## 6. Subscriptions — push, never poll

An MCP client subscribes to a resource URI (`resources/subscribe` with a
`shoal://events/{ch}` or `shoal://task/{id}/out` URI); the kernel pushes
`notifications/resources/updated` carrying `{uri, seq, payload}` as events arrive. Native
JSON-RPC clients use `events.subscribe {channel, since?}` → a stream of `event` notifications on
the same socket; `events.unsubscribe {channel}`. Delivery is at-least-once with per-channel `seq`;
a client that missed events replays with `?since={last_seq}` (ring-buffered channels) or a journal
query (journal-backed channels). **A correct agent never calls the same read twice hoping for
change — it subscribes.** The word "poll" appears in this system only as an anti-pattern.

Backpressure: a slow subscriber's queue is bounded; on overflow the kernel drops to a **coalesced
summary** for that subscriber (`{dropped: n, latest_seq}`) rather than unbounded buffering or
blocking producers — the subscriber re-reads from `latest_seq`. Liveness beats completeness on a
saturated channel; completeness is always recoverable from the journal.

## 7. In-language channel & event API (the pair-shelling primitive)

Channels are first-class in the language, not just the wire — this is how a human session and its
agents coordinate without files, sentinels, or tty-scraping:

```
channel("deploy")                     → a channel handle (creates on first use, session-scoped
                                        unless `channel("name", scope: global)`)
channel("deploy").emit(value)         → publish a $-tagged value (event on user.deploy)
channel("deploy").events()            → a stream<event> (subscribe; lazy, single-consumption)
channel("deploy").latest()            → last value or null (no wait)
channel("deploy").take()              → block for the next value (with timeout: duration)
on channel("deploy") { ev => … }      → register a handler (desugars to .events().each in a task)
```

`emit` is the structural signal; `events()` feeds the same stream combinators as any other source
(§STREAMS). An agent watching a human's `channel("review")` and a human watching an agent's
`channel("progress")` is the entire pair-shelling protocol — no polling, no text.

## 8. MCP resource mechanics

`resources/list` enumerates the stable roots (§1) plus per-session dynamic entries (open tasks,
recent `out:n`). `resources/read` on a value URI returns `structuredContent` = the `$`-tagged (or
elided) value; on an events URI returns the buffered tail. `resources/templates/list` advertises
the query-parameterized forms (`shoal://journal{?since,head,limit}`,
`shoal://out/{n}{?path,slice,format}`). Every `tools/call` result that produces a value includes a
`resource_link` to its ref so the agent can drill in later for zero tokens — the tool result in
context stays a one-line render + the ref, never the payload.

**The contract, in one sentence:** the agent's context is a working set of *refs and shapes*; every
byte of actual payload is pulled on purpose, structured, from an addressable noun — or pushed,
structured, from a subscribed channel. There is no other way for data to reach the agent, by
construction.