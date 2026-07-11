---
name: shoal
description: Complete operating manual for driving shoal — the agent-first structured shell — over its MCP facade (shoal_exec, shoal_plan, shoal_apply, shoal_get, shoal_journal, shoal_cap_request, shoal_cancel). Load this whenever you are about to run a command in a shoal session, write `.shl` source, translate a bash idiom into shoal, or interpret a shoal MCP tool result. Covers the full language grammar, the exact wire protocol as actually implemented (not just as spec'd), every hard rule/gotcha, and what is not yet implemented.
---

# shoal — the language card

shoal is a **typed value graph over one session kernel**, not a text-stream router. You never pipe
bytes between processes and re-parse them; you get back **structured values** with **stable refs**,
and you drill into them by field path. This card is exhaustive and precise on purpose: every claim
below is traced to `docs/TDD.md`, `docs/VISION.md`, `docs/REEF.md`, `docs/AGENT-SURFACE.md`,
`docs/IO.md`, `docs/STREAMS.md`, `docs/CONTRACTS.md`, the 1100+-case conformance corpus at
`spec/cases/*.toml`, or a direct read of `crates/shoal-mcp`, `crates/shoal-proto`,
`crates/shoal-kernel`. Anything not yet implemented is called out explicitly — do not attempt it.

**The one rule above all others:** never parse shoal's own rendered text. Every value you need is
already structured on the wire. Reach for `structuredContent` / `value.get` / `shoal_get`, never for
`content[0].text` or the human `render` string.

> **Surface-currency note (read once — UPDATED).** This card originally hedged six MCP-facade features
> as **(P1)** ("intended, re-verify before trusting"). **All six have since been confirmed landed
> against source — treat every `(P1)` marker below as DONE, not pending:**
> 1. MCP `resources/*` — `resources/list`/`read`/`subscribe`/`unsubscribe` are dispatched
>    (`crates/shoal-mcp/src/lib.rs` `Facade::handle`), and `initialize` advertises
>    `capabilities.resources.subscribe = true`. Use `resources/read` to drill into an elided value's
>    `shoal://…` uri directly — the manual `ref`+`path` translation (§0.2/§4-rule-15) is now just a
>    fallback, not the primary path.
> 2. The `events`/channel subsystem — `resources/subscribe` on a `shoal://events/{ch}` uri starts a
>    forwarder that pushes `notifications/resources/updated` frames (`client.rs::run_event_forwarder`).
> 3. `shoal_cancel` — present in `tools()` (the tool list has seven entries now).
> 4. Real (non-hardcoded) `reversibility` — the kernel computes it via `reversibility_from_effects`
>    (`handlers_exec.rs`/`handlers_session.rs`), not the literal `"unknown"`.
> 5. macOS-safe socket-path fallback in `shoal-mcp` matching `shoal-kernel`.
> 6. Elision on the `render`/`content[0].text` fields.
>
> Language-surface staleness has also been corrected: `.feed`, interpreter blocks (`python { }` /
> `jq { }` / …), and reactive streams/channels are **implemented** (see §6). The remaining genuinely-open
> gaps this card documents (dead `capture`/`timeout` params, unexposed `elide` budgets,
> `shoal_journal`'s `until`/`effects` schema mismatch, the narrow `value.get` path grammar,
> `shoal_cap_request`'s unused `effects`, `complete`/`explain` being un-dispatched, background exec via
> MCP, and real OS-level sandbox enforcement) are still open — a one-line probe beats trusting any banner.

---

## 0. How you talk to shoal

You do not have a bash tool here. You have **seven** MCP tools — six implemented at authoring time in
`crates/shoal-mcp/src/lib.rs` plus `shoal_cancel` **(P1)** — all forwarding to a running `shoal-kernel`
process over a newline-delimited JSON-RPC 2.0 Unix-socket connection (`docs/TDD.md` §7,
`docs/AGENT-SURFACE.md` §5). Alongside the tools, `docs/AGENT-SURFACE.md` §6/§8 specs a full MCP
**resources** layer (`resources/list`/`read`/`subscribe`, push notifications) — this is the intended
way to fetch elided payloads and subscribe to live output **(P1** — see §0.8 below for how to use it
and how to fall back if it isn't dispatched yet in the build you're talking to**)**.
**A `shoal-kernel` must already be running and reachable** (see the plugin `README.md` — this is a
separate prerequisite from the plugin itself; if a tool call fails with a connection error, that is
the first thing to check, not a language bug).

Every tool result comes back as an MCP `tools/call` result shaped:

```json
{"content":[{"type":"text","text":"<pretty-printed JSON copy of the result>"}],
 "structuredContent": <the same JSON value, structured>,
 "isError": false}
```

**Always read `structuredContent`. Never read `content[0].text` as your source of truth** — it is a
byte-identical pretty-printed dump of the same JSON for surfaces that only render text, and it is
**not elided/size-bounded** (see §4 rule 14). Reading it is exactly the "wall of bytes" anti-pattern
this whole system exists to end.

On success, `structuredContent` is the tool's own result object. On failure (`isError: true`),
`structuredContent` is the raw JSON-RPC error object: `{"code": <int>, "message": <string>, "data": {...}}`.
`code` here is a **JSON-RPC transport code** (e.g. `-32002`), not a shoal language error code — the
shoal error code (`type_error`, `div_zero`, ...) lives at `data.code` for evaluation errors, and is
**absent** for parse errors. See §6 for the exact table; do not assume `data.code` is always present.

### 0.1 `shoal_exec` — run source, get a ref + a structured value

**Params** (from the tool's actual JSON Schema): `{src: string (required), position?: "stmt"|"value", capture?: object, timeout?: number}`.
**Gap**: `capture` and `timeout` are accepted by the schema but are currently **not wired to anything**
— the kernel's `ExecParams` has no such fields, so they are silently dropped. Do not rely on them
(see §6's "shoal_exec's capture/timeout params" bullet). If you omit `position`, the MCP facade defaults it to `"value"` (note: this differs from the
raw kernel's own default of `"stmt"` — the MCP default is the one that matters to you).

**What `position` actually controls** (read this carefully — it is the single sharpest edge in this
surface): the kernel only special-cases `position: "value"` when `src` parses to **exactly one bare
expression statement**. In that case, a failing command's `outcome` is *captured* (returned as a
normal value, `.ok == false`, inspectable) instead of raised as an MCP error. **Any `src` with more
than one statement — including a `let` followed by a command — always evaluates with raise-on-failure
semantics regardless of what you pass for `position`.** If you need to inspect a failure inside a
multi-statement program, wrap the risky part in `try { ... } catch e { e }` inside the source itself.

**Result** (`ExecResult`): `{"ref": "out:<n>", "value": <$-tagged wire value, elided if large>, "render": "<full human string>"}`.

- `ref` is a **session-scoped transcript ref** like `"out:12"` — hand this to `shoal_get` later. There
  is no other ref form produced today (see §6's "Content-addressed val:blake3:... refs" bullet —
  those are spec'd, not implemented).
- `value` is the real payload, `$`-tagged, elided per the rule in §1 if large.
- `render` is a **full, non-elided** human string — see the flagged gap in §4 rule 14. Do not trust its size.
- **A raised error produces no `ref` at all.** There is nothing to `shoal_get` afterward for a failed
  call — the transcript entry is never created on the error path. Plan for this (see the "how to
  inspect a failure" rule above).

Worked example (kernel test `unix_stream_session_roundtrip`, `crates/shoal-kernel/src/lib.rs`):

```json
// call
{"name":"shoal_exec","arguments":{"src":"[1,2,3]","position":"value"}}
// structuredContent
{"ref":"out:1","value":{"$":"list","v":[{"$":"int","v":1},{"$":"int","v":2},{"$":"int","v":3}]},"render":"[1, 2, 3]"}
```

Every nested primitive is `$`-tagged too (`{"$":"int","v":1}`) — this is the **real** wire shape, and
it is stricter than `docs/AGENT-SURFACE.md` §2's prose, which implies `null`/`bool`/`int`/`float`
might travel untagged when nested. They do not, in the implementation you are talking to. Expect the
tag everywhere.

A command's outcome, corpus-grounded (`spec/cases/outcome.toml`, case `outcome-echo-out`, `echo hi`
evaluated at value position → `.out` is `"hi"`; rendered bare it is `outcome(status: 0, ok: true)`):

```json
{"ref":"out:2","value":{"$":"outcome","status":0,"ok":true,"signal":null,
  "out":{"$":"str","v":"hi"},"err":"","dur_ns":123456,"pid":4242,"cmd":"echo hi"},
 "render":"outcome(status: 0, ok: true)"}
```

### 0.2 `shoal_get` — drill into a transcript value without re-executing

**Params**: `{ref: string (required), path?: string, slice?: [int, int]}` (`slice` is exactly 2
integers). **Gap**: the kernel's `value.get` also accepts an `elide` budget (`{max_bytes?, max_rows?,
max_items?}`) — `shoal_get`'s schema does not expose it (`additionalProperties: false`), so you cannot
tighten/loosen elision through this tool today (see §6's "Per-call elision tuning via MCP" bullet).

**Path grammar, exactly as implemented** (`resolve_value_path` in `crates/shoal-kernel/src/lib.rs`):
dotted field names and bracketed non-negative integer indices — `out[3]`, `rows[0].name`, `out.status`.
**`path` is always evaluated from the root value bound to `ref`**, never relative to whatever was
already elided — so if `.out` inside an outcome elided, you still pass `path: "out[3]"` against the
*original* `ref`, not some new sub-ref. There is **no `[a..b]` range syntax inside a path string**
(despite `docs/AGENT-SURFACE.md` §1's prose implying one) and **no negative indices** (`[-1]` works at
the *language* level — corpus case `list-index-negative` — but not inside a `value.get`/`shoal_get`
path string, which only parses `usize`). The separate top-level `slice: [start, end]` parameter is the
only slicing mechanism, and it only applies when the *resolved* value (after `path`) is a `list` — on
any other type it is silently ignored, not an error.

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

Note the elided `out` field's embedded `uri` is `shoal://out/2?path=out`. **(P1)** this may now be
directly fetchable via `resources/read` (§0.8) — try that first and fall back to the manual
translation below only if it 404s: the part before `?path=` gives you the short ref (`out:2`), the
part after `?path=` gives you the `path` argument to pass to `shoal_get` instead.

### 0.3 `shoal_plan` — derive effects without spawning anything

**Params**: `{src: string (required)}`. Internally forced to `mode: "plan", position: "value"` — you
cannot change those. **Result** (`PlanResult`): `{"plan_ref": "plan:<16 hex chars>", "effects": [...],
"reversibility": <see below>, "verdict": "allow"|"deny"|"approval_required", "approval_pending": bool}`.

**(P1) `reversibility` fix, intended-not-independently-verified.** At authoring time, `reversibility`
was **hard-coded to the string `"unknown"`** in the kernel dispatch regardless of what
`shoal-leash`'s `Plan` actually computed. This is one of the six named P1 fixes — the intended
post-fix value is `shoal-leash`'s real computed reversibility signal (its own type; check
`shoal-leash`'s `Plan.reversibility` for the concrete variant names, since this card does not have
them confirmed). **Before trusting a specific reversibility string in your reasoning, read
`crates/shoal-kernel/src/lib.rs`'s plan dispatch once to confirm it no longer returns the literal
`"unknown"`.** If it still does, treat it exactly as before: not a real signal.

Effects are `$`-free plain JSON, tagged by a `"kind"` field (from `shoal-leash`'s `Effect` enum,
`#[serde(tag="kind", rename_all="snake_case")]`): `fs_read{paths}`, `fs_write{paths}`,
`fs_delete{paths}`, `proc_spawn{bin_hash, argv0}`, `net_connect{host, port}`, `net_listen{port}`,
`env_read{names}`, `env_write{names}`, `secret_use{names}`, `session_write`, `journal_read`, `time`,
`opaque` (T0/`sh{}`'s ⊤; unresolvable effects, spawns nothing when planned). Grounded directly from
`shoal-eval`'s own test suite (`crates/shoal-eval/src/lib.rs`):

```json
// {"name":"shoal_plan","arguments":{"src":"git push origin main"}}
{"plan_ref":"plan:8f2c...","verdict":"allow","approval_pending":false,
 "reversibility":"unknown",   // (P1) may now be a real computed value — see note above, verify
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
plan, as the same principal, in the same session, bypassing the leash re-check (trusting that the
plan was already approved — either auto-`allow`, or approved via `shoal_cap_request`). **Result**:
identical shape to `shoal_exec`'s (`{ref, value, render}`). Fails with a JSON-RPC error if the
`plan_ref` is unknown, belongs to a different session/principal, or is still `approval_pending`.

### 0.5 `shoal_journal` — query what already happened

**Params** (tool schema, `additionalProperties: false`): `{since?: int, until?: int, principal?:
string, effects?: string[], head?: string, limit?: int (>=1)}`.

**Gap, verified in source**: the kernel's `JournalQueryParams` only has `since, principal, head, ok,
limit` — **`until` and `effects` are accepted by the schema and silently ignored** (they map to no
kernel field); conversely **`ok` (bool) is a real, working filter that the tool schema does not let
you pass at all** (blocked by `additionalProperties: false`). Only `since`, `principal`, `head`,
`limit` reliably do something today.

**Result**: an array of journal entries: `{id, session, principal, ts, dur_ns, cwd, src, ast,
effects, status, ok, opaque, outputs: [{kind, hash, len}]}` — one row per past `exec`. `head` filters
by the source's first word (e.g. `head: "git"` matches every `git ...` invocation). This is how you
answer "what actually ran" without re-executing or scraping a transcript.

### 0.6 `shoal_cap_request` — unstick a plan awaiting approval

**Params**: `{plan_ref: string (required), effects?: array}`. **Gap**: `effects` is accepted but
**entirely unused** by the handler — it looks up the stored plan by `plan_ref` only, re-checks policy
isn't an outright `deny`, and if so marks the whole plan `approved: true`. There is no fine-grained
per-effect grant today despite the parameter's name — it is all-or-nothing at the plan level.
**Result**: `{"grant":"approved","plan_ref":"...","enforced":false}` on success (`enforced` is always
`false` — see §6's "Real OS-level sandboxing" bullet). Use this only after a `shoal_plan`/`shoal_exec` came back
`approval_required`/`approval_pending`; call `shoal_apply` afterward to actually run it.

### 0.7 `shoal_cancel` — stop a running/background task **(P1 — new tool, verify it exists)**

At authoring time this tool **did not exist** — `crates/shoal-mcp/src/lib.rs`'s tool list had six
entries, not seven, and the kernel's own `task.cancel` was reachable only over raw JSON-RPC, never
through MCP. This is one of the six named P1 additions. Per `docs/AGENT-SURFACE.md` §5 the intended
shape is:

**Params**: `{task: string}` — a task ref/id (`"task:7"` per §1's ref grammar). **Result** (intended):
the task record transitioning to a cancelled state, mirroring the kernel's native `task.cancel`
result shape — **not independently confirmed against source for this card; read
`crates/shoal-mcp/src/lib.rs`'s `tools()`/`tools_call()` to get the exact schema before relying on a
field name.**

**Before you can call this productively**, you need a `task` ref in the first place — that means
`shoal_exec`'s `background`/async path must also be reachable through MCP (see
`docs/AGENT-SURFACE.md` §5's `shoal_exec {... background:bool, timeout_ms?}`). **That wiring is not
one of the six things P1 was asked to fix** — do not assume it landed just because `shoal_cancel`
did. If `shoal_exec`'s schema still has no `background`/`timeout_ms` field, `shoal_cancel` exists but
has nothing reachable to cancel from this plugin (a task created by some *other* client sharing the
session is the only way one would show up). Check the schema before building a workflow around this.

### 0.8 MCP resources — fetching elided payloads and subscribing to live output **(P1 — verify dispatch)**

At authoring time, `Facade::handle()` dispatched exactly `initialize`, `ping`, `tools/list`,
`tools/call` — no `resources/*` method existed, so every elided value's embedded `shoal://...` uri was
a dead end you had to hand-translate (§4 rule 15 explains the manual fallback; keep reading that rule
even if resources now work, since it's your escape hatch if a particular URI still 404s).
`docs/AGENT-SURFACE.md` §1/§6/§8 spec the intended behavior, which this card assumes but does not
independently confirm:

- `resources/list` enumerates stable roots (`shoal://jobs`, `shoal://journal`, `shoal://session/...`)
  plus per-session dynamic entries (open tasks, recent `out:n`).
- `resources/read {uri}` on a value URI (e.g. `shoal://out/12?path=.rows[3].name`) returns
  `structuredContent` — the `$`-tagged (or further-elided) value at that path/slice, **without**
  re-executing anything. This is the *intended* primary way to drill into an elided `Ref` — prefer it
  over the §0.2/§4-rule-15 manual `ref`+`path` translation once you've confirmed it's live.
- `resources/subscribe {uri}` on `shoal://events/{channel}` or `shoal://task/{id}/out` starts a push
  subscription; the server sends `notifications/resources/updated` with `{uri, seq, payload}` as
  events occur (§4 of `docs/AGENT-SURFACE.md`). **Never poll a resource you could instead subscribe
  to** — that is the entire point of this layer existing.
- Query params on any value-bearing URI: `?path=<fieldpath>&slice={a}..{b}&format=json|render|raw`
  (`docs/AGENT-SURFACE.md` §1) — same field-path grammar caveats as §0.2 (no `[a..b]` inside `path`,
  no negative indices) apply here too, since both go through the same `resolve_value_path`.

**How to tell, in one call, whether this landed**: call `resources/list`. A `-32601 method not found`
means it has not (fall back to §0.2's `shoal_get` + manual URI translation, §4 rule 15); any other
response means it has, and resources are your preferred path for drilling into elided values and for
subscribing to `task.{id}`/`session.transcript`/`journal` channels instead of re-calling
`shoal_journal`/`shoal_get` in a loop (polling a tool result is always wrong here regardless of which
layer is live — `docs/AGENT-SURFACE.md` §6 names polling explicitly as the anti-pattern this system
exists to end).

---

## 1. The 60-second model

- **Everything is a typed value.** Numbers, strings, lists, records, tables, paths, durations,
  sizes, outcomes, errors — every one of `docs/TDD.md` §4.1's types renders unambiguously and never
  degrades to "just text." A `table` *is* `list<record>`, structurally.
- **Composition is the dot-chain, not the pipe.** `ls.where(.size > 1mb).map(.name)` — no `|`
  anywhere, ever, outside `sh { }` or a `match` alternation pattern.
- **Commands are values too.** Running `git status` produces an `outcome` value
  (`{status, ok, out, err, dur, pid, cmd}`); an unknown field/method on an outcome forwards to
  `.out`, so `git_log.subject` reads a field of the *parsed* log row, not a string you'd need to
  regex.
- **`fn` IS a command.** `fn deploy(env: str, dry: bool = false) { ... }` is immediately callable as
  `deploy staging --dry` — no separate "make this a CLI" step.
- **No ambient ("invisible") state.** `cwd`/`env` are explicit session state, mutated only at session
  top level (never inside a `fn` body) or scoped dynamically with `with cwd:`/`with env: { ... }`,
  which always restores on exit — including through an error.
- **No truthiness, ever.** `if`/`&&`/`||` accept only `bool` or a command `outcome` (success = true).
  Everything else in a condition position is a `type_error`.
- **Every value you get back over MCP has a ref.** Large ones arrive elided (shape + small preview +
  ref); you fetch more with `shoal_get`, surgically, never by re-running the command.

---

## 2. Translating from bash

Every method named below is pinned in `docs/CONTRACTS.md` §3 / `docs/TDD.md` §5's builtin surface.
Rows marked **(corpus)** have a direct, exact `spec/cases/*.toml` example — check the named case
yourself if you want the ground truth. Unmarked rows use a pinned-but-not-individually-corpus-exercised
method; treat the signature as authoritative per CONTRACTS but verify empirically if a call surprises you.

| bash | shoal | why / grounding |
|---|---|---|
| `ls \| grep x` | `ls.where(.name.contains("x"))` | `\|` is a hard parse error with a teaching message: *"shoal has no pipe operator — data composes with `.` (try `ls.where(.size > 1mb)`)..."* (TDD §1.4; **corpus** `literals.toml:parse-pipe-teaching`, `src="ls \| wc"` → `parse_error`, message contains "no pipe operator"). The suggested replacement text is quoted verbatim from the error itself. |
| `grep ERROR file` | `path("file").read_str().lines().where(.contains("ERROR"))` | `.lines()` **(corpus** `strings.toml:str-lines-strips-crlf`**)**; substring test via `in` is **(corpus** `operators.toml:op-in-string-substring`, `"ell" in "hello"` → `true`**)** — prefer `"ERROR" in line` over `.contains` if you want a corpus-nailed-down spelling. |
| `$VAR`, `$HOME` | `env.VAR` | `$` is illegal everywhere: *"shoal variables have no sigil"* (TDD §2.1; **corpus** `core.toml:parse-dollar`, `src="$HOME"` → `parse_error`, "no sigil"). Reading: `env.NAME` or `(env NAME).out`; writing at session top level: `env.NAME = "v"` (**corpus** `reef.toml:reef-env-assign-writes-session-env-for-a-child`). |
| `$(cmd)` command substitution | `(cmd)` | CMD grammar's `arg = ... \| "(" expr ")"` — a full EXPR embeds as one word/argument; no special substitution syntax needed. A parenthesized command used as a value: **(corpus** `outcome.toml:outcome-echo-out`, `(echo hi).out` → `"hi"`**)**. |
| `` `cmd` `` backticks | `(cmd)` or `sh { cmd }` | Backtick is illegal, error points at `sh { }`/`re"..."`/`t"..."` (TDD §2.1; **corpus** `core.toml:parse-backtick`). |
| `*.txt` glob | `*.txt` (bare, CMD position) or `glob("*.txt")` | Word containing unquoted `*`/`?`/`[...]`/`**` lexes as a `glob` literal; expansion happens at the callee, never at the shell (TDD §4.3). Explicit constructor **(corpus** `literals.toml:lit-glob-constructor-render`, `glob("*.rs")` renders `*.rs`**)**; unexpanded pattern bound to a `glob`-typed param **(corpus** `coercion.toml:word-bind-glob-not-expanded`**)**. |
| glob matches nothing | (silently an empty list) | Nullglob **by construction** — never a literal `*` string (TDD §1.5); a statement-level lint additionally flags a glob that matched nothing. |
| `find . -name '*.rs' -size +1M` | `ls.where(.size > 1mb)` | This exact phrase is TDD §1.4's own canonical pipe-replacement example — the size unit is a first-class literal, not a flag to parse (`1mb`, **corpus** `literals.toml:lit-size-mb-frac`). |
| `cmd > file`, `cmd >> file` | `cmd > file`, `cmd >> file` (kept!) | Muscle-memory sugar, CMD-mode only, desugars to `.save(file)`/`.append(file)` on stdout bytes (TDD §1.3, §3.4). The **modern, canonical** form is calling `.save`/`.append` directly: `(cmd).save(file)`. |
| `cmd < file` | `cmd < file` (kept) | Sole stdin sugar; desugars to `StdinSpec::File` directly (IO.md §1.1). No numeric variant, no here-string variant. |
| `cmd <<EOF ... EOF` (heredoc) | **forbidden, permanently** — use an interpreter block | Not lexed — no `<<` token exists at all. Diagnostic: *"shoal has no heredocs — feed a string or multiline literal instead: `value.feed(cmd)`, or use an interpreter block: `python { ... }`"* (IO.md §4). **Interpreter blocks are IMPLEMENTED and this is the answer**: `python { import json; print(json.dumps(...)) }.out` runs the program and auto-parses its stdout to a structured value; `sh { ... }` (TDD §13.13) and a multiline `"""..."""` literal also work. |
| `cmd <<< "text"` (here-string) | `"text".feed(cmd args…)` (works) | *"shoal has no here-strings — `"text".feed(cmd)`"* (IO.md §4). `.feed` IS implemented, args and all: `"text".feed(grep "foo").out`, `"text".feed(sort -r).out`. Blocks also work: `"text".feed(sh { grep foo })` / `.feed(jq { … })`. |
| `cmd 2>file`, `cmd 2>&1`, `cmd &>file` | **forbidden** | No fd-number tokens exist in the grammar at all. Use `.stderr` on a captured outcome: `cmd.stderr.save(file)`, or `try { cmd } catch e { e.stderr }` (IO.md §4). A live PTY run (statement position) already merges stdout/stderr by construction — this is honest PTY semantics, not a missing flag. |
| `cmd1 \| cmd2` raw byte plumbing | `value.feed(cmd args…)` / `cmd.feed(value)` | The one asylum the pipe error names for genuine byte plumbing. **IMPLEMENTED, including args/flags**: `["b","a","c"].feed(sort -r).out`, `data.feed(grep "foo").out`, `{a:1}.feed(jq ".a").out`. The inverted `cmd.feed(value)` form works too. Interpreter/`sh` blocks are also valid feed targets: `.feed(sh { sort -r })`, `.feed(jq { .a })`. |
| `cmd1 && cmd2`, `cmd1 \|\| cmd2` | kept, unchanged | `&&`/`||` operate on `bool` or command **outcomes** (success = true), short-circuiting, returning the deciding operand *verbatim* — not force-cast to `bool` (**corpus** `outcome.toml:outcome-and-chain-both-outcomes`, `outcome-and-bool-then-outcome`; CMD-mode chaining needs `^` when the head is a reserved word: **corpus** `operators.toml:op-cmd-and-and-runs-both-on-success`, `^true && ^true`). |
| `cmd &` (background) | `cmd &` (kept) | Desugars to `spawn { cmd }`, prints a task handle (TDD §1.3). `shoal_cancel` exists **(P1, §0.7)** to stop a task once you have its ref, but confirm `shoal_exec` actually exposes a `background`/`timeout_ms` param before assuming you can create one through this plugin at all (§4 rule 16). |
| `for f in *.txt; do ...; done` | `for f in glob("*.txt") { ... }` or `glob("*.txt").each(f => ...)` | `for` binds a pattern over any iterable (EBNF `"for" pattern "in" expr block`); basic range form is **(corpus** `closures.toml:for-loop-break-stops-early`, `core.toml:for-range-sum`**)**. |
| `while [ cond ]; do ...; done` | `while cond { ... }` | Direct — **(corpus** `core.toml:while-basic`**)**. `cond` must be `bool`/outcome, never a bare list/string (no truthiness). |
| `if [ -n "$x" ]; then ... fi` (truthiness) | `if x.is_empty() { } else { }` / `if x != null { }` | No truthiness anywhere: `if [1] { 1 }` is `type_error`, "no truthiness" (TDD §1.10; **corpus** `core.toml:no-truthiness`). `.is_empty()` **(corpus** `core.toml:method-is-empty`**)**; `.is_some()`/`!= null` are named in TDD §1.10 for nullable values (not individually corpus-exercised). |
| `grep`/regex extraction | `.matches(re"...")`, `.match(re"...")` | **(corpus** `strings.toml:str-matches-regex-all-occurrences`, `str-match-regex-first-occurrence`**)** — a `regex` is a tagged literal, `re"[0-9]+"`, compiled once. |
| `awk '{print $1}'` (field split) | `.words()[0]` (whitespace) or `.split(",")[i]` (delimiter) | `.words()` splits on whitespace **(corpus** `strings.toml:str-words-splits-on-whitespace`**)**; `.split(sep)` on an explicit delimiter **(corpus** `strings.toml:str-split-on-separator`**)**. |
| `sed 's/foo/bar/g'` | `.replace("foo", "bar")` or `.replace(re"f.o", "bar")` | Replaces **all** occurrences **(corpus** `strings.toml:str-replace-all-occurrences`**)**; the pattern may be a literal `str` OR a `regex` (`$1`/`$name` in the replacement expand capture groups) **(corpus** `strings-methods-2.toml:str2-replace-regex-*`**)**. No first-occurrence-only variant; slice/index manually for that. |
| `sed -E 's/(a)(b)/\2\1/'` (regex capture) | `.replace(re"(a)(b)", "$2$1")` | Capture-group refs use `$1`/`$name`, per the `regex` crate **(corpus** `str2-replace-regex-capture-groups`**)**. |
| `${str:0:7}` (substring) | `str.take(7)`, `str.skip(3)`, `str.skip(2).take(3)` | `.take`/`.skip` slice a `str` **by char** into a substring (not just collections), so fixed-width fields read cleanly — `line.take(7)` is a git short hash **(corpus** `strings-methods-2.toml:str2-take-slices-by-char`, `str2-take-skip-compose-for-substring`**)**. |
| `cut -d, -f1` | `row.split(",")[0]` or `table.map(r => r.split(",")[0])` | Same `.split` grounding as above. |
| `sort` | `.sort()` (plain) / `.sort_by(f)` (key function) | `.sort_by` is **(corpus** `collections.toml:list-sort-by-key-function`, sorts by `.len()`**)**; plain `.sort()` is pinned in CONTRACTS §3 but not individually corpus-exercised. |
| `uniq` | `.uniq()` | Preserves **first-occurrence order**, not a sorted dedup **(corpus** `collections.toml:list-uniq-preserves-first-occurrence-order`, `[3,1,3,2,1].uniq()` → `[3, 1, 2]`**)**. |
| `wc -l`, `wc -c` | `.lines().len()`, `.len()` | **(corpus** `core.toml:method-len`, `strings.toml:str-len-counts-chars`**)**. |
| `awk '{s+=$1} END{print s}'` (fold) | `.reduce(0, (acc, x) => acc + x)` (alias `.fold`) | Left fold — the general aggregation escape hatch when no named op (`.sum`/`.min`/`.max`/`.group`) fits; empty list returns the init **(corpus** `list-methods-3.toml:lm3-reduce-*`**)**. |
| `jq '. + {c:3}'` / build an object | `{a:1}.set("c", 3)`, `r.merge(other)` | Records are immutable values: `.set(k, v)` inserts/replaces one key (keeping position), `.merge(other)` layers `other`'s keys over the receiver (right wins). No `{...spread}` grammar and `+` on records is a `type_error` — use these **(corpus** `record-table-methods-2.toml:rt2-set-*`, `rt2-merge-*`**)**. Build from pairs: `pairs.reduce({}, (acc, kv) => acc.set(kv[0], kv[1]))`. |
| `printf '%.2f' x` (round) | `x.round(2)`, `x.floor(2)`, `x.ceil(2)` | Round a `float` to N decimals (N optional, default 0 → nearest integer); ints pass through **(corpus** `numbers-more.toml:num-round-two-decimals`**)**. |
| `find . -type f` | `glob("**/*")` or `ls` (non-recursive) | `ls` is a builtin returning a `table` (list<record>) **(corpus** `collections.toml:table-ls-len-counts-entries`, `table-ls-where-type-then-map-names`**)**; `**` recurses, dotfiles excluded unless the pattern starts with `.` (TDD §4.3). |
| `xargs` | `.each(f)` | **(corpus** `collections.toml:list-each-side-effect-then-void`**)**. For "read lines from a file, run a command per line": `path("list.txt").read_str().lines().each(f => rm f)` (chains `.read_str()`→`.lines()`→`.each`, all individually corpus-grounded methods). |
| `which cmd` | `which cmd` (kept, richer) | Not forensics — returns a full resolution-chain **record**, not just a path (`docs/REEF.md` §6). `.name` always echoes the query **(corpus** `reef.toml:reef-which-name-field-echoes-query`**)**; unresolved tool's `.out` is `null`, not an error **(corpus** `reef-which-unresolved-tool-out-is-null`**)**; exactly one tool name — `which "a" "b"` is `arg_error` **(corpus** `reef-which-arity-error`**)**. |
| `cd dir` (permanent) | `cd dir` at session top level | Legal and journaled at session top level; **illegal inside a `fn` body** — error names `with cwd:` as the fix **(corpus** `reef.toml:reef-cd-inside-fn-body-is-illegal`, error `custom`, contains `"with cwd:"`**)**. |
| `(cd dir && cmd)` (scoped cd) | `with cwd: "dir" { cmd }` | Restores cwd on **any** exit path, including an error thrown inside the block **(corpus** `reef.toml:reef-cwd-restores-after-with-block`, `reef-cwd-restores-after-error-inside-with-block`, `reef-cwd-nested-with-blocks-restore-outer`**)**. |
| `FOO=bar cmd` (scoped env) | `FOO=bar cmd` (kept) or `with env: {FOO: "bar"} { cmd }` | Leading `IDENT=word` desugars to `with env: {NAME: "value"} { cmd }` (TDD §1.3); explicit block form restores after **(corpus** `reef.toml:reef-env-with-block-sets-var-during`, `reef-env-with-block-restores-after`**)**. |
| `test -f file`, `[ -f file ]` | *(unconfirmed — verify `path.*` method name before relying on it)* | TDD names a `path.*` namespace of methods generally; no corpus case names a specific existence-check method in the material reviewed for this card. Do not assume a spelling; check `docs/TDD.md` §5's namespace list or ask the running kernel via `complete`/`explain` if those ever land (§6 — currently unimplemented too, so today: try it and read the error). |
| `docker-compose up` (hyphenated command) | `^docker-compose up` or `run("docker-compose", "up")` | Hyphenated identifiers don't lex in EXPR mode; a hyphenated command name needs the `^` escape hatch or the fully-dynamic `run(name, args...)` form (TDD §2.3, §3.1.4). |
| `alias ll='ls -la'` | `alias gs = git status` | AST-level partial application — binds `gs` to a partial call node; `gs -sb` appends args to the AST, never text splicing (TDD §1.8). *Not verified against a corpus case in this pass — confirm behavior empirically.* |

---

## 3. The complete syntax

### 3.1 Lexical structure (TDD §2)

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

**CMD-mode word shapes** (TDD §2.2): a *word* begins `~/`, `./`, `../`, `/` → **path** literal
(bytes-backed, `~` expands now); contains unquoted `* ? [...] **` → **glob** literal; matches
`--ident(=...)?` or `-[A-Za-z0-9]+` → **flag**; is `IDENT=rest` at head position → **env-prefix**;
otherwise → **bare word**, type `str`. `(expr)` embeds a full EXPR expression as one argument. `>`
`>>` `<` are redirects; `&&`/`||` chain; trailing `&` backgrounds; a trailing `{` opens a thunk (a
literal-brace *argument* must be quoted).

**EXPR-mode**: conventional identifiers `[A-Za-z_][A-Za-z0-9_]*` — **no hyphens**. `-` is always
minus. Bare paths/globs don't lex in EXPR mode — use `path("...")`/`glob("...")` constructors or
string coercion.

### 3.2 The two-mode statement dispatch (TDD §3.1) — read this before writing any multi-line script

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
     `fn`/`alias` → adapter → PATH; unresolved = command-not-found with unified did-you-mean.
   - refinement: `X` immediately followed by `.` then an identifier (no whitespace) → EXPR statement,
     invoke-then-chain desugar: `ls.where(...)` ≡ `ls().where(...)`.
4. Escape hatches: `^X ...` forces command interpretation regardless of shadowing; `run("name",
   args...)` is the fully dynamic form. Shadowing a resolvable command with `let` is legal, linted,
   never an error — `^ls` always still reaches the real command.

### 3.3 Grammar reference (normative EBNF, TDD §3.2)

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

### 3.4 Desugaring table (TDD §3.4 — what you write vs. what actually runs)

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

### 3.5 Types (TDD §4.1)

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
  — it depends on whether the producer was a builtin or an adapter-backed external command.
- **`table`** is `list<record>` semantically — every table method is also a list method.
- Equality is **structural** for data types, **identity** for `task`/`stream`; comparing streams is
  an error.

### 3.6 Coercion — the whole matrix (TDD §4.2), corpus-verified exactly

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

### 3.7 Comparisons and logic (TDD §1.10, §3.3)

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
  1) { a + by }`, `inc(4)` → `5` (**corpus** `fn-default`).
- Lambdas: `x => expr` or `(a, b) => expr`/block. **(corpus** `core.toml:multi-lambda`,
  `lambda-call-method`**)**. Closures capture the *enclosing binding itself* (a shared cell, not a
  copy) — a `var` mutated by a closure through repeated calls accumulates across calls (**corpus**
  `closures.toml:closure-mutates-captured-var-via-each`).
- **Implicit `.field`/`.method` lambda sugar** — in **argument position only**: `.field <op> e`
  desugars to `x => x.field <op> e`, and `.method(args)` desugars to `x => x.method(args)`. This is
  exactly what makes `ls.where(.size > 1mb)` (TDD §1.4's own canonical example) and
  `ls.where(.name.contains("x"))` read the way they do — no explicit lambda parameter needed for the
  common case. A **bare `.field`** with no op/args also works and reaches a zero-arg **method** of
  that name when there's no such field: `["a","b"].map(.upper)`, `paths.map(.name)`,
  `[[],[1]].where(.is_empty)` (**corpus** `field-method-fallback.toml`). A real field always wins over
  a same-named method (user data first). This is why `path` accessors (`.name .stem .ext .parent
  .read .size .exists …`) read as fields inside a `.map(...)`.
- Recursion works normally: `fn fact(n: int) { if n <= 1 { 1 } else { n * fact(n - 1) } }` (**corpus**
  `closures.toml:recursive-fn-factorial`); a `fn`'s own parameter can be captured by a lambda defined
  inside it and returned (**corpus** `fn-returns-closure-capturing-param`).

### 3.9 `match` — every pattern kind (TDD §3.2 grammar)

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
- **Every command call yields an `outcome`.** At bare/statement rendering it shows as
  `outcome(status: 0, ok: true)` (**corpus** `outcome.toml:outcome-echo-render-inline`); `if (echo hi)
  { "yes" } else { "no" }` reads its truthiness from `.ok` automatically (`outcome-if-position`).

### 3.11 `cwd`/`env` scoping (TDD §4.6)

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
   untyped byte hoses; shoal composes typed values instead (VISION §2). *Proof*: `spec/cases/core.toml:parse-pipe-teaching`.
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
   runtime errors, curated *parse-time* diagnostics naming the modern replacement (IO.md §4). This is
   the same enforcement class as the pipe/`$`/backtick errors — the parser recognizes the box-era
   *shape* specifically so it can teach, not just reject.
9. **Size/duration arithmetic is asymmetric on purpose.** `size * float` is fine; `size / float` is
   `type_error`. `size ± int` (bare) is always `type_error` — both operands must be sized. Negative
   size results/multipliers are `type_error` with hint "negative"; only `size/int`, `size/size`,
   `duration/duration`, `duration/int` reach `div_zero` on an actual zero divisor. *Proof*: the full
   `coercion.toml` block cited in §3.6 above. Don't guess at this matrix — re-check the table.
10. **Streams are single-consumption.** A second consumption is a runtime error (`stream_consumed`,
    fix-it "collect first, or `.tee(2)`") — TDD §1.9. Streams **are implemented** (channels,
    `every(dur)`, `.map`/`.scan`/`.take`/`.collect` all work, §6) — so this rule bites now: don't read
    one twice.
11. **`it`/`out[n]` are REPL-only.** TDD's edge-case register (§13.16) makes this a parse error outside
    a REPL, with the fix-it "bind a variable." The kernel forces `evaluator.interactive = false` for
    **every** MCP-driven `exec` call (verified in `crates/shoal-kernel/src/lib.rs`) — treat `it`/`out`
    as **unavailable through this MCP surface entirely**. Always bind with `let`, or keep the
    returned `ref` and use `shoal_get`.
12. **A raised MCP error produces no transcript ref.** There is nothing to `shoal_get` afterward — the
    entry is only created on the success path (verified: `crates/shoal-kernel/src/lib.rs`'s `exec`
    dispatch only calls `journal.append`/creates `value_ref` *after* `eval_with_position` returns
    `Ok`). *Alternative*: send a **single bare expression** with `position: "value"` so a failing
    outcome is captured instead of raised, or wrap the risky part in `try { } catch e { e }` inside
    `src` so the caught error becomes the (successful) returned value.
13. **`position: "value"`'s capture behavior only applies to a single bare expression statement.** Any
    `src` with more than one statement (even a `let` followed by one command) always uses
    raise-on-failure semantics, whatever `position` says (verified in `eval_with_position`,
    `crates/shoal-kernel/src/lib.rs`). Keep single risky commands as standalone `shoal_exec` calls, or
    wrap multi-statement risk in `try/catch` inside the source.
14. **(P1 — was true at authoring, being fixed) The `render` field and the tool result's
    `content[0].text` were NOT elided or size-bounded** — only `structuredContent.value` (and its
    nested `Ref` elision) was. *Prior proof*: `crates/shoal-kernel/src/lib.rs`'s `exec` dispatch
    computed `render` from the *full* un-elided value before any budget check ran;
    `crates/shoal-mcp/src/lib.rs`'s `tool_result` pretty-printed the *entire* result (including that
    unbounded `render`) into `content[0].text`. This is one of the six named P1 fixes — **verify it
    against current source before assuming it's safe**; until you've confirmed the fix, keep treating
    both fields as unbounded and always read `structuredContent.value`/`.ref`, drilling in with
    `shoal_get`/resources rather than trusting `render`'s size.
15. **An elided value's embedded `uri` (`shoal://...`) may now be independently fetchable via
    `resources/read` (P1, §0.8)** — at authoring time no MCP `resources/*` method was implemented at
    all (`crates/shoal-mcp/src/lib.rs`'s `handle` dispatched only `initialize`, `ping`, `tools/list`,
    `tools/call`). If `resources/list` still 404s for you, fall back to translating the `uri`
    yourself: the part before `?path=` is the short `ref` you already have; the part after is the
    `path` argument to `shoal_get`.
16. **Background execution and task management may now be partly reachable via `shoal_cancel` (P1,
    §0.7)** — but confirm `shoal_exec`'s schema actually accepts a `background`/`timeout_ms` field
    before assuming you can *create* a cancellable task from this plugin in the first place; that
    wiring was not one of the six named P1 changes. Absent that, every `shoal_exec` call still blocks
    until the command finishes, and `shoal_cancel` has nothing of yours to act on.
17. **Hyphenated command names are not EXPR identifiers.** `docker-compose` needs `^docker-compose` or
    `run("docker-compose", args...)` (TDD §2.3).
18. **Shadowing a resolvable command with `let` is legal (linted, not fatal)** — but `^name` always
    still reaches the real command (TDD §3.1.4, §13.15). Don't assume a shadowed name is gone.

---

## 5. Error codes

Two different tables. Do not conflate them.

### 5.1 JSON-RPC transport codes (what you see in a failed MCP call's `structuredContent.code`)

Sourced directly from `crates/shoal-kernel/src/lib.rs`'s dispatch:

| code | meaning | `data` shape | recovery |
|---|---|---|---|
| `-32001` | **parse error** — `src` doesn't parse. No `data.code` string is set here (just `span`/`hint`) — infer "parse error" from this transport code itself. | `{span, hint}` | Fix the source per `hint`; re-check §2/§4's forbidden-spelling list first. |
| `-32002` | **evaluation error** — parsed fine, failed at runtime. `data.code` is the real shoal error code string (§5.2). | `{code, span, hint, status, stderr}` | Branch on `data.code`; see §5.2. |
| `-32004` | unknown value `ref` passed to `value.get`/`shoal_get` (stale, wrong session, or never existed) | `{}` | Re-`shoal_exec` to get a fresh ref; refs don't survive kernel restarts. |
| `-32005` | bad field `path` in `value.get`/`shoal_get` (no such field/index, or path syntax error) | `{ref, path}` | Check §0.2's path grammar; no negative indices, no `[a..b]` ranges. |
| `-32010` | leash **denied** execution, or a `plan_ref` belongs to a different session/principal | `{effects}` | Under the default permissive policy this should not happen; if it does, the kernel was started with a stricter `--policy`. |
| `-32011` | **approval required** (a plan's verdict) or **approval still pending** (on `shoal_apply`) | `{effects}` | Call `shoal_cap_request {plan_ref}`, then `shoal_apply {plan_ref}`. |
| `-32012` | unknown `plan_ref` (never created, or the kernel restarted — plans are in-memory, not journaled) | `{}` | Re-derive with `shoal_plan`. |
| `-32020` | task suspension requested — **always** returned; not implemented | `{task}` | Don't call `task.suspend` (not reachable via MCP tools anyway). |
| `-32021` | unknown task ref | `{}` | Possible via `shoal_cancel` **(P1, §0.7)** with a stale/wrong task ref — re-check §4 rule 16 for whether you can even create a task ref through this plugin before assuming this path is reachable. |
| `-32030` | bearer token missing/invalid/expired, or tokens unavailable on an ephemeral (`Kernel::new()`) kernel | `{}` | Check `SHOAL_TOKEN`; ensure the kernel was started with a state dir (`shoal-kernel` without `--socket`-only ephemeral mode). |
| `-32600` | invalid JSON-RPC request/version | — | Transport bug — should not occur through this plugin's tools. |
| `-32601` | method not found | `{method}` | You (or a future card revision) called something the kernel doesn't dispatch — see §6's unimplemented-methods list (`complete`, `explain`, `events.*`, `task.resume`, any `resources/*`). |
| `-32602` | invalid params (missing required field, wrong shape) | — | Check the tool's exact schema in §0. |
| `-32603` | internal error | — | Not a language-level problem; report it. |

### 5.2 Language-level error codes (`data.code` on a `-32002`, per `docs/CONTRACTS.md` §4)

Pinned registry. **✓** = directly exercised by a named corpus case; no mark = pinned but not
individually corpus-exercised in the material reviewed for this card (still authoritative — CONTRACTS
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
| `recursion_limit` | recursion/loop depth exceeded (depth 10k, TDD §13.12) | restructure; loop limit is off in script mode |
| `reef_unlocked` ✓ | a `with reef:`-constrained tool used in a non-interactive/script context without a lock | `reef.toml:reef-with-reef-constrains-a-spawn-inside-the-block` |
| `reef_drift` | resolved binary's hash no longer matches the lock | `reef lock --refresh` (REEF.md §2; not verified reachable in this pass) |
| `reef_conflict` | two reef scopes constrain one tool incompatibly | (not verified reachable in this pass) |
| `reef_not_found` | a reef-constrained tool has no resolvable candidate | (not verified reachable in this pass) |
| `reef_provider` | a reef provider itself failed | (not verified reachable in this pass) |
| `feed_error` | `.feed` **is implemented**; fires on feeding a never-feedable type (`secret`/`task`/`closure`/`error`/`glob`/`regex`) | feed a serializable value (str/bytes/list/record/table) instead |
| `lang_block_unbalanced` | interpreter blocks **are implemented**; an unterminated brace in a `python { … }`/`jq { … }`/etc. block | balance the braces in the block payload |
| `runner_not_found` | reef `run <path>` extension/shebang resolution (reef integration still partial — verify against source) | n/a for most flows |
| `stream_unbounded` | **implemented and correct** — you `.collect()`d a stream with no natural end | bound it first with `.take(n)`/`.take_until(…)`, or use `.each(f)` |

---

## 6. Implementation status — what works, what to skip

Stated plainly so you never waste a turn. **This card was first written against an early build and
over-reported "not implemented"** — `.feed`, interpreter blocks, streams/channels, and all six MCP
`(P1)` items were verified working against the current source and are now marked done. The
genuinely-still-missing items are: background/async exec via MCP, per-call elision tuning, `capture`/
`timeout` on `shoal_exec`, `complete`/`explain`, and real OS-level sandbox enforcement. When in doubt,
run a one-line probe rather than trusting a stale banner.

- **DONE — The MCP `resources/*`/events subsystem.** `crates/shoal-mcp/src/lib.rs`'s `handle()` now
  dispatches `resources/list`/`read`/`subscribe`/`unsubscribe`, and `initialize` advertises
  `capabilities.resources.subscribe = true`; event notifications forward as
  `notifications/resources/updated` (`client.rs::run_event_forwarder`). Use `resources/read` on a
  `shoal://…` uri to drill into an elided value directly — the `shoal_get`+manual-URI translation
  (§0.2, §4 rule 15) is now just a fallback. Confirmed by the live e2e test
  `crates/shoal-mcp/tests/live_kernel.rs`.
- **DONE — `shoal_cancel`.** Present in `tools()` (seven tools now). Note `task.suspend` still errors
  (unimplemented even over raw JSON-RPC), and creating a cancellable task still needs the
  background-exec path below, which is *not* yet wired through MCP.
- **Background/async execution via MCP is still presumed missing** (not one of the six named P1
  fixes) — the kernel's `exec` supports an `async`/`background` flag that spawns a trackable task,
  but as of authoring `shoal_exec`'s MCP schema never forwarded it. Confirm the schema directly
  (`tools/list`'s `shoal_exec` entry) before assuming you can create a task to hand to `shoal_cancel`.
- **Per-call elision tuning via MCP** — the kernel's `value.get`/`exec` both accept an `elide` budget;
  neither `shoal_get` nor `shoal_exec`'s MCP schema exposes it.
- **`shoal_exec`'s `capture`/`timeout` params** — accepted by the tool schema, forwarded to nothing.
- **`shoal_journal`'s `until`/`effects` filters** — accepted by the tool schema, forwarded to nothing;
  conversely its real `ok` filter isn't exposed by the schema at all.
- **`complete` and `explain`** JSON-RPC methods (typed completions, structured explanations) —
  `docs/TDD.md` §7 and `docs/AGENT-SURFACE.md` §5 both name them; the kernel's `dispatch` has no case
  for either — calling them 404s with `-32601`.
- **~~`.feed` and interpreter blocks~~ — NOW IMPLEMENTED (this card's original banner was stale).**
  Verified working against the current binary: `["b","a","c"].feed(sort).out`, and **commands with
  args/flags parse bare** — `["b","a","c"].feed(sort -r).out`, `data.feed(grep "foo").out`,
  `{a:1}.feed(jq ".a").out` (the argument parses in CMD mode when it starts with a command head; the
  inverted `cmd.feed(value)` form still parses its arg as a value). Interpreter blocks
  `python { print(6*7) }.out`, `jq { .a }`, `sh { sort -r }` work as feed targets too —
  `{a:1,b:2}.feed(jq { .a }).out` → `1`. An interpreter block's stdout **auto-parses to a structured
  value** on `.out` (`python { import json; print(json.dumps({"n":42})) }.out` → the record `{n: 42}`).
  Heredocs stay gone; this is their replacement, and it works.
- **Reactive streams — SUBSTANTIALLY IMPLEMENTED (card's original "pending" banner was stale).**
  Verified working: `channel(name)` with `.emit(v)`/`.events()`/`.latest()`; `every(dur)`; stream
  pipelines `every(10ms).take(3).collect()` → 3, `.map`, `.scan(init, f)`; and the `stream_unbounded`
  guard fires correctly when you `.collect()` an unbounded stream (bound it with `.take(n)` first).
  `watch(...)`/`tail(...)` exist. Before relying on a *specific* combinator (`.debounce .throttle
  .window .merge .buffer .dedupe .distinct`) or sink (`.into(channel)`, `on channel(...) { }`), test it
  once — coverage is broad but this card no longer claims to enumerate exactly which are live.
- **Most of reef's real surface**: `.reef.toml` project-scope walking, `[runners]`-based `run
  <path>`/bare-path resolution, `reef add/lock/fetch/doctor`, drift detection. `docs/REEF.md`'s status
  banner: *"crate built+tested; eval integration landing this wave."* What **does** work today,
  corpus-confirmed: `which "name"` (only `.name` is host-independent — the rest of the record depends
  on the host's tool inventory), `with reef: { } { }` override + restore, and the `reef_unlocked`
  error path.
- **Real OS-level sandboxing (leash tiers A/B/C).** `session.attach` always reports `{"enforced":
  false, "tier": "D"}` in this codebase today, regardless of platform — Landlock/seccomp/Seatbelt
  enforcement is not wired into the code path this plugin talks to. The policy engine's *logical*
  allow/deny/approval_required decisions are real; the *sandbox* backing them is not, yet.
- **TUI-only affordances**: statement-position PTY passthrough (color, progress bars), Ctrl-C/Ctrl-Z
  job control, live-rendering streams at the prompt, `pick()`/`interact`. The kernel forces
  `evaluator.interactive = false` for every exec dispatched through this surface — MCP execution is
  **always** headless/capture-mode. Do not expect (or try to request) a colorized/interactive run.
- **Content-addressed `val:blake3:...` refs.** The kernel only mints session-scoped `out:N` transcript
  refs today.
- **`alias`, journal `undo` replay** — not exercised in the corpus/source reviewed for this card;
  treat as unconfirmed rather than assuming either the spec'd behavior or its absence.

---

*This card is derived entirely from `docs/TDD.md`, `docs/VISION.md`, `docs/REEF.md`,
`docs/AGENT-SURFACE.md`, `docs/IO.md`, `docs/STREAMS.md`, `docs/CONTRACTS.md`, the 328-case corpus at
`spec/cases/*.toml`, and a direct read of `crates/shoal-mcp/src/lib.rs`, `crates/shoal-proto/src/lib.rs`,
`crates/shoal-kernel/src/lib.rs` at authoring time — a moment when a concurrent change was landing
`resources/*`, events, `shoal_cancel`, render/text elision, a macOS socket fallback, and a real
`reversibility` value (every item marked **(P1)** above). If shoal's implementation changes, re-derive
— never patch this card from memory, and re-read the six `crates/shoal-mcp`/`crates/shoal-kernel`
call sites named throughout §0 before trusting a **(P1)** item as landed.*
