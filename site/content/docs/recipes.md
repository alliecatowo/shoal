+++
title = "Recipes"
description = "Practical Shoal patterns for files, structured data, external commands, HTTP, streams, modules, Reef, journaling, and agents."
weight = 130
template = "docs/page.html"

[extra]
eyebrow = "Cookbook"
group = "Shell & tools"
audience = "Users moving from examples to real scripts"
status = "Current language patterns"
toc = true
+++

These recipes favor typed values and explicit failure handling. Adapt paths/URLs before running, and plan or inspect any mutation in a real environment.

## Find the largest files in a directory

```text
(ls ./target)
  .where(.type == "file")
  .sort_by(.size)
  .reverse()
  .take(20)
  .map({path: .path, size: .size, modified: .modified})
```

`size` stays a typed byte quantity, so comparisons/rendering are not lexicographic text tricks:

```text
(ls ./downloads)
  .where(.type == "file" and .size >= 100mb)
  .map(.path)
```

## Summarize file types

```text
let entries = (ls .)
{
  total: entries.len(),
  files: entries.count(.type == "file"),
  dirs: entries.count(.type == "dir"),
  symlinks: entries.count(.type == "symlink"),
}
```

For grouping:

```text
(ls .).group(.type)
```

## Read a file as text, lines, or bytes

```text
let p = path("./notes.txt")
p.read
p.lines
p.read_bytes
```

Use bytes for exact content, text for Unicode/string methods, and lines for line-oriented transforms.

## Safely replace a file and retain undo metadata

```text
let target = path("./generated/config.json")
json.stringify({enabled: true, workers: 4}, pretty: true).save(target)
```

In a journal-enabled local session, overwriting an existing file can record its prior bytes and post-fingerprint. Inspect immediately:

```text
journal --head=save --limit=5
```

Then, if no later change made the target stale:

```text
undo
```

This is not a transaction: creating a previously absent file does not currently record a delete inverse, and large prior content may exceed the journal cap.

## Use recoverable removal instead of permanent deletion

```text
let removed = (rm ./scratch.txt)
removed
```

Default `rm` moves the target to Shoal's temporary trash and returns `{path, trash}` records. Verify the journal, then:

```text
undo
```

Avoid `rm --permanent` when recovery matters.

## Parse and reshape JSON

```text
let doc = json.parse('{"users":[{"name":"Ada","active":true},{"name":"Lin","active":false}]}')
doc.users
  .where(.active)
  .map(.name)
```

Serialize a compact or pretty result:

```text
json.stringify({names: doc.users.map(.name)})
json.stringify({names: doc.users.map(.name)}, pretty: true)
```

## Convert CSV to JSON

```text
let rows = csv.parse("name,count\napi,3\nweb,2\n")
json.stringify(rows, pretty: true)
```

CSV fields are strings. Convert explicitly when numeric behavior matters:

```text
rows.map({name: .name, count: .count.int()})
```

## Convert records to CSV

```text
csv.stringify([
  {name: "api", count: 3},
  {name: "web", count: 2},
])
```

Keep record keys consistent so the table schema is predictable.

## Handle an expected external-command failure

```text
let result = (^git rev-parse --verify refs/heads/release)
if result.ok {
  {exists: true, commit: result.out}
} else {
  {exists: false, status: result.status, stderr: result.stderr}
}
```

Parentheses place the command in value position. Without capture at statement position, a nonzero outcome raises `cmd_failed`.

## Preserve exact argv boundaries

```text
let filename = "report with spaces.csv"
run("printf", "%s\n", filename)
```

Shoal values become arguments; there is no implicit whitespace splitting of `filename`. Use `run` when the command name is dynamic:

```text
let tool = "git"
run(tool, "status", "--short")
```

## Diagnose an adapter

Compare adapted and forced-native paths:

```text
git status --short
^git status --short
```

If the first produces a structured table and the second text/bytes, the adapter is active. If adapted parsing fails after a tool upgrade, capture the native output/version and update/pin the adapter contract.

## Make an HTTP JSON request

```text
let response = http.get(
  "https://api.example.test/v1/items",
  headers: {Accept: "application/json"},
)

if response.ok {
  response.json
} else {
  {status: response.status, body: response.body}
}
```

HTTP 4xx/5xx returns a response record; it does not raise. Transport/timeout/body-read errors raise `net_error`.

POST JSON:

```text
let response = http.post(
  "https://api.example.test/v1/items",
  json.stringify({name: "demo"}),
  headers: {
    Accept: "application/json",
    "Content-Type": "application/json",
  },
)
```

## Inject a secret into an HTTP header

First set it outside the language without printing it:

```bash
printf %s "$API_TOKEN" | shoal-secret set api-token
```

Then:

