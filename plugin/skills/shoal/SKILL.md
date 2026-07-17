---
name: shoal
description: Complete operating manual for driving shoal — the agent-first structured shell — over its 13-tool MCP facade, including structured exec, plans, refs, resources, subscriptions, and interactive PTYs. Load this whenever you are about to run a command in a shoal session, write `.shl` source, translate a shell idiom into shoal, or interpret a shoal MCP result.
---

# shoal — the language card

shoal is a **typed value graph over one session kernel**, not a text-stream router. You never pipe
bytes between processes and re-parse them; you get back **structured values** with **addressable refs**,
and you drill into them by field path. This card is derived from the stable Zola sources under
`site/content/docs/` and `site/content/internals/`, the 1,331-case corpus across 78 suites at
`spec/cases/*.toml`, and the current `shoal-mcp`/`shoal-proto`/`shoal-kernel` source. When prose and
the corpus disagree, the corpus is the behavioral authority.

**The one rule above all others:** never parse shoal's own rendered text. Every value you need is
already structured on the wire. Reach for `structuredContent` / `value.get` / `shoal_get`, never for
`content[0].text` or the human `render` string.

> **Current surface snapshot.** The MCP facade ships 13 tools, including six PTY tools; resources,
> templates, reads, and subscriptions are live. `resources/unsubscribe` removes the URI worker,
> closes its dedicated kernel connection, and joins the forwarding thread.
> `user.*` channels bridge in both directions; background cancellation and timeout-to-task
> conversion work; and render/text
> previews are capped at 64 KiB. Kernel autostart is on unless a non-empty
> `SHOAL_NO_AUTOSTART` disables it. Scoped child processes use the strongest available filesystem
> sandbox (Landlock on Linux, Seatbelt on macOS), while unsupported dimensions are reported rather
> than implied. Spawn-hash pinning is enforced only when the principal has opted into a non-empty
> `proc_spawn` allowlist. CAS-backed captures expose recoverable `val:blake3:…` refs.

---

## 0. How you talk to shoal

You do not have a bash tool here. You have **13 MCP tools**: seven structured-execution tools and
six interactive-PTY tools. They forward to a `shoal-kernel` over newline-delimited JSON-RPC 2.0 on
a Unix socket. MCP resources are the read/subscribe side of the same session.

The bridge probes the socket and **autostarts a detached `shoal-kernel` by default**. It reuses a
live listener and waits up to about five seconds for a new daemon. Set any non-empty
`SHOAL_NO_AUTOSTART` when a service manager or human owns kernel lifecycle. Autostart still requires
`shoal-kernel` on `PATH`; a failed best-effort spawn surfaces as the real connection error.

Every tool result comes back as an MCP `tools/call` result shaped:

```json
{"content":[{"type":"text","text":"<bounded human render, or pretty JSON when no render exists>"}],
 "structuredContent": <the same JSON value, structured>,
 "isError": false}
```

When the result is addressable, `content` also includes a `resource_link` item for its `shoal://`
URI.

**Always read `structuredContent.value`, not `render` or `content[0].text`, for data.** `content[0].text`
is a pretty-printed dump of the result for surfaces that only render text; it and the nested `render`
field are now both **size-capped at 64 KiB** with a `…(N more lines, fetch via <uri>)` truncation
marker (see §4 rule 14 — this used to be a real unbounded-wall-of-bytes gap, now closed both at the
MCP boundary and at the kernel wire layer). Headless/MCP renders have ANSI control sequences
stripped before bounding; an actual TTY attachment may preserve them. Either way, `render`/`text`
are not reliably field-addressable. Reach for `structuredContent.value`/`shoal_get` for anything
you intend to parse or branch on; treat `render`/`text` as a human-only preview.

On success, `structuredContent` is the tool's own result object. On failure (`isError: true`),
`structuredContent` is the raw JSON-RPC error object: `{"code": <int>, "message": <string>, "data": {...}}`.
`code` here is a **JSON-RPC transport code** (e.g. `-32002`), not a shoal language error code — the
shoal error code (`type_error`, `div_zero`, ...) lives at `data.code` for evaluation errors, and is
**absent** for parse errors. See §5 for the exact table; do not assume `data.code` is always present.

### 0.1 `shoal_exec` — run source, get a ref + a structured value

**Params** (from the tool's actual JSON Schema, verified in `crates/shoal-mcp/src/tools.rs`;
`additionalProperties: false`): `{src: string (required), mode?: "run"|"plan", position?:
"stmt"|"value", background?: bool, timeout_ms?: int (≥1), elide?: {max_bytes?, max_rows?,
max_items?}}`. The old dead `capture`/`timeout` params are **gone** — every field above is
forwarded to the kernel and real:

- `background: true` → the call returns immediately with `{"task": "task:<n>", "events":
  "task.<n>"}` (both plain strings); the command keeps running as a kernel task. Cancel with
  `shoal_cancel {task}`; watch via the `task.<n>` events channel (§0.8).
- `timeout_ms` does **not kill anything**: a synchronous run that outlives the deadline is
  *converted* to a background task and you get back `{"task": …, "events": …, "timed_out": true}`
  (verified in `handlers_exec.rs`) — the command is still running; treat it exactly like a
  `background:true` result.
- `elide` is the per-call elision budget (tighten/loosen §1's defaults; `max_bytes` clamps at the
  64 KiB hard cap).
- Kernel-side, the wire field for `background` is named `async` (serde alias `background` —
  `shoal-proto`'s `ExecParams`), and the kernel's `exec` additionally accepts `plan_ref` with
  `mode: "approved"` — that mode is **`plan.apply`'s re-entry, not a caller-assertable privilege**:
  the kernel verifies the named plan is approved for the calling session/principal and carries the
  same source before skipping the leash verdict, and the MCP tool neither exposes nor forwards
  `plan_ref`, so through this surface you always go `shoal_plan` → (`shoal_cap_request`) →
  `shoal_apply`.

If you omit `position`, the MCP facade defaults it to `"value"` (note: this differs from the
raw kernel's own default of `"stmt"` — the MCP default is the one that matters to you).

**What `position` actually controls** (read this carefully — it is the sharpest edge in this
surface): with `"value"`, the kernel executes every statement before the last with ordinary
statement semantics, then evaluates a final expression in value position. A non-OK external command
in that final expression is returned as an inspectable outcome (`.ok == false`) instead of raised.
An earlier non-OK command still raises. A final declaration/control statement has no special value
reading and uses normal statement evaluation. Language/builtin errors such as `div_zero` and
`index_range` raise in either position; they are errors, not failed command outcomes. With
`"stmt"`, every non-OK command raises `cmd_failed`.

```text
# Captured: the final expression is in value position.
let x = 1
sh { exit 3 }

# Raised before the final expression is reached.
sh { exit 3 }
x + 1
```

**Result** (`ExecResult`): `{"ref": "out:<n>", "value": <$-tagged wire value, elided if large>, "render": "<bounded human string>"}`.

- `ref` is a **session-scoped transcript ref** like `"out:12"` — hand this to `shoal_get` later. There
  is always an `out:<n>` at the top level. A large CAS-backed byte capture can additionally expose a
  content ref such as `val:blake3:<hash>` inside the value/render; fetch that through
  `shoal://val/blake3:<hash>` or the corresponding blob path without replaying the command.
- `value` is the real payload, `$`-tagged, elided per the rule in §1 if large.
- `render` is a human string, **size-capped at 64 KiB** with a fetch hint on overflow. Headless/MCP
  sessions have ANSI control sequences stripped before the cap is applied. It is not
  field-addressable: read `value`, not `render`, for data.
- **A raised error now DOES mint a ref** (verified in `handlers_exec.rs`): the structured error
  value is stored in the transcript at `out:<n>`, and the JSON-RPC error's `data.ref` / `data.uri`
  point at it — `shoal_get {ref: data.ref}` fetches the full `{code, msg, span, hint, stderr}`
  error value after the fact, so a failed call is no longer a dead end.

Worked example (kernel test `unix_stream_session_roundtrip`, `crates/shoal-kernel/src/lib.rs`):

```json
// call
{"name":"shoal_exec","arguments":{"src":"[1,2,3]","position":"value"}}
// structuredContent
{"ref":"out:1","value":{"$":"list","v":[{"$":"int","v":1},{"$":"int","v":2},{"$":"int","v":3}]},"render":"[1, 2, 3]"}
```

Every nested primitive is `$`-tagged too (`{"$":"int","v":1}`); expect the tag at every depth.

A command's outcome, corpus-grounded (`spec/cases/outcome.toml`, case `outcome-echo-out`, `echo hi`
evaluated at value position → `.out` is `"hi"`; rendered bare it is `outcome(status: 0, ok: true)`):

```json
{"ref":"out:2","value":{"$":"outcome","status":0,"ok":true,"signal":null,
  "out":{"$":"str","v":"hi"},"err":"","dur_ns":123456,"pid":4242,"cmd":"echo hi",
  "span":{"start":0,"end":7}},
 "render":"outcome(status: 0, ok: true)"}
```

Spawned command outcomes carry the invocation span. Outcomes synthesized without a source site
(for example a journal reconstruction or some builtin wrappers) omit `span` rather than fabricating
one.

### 0.2 `shoal_get` — drill into a transcript value without re-executing

**Params**: `{ref: string (required), path?: string, slice?: [int, int], elide?: {max_bytes?,
max_rows?, max_items?}}` (`slice` is exactly 2 integers). The `elide` budget **is now exposed and
forwarded** to the kernel's `value.get` (verified in `crates/shoal-mcp/src/tools.rs`) — tighten or
loosen per call; `max_bytes` still clamps at the 64 KiB hard cap.

**Path grammar, exactly as implemented** (`resolve_value_path` in
`crates/shoal-kernel/src/wire.rs`): dotted field names, bracketed non-negative integer indices, and
half-open bracketed ranges — `out[3]`, `rows[0].name`, `out.status`, `rows[0..5]`.
**`path` is always evaluated from the root value bound to `ref`**, never relative to whatever was
already elided — so if `.out` inside an outcome elided, you still pass `path: "out[3]"` against the
*original* `ref`, not some new sub-ref. Ranges are half-open and clamp to the collection length;
they work on lists and tables. There are **no negative indices** (`[-1]` works at the *language*
level — corpus case `list-index-negative` — but not inside a `value.get`/`shoal_get` path string,
whose indices parse as `usize`). The separate top-level `slice: [start, end]` applies after `path`:
it slices lists and tables by element, strings by Unicode scalar value, and bytes/CAS-backed bytes
by byte. A slice on an unordered or scalar value is an explicit `-32005` error.

**Result**: `{"ref": "<ref>", "value": <wire value, elided if large>}`. No `render` field here.

Worked example — drilling into an elided `ls` result (kernel test
`big_table_exec_elides_then_drills_by_path`, a real 150-file directory):

```json
// exec: {"src": "ls /some/dir/with/150/files"}  (position defaults to "value" via MCP)
// structuredContent.value:
{"$":"outcome","status":0,"ok":true,"out":
  {"$":"ref","uri":"shoal://out/2?path=out","of":"table","n":150,
   "cols":{"name":"path", "...":"..."},
   "preview":{"$":"table","cols":{"...":"first 5 rows..."},"n":5},
   "render_head":"name  ...\n(first 10 lines)"}, "...":"..."}

// follow-up: {"name":"shoal_get","arguments":{"ref":"out:2","path":"out[3]"}}
// structuredContent:
{"ref":"out:2","value":{"$":"record","v":{"name":{"$":"path","v":"f0003.txt"},"...":"..."}}}
```

Note the elided `out` field's embedded `uri` is `shoal://out/2?path=out`. This is **directly
fetchable via `resources/read`** (§0.8, DONE) — prefer that over the manual translation below, which
remains only as a fallback for the rare URI that still 404s: the part before `?path=` gives you the
short ref (`out:2`), the part after `?path=` gives you the `path` argument to pass to `shoal_get`
instead.

### 0.3 `shoal_plan` — derive effects without spawning anything

**Params**: `{src: string (required)}`. Internally forced to `mode: "plan", position: "value"` — you
cannot change those. **Result** (`PlanResult`): `{"plan_ref": "plan:<16 hex chars>", "effects": [...],
"reversibility": <see below>, "verdict": "allow"|"deny"|"approval_required", "approval_pending": bool}`.

**`reversibility` is now a real, computed signal (DONE — verified directly in source).** The kernel's
`reversibility_from_effects` (`crates/shoal-kernel/src/lib.rs`) derives it from the plan's own
concrete effects rather than trusting `shoal-leash`'s coarser signal or returning the old hard-coded
`"unknown"`: `"irreversible"` if any effect is `opaque` (T0/`sh{}` — unresolvable, so assume the
worst), `net_connect`, or `net_listen`; `"reversible"` otherwise. **`fs_delete` (from `rm`/`mv`) is
now correctly classified `"reversible"`, not `"irreversible"`** — shoal's default `rm` moves files
into a journaled trash and `mv`'s source-clearing step is journaled too, so `shoal_apply`'s effects
fully recover through the journal's undo inverses (`UndoInverse::TrashMove`/`MoveBack`/
`RestoreBytes`); a plain `sh { rm -rf ... }` is structurally `opaque` instead (never `fs_delete`) and
stays `"irreversible"`, correctly, since that path has no trash/undo record at all. **Known caveat**:
`fs_delete` carries no field distinguishing shoal's trash-backed `rm` from a hypothetical
`rm --permanent` (genuinely irreversible) — the effect type doesn't carry that distinction across the
crate boundary yet, so don't read `"reversible"` as an ironclad guarantee for every conceivable delete
path, only the default one.

