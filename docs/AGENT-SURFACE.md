# The agent surface — wire contract v2

**Status:** implemented and live-verified end-to-end (`crates/shoal-mcp/tests/live_kernel.rs` spins
up a real `shoal-kernel` on a real socket and drives it through both the MCP facade and raw
JSON-RPC). `$`-tagged value encoding, the elision rule (§3), MCP `resources/list|read|subscribe|
unsubscribe`, `events.subscribe`/push notifications, all seven MCP tools (`shoal_exec shoal_plan
shoal_apply shoal_get shoal_journal shoal_cancel shoal_cap_request`), and the in-language
`channel()` ↔ kernel-wire-bus bridge (§7) are all real, not spec'd-but-pending.
`shoal_cap_request`'s grant response reports the SAME honest enforcement truth `session.attach`'s
`caps_enforced` does (`Kernel::caps_enforced_for`, shared by both call sites in
`crates/shoal-kernel/src/handlers_task.rs`/`lib.rs`) — it does **not** hardcode `"enforced": false`;
an earlier revision of this doc said otherwise, that line was stale (pinned by
`cap_request_reports_the_same_enforcement_truth_attach_does` and
`cap_request_reports_false_for_the_default_permissive_principal`, `shoal-kernel/src/lib.rs`).

The event bus is backpressured per §6: `crates/shoal-kernel/src/eventbus.rs` gives every
subscription its own bounded queue and a dedicated writer thread, so `publish()` is a
lock-and-append operation that never performs a blocking socket write itself — a slow or fully
stalled subscriber can delay at most its own delivery, never another subscriber's, and never the
producer. A subscriber whose queue overflows gets a coalesced `{dropped, latest_seq}` summary
instead of unbounded buffering (pinned by `eventbus::tests::publish_does_not_block_when_a_subscriber
_never_reads`, `a_stalled_subscriber_never_stalls_a_healthy_one`,
`a_stalled_subscriber_gets_a_coalesced_dropped_summary`). One consequence worth knowing: because
delivery is now off the dispatch call path, an event pushed on the SAME connection that triggered
it is no longer guaranteed to arrive before that call's own RPC response — only that it arrives
(at-least-once, per §4/§6).

The `approval` channel (§4) is wired: `exec {mode:"plan"}` publishes on it the moment a plan lands
at `Verdict::ApprovalRequired`, so a second principal (a human's session, a supervising agent)
learns about a pending approval by subscribing, not by polling `journal.query` or re-deriving the
plan (pinned by `approval_channel_fires_when_a_plan_needs_approval`). `journal` and `render` are
now wired too — `journal` fires `{entry_id, head, ok, principal}` once per finished journal entry;
`render` fires `{ref, render}` alongside every `session.transcript` announcement. `reef` does
**not** emit and has been removed from `STATIC_CHANNELS` rather than left advertised-but-dead: tool
lock/drift/fetch events originate inside `shoal-eval`'s reef resolution (a different crate), and no
natural emit point for them exists inside `shoal-kernel` itself yet — it needs an eval-side
event-forwarder hook analogous to `session.rs`'s existing `user.*` bridge.

Real gaps that remain, tracked in `docs/ROADMAP.md`'s open-items list:
- An `Outcome`'s wire `span` field is always `None` (never threaded from the spawning call;
  `OutcomeVal` carries no field for it yet — see `wire::outcome_span`'s doc comment for exactly
  what an eval-side plumb would need).
- `session.transcript`/`journal`/`approval`/`render` are ring-buffered (≥1024 events per channel)
  like every other channel, but not yet *journal-backed* for replay past that ring depth — a
  subscriber that falls behind by more than the ring cap loses those events for good; recoverable
  today only via `journal.query`'s own independent SQLite-backed history, not via
  `events.read`/`resources/read` on the channel itself.
- Per-client `it` (`Session::client_it`, the last transcript value a given connection saw) is
  tracked on every `exec` but not yet exposed through any wire method — nothing reads it back
  today.