```text
let token = secret.get("api-token")
http.get(
  "https://api.example.test/v1/me",
  headers: {Authorization: token},
)
```

The typed secret is redacted in ordinary rendering/wire encoding. The remote server/HTTP stack receives it; downstream output can still leak it. See [Security](@/docs/security.md#secrets).

## Run work in a temporary directory/environment scope

```text
with cwd: path("./crates/shoal"), env: {RUST_LOG: "debug"} {
  cargo test
}
```

Prefer scoped state over a global `cd`/environment mutation inside reusable code.

## Build a reusable module

`lib/report.shl`:

```text
export fn summarize(rows) {
  {
    total: rows.len(),
    failed: rows.where(not .ok).len(),
  }
}
```

Caller:

```text
use ./lib/report
report.summarize(results)
```

Use `as` when the file-stem namespace would collide:

```text
use ./lib/report as reports
```

## Bound a timer stream

```text
every(1s).take(5).collect()
```

Live/infinite sources need a bound or a long-running sink. Never call `.collect()` on an unbounded stream without `take`/termination.

## Watch a directory and rescan after coalescing

```text
watch(path("./src"), recursive: true)
  .where(.kind == "modified")
  .take(20)
  .collect()
```

The watch queue is bounded. If an event reports `coalesced: true`, treat it as “changes occurred” and rescan authoritative state rather than assuming every filesystem event was preserved.

## Tail and filter a log

```text
tail(path("./service.log"))
  .where(line => line.contains("ERROR"))
  .take(20)
  .collect()
```

Use a bound in short scripts. A persistent monitoring task can use `.each` and cancellation instead.

## Connect a stream to a user channel

```text
watch(path("./src"))
  .into(channel("user.files"))
```

Consume:

```text
channel("user.files").events().take(10).collect()
```

Only `user.*` channels bridge to kernel clients; language code cannot spoof kernel-owned `journal`/`approval` channels.

## Register a channel handler

```text
let handler = on(channel("user.builds"), event => {
  echo (event)
})
```

`on(...)` is a function call in the current grammar; there is no `on channel {}` keyword form. Remember the current nested-evaluator policy propagation limitation before using handlers for scoped agent work.

## Start and observe a language task

```text
let tests = spawn { cargo test }
tests.is_done()
tests.await()
```

Or trailing ampersand:

```text
sleep 30s &
jobs
```

Cancellation:

```text
tests.cancel()
```

Cancellation is not rollback; inspect partial effects.

## Plan a mutation before running it

```text
plan cp ./report.csv ./archive/report.csv
```

or a block:

```text
plan {
  mkdir --parents ./archive
  cp ./report.csv ./archive/report.csv
}
```

Review concrete effects and reversibility. In the agent/kernel surface, use `shoal_plan` and `shoal_apply`; read the current security limitations around approval and plan refs first.

## Create a reproducible Reef scope

Project `.reef.toml`:

```toml
[tools]
rg = "*"
jq = "1"
```

Then interactively:

```text
reef status
reef lock
which rg
which jq
```

Commit the lockfile before expecting script policy to accept constrained tools. Set hermetic scope only when all child tools/interpreters are declared.

## Page a large value through MCP

Execute once:

```json
{
  "src": "(ls ./large-directory)",
  "position": "value",
  "elide": {"max_rows":20,"max_bytes":4096}
}
```

If the result is `out:17`, page without rerunning:

```json
{"ref":"out:17","slice":[0,100]}
```

```json
{"ref":"out:17","slice":[100,200]}
```

or:

```text
shoal://out/17?slice=100..200
```

## Run a long MCP action without blocking context

```json
{
  "src":"cargo test --workspace",
  "position":"value",
  "background":true
}
```

Subscribe/read `shoal://task/N`, then fetch `shoal://task/N/out` after completion. A timeout similarly returns a task; it does not kill execution.

## Drive a small REPL through PTY

Open:

```json
{"cmd":"python3","args":["-q"],"cols":80,"rows":24}
```

Read the screen, then send:

```json
{
  "pty_id":"pty:1",
  "input":["print(6 * 7)",{"key":"Enter"}]
}
```

Poll with delay until the expected prompt/`42` appears, then close. Prefer ordinary `shoal_exec` when terminal behavior is unnecessary.

## Retry a read, not an ambiguous mutation

Safe pattern after a lost response:

```text
1. query journal/transcript event cursor
2. inspect intended artifact/remote idempotency key
3. decide whether operation completed
4. only then retry or compensate
```

Blindly retrying `cp`, deployment, payment, or API mutation can duplicate effects. Addressable refs and the journal exist to support reconciliation.

For more translation patterns, see [Migrating from traditional shells](@/docs/migration-from-shells.md).
