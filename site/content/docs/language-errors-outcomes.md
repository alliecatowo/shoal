+++
title = "Outcomes and errors"
description = "Handle command status without losing output, understand statement versus value position, recover typed errors, and propagate failures intentionally."
weight = 80
template = "docs/page.html"

[extra]
eyebrow = "Language guide"
group = "Language"
audience = "Script and automation authors"
status = "Current evaluator"
toc = true
+++

Shoal separates two related ideas:

- an **outcome** is the result of a command invocation, including status, output, and metadata;
- an **error** is a control-flow failure that stops evaluation unless caught.

A non-zero external status always exists in an outcome. Whether it becomes an error immediately depends on where the command appears.

## Outcome anatomy

```text
let result = (^printf '{"answer":42}\n')

result.status    # int or null
result.ok        # bool
result.signal    # signal name or null
result.dur       # duration
result.pid       # int
result.cmd       # display form
result.stdout    # bytes or lazy CAS-backed bytes
result.stderr    # bytes
result.out       # semantic payload on success
result.err       # stderr bytes on success; failed payload access raises
```

Raw stdout is always retained up to the host's capture/spill policy. `out` is the semantic payload: an adapter may supply a typed value, or raw output may become parsed JSON when it has a JSON object/array shape. Otherwise it is decoded text with one trailing newline removed.

On a failed outcome, `.stdout` and `.stderr` remain inspectable, while `.out`, `.err`, or forwarded semantic field access raises `cmd_failed`. Check `.ok` first when failure is expected.

```text
let probe = (^sh -c 'echo detail >&2; exit 7')
if !probe.ok {
  { status: probe.status, stderr: probe.stderr.str() }
}
```

## Statement position versus value position


```text
^false                 # statement position: raises cmd_failed
let probe = (^false)   # value position: binds a failed outcome
(^false).ok            # value expression: false
```

An unredirected statement can write stdout before it fails. Shoal preserves
those bytes through the normal output sink and then reports `cmd_failed`, like
traditional shells. PTY output and redirected stdout are never printed a
second time.

Parentheses are the clearest way to place a command inside an expression. This rule lets ordinary interactive failures stop the line while allowing probes, retries, fallbacks, and exact status propagation.

Most structured builtins also return successful outcomes so external and builtin transformations can chain similarly. Evaluator-native session operations such as `pwd` can return a bare value.

## Forwarding through `out`

Successful outcomes forward unknown fields and methods to their semantic payload:

```text
(ls .).where(.type == "file").sort_by(.size)
(stat ./Cargo.toml).size
```

This is why builtins can feel like direct tables or records in a chain. Explicit `.out` remains useful at boundaries and is required before indexing:

```text
let result = (some_json_command)
result.out[0]
```

Forwarding never hides status fields such as `.ok`, `.status`, or `.stderr`.

## Boolean chaining with outcomes

`&&` and `||` accept booleans or outcomes. They short-circuit and return the operand that determined the result, unchanged.

```text
(^test -f ./Cargo.toml) && "manifest exists"
(^git diff --quiet) || "working tree changed"

let final = (^step_one) && (^step_two)
final.status
```

Command operands in a statement chain render each executed command once. In value position, the chain remains an inspectable result rather than narrowing everything to `true`/`false`.

Do not confuse `??` with error recovery. It only coalesces `null`:

```text
config.get("port") ?? 8080
```

## Catch a block

`try` evaluates a block and runs its handler only when the block raises:

```text
let config_value = try {
  json.parse(path("config.json").read)
} catch err {
  { failed: true, code: err.code, message: err.msg }
}
```

The catch binder is optional:

```text
try {
  risky()
} catch {
  null
}
```

The handler has a child scope. An error handled there does not erase unrelated outer bindings.

## Postfix recovery

For a local fallback, use postfix `catch`:

```text
let port = env.PORT.parse_int() catch 8080
let body = path("optional.json").read catch "{}"

let report = parse_report()
catch err { { code: err.code, message: err.msg } }
```

The handler can be an expression or block, with an optional binder between `catch` and the handler. A following-line `catch` attaches as a continuation.

## Error fields

A caught error exposes:

| Field | Type | Meaning |
|---|---|---|
| `code` | `str` | machine-oriented category |
| `msg` | `str` | human-readable detail |
| `hint` | `str` or `null` | suggested corrective action |
| `stderr` | `str` or `null` | captured diagnostic text when attached |
| `status` | `int` or `null` | originating external status when attached |

Source spans are used by host diagnostics and wire encodings where available, but the current in-language caught-error accessor does not expose a `.span` field.

Common currently emitted codes include:

| Family | Codes you will commonly handle |
|---|---|
| Syntax/binding | `parse_error`, `undefined_var`, `arg_error`, `type_error` |
| Data/access | `field_missing`, `index_range`, `utf8_error`, `overflow`, `div_zero`, `collection_materialization_limit`, `string_materialization_limit`, `data_materialization_limit`, `cas_materialization_limit`, `path_read_limit`, `builtin_output_limit`, `builtin_work_limit` |
| Commands/host | `not_found`, `cmd_failed`, `io_error`, `net_error`, `permission`, `timeout` |
| Streams/channels | `stream_consumed`, `stream_unbounded`, `stream_collect_limit`, `stream_distinct_limit`, `stream_window_limit`, `stream_line_limit`, `range_materialization_limit`, `range_length_overflow`, `channel_closed`, `channel_poisoned`, `channel_name_limit`, `channel_registry_limit`, `channel_subscriber_limit`, `channel_payload_limit`, `channel_payload_type`, `feed_error` |
| Reef | `reef_unlocked`, `reef_drift`, `reef_conflict`, `reef_not_found`, `reef_provider` |
| Control | `assert_failed`, `recursion_limit`, `stale_undo` |

This is an operational list, not a promise that no new code will be added during the preview. Branch on a code only when recovery genuinely differs; otherwise preserve the error or report its message and hint.

```text
try {
  path("settings.toml").read
} catch err {
  match err.code {
    "not_found" => ""
    "permission" => { return { error: "cannot read settings" } }
    _ => { return { error: err.msg } }
  }
}
```

## Assertions

`assert(condition, message?)` returns `null` when the condition succeeds and raises `assert_failed` otherwise.

```text
assert(args.len() >= 1, "usage: deploy.shl ENV")
assert((^git diff --quiet), "working tree must be clean")
```

Assertions are appropriate for preconditions and invariants. For expected user choices, return a structured result or catch a narrower operation instead.

## Preserve an external status

An uncaught statement-position external failure carries its status into the non-interactive host, which propagates statuses `1..=255`; absent, signal-only, or out-of-range statuses fall back to `1`. Capture and exit explicitly only when you need to inspect output or transform/control the code:

```text
let result = (^tool --check)
if !result.ok {
  echo (result.stderr.str())
  exit (result.status)
}
```

If a process died by signal, `status` may be null and `signal` contains its name. Choose an application-specific fallback exit code in that case.

## Rendering and duplicate output

An interactive external command can run through a real PTY and tee bytes to the terminal. The final renderer marks that outcome as already streamed so it does not print the same payload twice. Captured command values and builtins have not streamed their value, so rendering their outcome remains necessary.

Scripts and `-c` use captured/non-interactive behavior. Never parse the human outcome rendering; read typed fields.

## Capture limits and content references

Shoal bounds resident process capture. In journal-enabled hosts, oversized stdout can spill to content-addressed storage, and `.stdout` becomes a lazy ref-backed bytes value with the correct total length. Data sinks such as redirects and save operations load the full content rather than writing only the resident preview.

Defaults and environment overrides are documented in [Current status and limits](@/docs/status-limits.md). Agent wire responses have a separate 64 KiB rendering cap and return session/content references for larger values; see [Agents, kernel, and MCP](@/docs/agents-kernel-mcp.md).

## Recovery design checklist

When a command may legitimately fail:

1. Put it in value position.
2. Inspect `.ok` before touching `.out`.
3. Preserve `.status`, `.signal`, and `.stderr` in diagnostics.
4. Use `&&`/`||` only when their returned-operand semantics are useful.
5. Catch evaluator errors narrowly; do not wrap an entire script when only parsing may fail.
6. Propagate exact external status explicitly when another program depends on it.