Effects are `$`-free plain JSON, tagged by a `"kind"` field (from `shoal-leash`'s `Effect` enum,
`#[serde(tag="kind", rename_all="snake_case")]`): `fs_read{paths}`, `fs_write{paths}`,
`fs_delete{paths}`, `proc_spawn{bin_hash, argv0}`, `net_connect{host, port}`, `net_listen{port}`,
`env_read{names}`, `env_write{names}`, `secret_use{names}`, `session_write`, `journal_read`, `time`,
`opaque` (T0/`sh{}`'s ⊤; unresolvable effects, spawns nothing when planned). Grounded directly from
`shoal-eval`'s own test suite (`crates/shoal-eval/src/lib.rs`):

```json
// {"name":"shoal_plan","arguments":{"src":"git push origin main"}}
{"plan_ref":"plan:8f2c...","verdict":"allow","approval_pending":false,
 "reversibility":"irreversible",   // net_connect present → irreversible (see note above)
 "effects":[{"kind":"fs_read","paths":["/abs/cwd"]},
            {"kind":"net_connect","host":"origin","port":443},
            {"kind":"proc_spawn","bin_hash":"...","argv0":"git"}]}
```

With the shipped default (maximally permissive) kernel policy, `verdict` is almost always `"allow"` —
you will only see `"deny"`/`"approval_required"` if the kernel was started with a stricter
`--policy` file (§ leash, below). Planning **never** spawns anything — `sh { touch marker }` planned
produces `effects: [{"kind":"opaque"}]` and the file is never created (`shoal-eval` test
`planning_unknown_and_sh_are_opaque_and_spawn_nothing`).

### 0.4 `shoal_apply` — execute a previously derived plan

**Params**: `{plan_ref: string (required)}`. Re-runs the *exact original source* that produced that
plan, as the same principal, in the same session. The kernel validates the stored plan's
session/principal/source binding and requires either its recorded approval (auto-`allow` or
`shoal_cap_request`) or a currently `allow` policy verdict; the internal `mode: "approved"` cannot
be asserted directly to bypass those checks. **Result**: identical shape to `shoal_exec`'s
(`{ref, value, render}`). Fails with a JSON-RPC error if the `plan_ref` is unknown, belongs to a
different session/principal, or is still `approval_pending`.

### 0.5 `shoal_journal` — query what already happened

**Params** (tool schema, `additionalProperties: false`): `{since?: int, until?: int, principal?:
string, ok?: bool, effects?: string[], head?: string, limit?: int (>=1)}`.

**Every filter above is now real** (verified: the schema in `crates/shoal-mcp/src/tools.rs` exposes
all seven and forwards them verbatim; the kernel's `JournalQueryParams` has `since, until,
principal, head, ok, effects, limit`). `until` is an upper time bound (ns since epoch, filtered
kernel-side); `ok` filters by success; `effects` keeps only entries whose effect set contains
**every** listed effect kind (e.g. `["fs_write"]` — a kernel-side post-filter). The old
schema/kernel mismatch (`until`/`effects` dropped, `ok` unpassable) is fixed.

**Result**: an array of journal entries: `{id, session, principal, ts, dur_ns, cwd, src, ast,
effects, status, ok, opaque, outputs: [{kind, hash, len}]}` — one row per past `exec`. Two budget
notes: each entry carries the **full canonical AST** (`ast`), so rows are heavy — keep `limit` small
and filter server-side rather than paging everything into context. `head` compares the **first
whitespace-separated word of the raw `src`** against your string — it usefully selects command
statements (`head: "git"` matches every `git ...` invocation) but nothing structural (a `let`-headed
src has head `"let"`). This is how you answer "what actually ran" without re-executing or scraping a
transcript.

### 0.6 `shoal_cap_request` — unstick a plan awaiting approval

**Params**: `{plan_ref: string (required), effects?: array}`. `effects` **now genuinely scopes the
grant** (updated — verified in `handle_cap_request`, `crates/shoal-kernel/src/handlers_task.rs`): if
you name effect kinds (strings, or `{kind: ...}` objects), the plan is only approved when the
request covers **every** effect the plan needs; otherwise you get back `{"grant":
"approval_pending", "why": "requested effect scope does not cover the plan", "uncovered_effects":
[...]}` and the plan stays pending — an approval can never silently widen past what was asked for.
An empty/omitted `effects` approves the whole plan. **Result** on success:
`{"grant":"approved","plan_ref":"...","enforced":<bool>,"granted_effects":[...]}`. `enforced`
uses the same honest host/principal-specific truth as `session.attach`: it is true only when a real
Landlock/Seatbelt backend exists and this principal resolves to a scoped sandbox. Use this only after a
`shoal_plan`/`shoal_exec` came back `approval_required`/`approval_pending`; call `shoal_apply`
afterward to actually run it.

### 0.7 `shoal_cancel` — stop a running/background task

**Params**: `{task: string (required)}` — a task ref like `"task:7"` (verified in
`crates/shoal-mcp/src/tools.rs`'s `tools()`; forwards to the kernel's `task.cancel`,
`additionalProperties: false`).

The whole background loop is now reachable through this plugin: `shoal_exec {background: true}` (or
a `timeout_ms` conversion — §0.1) hands you `{"task": "task:<n>", "events": "task.<n>"}`; watch the
`task.<n>` channel (§0.8) for `started` and then a terminal `completed`/`failed`/`cancelled` record
carrying the result `ref`; `shoal_cancel {task}` requests cancellation. Note `task.suspend` is still
unimplemented (always `-32020`, even over raw JSON-RPC).

### 0.8 MCP resources — fetch, browse, and subscribe

`Facade::handle()` (`crates/shoal-mcp/src/lib.rs`) dispatches `resources/list`, `resources/read`, and
`resources/subscribe`, and `initialize` advertises `capabilities.resources.subscribe = true` —
confirmed against source and by the live e2e test
`crates/shoal-mcp/tests/live_kernel.rs`. Every elided value's embedded `shoal://...` uri is a live
fetch target, not a dead end. §4 rule 15 keeps the manual `shoal_get`+URI-translation fallback
documented anyway — it still works and is your escape hatch if a *particular* URI 404s.

- **`resources/list` enumerates the stable roots** `shoal://journal`, `shoal://jobs`,
  `shoal://session/cwd`, `shoal://session/env`, `shoal://session/reef`, and `shoal://pty`, plus
  this session's open tasks, stored plans, and open PTYs. It does **not** enumerate recent `out:n`
  transcript values — those are only
  fetchable by URI if you already have one (from an `ExecResult`'s elided `Ref` or a prior call),
  never discoverable by listing. Don't expect to browse your way to an arbitrary past `out:n`.
- `resources/read {uri}` on a value URI (e.g. `shoal://out/12?path=.rows[3].name`) returns
  `structuredContent` — the `$`-tagged (or further-elided) value at that path/slice, **without**
  re-executing anything. This is the primary way to drill into an elided `Ref` — prefer it over the
  §0.2/§4-rule-15 manual `ref`+`path` translation.
- `resources/templates/list` advertises parameterized transcript values, CAS values, tasks/task
  output, plans, session views, PTY screens, journal queries, and event channels.
- `resources/subscribe {uri}` on `shoal://events/{channel}` or `shoal://task/{id}[/out]` starts a push
  subscription; the server sends `notifications/resources/updated` with `{uri, seq, payload}` as
  events occur. **Never poll a resource you could instead subscribe to.**
- `resources/unsubscribe {uri}` removes that URI's worker; dropping the worker shuts down its
  dedicated kernel connection and joins the forwarding thread. Facade teardown performs the same
  cleanup for every remaining subscription.
- **The language-channel→kernel-bus bridge now works, both directions, `user.*`-scoped (fixed —
  this card previously and wrongly called this a gap).** Verified live: an in-language
  `channel("user.x").emit(v)` (evaluated inside `src`) is forwarded to the kernel's wire bus and
  **does** reach a `resources/subscribe` on `shoal://events/user.x` (`crates/shoal-kernel/src/
  session.rs`'s `set_event_forwarder`); the reverse direction also works — a wire `events.publish`
  on `user.x` is mirrored back into that session's in-language `channel("user.x")`
  (`crates/shoal-kernel/src/eventbus.rs`'s `handle_events_publish` → `lang_bus.inject`). Only `user.*`
  channels cross in either direction — kernel-owned semantic channels (`task.*`, `session.transcript`,
  `journal`, `approval`) stay kernel-only and are not writable from language code. Cross-principal
  signaling via `user.*` channels is a real, working substrate now, not a gap to route around.
- Query params on any value-bearing URI: `?path=<fieldpath>&slice=<a>..<b>&format=json|render|raw`.
  Paths accept fields, non-negative indices, and half-open `[a..b]` ranges; the separate `slice`
  query is also half-open. Negative indices are not accepted here because both forms ultimately use
  unsigned bounds.

Resources are your preferred path for drilling into elided values and for subscribing to
`task.{id}`/`session.transcript`/`journal`/`user.*` channels instead of re-calling
`shoal_journal`/`shoal_get` in a loop — polling a tool result is always wrong here.

### 0.9 Interactive PTYs — real terminal programs without raw escape-byte walls

`shoal_exec` is a headless structured evaluation path. Use these six tools when the program's
terminal behavior is the task: an editor, installer, debugger, REPL, password prompt, or full-screen
TUI.

1. `shoal_pty_open {cmd, args?, cols?, rows?, env?}` starts a real PTY in the session cwd and
   environment, layering any string-valued `env` overrides. Dimensions default to 80×24 and are
   clamped to 1…1000. The result is `{pty_id, pid, cols, rows, cmd}`.
2. `shoal_pty_read {pty_id}` returns `{screen, cursor, changed, alive, exit, ...}`. `screen` is an
   array of rendered text rows bounded by the terminal grid. It never contains a raw ANSI stream.
3. `shoal_pty_send {pty_id, input}` accepts a literal string, an object with one of `key`, `text`, or
   base64 `bytes`, or an array mixing those forms. Named keys include Enter, Tab, Escape, Backspace,
   Delete, arrows, Home/End, PageUp/PageDown, F1–F12, and `Ctrl-<letter>`.
4. `shoal_pty_resize {pty_id, cols, rows}` updates the child window size and emulator grid.
5. `shoal_pty_list {}` recovers this session's open PTYs without returning every screen. The same
   list is readable at `shoal://pty`; `shoal://pty/{id}` reads one screen.
6. `shoal_pty_close {pty_id}` terminates and reaps the child. Always close a PTY you no longer need.

A good edit workflow is:

```json
{"name":"shoal_pty_open","arguments":{"cmd":"vim","args":["note.txt"],"cols":100,"rows":30}}
{"name":"shoal_pty_send","arguments":{"pty_id":"pty:1","input":["i","hello",{"key":"Escape"},":wq",{"key":"Enter"}]}}
{"name":"shoal_pty_read","arguments":{"pty_id":"pty:1"}}
{"name":"shoal_pty_close","arguments":{"pty_id":"pty:1"}}
```

PTY IDs are session-scoped; another session sees an opaque unknown-PTY error. `pty.open` goes
through the same conditional spawn-pin gate and Leash filesystem sandbox lowering as other external
spawns. It is not a bypass around approval or confinement.

---

## 1. The 60-second model

- **Everything is a typed value.** Numbers, strings, lists, records, tables, paths, durations,
  sizes, outcomes, errors — every type in the stable language contract renders unambiguously and never
  degrades to "just text." A `table` *is* `list<record>`, structurally.
- **Composition is the dot-chain, not the pipe.** `ls.where(.size > 1mb).map(.name)` — no `|`
  anywhere, ever, outside `sh { }` or a `match` alternation pattern.
- **Commands are values too.** Running `git status` produces an `outcome` value
  (`{status, ok, out, err, dur, pid, cmd}`); an unknown field/method on an outcome forwards to
  `.out`, so `git_log.subject` reads a field of the *parsed* log row, not a string you'd need to
  regex. (Qualification, verified against the binary: a few builtins return a **bare value**
  instead of an outcome — `pwd` yields a `path` directly. And an outcome's stderr accessor is
  **`bytes`**, not `str` — `.str()` it first.)
- **`fn` IS a command.** `fn deploy(env: str, dry: bool = false) { ... }` is immediately callable as
  `deploy staging --dry` — no separate "make this a CLI" step.
- **No ambient ("invisible") state.** `cwd`/`env` are explicit session state, mutated only at session
  top level (never inside a `fn` body) or scoped dynamically with `with cwd:`/`with env: { ... }`,
  which always restores on exit — including through an error.
- **No truthiness, ever.** `if`/`&&`/`||` accept only `bool` or a command `outcome` (success = true).
  Everything else in a condition position is a `type_error`.
- **Every exec result gets a transcript ref.** Large values arrive elided (shape + small preview +
  fetch URI); you fetch more with `shoal_get`, surgically, never by re-running the command. Plan,
  journal, task, and PTY control results use their own documented identities/shapes.

---

## 2. Translating from bash

Every method named below is pinned by the intercrate and language contracts plus the central method registry.
Rows marked **(corpus)** have a direct, exact `spec/cases/*.toml` example — check the named case
yourself if you want the ground truth. Unmarked rows use a pinned-but-not-individually-corpus-exercised
method; treat the signature as authoritative per site/content/internals/intercrate-protocol-contracts.md but verify empirically if a call surprises you.

| bash | shoal | why / grounding |
|---|---|---|
| `ls \| grep x` | `ls.where(.name.contains("x"))` | `\|` is a hard parse error with a teaching message, **verified against the binary**: *"shoal has no pipe operator"*, hint *"data composes with `.` (try `ls.where(.size > 1mb)`); raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`"* (site/content/internals/language-conformance-contract.md; **corpus** `literals.toml:parse-pipe-teaching`). The same curated error now fires in **infix EXPR positions** too — `1 \| 2` and `let c = a \| b` teach identically (verified), not just command-position pipes. |
| `grep ERROR file` | `path("file").read.lines().where(.contains("ERROR"))` | `.read` reads the file as a `str` (a field-reachable path accessor, site/content/internals/intercrate-protocol-contracts.md — there is **no `.read_str()`**; that spelling is `field_missing`, verified against the binary). `.lines()` **(corpus** `strings.toml:str-lines-strips-crlf`**)**; substring test via `in` is **(corpus** `operators.toml:op-in-string-substring`, `"ell" in "hello"` → `true`**)** — prefer `"ERROR" in line` over `.contains` if you want a corpus-nailed-down spelling. |
| `$VAR`, `$HOME` | `env.VAR` | `$` is illegal everywhere: *"shoal variables have no sigil"* (site/content/internals/language-conformance-contract.md; **corpus** `core.toml:parse-dollar`, `src="$HOME"` → `parse_error`, "no sigil"). Reading: `env.NAME` or `(env NAME).out`; writing at session top level: `env.NAME = "v"` (**corpus** `reef.toml:reef-env-assign-writes-session-env-for-a-child`). |
| `$(cmd)` command substitution | `(cmd)` | CMD grammar's `arg = ... \| "(" expr ")"` — a full EXPR embeds as one word/argument; no special substitution syntax needed. A parenthesized command used as a value: **(corpus** `outcome.toml:outcome-echo-out`, `(echo hi).out` → `"hi"`**)**. |
| `` `cmd` `` backticks | `(cmd)` or `sh { cmd }` | Backtick is illegal, error points at `sh { }`/`re"..."`/`t"..."` (site/content/internals/language-conformance-contract.md; **corpus** `core.toml:parse-backtick`). |
| `*.txt` glob | `*.txt` (bare, CMD position) or `glob("*.txt")` | Word containing unquoted `*`/`?`/`[...]`/`**` lexes as a `glob` literal; expansion happens at the callee, never at the shell (site/content/internals/language-conformance-contract.md). Explicit constructor **(corpus** `literals.toml:lit-glob-constructor-render`, `glob("*.rs")` renders `*.rs`**)**; unexpanded pattern bound to a `glob`-typed param **(corpus** `coercion.toml:word-bind-glob-not-expanded`**)**. |
| glob matches nothing | (silently an empty list) | Nullglob **by construction** — never a literal `*` string (site/content/internals/language-conformance-contract.md); a statement-level lint additionally flags a glob that matched nothing. |
| `find . -name '*.rs' -size +1M` | `ls.where(.size > 1mb)` | This exact phrase is site/content/internals/language-conformance-contract.md's own canonical pipe-replacement example — the size unit is a first-class literal, not a flag to parse (`1mb`, **corpus** `literals.toml:lit-size-mb-frac`). |
| `cmd > file`, `cmd >> file` | `cmd > file`, `cmd >> file` (kept!) | Muscle-memory sugar, CMD-mode only, desugars to `.save(file)`/`.append(file)` on stdout bytes (site/content/internals/language-conformance-contract.md). The **modern, canonical** form is calling `.save`/`.append` directly: `(cmd).save(file)`. |
| `cmd < file` | `cmd < file` (kept) | Sole stdin sugar; desugars to `StdinSpec::File` directly (site/content/internals/values-streams-execution.md). No numeric variant, no here-string variant. |
| `cmd <<EOF ... EOF` (heredoc) | **forbidden, permanently** — use an interpreter block | Curated parse error, **verified against the binary**: *"shoal has no heredocs"*, hint *"feed a string or multiline literal instead: `value.feed(cmd)`, or use an interpreter block: `python { … }`"* (site/content/internals/values-streams-execution.md). **Interpreter blocks are IMPLEMENTED and this is the answer**: `python { import json; print(json.dumps(...)) }.out` runs the program and auto-parses its stdout to a structured value; `sh { ... }` (site/content/internals/language-conformance-contract.md) and a multiline `"""..."""` literal also work. |
| `cmd <<< "text"` (here-string) | `"text".feed(cmd args…)` (works) | Curated parse error, **verified against the binary**: *"shoal has no here-strings"*, hint *"feed the value instead: `"text".feed(cmd)`"* (site/content/internals/values-streams-execution.md). `.feed` IS implemented, args and all: `"text".feed(grep "foo").out`, `"text".feed(sort -r).out`. Blocks also work: `"text".feed(sh { grep foo })` / `.feed(jq { … })`. |
| `cmd 2>file`, `cmd 2>&1`, `cmd &>file` | **forbidden** | Curated parse errors, **verified against the binary**. Glued fd forms (`2>file`, `2>&1`): *"shoal has no fd-numbered redirects"*, hint *"stderr is structured — `(cmd).stderr`, or `try { cmd } catch e { e.stderr }`; a statement-position PTY run already merges the streams"*. `&>file`: *"shoal has no stream-merging redirect"*, hint *"capture is structured: `(cmd).out` / `(cmd).stderr`; a statement-position PTY run already merges the streams"*. `.stderr` is **`bytes`** — `.str()` it before string methods. A live PTY run (statement position) already merges stdout/stderr by construction — honest PTY semantics, not a missing flag. |
| `cmd1 \| cmd2` raw byte plumbing | `value.feed(cmd args…)` / `cmd.feed(value)` | The one asylum the pipe error names for genuine byte plumbing. **IMPLEMENTED, including args/flags**: `["b","a","c"].feed(sort -r).out`, `data.feed(grep "foo").out`, `{a:1}.feed(jq ".a").out`. The inverted `cmd.feed(value)` form works too. Interpreter/`sh` blocks are also valid feed targets: `.feed(sh { sort -r })`, `.feed(jq { .a })`. |
| `cmd1 && cmd2`, `cmd1 \|\| cmd2` | kept, unchanged | `&&`/`||` operate on `bool` or command **outcomes** (success = true), short-circuiting, returning the deciding operand *verbatim* — not force-cast to `bool` (**corpus** `outcome.toml:outcome-and-chain-both-outcomes`, `outcome-and-bool-then-outcome`; CMD-mode chaining needs `^` when the head is a reserved word: **corpus** `operators.toml:op-cmd-and-and-runs-both-on-success`, `^true && ^true`). |
| `cmd &` (background) | `cmd &` (kept) | Desugars to `spawn { cmd }`, prints a task handle (site/content/internals/language-conformance-contract.md). Over MCP, `shoal_exec` now exposes `background`/`timeout_ms` (verified in `tools()`'s schema — §0.1), and `shoal_cancel` (§0.7) stops a task once you have its ref. |
| `for f in *.txt; do ...; done` | `for f in glob("*.txt") { ... }` or `glob("*.txt").each(f => ...)` | `for` binds a pattern over any iterable (EBNF `"for" pattern "in" expr block`); basic range form is **(corpus** `closures.toml:for-loop-break-stops-early`, `core.toml:for-range-sum`**)**. |
| `while [ cond ]; do ...; done` | `while cond { ... }` | Direct — **(corpus** `core.toml:while-basic`**)**. `cond` must be `bool`/outcome, never a bare list/string (no truthiness). |
| `if [ -n "$x" ]; then ... fi` (truthiness) | `if x.is_empty() { } else { }` / `if x != null { }` | No truthiness anywhere: `if [1] { 1 }` is `type_error`, "no truthiness" (site/content/internals/language-conformance-contract.md; **corpus** `core.toml:no-truthiness`). `.is_empty()` **(corpus** `core.toml:method-is-empty`**)**; `.is_some()`/`!= null` are named in site/content/internals/language-conformance-contract.md for nullable values (not individually corpus-exercised). |
| `grep`/regex extraction | `.matches(re"...")`, `.match(re"...")` | **(corpus** `strings.toml:str-matches-regex-all-occurrences`, `str-match-regex-first-occurrence`**)** — a `regex` is a tagged literal, `re"[0-9]+"`, compiled once. |
| `awk '{print $1}'` (field split) | `.words()[0]` (whitespace) or `.split(",")[i]` (delimiter) | `.words()` splits on whitespace **(corpus** `strings.toml:str-words-splits-on-whitespace`**)**; `.split(sep)` on an explicit delimiter **(corpus** `strings.toml:str-split-on-separator`**)**. |
| `sed 's/foo/bar/g'` | `.replace("foo", "bar")` or `.replace(re"f.o", "bar")` | Replaces **all** occurrences **(corpus** `strings.toml:str-replace-all-occurrences`**)**; the pattern may be a literal `str` OR a `regex` (`$1`/`$name` in the replacement expand capture groups) **(corpus** `strings-methods-2.toml:str2-replace-regex-*`**)**. No first-occurrence-only variant; slice/index manually for that. |
| `sed -E 's/(a)(b)/\2\1/'` (regex capture) | `.replace(re"(a)(b)", "$2$1")` | Capture-group refs use `$1`/`$name`, per the `regex` crate **(corpus** `str2-replace-regex-capture-groups`**)**. |
| `${str:0:7}` (substring) | `str.take(7)`, `str.skip(3)`, `str.skip(2).take(3)` | `.take`/`.skip` slice a `str` **by char** into a substring (not just collections), so fixed-width fields read cleanly — `line.take(7)` is a git short hash **(corpus** `strings-methods-2.toml:str2-take-slices-by-char`, `str2-take-skip-compose-for-substring`**)**. |
| `cut -d, -f1` | `row.split(",")[0]` or `table.map(r => r.split(",")[0])` | Same `.split` grounding as above. |
| `sort` | `.sort()` (plain) / `.sort_by(f)` (key function) | `.sort_by` is **(corpus** `collections.toml:list-sort-by-key-function`, sorts by `.len()`**)**; plain `.sort()` is pinned in site/content/internals/intercrate-protocol-contracts.md but not individually corpus-exercised. |
| `uniq` | `.uniq()` | Preserves **first-occurrence order**, not a sorted dedup **(corpus** `collections.toml:list-uniq-preserves-first-occurrence-order`, `[3,1,3,2,1].uniq()` → `[3, 1, 2]`**)**. |
| `wc -l`, `wc -c` | `.lines().len()`, `.len()` | **(corpus** `core.toml:method-len`, `strings.toml:str-len-counts-chars`**)**. |
| `awk '{s+=$1} END{print s}'` (fold) | `.reduce(0, (acc, x) => acc + x)` (alias `.fold`) | Left fold — the general aggregation escape hatch when no named op (`.sum`/`.min`/`.max`/`.group`) fits; empty list returns the init **(corpus** `list-methods-3.toml:lm3-reduce-*`**)**. |
| `awk '{a[$1]++} END{for (k in a) print k, a[k]}'` (group-by) | `.group(keyfn)` | Returns a **table whose rows are shaped `{key, values}`** — **not** `{items}`/`{rows}`/`{group}`. Verified against the binary: `[1,2,3,4].group(x => x % 2)` renders a two-row table with columns `key`/`values` (`{key: 1, values: [1, 3]}`, `{key: 0, values: [2, 4]}`); `g.map(.key)` → `[1, 0]`, `g.map(.values)` → `[[1, 3], [2, 4]]`. Guessing `.items`/`.rows` on a row (or the table) is a **silent-looking but loud** `field_missing` — don't guess the field name, it's `key`/`values`. |
| `jq '. + {c:3}'` / build an object | `{a:1}.set("c", 3)`, `r.merge(other)` | Records are immutable values: `.set(k, v)` inserts/replaces one key (keeping position), `.merge(other)` layers `other`'s keys over the receiver (right wins). No `{...spread}` grammar and `+` on records is a `type_error` — use these **(corpus** `record-table-methods-2.toml:rt2-set-*`, `rt2-merge-*`**)**. Build from pairs: `pairs.reduce({}, (acc, kv) => acc.set(kv[0], kv[1]))`. |
| `printf '%.2f' x` (round) | `x.round(2)`, `x.floor(2)`, `x.ceil(2)` | Round a `float` to N decimals (N optional, default 0 → nearest integer); ints pass through **(corpus** `numbers-more.toml:num-round-two-decimals`**)**. |
| `$(( x + 1 ))` / str↔int | `"42".parse_int()` (str→int); `"{n}"` (int→str) | `.parse_int`/`.parse_float` are pinned in site/content/internals/intercrate-protocol-contracts.md; int→str is plain interpolation — no cast syntax. Verified against the binary: `"42".parse_int()` → `42`; `let n = 7; "{n}"` → `"7"`. |
| `find . -type f` | `glob("**/*")` or `ls` (non-recursive) | `ls` is a builtin returning a `table` (list<record>) **(corpus** `collections.toml:table-ls-len-counts-entries`, `table-ls-where-type-then-map-names`**)**; `**` recurses, dotfiles excluded unless the pattern starts with `.` (site/content/internals/language-conformance-contract.md). |
| `xargs` | `.each(f)` | **(corpus** `collections.toml:list-each-side-effect-then-void`**)**. For "read lines from a file, run a command per line": `path("list.txt").read.lines().each(f => rm f)` (chains `.read`→`.lines()`→`.each`, all individually grounded methods). |
| `which cmd` | `which cmd` (kept, richer) | Not forensics — returns a full resolution-chain **record**, not just a path. `.name` always echoes the query **(corpus** `reef.toml:reef-which-name-field-echoes-query`**)**; unresolved tool's `.out` is `null`, not an error **(corpus** `reef-which-unresolved-tool-out-is-null`**)**; exactly one tool name — `which "a" "b"` is `arg_error` **(corpus** `reef-which-arity-error`**)**. |
| `cd dir` (permanent) | `cd dir` at session top level | Legal and journaled at session top level; **illegal inside a `fn` body** — error names `with cwd:` as the fix **(corpus** `reef.toml:reef-cd-inside-fn-body-is-illegal`, error `custom`, contains `"with cwd:"`**)**. |
| `(cd dir && cmd)` (scoped cd) | `with cwd: "dir" { cmd }` | Restores cwd on **any** exit path, including an error thrown inside the block **(corpus** `reef.toml:reef-cwd-restores-after-with-block`, `reef-cwd-restores-after-error-inside-with-block`, `reef-cwd-nested-with-blocks-restore-outer`**)**. |
| `cd -` (OLDPWD) | `cd -` (kept) | Round-trips to the previous cwd via a session-scoped `OLDPWD`, same top-level-only rule as `cd dir`; erroring `custom` with `"OLDPWD"` in the message if nothing has been recorded yet **(corpus** `dir-stack.toml:cd-dash-round-trips-to-previous-dir`, `cd-dash-without-oldpwd-errors`**)**. |
| `pushd dir` / `popd` / `dirs` | same names, kept | A session-scoped directory stack: `pushd dir` cds and pushes (no-arg `pushd` swaps the top two instead), `popd` pops and cds there (`custom` error, `"empty"`, on an empty stack), `dirs` returns the stack as a `list<path>` with the **current dir first** — all top-level-only, same `fn`-body restriction as `cd` **(corpus** `dir-stack.toml:pushd-deepens-the-stack`, `pushd-popd-round-trips-to-origin`, `popd-on-empty-stack-errors`, `pushd-no-arg-swaps-top-two`, `pushd-inside-fn-body-is-illegal`**)**. |
| `FOO=bar cmd` (scoped env) | `FOO=bar cmd` (kept) or `with env: {FOO: "bar"} { cmd }` | Leading `IDENT=word` desugars to `with env: {NAME: "value"} { cmd }` (site/content/internals/language-conformance-contract.md); explicit block form restores after **(corpus** `reef.toml:reef-env-with-block-sets-var-during`, `reef-env-with-block-restores-after`**)**. |
| `test -f file`, `[ -f file ]` | `path("file").exists` / `.is_file` / `.is_dir` | Zero-arg `path` accessors, field-reachable (site/content/internals/intercrate-protocol-contracts.md's path-accessor list: `.read .read_bytes .lines .exists .is_dir .is_file .size .modified`). Verified against the binary: `path("Cargo.toml").exists` → `true`. |
| `docker-compose up` (hyphenated command) | `^docker-compose up` or `run("docker-compose", "up")` | Hyphenated identifiers don't lex in EXPR mode; `^` forces CMD parsing and bypasses non-callable shadows **and adapters**, reaching external/reef resolution. Session functions and aliases remain callable. `run` is the fully dynamic alternative. |
| `alias ll='ls -la'` | `alias gs = git status` | AST-level partial application: `gs extra` appends arguments to the stored call node, never text-splices. Positional and flag forwarding are corpus-pinned in `desugar.toml` and `desugar-more.toml`; aliases remain callable even as `^gs`. |
| undo the last safe mutation | `undo` / `undo 12` / REPL-only `undo out[-1]` | The evaluator replays typed journal inverses for trash-backed removal, overwrite restoration, and moves, refusing stale fingerprints. Bare `undo` selects the newest reversible entry; `undo <id>` is host-independent. Only the interactive REPL knows the `out[n]`→journal-entry map and rewrites literal `undo out[n]`. |

### Format & system namespaces

Eight namespaces live as names in the root env (`crates/shoal-eval/src/namespaces.rs`): `json`,
`yaml`, `toml`, `csv`, `math`, `os`, `http`, `config`. Every call below was **verified directly
against the binary** unless marked otherwise.

- **`json` / `yaml` / `toml` / `csv`** — each has `.parse(str)` and `.stringify(value)`:
  `json.parse("[1,2]")` → `[1, 2]`; `json.stringify({a:1})` → `'{"a":1}'`; `yaml.parse("a: 1")` →
  `{a: 1}`; `toml.parse(path("Cargo.toml").read)` → a record you drill with field access;
  `csv.parse("a,b\n1,2")` → a **table** (drive it with `.where`/`.map` — indexing a table with
  `[0]` is a `type_error`); `csv.stringify([{a:1,b:2}])` → `"a,b\n1,2\n"`. This replaces most
  `jq`/`yq` shell-outs.
- **`math`** — functions take/return floats: `math.sqrt(144)` → `12`, plus `cbrt sin cos tan
  asin acos atan atan2 ln log10 log2 log exp floor ceil round trunc abs sign pow min max hypot
  clamp`. Constants are plain field reads: `math.pi`, `math.e`, `math.tau`, `math.inf`,
  `math.nan`, `math.sqrt2`.
- **`os`** — nullary accessors (passing any arg is `arg_error`): `os.platform()` → `"linux"`,
  `os.arch()` → `"x86_64"`, `os.env()` → the environment as a record (`os.env().HOME`), plus
  `os.pid() os.hostname() os.username() os.cpus() os.uptime()`.
- **`http`** — `http.get(url)` / `http.delete(url)` (no body) and `http.post(url, body)` /
  `http.put(url, body)`; non-2xx statuses come back as values, not raises (*surface read from
  source, not exercised live in this pass — it does real network IO*).
- **`config`** — reads the project's `shoal.toml`: `config.all()` for the whole record,
  `config.get("key")`, or plain field projection `config.<key>`.

---

## 3. The complete syntax

### 3.1 Lexical structure (site/content/internals/language-conformance-contract.md)

Source is UTF-8. The lexer is **modal**, switching between `CMD` mode (command word soup) and `EXPR`
mode (conventional tokens) based purely on **grammar position**, never runtime state.

- **Comments**: `#` starts a comment only at token start (after whitespace/line-start/opening
  delimiter); `ver#2` in CMD mode is one word, not `ver` + a comment.
- **Terminators**: newline or `;`. A statement continues across a newline when the line ends with a
  binary operator, `,`, or an unclosed `( [ {`; when the *next* line starts with `.` (chain
  continuation) or `catch`/`else`; or after a trailing `\`.
- **Strings**: `"..."` interpolating (`{expr}` embeds any expression; escapes `\n \t \r \0 \\ \" \{
  \} \u{1F980}`); `'...'` raw, zero escapes, cannot contain `'`. Triple forms `"""..."""`/`'''...'''`
  are multiline with common-leading-whitespace stripped (**corpus** `strings.toml:str-triple-double-dedent`,
  `str-triple-raw-dedent`). Interpolation nests: `"answer {6 * 7}"` → `"answer 42"` (**corpus**
  `core.toml:string-interp`); `\{ \}` escape braces to suppress interpolation entirely (**corpus**
  `strings.toml:str-escape-braces-suppress-interp`, `"a\{b\}"` → `"a{b}"`).
- **Numbers**: `123`, `1_000_000`, `0xFF`, `0o755`, `0b1010`, `3.14`, `1e9` — all **(corpus**
  `literals.toml`**)**. Maximal munch binds a trailing unit into a *single* literal:
  - **size**: decimal `b kb mb gb tb`, binary `kib mib gib tib` — e.g. `1kb`, `4kib` (**corpus**
    `lit-size-kb`, `lit-size-kib-binary-renders-decimal`). All size units render in **decimal** form
    in v1 even when constructed with a binary suffix: `1kib` → `1.02kb`, `4mib` → `4.19mb` (**corpus**
    `lit-size-kib-4096`, `lit-size-mib-frac-renders-decimal`).
  - **duration**: `ns us ms s m h d w` — e.g. `250ms`, `30d` → renders `4w2d` (**corpus**
    `lit-duration-weeks`), `1.5h` → `1h30m` (**corpus** `lit-duration-frac-hour`).
  - **time**: `10:00am`, `23:15`, `10:30:15pm` lex as one time literal, always rendering 24h,
    zero-padded (**corpus** `literals.toml:lit-time-*`).
- **Tagged literals**: `re"..."` compiles a `regex` value, raw semantics inside (**corpus**
  `lit-regex-render`, `lit-regex-render-escaped-dot` — the backslash survives verbatim). `t"..."` is
  the **only** spelling for an absolute date/datetime — `t"2026-07-09T14:00Z"` (**corpus**
  `lit-datetime-render`).
- **Reserved words**: `let var fn alias use export return break continue if else match for in while
  try catch true false null`.
- **Illegal everywhere**, with curated diagnostics: a lone `|` outside `match` alternation/`sh{}`
  (**corpus** `core.toml:parse-pipe-teaching`); `$` (**corpus** `parse-dollar`); backtick (**corpus**
  `parse-backtick`).

**CMD-mode word shapes** (site/content/internals/language-conformance-contract.md): a *word* begins `~/`, `./`, `../`, `/` → **path** literal
(bytes-backed, `~` expands now); contains unquoted `* ? [...] **` → **glob** literal; matches
`--ident(=...)?` or `-[A-Za-z0-9]+` → **flag**; is `IDENT=rest` at head position → **env-prefix**;
otherwise → **bare word**, type `str`. `(expr)` embeds a full EXPR expression as one argument. `>`
`>>` `<` are redirects; `&&`/`||` chain; trailing `&` backgrounds; a trailing `{` opens a thunk (a
literal-brace *argument* must be quoted).

**EXPR-mode**: conventional identifiers `[A-Za-z_][A-Za-z0-9_]*` — **no hyphens**. `-` is always
minus. Bare paths/globs don't lex in EXPR mode — use `path("...")`/`glob("...")` constructors or
string coercion.

### 3.2 The two-mode statement dispatch (site/content/internals/language-conformance-contract.md) — read this before writing any multi-line script

For each **statement**, look at the first token:

1. A **reserved word** → parse that construct (`let`, `if`, `for`, `fn`, ...).
2. A **non-identifier** (literal, `(`, `[`, `{`, `-`, `!`, a leading `.` continuation) → EXPR
   statement.
3. An **identifier `X`**. Peek one token:
   - next is `=`/a compound-assign → **assignment** (`X` must be a `var`).
   - `X` is a bound variable in lexical scope → EXPR statement; the rest of the line lexes EXPR
     (`x - 1` is subtraction). A stray bare word right after a variable is a parse error hinting
     `^x` for the command reading.
   - otherwise → **COMMAND statement**; the rest of the line lexes CMD. `X` resolves: session
     `fn`/`alias` → builtin/adapter → reef/external executable; unresolved = command-not-found with
     unified did-you-mean.
   - refinement: `X` immediately followed by `.` then an identifier (no whitespace) → EXPR statement,
     invoke-then-chain desugar: `ls.where(...)` ≡ `ls().where(...)`.
4. Escape hatches: `^X ...` forces CMD parsing, bypasses a non-callable `let`/`var` shadow, and
   bypasses adapter dispatch so the external/reef-resolved command receives raw argv. A session
   function or alias named `X` remains callable even when careted. `run("name", args...)` is the
   fully dynamic form. Shadowing a resolvable command is legal and linted, never fatal.

### 3.3 Grammar reference (normative EBNF, site/content/internals/language-conformance-contract.md)

```ebnf
statement   = decl | ctrl | command | expr ;
decl        = ("let" | "var") pattern [":" type] "=" expr
            | "fn" IDENT "(" [params] ")" ["->" type] block
            | "alias" IDENT "=" command
            | "use" mod_path | "export" decl ;
ctrl        = "return" [expr] | "break" | "continue"
            | "for" pattern "in" expr block | "while" expr block ;
command     = { ENVPREFIX } head { arg } { redirect } ["&"] [trailing] ;
expr        = assign ;
assign      = lvalue ("=" | "+=" | "-=" | "*=" | "/=") assign | coalesce ;
coalesce    = orx { "??" orx } ; orx = andx { "||" andx } ; andx = cmp { "&&" cmp } ;
cmp         = rng { ("=="|"!="|"<"|"<="|">"|">="|"in") rng } ;   (* non-assoc: no chaining *)
rng         = add [ (".." | "..=") add ] ; add = mul { ("+"|"-") mul } ;
mul         = unary { ("*"|"/"|"%") unary } ; unary = ("!" | "-") unary | postfix ;
postfix     = primary { "." IDENT [call] [trailing] | "?." IDENT [call] | "[" expr "]" | call [trailing] } ;
primary     = literal | IDENT | "(" expr ")" | list | rec_or_blk
            | lambda | ifx | matchx | tryx | "sh" RAWBLOCK | "spawn" block ;
matchx      = "match" expr "{" { arm TERM } "}" ; arm = pat { "|" pat } ["if" expr] "=>" (expr | block) ;
pat         = literal | rangepat | "_" | IDENT | type IDENT | "{" fieldpats "}" | "[" listpats "]" ;
tryx        = "try" block "catch" [pat] block ;
```

**Precedence, tight → loose**: `. ?. [] ()` → unary `! -` → `* / %` → `+ -` → `.. ..=` →
`== != < <= > >= in` → `&&` → `||` → `??` → `catch` (postfix) → `=`. **Comparisons do not chain** —
`1 < 2 < 3` is a parse error with a fix-it (**corpus** `core.toml:parse-comparison-chain`, `operators.toml:op-cmp-chain-le-lt-is-error`,
message contains "do not chain").

### 3.4 Desugaring table (site/content/internals/language-conformance-contract.md — what you write vs. what actually runs)

| Sugar | Canonical |
|---|---|
| `git push origin main` | `call(cmd:"git", [w"push", w"origin", w"main"])` |
| `NAME=v cmd ...` | `with(env: {NAME: "v"}) { call(...) }` |
| `cmd ... &` | `spawn { call(...) }` |
| `cmd ... > f` / `>> f` / `< f` | `.save(f)` / `.append(f)` / stdin-from-file |
| `f(a) { ... }` | `f(a, () => { ... })` |
| `.field <op> e` (arg position) | `x => x.field <op> e` |
| `.method(args)` (arg position) | `x => x.method(args)` |
| `IDENT.foo` where `IDENT` resolves to a command | `IDENT().foo` (invoke-then-chain) |
| `e catch h` | `try { e } catch { h }` |
| `x?.f` | `if x == null { null } else { x.f }` (**corpus** `operators.toml:op-safenav-null-short-circuits`, `op-safenav-nonnull-accesses-field`) |

### 3.5 Types (site/content/internals/language-conformance-contract.md)

`null bool int(i64) float(f64) str path glob regex size(u64 bytes) duration(i64 ns) datetime time
bytes list<T> record table stream<T> error outcome task plan cmd secret`.

- **`path`** is bytes-backed (`OsString`) — `path → str` is fallible (`.str()` errors on invalid
  UTF-8; `.display()` is lossy-with-replacement).
- **`secret`** is opaque: renders `secret(NAME)`, cannot be interpolated into a `str` (type error),
  injected by the kernel at spawn time — only its *name* ever reaches the journal or the wire.
- **`outcome`**: `{status, ok, out, err, dur, pid, cmd}`. `.out` is structurally parsed lazily.
  **Unknown field/method access on an outcome forwards to `.out`** — a real subprocess outcome
  auto-upgrades `[`/`{`-shaped stdout to a structured list/record; a **builtin's** outcome (like
  `echo`) does *not* re-parse its own bytes — `.out` is the builtin's own `Value` verbatim, so
  `(echo '[1,2,3]').out` stays the **string** `"[1,2,3]"`, not a list (**corpus**
  `outcome.toml:outcome-echo-out-json-list`). Don't assume every outcome's `.out` structurally parses
  — it depends on whether the producer was a builtin or an adapter-backed external command. The
  stderr accessors (`.err`/`.stderr`) are **`bytes`**, not `str` (verified: `.lines()` on one is
  `type_error: expected str, found bytes` — call `.str()` first).
- **`table`** is `list<record>` semantically — every table method is also a list method.
- Equality is **structural** for data types, **identity** for `task`/`stream`; comparing streams is
  an error.

### 3.6 Coercion — the whole matrix (site/content/internals/language-conformance-contract.md), corpus-verified exactly

There are exactly **two** coercion sites. Everything else is a `type_error`.

**Site 1 — arithmetic promotion** (`+ - * / %`):

| Operands | Result | Notes / corpus |
|---|---|---|
| `int` ⊕ `float` (either order, all 4 ops) | `float` | `coercion.toml:coerce-*-int`/`coerce-*-float` — e.g. `0.5 + 2` → `2.5` |
| `size ± size` | `size` | `1.5kb` from `2kb - 500b`; **negative result is `type_error`, hint "negative"** (`coerce-size-minus-size-negative-is-error`) |
| `size / size` | `float` (ratio) | `10kb / 4kb` → `2.5` |
| `size * int`, `int * size`, `size / int` | `size` | `size / int` is fractional, not truncating: `10kb / 3` → `3.33kb` (`coerce-size-div-int-truncates`) |
| `size * float`, `float * size` | `size` | `2kb * 1.5` → `3kb` — **but** |
| `size / float` | **`type_error`** | asymmetric on purpose — multiplying by a float is fine, dividing by one is not (`coerce-size-div-float-is-error`) |
| `size ± int` (bare, either order) | **`type_error`** | must be `size ± size`; `1b + 1` errors (`coerce-size-plus-int-is-error`) |
| `size * (negative int)`, `size / (negative int)` | **`type_error`**, "negative" | `10kb * -1`, `10kb / -1` |
| `size / 0`, `size / 0b` | **`div_zero`** | not `type_error` — zero division is its own code |
| `size + duration` | **`type_error`** | no cross-unit arithmetic |
| `duration ± duration` | `duration` | `2h - 30m` → `1h30m` |
| `duration / duration` | number (int if exact, float if not) | `90s / 30s` → `3`; `90s / 60s` → `1.5` |
| `duration * int/float`, `int/float * duration` | `duration` | `30m * 2` → `1h`; `1.5 * 30s` → `45s` |
| `duration / 0`, `duration / 0s` | **`div_zero`** | |
| `duration ± int` (bare) | **`type_error`** | same asymmetry as size |
| `datetime + duration`, `duration + datetime` | `datetime` | commutative (`coerce-duration-plus-datetime-commutative`) |
| `datetime - duration` | `datetime` | |
| `duration - datetime` | **`type_error`** | only one subtraction direction is defined |
| `datetime - datetime` | `duration` | `t"...14:00Z" - t"...12:30Z"` → `1h30m` |
| `datetime + datetime`, `datetime * int` | **`type_error`** | |
| `list + list` | `list` (concat) | `[1,2] + [3]` → `[1, 2, 3]` |
| `str + str` | `str` (concat) | `"a" + "b"` → `"ab"` |
| `list/str/bool + <mismatched type>` | **`type_error`** | `[1,2] + 1`, `"a" + 1`, `true + false` all error — no arithmetic on `bool` at all |

**Site 2 — word binding** (CMD-mode word → declared parameter type, at call-bind): `str` (identity),
`path`, `glob` (compiled pattern, **unexpanded**), `int/float/size/duration/time/datetime` (parse;
failure = `arg_error`), `bool` (flag *presence*, not a parsed word — `--b` present → `true`), `list<T>`
(repeated flags/positionals accumulate — variadic `...nums: list<int>` sums correctly, **corpus**
`word-bind-list-int-variadic-accumulates`). Every one of these is individually corpus-verified in
`spec/cases/coercion.toml`'s `word-bind-*` cases. **Unknown-signature (T0) targets** — a raw external
binary with no adapter — receive every word as `str`, verbatim, always; no coercion is attempted.

Value-carrying flags on user functions accept both `--flag=value` and `--flag value` when the
declared parameter is non-`bool`; a `bool` flag keeps presence semantics. Excess positionals without
a declared rest parameter and unknown named arguments raise `arg_error` instead of being dropped.

### 3.7 Comparisons and logic (site/content/internals/language-conformance-contract.md)

`&&`/`||` admit **only** `bool` or a command **outcome** (success = true) as operands — `1 && true` is
`type_error` (**corpus** `operators.toml:op-and-int-operand-is-error`); `!5` is `type_error`
(**corpus** `op-not-int-is-error`). They **short-circuit** and return the *deciding operand verbatim*,
not a forced `bool` — chaining stays chainable (`.status`/`.out` still reachable on the result).
Comparison operators (`< <= > >= == !=`) do not chain; mixed-type comparison like `"a" < 1` is
`type_error` (**corpus** `op-cmp-str-lt-int-is-error`); same-type comparisons including `bool < bool`
work (`false < true` → `true`).

### 3.8 Variables, functions, lambdas

- `let` is immutable — reassigning is `type_error` (**corpus** `core.toml:let-immutable`); shadowing
  is legal with a lint, never an error. `var` is mutable, with `+= -= *= /=` compound assignment
  (**corpus** `var-assign`, `var-compound`).
- `fn add(a: int, b: int) { a + b }` then calling `add(2, 5)` (EXPR call) **or** `add 2 3` (CMD call,
  word-bound) both work identically — a `fn` genuinely *is* a command (**corpus**
  `core.toml:fn-call`, `coercion.toml:word-bind-int-positional`). Defaults: `fn inc(a: int, by: int =
  1) { a + by }`, `inc(4)` → `5` (**corpus** `fn-default`). To **capture a CMD-form call's result in
  a binding, parenthesize it**: `let x = (deploy staging --dry)` (verified against the binary) — the
  unparenthesized `let x = deploy staging --dry` is a parse error (`expected newline or `;` between
  statements`), because a `let` RHS lexes in EXPR mode where bare words don't glue into a command.
- Lambdas: `x => expr` or `(a, b) => expr`/block. **(corpus** `core.toml:multi-lambda`,
  `lambda-call-method`**)**. Closures capture the *enclosing binding itself* (a shared cell, not a
  copy) — a `var` mutated by a closure through repeated calls accumulates across calls (**corpus**
  `closures.toml:closure-mutates-captured-var-via-each`).
- **Implicit `.field`/`.method` lambda sugar** — in **argument position only**: `.field <op> e`
  desugars to `x => x.field <op> e`, and `.method(args)` desugars to `x => x.method(args)`. This is
  exactly what makes `ls.where(.size > 1mb)` (site/content/internals/language-conformance-contract.md's own canonical example) and
  `ls.where(.name.contains("x"))` read the way they do — no explicit lambda parameter needed for the
  common case. A **bare `.field`** with no op/args also works and reaches a zero-arg **method** of
  that name when there's no such field — **but only on non-record receivers**: `str`/`path`/`int`/
  `list` all still fall back this way (`["a","b"].map(.upper)`, `paths.map(.name)`,
  `[1,2,3].sum`, `[[],[1]].where(.is_empty)` — **corpus** `field-method-fallback.toml`). A real field
  always wins over a same-named method (user data first). This is why `path` accessors (`.name .stem
  .ext .parent .read .size .exists …`) read as fields inside a `.map(...)`.
  **`record` is strict (a fix just landed) — no fallback at all.** `.field`/bare-`.method` access on a
  `record` that isn't one of its actual fields now raises `field_missing` loudly instead of silently
  invoking a same-named method: `{a:1}.items`, `{a:1}.json`, `{a:1}.set` are all `field_missing:
  missing field ...` (verified against the binary), even though `.items()`/`.set(k, v)` exist as real
  record methods — call them **with parens** to reach the method (`{a:1}.set("c", 3)` works;
  `{a:1}.set` does not). This also applies inside `.map(...)` over a list of records —
  `[{a:1}].map(.items)` raises the same `field_missing`, not a silent method call. Don't guess a
  result's field name against a record and expect the old fallback to save you; it won't.
- Recursion works normally: `fn fact(n: int) { if n <= 1 { 1 } else { n * fact(n - 1) } }` (**corpus**
  `closures.toml:recursive-fn-factorial`); a `fn`'s own parameter can be captured by a lambda defined
  inside it and returned (**corpus** `fn-returns-closure-capturing-param`).

### 3.9 `match` — every pattern kind (site/content/internals/language-conformance-contract.md grammar)

All corpus-grounded in `spec/cases/match.toml`:

- **Literal + range**: `match 5 { 0..3 => "low"; 3..=10 => "mid"; _ => "high" }` → `"mid"`
  (`match-range-basic`). Ranges are exclusive/inclusive exactly like the `..`/`..=` operators — `3` is
  excluded from `0..3`, included in `3..=10` (`match-range-boundary-exclusive-vs-inclusive`).
- **Type pattern**: `match 5 { int n => "int:{n}"; _ => "other" }` → `"int:5"`; a type mismatch falls
  through to the next arm, not an error (`match-type-mismatch-falls-through`).
- **Record pattern**, with shorthand binding and nested subpatterns: `match {name: "ada", age: 30} {
  {name, age} => "{name} is {age}"; _ => "no match" }` → `"ada is 30"` (`match-record-shorthand`);
  nested: `{point: {x, y}} => x + y` (`match-record-subpattern`); a missing field falls through
  (`match-record-missing-field-falls-through`).
- **List pattern**, fixed-arity and rest-binding: `[a, b, c] => a + b + c`
  (`match-list-fixed-arity`); `[first, ...rest] => rest.len()` (`match-list-rest-binding`); arity
  mismatch falls through to the next, more general arm (`match-list-arity-mismatch-falls-through`);
  `[]` matches only the empty list (`match-list-empty-pattern`).
- **Guards**: `{status} if status >= 200 && status < 300 => "ok"` (`match-record-guard`).
- **Alternation** (`|`, the one legal use of `|` outside `sh{}`): `1 | 2 | 3 => "small"; n if n > 5
  => "big"; _ => "medium"` (`match-alternation-with-guard`).

### 3.10 Control flow, errors, commands

- `if`/`else if`/`else` — condition must be `bool`/outcome (**corpus** `core.toml:if-true`,
  `if-false`, `no-truthiness`).
- `for pattern in expr { }` — supports `break`/`continue` (**corpus**
  `closures.toml:for-loop-break-stops-early`, `for-loop-continue-skips-iteration`); `while expr { }`
  likewise (`while-loop-break-stops-early`).
- `try { } catch [pat] { }` — binds the caught `error` value if a pattern name is given; the render
  form is `error(<code>: <msg>)` (**corpus** `operators.toml:op-try-catch-binds-error-value`, `1/0`
  caught → `error(div_zero: division by zero)`). **The bound error is introspectable** — it exposes
  `.code .msg .hint .stderr .status` as fields, so a handler branches on the failure:
  `catch err { if err.code == "not_found" { ... } else { ... } }` (**corpus**
  `catch-forms.toml:catch-error-branch-on-code`). Absent optionals read as `null`. The binding syntax
  is `catch IDENT block` (or `catch IDENT expr`), **not** `catch IDENT => ...` (that `=>` is lambda
  syntax and is a parse error here). **Postfix `catch`** is the same thing as sugar:
  `expr catch handler` — `(1/0) catch e { "caught" }` and the bare-value form `1/0 catch "fallback"`
  both work (**corpus** `core.toml:catch-unbound-fallback`, `operators.toml:op-catch-bound-name`). A
  successful `try`/expression short-circuits `catch` entirely — the RHS of `??`/`catch` on a
  non-error value is never evaluated (**corpus** `operators.toml:op-coalesce-does-not-evaluate-rhs-on-value`,
  `3 ?? (1 / 0)` → `3`, no division ever happens).
- **Every external command call yields an `outcome`** — but qualify this for builtins: some return a
  **bare value** instead (`pwd` → a `path`, verified against the binary), so don't unconditionally
  reach for `.ok`/`.out` on a builtin's result. At bare/statement rendering an outcome shows as
  `outcome(status: 0, ok: true)` (**corpus** `outcome.toml:outcome-echo-render-inline`); `if (echo hi)
  { "yes" } else { "no" }` reads its truthiness from `.ok` automatically (`outcome-if-position`).

### 3.11 `cwd`/`env` scoping (site/content/internals/language-conformance-contract.md)

The **session** owns `cwd`/`env`. `cd`/`env.NAME = v` are legal and journaled **only at session top
level** — both are `custom`-coded errors naming the `with cwd:`/`with env:` fix when attempted inside
a `fn` body (**corpus** `reef.toml:reef-cd-inside-fn-body-is-illegal`, `reef-env-assign-inside-fn-body-is-illegal`).
`with cwd: p, env: {...} { }` scopes both dynamically and **restores on any exit path**, including an
error raised inside the block (**corpus** `reef-cwd-restores-after-with-block`,
`reef-cwd-restores-after-error-inside-with-block`, and nested blocks restore all the way out,
`reef-cwd-nested-with-blocks-restore-outer`).

---

## 4. Hard rules — never violate these

Each rule: what's forbidden, why, the corpus/source proof, and the correct alternative.

1. **No `|` pipe operator**, ever, outside `sh { }` or a `match` alternation pattern. *Why*: pipes are
   untyped byte hoses; shoal composes typed values instead (site/content/internals/system-map.md). *Proof*: `spec/cases/core.toml:parse-pipe-teaching`;
   verified against the binary that infix EXPR positions (`1 | 2`, `let c = a | b`) get the same
   curated *"shoal has no pipe operator"* teaching error, not a generic parse failure.
   *Alternative*: `.where`/`.map`/dot-chains; `sh { }` for verbatim POSIX.
2. **No `$` sigil.** *Proof*: `core.toml:parse-dollar`, "no sigil". *Alternative*: bare identifiers;
   `env.VAR` for the environment.
3. **No backtick command substitution.** *Proof*: `core.toml:parse-backtick`. *Alternative*: `(expr)`
   or `sh { }`.
4. **No truthiness.** A non-`bool`/non-outcome condition is `type_error`. *Proof*: `core.toml:no-truthiness`,
   `operators.toml:op-and-int-operand-is-error`, `op-not-int-is-error`. *Alternative*:
   `.is_empty()`, `.is_some()`, `!= null`, explicit comparisons.
5. **Comparisons never chain.** `a < b < c` is a parse error with a fix-it. *Proof*:
   `core.toml:parse-comparison-chain`, `operators.toml:op-cmp-chain-le-lt-is-error`. *Alternative*:
   `a < b && b < c`.
6. **`let` is immutable.** Reassigning is `type_error`. *Proof*: `core.toml:let-immutable`.
   *Alternative*: declare with `var` up front if you'll mutate it.
7. **`cd`/`env.NAME=` are illegal inside `fn` bodies.** *Proof*: `reef.toml:reef-cd-inside-fn-body-is-illegal`,
   `reef-env-assign-inside-fn-body-is-illegal`. *Alternative*: `with cwd:`/`with env: { }`.
8. **Heredocs, here-strings, and any fd-numbered/`&>`-style redirect are permanently forbidden** — not
   runtime errors, curated *parse-time* diagnostics naming the modern replacement (site/content/internals/values-streams-execution.md). This is
   the same enforcement class as the pipe/`$`/backtick errors — the parser recognizes the box-era
   *shape* specifically so it can teach, not just reject. All four are **implemented and verified
   against the binary**: `cat << EOF` → *"shoal has no heredocs"* (hint: *"feed a string or multiline
   literal instead: `value.feed(cmd)`, or use an interpreter block: `python { … }`"*); `cat <<< "hi"`
   → *"shoal has no here-strings"* (hint: *"feed the value instead: `"text".feed(cmd)`"*);
   `cmd 2>file` / `cmd 2>&1` → *"shoal has no fd-numbered redirects"* (hint: *"stderr is structured —
   `(cmd).stderr`, or `try { cmd } catch e { e.stderr }`; a statement-position PTY run already merges
   the streams"*); `cmd &>file` → *"shoal has no stream-merging redirect"* (hint: *"capture is
   structured: `(cmd).out` / `(cmd).stderr`; a statement-position PTY run already merges the
   streams"*).
9. **Size/duration arithmetic is asymmetric on purpose.** `size * float` is fine; `size / float` is
   `type_error`. `size ± int` (bare) is always `type_error` — both operands must be sized. Negative
   size results/multipliers are `type_error` with hint "negative"; only `size/int`, `size/size`,
   `duration/duration`, `duration/int` reach `div_zero` on an actual zero divisor. *Proof*: the full
   `coercion.toml` block cited in §3.6 above. Don't guess at this matrix — re-check the table.
10. **Streams are single-consumption.** A second consumption is a runtime error (`stream_consumed`,
    fix-it "collect first, or `.tee(2)`") — site/content/internals/language-conformance-contract.md. Streams **are implemented** (channels,
    `every(dur)`, `.map`/`.scan`/`.take`/`.collect` all work, §6) — so this rule bites now: don't read
    one twice.
11. **`it`/`out[n]` are reserved outside a REPL — everywhere, not just over MCP.** This is a
    *parser*-level restriction, not an MCP-specific one — verified directly against the plain CLI
    binary, no kernel/MCP involved: `shoal -c 'out'` and `shoal -c 'it'` are each the parse error
    *"`it`/`out` is REPL-only"*, hint *"bind a variable to reuse a previous result"*; the identical
    error fires from a `.shl` script file and from an MCP `shoal_exec` call alike. One nuance:
    `let out = 1` *alone* still parses (the bare word `out` is a legal binding name) — it's any
    subsequent bare-`out`/`it` **read** that errors, e.g. `shoal -c $'let out = 1\nout'` fails on the
    second line with the same *"`out` is REPL-only"*. Over MCP specifically, the kernel additionally
    forces `evaluator.interactive = false` for every exec (`crates/shoal-kernel`), but that's belt
    and suspenders — the parse error alone already makes `it`/`out` unavailable in any non-REPL
    context. Always bind with a different name via `let`, or keep the returned `ref` and use
    `shoal_get`.
12. **A raised MCP error now DOES mint a transcript ref** (updated — verified in
    `crates/shoal-kernel/src/handlers_exec.rs`): the structured error value is stored at `out:<n>`
    and the `-32002` error's `data.ref`/`data.uri` point at it, so `shoal_get {ref: data.ref}`
    fetches the full `{code, msg, span, hint, stderr}` error after the fact. Two nuances remain:
    value-position **capture** (a normal, non-error result with `.ok == false`) applies to
    **external-command failures** only; a **builtin's** error (`div_zero`, `index_range`, …) raises
    even for a single bare expression — you get the `-32002` + `data.ref`, not a captured outcome.
    `try { } catch e { e }` inside `src` still works when you want the error as the *successful*
    return value.
13. **`position: "value"`'s capture behavior applies to the *final* statement of `src`, however many
    statements precede it — a multi-statement program does NOT force raise-on-failure across the
    board (correcting this card's earlier, wrong claim).** `eval_with_position`
    (`crates/shoal-kernel/src/lib.rs`) runs every statement *except the last* with ordinary
    statement (raise-on-failure) semantics — so an earlier statement's failure always raises,
    regardless of `position` — but then evaluates the trailing statement in true value position
    **if and only if it's a bare expression**, exactly like the single-statement case: a failing
    external command there is captured (`.ok == false`), not raised. For example,
    `{src: "let x=1\nsh{exit 3}", position:
    "value"}` comes back **captured** (`isError: false`, a normal outcome value with `.ok == false`),
    not as an MCP error — the preceding `let` doesn't change that. Only a **builtin's** raised error
    (`div_zero`, `index_range`, …) stays position-invariant (rule 12) — that one always raises even
    as the trailing statement. If a command *before* the last statement might fail and you want to
    inspect that failure without raising, wrap it in `try { ... } catch e { ... }` inside `src`.
14. **DONE — `render` and the tool result's `content[0].text` are now both size-capped at 64 KiB**
    (this used to be a real gap; the old unbounded behavior is what this rule described before the
    fix). Two independent caps, verified directly in source: kernel-side, `bound_render`
    (`crates/shoal-kernel/src/wire.rs`) bounds any `ExecResult`/`value.get` `render` string to
    `ELIDE_HARD_CAP` (64 KiB) before it ever reaches the wire, so `structuredContent.render` is
    capped too, not just `content[0].text`; MCP-side, `bound_text`/`RESULT_TEXT_HARD_CAP`
    (`crates/shoal-mcp/src/tools.rs`) independently bounds `content[0].text` the same way. Both keep
    a head of whole lines and append a `…(N more lines, fetch via <uri>)` marker — a huge render (the
    comment in `wire.rs` names its own motivating case: "a huge outcome's ANSI-laden stdout") can no
    longer bypass the wall the structured `value` already respects. This closes the "wall of bytes"
    risk, but `render`/`text` remain ANSI-laden and non-structured — keep reading
    `structuredContent.value`/`shoal_get` for anything you need to parse or branch on.
15. **An elided value's embedded `uri` (`shoal://...`) is independently fetchable via
    `resources/read` (DONE, §0.8)** — `resources/*` is confirmed dispatched
    (`crates/shoal-mcp/src/lib.rs`'s `handle` handles `resources/list`/`read`/`subscribe`), so this is
    the preferred path, not a maybe. Its `resources/unsubscribe` removes and joins the URI's
    forwarding worker as documented in §0.8. If a *particular* URI
    still 404s, fall back to translating it yourself: the part before `?path=` is the short `ref`
    you already have; the part after is the `path` argument to `shoal_get`.
16. **Background execution and task management are now fully reachable through MCP** (updated —
    verified in `crates/shoal-mcp/src/tools.rs` and `crates/shoal-kernel/src/handlers_exec.rs`):
    `shoal_exec {background: true}` returns a task ref immediately, `timeout_ms` converts an overdue
    synchronous run into a background task (`timed_out: true` — it does **not** kill the command,
    §0.1), and `shoal_cancel {task}` requests cancellation (§0.7). A plain `shoal_exec` without
    either field still blocks until the command finishes.
17. **Hyphenated command names are not EXPR identifiers.** `docker-compose` needs `^docker-compose` or
    `run("docker-compose", args...)` (site/content/internals/language-conformance-contract.md).
18. **Shadowing a resolvable command with `let` is legal (linted, not fatal)** — and `^name` bypasses
    the non-callable shadow. **`^` also bypasses adapter dispatch**, so `^git log --oneline -1`
    reaches the raw external/reef-resolved command with its argv intact. It does not disable session
    functions or aliases: callable bindings still resolve first. Use `run("name", args...)` when the
    command name itself is computed rather than syntactically known.
19. **`glob("...")` constructs a glob *pattern value*, not an expanded list — indexing it directly is
    a `type_error`.** `glob("*.rs")[0]` → `type_error: cannot index glob with int` (verified against
    the binary; `crates/shoal-eval/src/expr_access.rs`'s `index` has no `glob` arm, so it falls
    through to the generic `cannot index X with Y`). **Expansion happens at the command *callee*,
    never at construction** (site/content/internals/language-conformance-contract.md's glob rows) — this is the load-bearing fact, not a
    throwaway one: pass the pattern to a command and let it expand there, either bare in CMD
    position (`ls *.rs`) or as a parenthesized value (`ls (glob("*.rs"))`) — both verified working.
    If you genuinely need the expanded list as a value with no command involved, `.expand()` on the
    glob does it eagerly (verified against the binary: `glob("Cargo.toml").expand()[0]` → the
    resolved path) — but handing the pattern to a callee is still the idiomatic path.
20. **An outcome's field/method fallback to `.out` (§3.5) does not extend to the index operator, and
    a `table` is never int-indexable at all.** `(ls)[0]` is `type_error: cannot index outcome with
    int` even though `.where`/`.map`/etc. on that same outcome forward fine to `.out`; `(ls).out[0]`
    (a `table`) is *also* `type_error: cannot index table with int` (both verified against the
    binary — `expr_access.rs`'s `index` only has arms for `list`, `str`, and `record`-by-string-key;
    everything else, including `outcome`/`table`/`glob`, falls to the generic error above).
    *Alternative*: index a genuine `list` (a non-table `.out`) directly; reach a specific row of a
    `table` with `.where(...)`, `.map(...)`, or `.first()` — never `[]`.
21. **A search-tool call with zero matches raises `cmd_failed`, not an empty result — a common
    bash-native surprise.** `sh { grep pattern file }` / `sh { rg pattern file }` on a clean miss
    exits `1` (grep/ripgrep's own convention for "no matches"), and the shoal adapter default is
    `ok_codes = {0}`, so the miss surfaces as `cmd_failed: ... exited with status 1` (verified
    against the binary) rather than a quiet empty result. *Alternative*: wrap in
    `try { ... } catch e { ... }` if "no matches" is an expected, non-exceptional outcome for your
    use case, or check the specific tool's own exit-code convention (some tools reserve other codes
    for "no results" vs. a genuine error) before assuming a non-zero status always means something
    broke.
22. **Spawn pinning is opt-in per principal, then fail-closed.** An absent or empty `proc_spawn`
    allowlist preserves ordinary spawning. Once a non-empty allowlist is configured, shoal resolves
    and hashes the executable (reusing Reef's hash when available) and evaluates the exact
    `{bin_hash, argv0}` effect before both captured and PTY spawns. A mismatch is denied; this is no
    longer a design-only Reef/Leash handoff.
23. **Undo targets are journal entries, not transcript values.** `undo` chooses the newest reversible
    journal entry and `undo 12` names an entry directly. Only the interactive REPL maintains the
    separate `out[n]`→entry map needed to rewrite literal `undo out[n]`; do not send that spelling
    through MCP. Undo stale-checks the recorded filesystem fingerprint and raises `stale_undo`
    instead of overwriting intervening changes.

---

## 5. Error codes

Two different tables. Do not conflate them.

### 5.1 JSON-RPC transport codes (what you see in a failed MCP call's `structuredContent.code`)

Sourced directly from `crates/shoal-kernel/src/lib.rs`'s dispatch:

| code | meaning | `data` shape | recovery |
|---|---|---|---|
| `-32000` | request requires a session, but this connection has not attached | — | Reconnect/reattach; the shipped MCP bridge attaches automatically. |
| `-32001` | **parse error** — `src` doesn't parse. No `data.code` string is set here (just `span`/`hint`) — infer "parse error" from this transport code itself. | `{span, hint}` | Fix the source per `hint`; re-check §2/§4's forbidden-spelling list first. |
| `-32002` | **evaluation error** — parsed fine, failed at runtime. `data.code` is the real shoal error code string (§5.2). | `{code, span, hint, status, stderr, ref, uri}` | Branch on `data.code`; the error value remains fetchable through `ref`/`uri`. |
| `-32004` | unknown value `ref` passed to `value.get`/`shoal_get` (stale, wrong session, or never existed) | `{}` | Re-`shoal_exec` to get a fresh ref; refs don't survive kernel restarts. |
| `-32005` | bad `path` or `slice` in `value.get`/`shoal_get` (missing field/index, malformed path, or slicing an unsupported type) | `{ref, path}` for path failures; `{ref}` or no data for other failures | Check §0.2's path grammar and value type; `[a..b]` ranges are supported, negative indices are not. |
| `-32010` | leash **denied** execution, or a `plan_ref` belongs to a different session/principal | `{effects}` | Under the default permissive policy this should not happen; if it does, the kernel was started with a stricter `--policy`. |
| `-32011` | **approval required** (a plan's verdict) or **approval still pending** (on `shoal_apply`) | `{effects}` | Call `shoal_cap_request {plan_ref}`, then `shoal_apply {plan_ref}`. |
| `-32012` | unknown `plan_ref` (never created, or the kernel restarted — plans are in-memory, not journaled) | `{}` | Re-derive with `shoal_plan`. |
| `-32020` | task suspension requested — **always** returned; not implemented | `{task}` | Don't call `task.suspend` (not reachable via MCP tools anyway). |
| `-32021` | unknown task ref | `{}` | A stale/wrong task ref passed to `shoal_cancel` (§0.7). Task refs come from `shoal_exec {background: true}` / a `timeout_ms` conversion (§0.1). |
| `-32022` | unknown, closed, or foreign-session `pty_id` | — | Recover this session's live IDs with `shoal_pty_list`; do not reuse a closed ID. |
| `-32023` | `shoal_pty_open` could not resolve/spawn the program or apply its sandbox | `{cmd}` | Inspect the message; verify command resolution, policy, and platform PTY support. |
| `-32030` | bearer token missing/invalid/expired, or tokens unavailable on an ephemeral (`Kernel::new()`) kernel | `{}` | Check `SHOAL_TOKEN`; ensure the kernel was started with a state dir (`shoal-kernel` without `--socket`-only ephemeral mode). |
| `-32600` | invalid JSON-RPC request/version | — | Transport bug — should not occur through this plugin's tools. |
| `-32601` | method not found | `{method}` | You (or a future card revision) called something the kernel doesn't dispatch. Note `complete`/`explain` and `resources/*` **are** dispatched now (§6) — an unexpected `-32601` most likely means a stale kernel binary. |
| `-32602` | invalid params (missing required field, wrong shape) | — | Check the tool's exact schema in §0. |
| `-32603` | internal error | — | Not a language-level problem; report it. |

### 5.2 Language-level error codes (`data.code` on a `-32002`)

Pinned registry. **✓** = directly exercised by a named corpus case; no mark = pinned but not
individually corpus-exercised in the material reviewed for this card (still authoritative — site/content/internals/intercrate-protocol-contracts.md
pins it — just verify empirically if you hit an edge).

| code | meaning | recovery |
|---|---|---|
| `parse_error` ✓ | source doesn't parse (surfaces as `-32001`, not `-32002` — see 5.1) | fix syntax per `hint` |
| `type_error` ✓ | operand/condition of the wrong type (no truthiness, bad coercion, wrong arg type, etc.) | check §3.6's coercion matrix / §3.7's logic rules |
| `arg_error` ✓ | wrong arity, or a CMD-mode word failed to parse into its declared param type | fix the call site; `addn 2 notanumber` → `arg_error` (`coercion.toml:word-bind-int-parse-failure-is-arg-error`) |
| `undefined_var` ✓ | referenced a name never bound | `core.toml:undefined`, `let x = missing` |
| `not_found` ✓ | command-not-found after the full resolution chain, or a `with reef:` override that expired | `reef.toml:reef-with-reef-restores-after-block` |
| `cmd_failed` | a statement-position command's outcome was non-`ok` and got raised (per adapter `ok_codes`, default `{0}`) | inspect via `try/catch` or single-expression `position:"value"` capture instead (rule 12/13) |
| `div_zero` ✓ | division by zero (`int`, `size`, `duration`) | `core.toml:div-zero`, and every `*-div-*-zero` case in `coercion.toml` |
| `index_range` ✓ | list/table index out of bounds | `literals.toml:list-index-range`, `[1][3]` |
| `field_missing` ✓ | record has no such field | `core.toml:record-missing` |
| `utf8_error` | `path.str()` on non-UTF-8 bytes | use `.display()` (lossy) instead |
| `stream_consumed` | a stream was driven to a sink twice | `.tee(n)` before the first consumption (streams **are** implemented — this fires for real) |
| `no_matches` | (pinned; no corpus case reviewed) | — |
| `custom` ✓ | a named, ad-hoc error with a specific message (e.g. the `cd`-in-`fn`/`env`-in-`fn` fix-its) | `reef.toml:reef-cd-inside-fn-body-is-illegal` |
| `assert_failed` | (pinned; no corpus case reviewed) | — |
| `permission` | (pinned; no corpus case reviewed) | — |
| `recursion_limit` | function recursion exceeded the native-stack-safe depth of 128 | restructure the recursion; loop iteration limits are a separate policy |
| `overflow` ✓ | numeric/quantity arithmetic overflowed its representation (pinned in site/content/internals/intercrate-protocol-contracts.md; **corpus** `numbers-more.toml`; verified against the binary: `52w * 200000000` → `overflow: duration overflow`) | keep duration/size arithmetic inside i64-ns / u64-byte bounds |
| `reef_unlocked` ✓ | a `with reef:`-constrained tool used in a non-interactive/script context without a lock | `reef.toml:reef-with-reef-constrains-a-spawn-inside-the-block` |
| `reef_drift` | resolved binary's hash no longer matches the lock | `reef lock --refresh` (site/content/internals/reef-resolution.md; not verified reachable in this pass) |
| `reef_conflict` | two reef scopes constrain one tool incompatibly | (not verified reachable in this pass) |
| `reef_not_found` | a reef-constrained tool has no resolvable candidate | (not verified reachable in this pass) |
| `reef_provider` | a reef provider itself failed | (not verified reachable in this pass) |
| `feed_error` | `.feed` **is implemented**; fires on feeding a never-feedable type (`secret`/`task`/`closure`/`error`/`glob`/`regex`) | feed a serializable value (str/bytes/list/record/table) instead |
| `lang_block_unbalanced` | interpreter blocks **are implemented**; an unterminated brace in a `python { … }`/`jq { … }`/etc. block | balance the braces in the block payload |
| `runner_not_found` | reef `run <path>` could not select an extension, shebang, or configured runner | add a supported extension/shebang or configure the runner |
| `stream_unbounded` | **implemented and correct** — you `.collect()`d a stream with no natural end | bound it first with `.take(n)`/`.take_until(…)`, or use `.each(f)` |

---

## 6. Implementation status — what works, what to skip

This snapshot is grounded in current source and the 1,331-case/78-suite corpus. The structured MCP
surface, resources, PTYs, channels, CAS-backed refs, spawn pin gate, and platform filesystem
sandboxes are shipped. Important boundaries remain: `task.suspend` is not implemented; network
sandboxing is not enforced; some in-process filesystem effects bypass the child-process sandbox;
and non-hermetic policies degrade honestly when a requested OS dimension is unavailable.

- **SHIPPED — The MCP `resources/*`/events subsystem.**
  `crates/shoal-mcp/src/lib.rs`'s `handle()` dispatches `resources/list`/`read`/`subscribe`, and
  `initialize` advertises
  `capabilities.resources.subscribe = true`; event notifications forward as
  `notifications/resources/updated` (`client.rs::run_event_forwarder`). Use `resources/read` on a
  `shoal://…` uri to drill into an elided value directly — the `shoal_get`+manual-URI translation
  (§0.2, §4 rule 15) is now just a fallback. Confirmed by the live e2e test
  `crates/shoal-mcp/tests/live_kernel.rs`. `resources/unsubscribe` stops and joins that dedicated
  connection/worker; process exit also cleans up any subscriptions still registered.
- **DONE — `shoal_cancel`.** Present in the 13-tool surface. Note `task.suspend` still errors
  (unimplemented even over raw JSON-RPC).
- **DONE — background/async execution via MCP.** `shoal_exec`'s schema exposes `background` and
  `timeout_ms` and forwards both (verified in `crates/shoal-mcp/src/tools.rs`); the kernel spawns a
  trackable task, `timeout_ms` converts an overdue run instead of killing it (§0.1), and terminal
  task states are `completed`/`failed`/`cancelled` on the `task.<n>` channel.
- **DONE — per-call elision tuning via MCP.** Both `shoal_exec` and `shoal_get` expose and forward
  the `elide` budget (`{max_bytes?, max_rows?, max_items?}`); the 64 KiB hard cap still applies. The
  old dead `capture`/`timeout` params on `shoal_exec` are gone from the schema entirely.
- **DONE — `shoal_journal`'s full filter set.** `since`/`until`/`principal`/`ok`/`effects`/`head`/
  `limit` are all in the tool schema and all honored kernel-side (`JournalQueryParams`).
- **DONE — `complete` and `explain`** JSON-RPC methods (typed completions, structured explanations)
  are dispatched kernel-side (`dispatch.rs` → `handle_complete`/`handle_explain`). They are
  kernel-JSON-RPC-only — no `shoal_*` MCP tool wraps them yet.
- **DONE — the language-channel→kernel-bus bridge (this card previously and wrongly called this a
  gap).** `channel("user.x").emit(v)` in evaluated source **does** reach a `resources/subscribe`/
  `events.subscribe` client on `shoal://events/user.x` — verified live, not just read from source:
  `crates/shoal-kernel/src/session.rs`'s `set_event_forwarder` mirrors in-language `user.*` emits
  onto the kernel's wire bus. The reverse direction works too: a wire `events.publish` on `user.x` is
  mirrored back into that session's in-language `channel("user.x")`
  (`crates/shoal-kernel/src/eventbus.rs`'s `handle_events_publish`). Only `user.*` channels cross,
  either direction — kernel-owned channels (`task.{id}`, `session.transcript`, `journal`, `approval`)
  stay kernel-only (see §0.8).
- **~~`.feed` and interpreter blocks~~ — NOW IMPLEMENTED (this card's original banner was stale).**
  Verified working against the current binary: `["b","a","c"].feed(sort).out`, and **commands with
  args/flags parse bare** — `["b","a","c"].feed(sort -r).out`, `data.feed(grep "foo").out`,
  `{a:1}.feed(jq ".a").out` (the argument parses in CMD mode when it starts with a command head; the
  inverted `cmd.feed(value)` form still parses its arg as a value). Interpreter blocks
  `python { print(6*7) }.out`, `jq { .a }`, `sh { sort -r }` work as feed targets too —
  `{a:1,b:2}.feed(jq { .a }).out` → `1`. An interpreter block's stdout **auto-parses to a structured
  value** on `.out` (`python { import json; print(json.dumps({"n":42})) }.out` → the record `{n: 42}`).
  Heredocs stay gone; this is their replacement, and it works.
- **Reactive streams are implemented.** `channel(name)` supports `.emit(v)`/`.events()`/`.latest()`;
  sources include `every`, `watch`, and `tail`; lazy combinators include `map`, `filter`, `scan`,
  `take`, `take_until`, `dedupe`, `distinct`, `debounce`, `throttle`, `window`, `buffer`, and `merge`.
  Sinks include `collect`, `count`, `first`, `last`, `reduce`, `each`, and the evaluator-hosted
  `.into(channel)`/`on(...)` paths. The `stream_unbounded` guard rejects an unbounded `collect`;
  bound the source first with `.take(n)` or another terminating stage.
- **Reef's integrated surface** includes project/user/system scope discovery, `which` reports,
  lock/fetch/doctor/add flows, provider resolution, drift checks, spawn-time hash reuse, runner
  selection, and `with reef:` overrides. Host-dependent path/version fields naturally vary.
- **Leash and OS enforcement are conditional, not fictional.** Policy verdicts gate execution.
  Scoped child filesystem access lowers to Landlock on Linux or Seatbelt on macOS when useful roots
  exist; the default-permissive policy intentionally produces no sandbox. A non-empty `proc_spawn`
  allowlist activates executable hashing and pin evaluation for normal and PTY spawns. Network
  enforcement remains unavailable, and status fields report that independently.
- **Interactive boundaries:** `shoal_exec` remains headless capture mode. For an interactive program,
  use the six `shoal_pty_*` tools in §0.9. Human-only REPL affordances such as job-control sugar,
  prompt paging, and direct terminal passthrough do not appear on `shoal_exec`.
- **Content-addressed refs are live for large CAS-backed byte captures.** Such values carry a
  `val:blake3:<hash>` ref, resolve through `blob.get`/`shoal://val/{hash}`, and can be sliced or read
  raw without resending the whole blob. Ordinary transcript values still use session-scoped
  `out:N` refs.
- **Aliases and undo are implemented.** Aliases are AST partial calls with later positional/flag
  forwarding. Journal undo replays typed inverses and stale-checks fingerprints; `undo out[n]` is
  REPL host sugar, while `undo` and `undo <journal-id>` work at evaluator level when a journal is
  installed.

---

*Canonical prose lives in `site/content/docs/` and `site/content/internals/`; executable semantics
live in the 1,331 cases across `spec/cases/*.toml`. Re-check the current source and corpus when this
card and a running binary disagree.*