- `approval`'s `expires` field is honestly `{"$":"null"}` always: `StoredPlan` carries no TTL/
  deadline field to report (same honest-omission precedent as `wire::outcome_span`).

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
`{"$":"time","v":"HH:MM:SS"}`; `{"$":"regex","src":…}`; `{"$":"glob","pattern":…}`;
`{"$":"range","start","end","inclusive"}`; `{"$":"list","v":[…]}`;
`{"$":"record","v":{k:…}}`; **table columnar**: `{"$":"table","cols":{name:[…],…},"n":N}`;
`{"$":"outcome","status","signal","ok","out":<tagged>,"err":str,"dur_ns","pid","cmd","span"}`;
`{"$":"error","code","msg","span","hint","stderr"}`; `{"$":"task","id","done"}`;
`{"$":"stream","label"}` (never inline payload); `{"$":"secret","name"}` (never material);
`{"$":"bytes","v":base64}` (elided per §3); `{"$":"closure","repr"}`;
`{"$":"cmd","repr"}` (alias/partial command application).

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
monotonic **per channel**; ring-buffered (≥1024 events per channel — every channel today, including
`session.transcript`/`journal`, uses the same in-memory ring; neither is yet *journal-backed* for
replay past that depth, see the status section above). Read = `shoal://events/{ch}?since={seq}`;
push = subscription (§6). At-least-once; consumers dedup by seq.

Channels (payloads are `$`-tagged):
```
session.transcript   {n, ref, summary:{type, ok?, cmd?, n?}}      every new out[n]
task.{id}            "started", then terminal {state:"completed"|"failed"|"cancelled", ref?}
journal              {entry_id, head, ok, principal}               every finished journal entry
approval             {plan_ref, effects, principal, expires}       plan awaiting approval
render               {ref, render}                                 alongside every session.transcript
user.{name}          arbitrary $-tagged value                      in-language channel(name)
```

`reef` (`{tool, event:"locked"|"drift"|"fetched", hash}`) is NOT in this list: it was previously
advertised in `STATIC_CHANNELS` with nothing ever publishing to it (a dead channel a subscriber
could wait on forever) and has been removed rather than left silently unwired — see the status
section above for why (no natural emit point inside `shoal-kernel`; the events would need to
originate in `shoal-eval`'s reef resolution).

`user.*` channels are the cross-principal primitive: a human's session and its agents signal each
other structurally (pair-shelling; no file-watching, no sentinel strings in ttys).

## 5. Tools (verbs) — MCP `tools/*` and kernel JSON-RPC

Small verb set; nouns live in §1. All results carry `structuredContent` (the `$`-tagged value or
its elided form) **and** a `resource_link` to the ref — text content is a render string only.

```
shoal_exec   {src|ast, mode:"run"|"plan", position:"stmt"|"value", background:bool,
              timeout_ms?, elide?}
  → run:  {ref, value|elided, render}          (no uri/ok/events fields today)
  → run+background: {task:"task:N", events:"task.N"}       (plain strings; never blocks context)
  → timeout hit:    {task:"task:N", events:"task.N", timed_out:true}   (command keeps running)
  → plan: {plan_ref, effects, reversibility, verdict, approval_pending}
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

**Status: backpressure implemented** (`crates/shoal-kernel/src/eventbus.rs`: a bounded per-subscriber
queue plus one dedicated writer thread per subscription — `publish()` only ever appends to the
queue, never performs the socket write itself). A slow subscriber's queue is bounded; on overflow
the kernel drops to a **coalesced summary** for that subscriber (`{dropped: n, latest_seq}`) rather
than unbounded buffering or blocking producers — the subscriber re-reads from `latest_seq`.
Liveness beats completeness on a saturated channel; completeness for ring-buffered channels is
recoverable up to the ring depth (§4), and only fully recoverable from the journal for entries the
journal itself records (i.e. `journal.query`, not yet every event channel — see the status section
above). One corollary of moving the write off the dispatch call path: an event pushed on the same
connection that triggered it is no longer ordered relative to that call's own RPC response, only
guaranteed to arrive (at-least-once).

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