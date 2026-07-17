+++
title = "Troubleshooting"
description = "Diagnose installation, parsing, command resolution, adapters, Reef, configuration, journals, kernel/MCP, tokens, tasks, events, PTYs, and platform-specific failures."
weight = 260
template = "docs/page.html"

[extra]
eyebrow = "Operations"
group = "Project"
audience = "Shoal users, operators, and agent integrators"
status = "Current preview diagnostics"
toc = true
+++

Start by identifying the failing layer. Shoal can run as a standalone REPL/script host, a long-lived kernel, or an MCP facade; those processes do not automatically share state.


## Capture a useful diagnostic bundle

From the repository or installed environment:

```bash
shoal --version
command -v shoal shoal-kernel shoal-mcp shoal-lsp
type -a shoal shoal-kernel shoal-mcp shoal-lsp
shoal doctor --json >shoal-doctor.json
printf 'doctor exit=%s\n' "$?"
```

Also record, without exposing secrets:

```bash
printf 'HOME=%s\n' "$HOME"
printf 'XDG_CONFIG_HOME=%s\n' "${XDG_CONFIG_HOME-}"
printf 'XDG_STATE_HOME=%s\n' "${XDG_STATE_HOME-}"
printf 'XDG_DATA_HOME=%s\n' "${XDG_DATA_HOME-}"
printf 'XDG_RUNTIME_DIR=%s\n' "${XDG_RUNTIME_DIR-}"
printf 'TMPDIR=%s\n' "${TMPDIR-}"
printf 'SHOAL_SESSION=%s\n' "${SHOAL_SESSION-}"
printf 'SHOAL_SOCKET=%s\n' "${SHOAL_SOCKET-}"
```

Do **not** print `SHOAL_TOKEN`, secret values, the entire environment, `tokens.json`, or `master.key` into an issue/log.

When reporting language behavior, include a minimal source snippet and whether it ran through:

```text
shoal -c
shoal SCRIPT.shl
interactive shoal
shoal_exec over MCP
raw kernel exec
```

Position (`stmt` versus `value`) matters for failures.

## Build and installation

### `cargo build` fails because Rust is too old

Shoal uses Rust edition 2024 workspace crates. Update the stable toolchain:

```bash
rustup update stable
rustup override set stable
rustc --version
cargo --version
cargo build --locked --workspace
```

If a workspace lock/dependency failure remains, include the exact compiler and first error; later messages are often cascading.

### `shoal: command not found`

For a source build:

```bash
cargo build --release -p shoal
./target/release/shoal --version
export PATH="$PWD/target/release:$PATH"
```

For Cargo install:

```bash
cargo install --path crates/shoal
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
rehash 2>/dev/null || true
```

`rehash` is useful in zsh after a new executable appears; it is harmlessly skipped elsewhere.

### `shoal lsp` or `shoal mcp` cannot launch companion

The main dispatcher asks the OS to find `shoal-lsp`/`shoal-mcp` through `PATH`; it does not search beside `shoal`.

```bash
command -v shoal-lsp
command -v shoal-mcp
cargo install --path crates/shoal-lsp
cargo install --path crates/shoal-mcp
```

If you built the workspace but installed only `shoal`, either add `target/release` to `PATH` or install each companion package. See [Companion CLI reference](@/docs/companion-cli-reference.md).

### Sandbox helper missing

Error resembles:

```text
shoal-sandbox-exec helper not installed beside executable
```

The spawn layer searches beside the current executable. Build/install it into the same bin directory:

```bash
cargo build --release -p shoal-exec --bin shoal-sandbox-exec
cargo install --path crates/shoal-exec
```

Confirm sibling paths:

```bash
dirname "$(command -v shoal)"
dirname "$(command -v shoal-sandbox-exec)"
```

Do not work around this by disabling an intended security policy without understanding the exposure.

## Shell starts but commands behave unexpectedly

### “Why is this parsed as a command?”

An unbound word at statement head is command-shaped. Bound values/functions use expression dispatch. Make the boundary explicit:

```text
let files = (ls .)
files.len()
(ls .).where(.type == "file")
```

Use [The command/expression model](@/docs/mental-model.md). Do not assume Bash `$var` or command substitution:

```text
let name = "shoal"
echo (name.upper())
```

### A command is shadowed by a binding/function/alias

Inspect the collision and force external execution:

```text
which NAME
^NAME args
run("NAME", "arg")
```

`^` bypasses an external adapter but still participates in Shoal's forced-head semantics; `run` is the unambiguous dynamic external form. If Reef constrains the tool, inspect `reef status` and `which NAME` first.

### Nonzero command raised instead of returning an outcome

Statement position raises a failed outcome. Capture it in value position:

```text
let probe = (^false)
probe.ok
probe.status
```

For MCP, set:

```json
{"src":"^false","position":"value"}
```

In multi-statement source, earlier statements retain statement semantics; bind expected failures explicitly.

### External output is text, not structured

Possible causes:

- no adapter matched the command head;
- the forced `^`/dynamic `run` path bypassed the adapter;
- upstream output was not valid detected JSON/YAML/CSV/etc.;
- the adapter parser failed or version/locale/color changed output;
- `SHOAL_ADAPTER_PATH` replaced the default catalog.

Compare:

```text
command args
^command args
```

The first should use an adapter when available; the second shows native behavior. Read [Command adapters](@/docs/adapters.md).

### `adapter_parse` or missing columns

Capture the exact upstream version and raw output without secrets:

```bash
command --version
LC_ALL=C NO_COLOR=1 command ... >raw-output.txt
```

Then compare the adapter's parser strategy/schema. Do not silently fall back to model-side whitespace splitting for consequential data. Update the adapter fixture or pin the expected upstream version.

### `^` did not mean “Bash”

`^head` forces an external command head; it does not turn the rest of the line into Bourne syntax. For a legacy fragment:

```text
sh {
  printf '%s\n' "$HOME"
}
```

Use the escape hatch deliberately and remember that its inside is text/legacy process semantics, not typed Shoal expressions.

## Parse errors and diagnostics

### Incomplete input in the REPL

The interactive host can continue multiline constructs. A script/`-c` invocation must receive the complete block. Check delimiters and match/function syntax:

```text
fn classify(x) {
  match x {
    0 => "zero",
    _ => "other",
  }
}
```

Run the formatter as a parser check:

```bash
shoal fmt --check file.shl
```

### LSP shows a parse error but formatting does nothing

Formatting is available only for a complete parse. Fix the single published syntax diagnostic, save, then format. The LSP does not currently offer recovery code actions.

### Error span looks like bytes, not characters

Kernel/language spans are UTF-8 byte offsets. LSP converts them to UTF-16 positions for editors. A raw client must perform the same conversion rather than treating offsets as Unicode scalar indexes.

## Configuration and keybindings

### Config appears ignored

Check the selected files and syntax:

```bash
ls -l "${XDG_CONFIG_HOME:-$HOME/.config}/shoal/shoal.toml"
shoal doctor
```

Unknown keys produce warnings; fix them rather than assuming they are future-compatible. Configuration is loaded at process startup, so restart the REPL/kernel after edits.

Use [Configuration and prompt](@/docs/configuration-prompt.md) for precedence and exact validation.

### Keybinding warning or no action

Chord names and action names have fixed parsers. Check modifier spelling, named key spelling, and action aliases in [Keybinding reference](@/docs/keybindings-reference.md). Unknown entries warn and are skipped; the rest of the config still loads.

If the terminal intercepts a chord (window manager, terminal multiplexer, OS shortcut), Shoal never receives it. Test a simpler chord and inspect terminal/tmux settings.

### Persistent line history is missing

Line history and structured execution journal are different stores. Verify `[history].enabled`, configured path, XDG state directory, permissions, ignore-space, dedup, and ignore patterns. A command beginning with space can be intentionally excluded.

## Reef and tool resolution

### `which tool` reports a constraint but no binding

The manifest can constrain a tool while the lock has no resolved entry. Run:

```text
reef status
reef lock
which tool
```

Under script policy, an unlocked constrained tool is an error. Interactive resolution may lock if a provider succeeds.

### Manifest edit is not noticed

The evaluator caches the discovered scope chain. Manual edits while remaining in the same cwd may stay stale. Trigger one of:

```text
cd ..
cd -
```

or restart the session. `reef add` explicitly invalidates the cache. This is a current limitation; file watching is roadmap work.

### Tool works interactively but script says unlocked

Interactive policy can resolve/write a lock; script policy requires reproducibility and rejects an unlocked constrained tool. Run `reef lock` intentionally, inspect/commit the lockfile, then rerun the script.

### Hermetic Reef cannot find ordinary tools

Hermetic Reef removes the ambient PATH tail. Declare/lock every required tool, including interpreters invoked by scripts. `hermetic` is PATH isolation only; it does not grant filesystem/network policy.

### Provider/download fails

Check:

- manifest constraint syntax;
- provider availability/login/network;
- cache/state permissions;
- exact locked version/platform;
- whether another nearer scope overrides the tool;
- whether Leash denies network/filesystem/spawn.

Read the structured error instead of deleting the lock reflexively. A lock mismatch can be an integrity signal.

### Nested `spawn` resolves differently

Nested evaluators do not consistently inherit parent Reef/Leash context in the current preview. This is a security/reproducibility defect. Avoid scoped-agent nested execution and use OS process isolation; do not “fix” it by widening policy. See [Security](@/docs/security.md#nested-evaluator-policy-propagation-is-incomplete).

## Journal, history, GC, and undo

### `shoal-history` returns nothing

It defaults to XDG **data**, while main shell/kernel normally use XDG **state**:

```bash
STATE="${XDG_STATE_HOME:-$HOME/.local/state}/shoal"
shoal-history --state-dir "$STATE" query --limit 20
```

For a custom kernel, use its exact `--state-dir`.

### In-language history is empty but kernel journal works

The kernel opens a second journal handle for each session evaluator. If that open failed, session creation continued with a warning and in-language history is disabled while kernel coarse journal remains available. Inspect kernel stderr and directory/SQLite permissions.

### Output says “aged out”

The journal row survives, but CAS GC deleted the referenced output. Check hash/metadata:

```bash
shoal-history --state-dir "$STATE" show ENTRY_ID
```

Pin important hashes before GC:

```bash
shoal-history --state-dir "$STATE" pin HASH
```

Deleted bytes are not reconstructable from the hash alone. Re-run only if the original operation is safely repeatable.

### GC deleted referenced output

Journal references do not make a blob immortal. TTL/budget GC can select referenced but unpinned blobs. Always dry-run first:

```bash
shoal-history --state-dir "$STATE" gc --ttl 2592000 --budget 1073741824
```

Then pin required blobs or apply deliberately.

### Undo refuses “stale”

The target changed since the inverse was recorded. This refusal prevents overwriting newer work. Inspect the current path and entry; do not remove fingerprint checks. Resolve manually or restore into a separate safe location.

### Undo says target escapes scope

`--root` must contain every absolute inverse target after normalization/leading alias resolution. Use the original trusted project root, not `/` as a reflexive bypass. Symlink parents are rejected to prevent traversal/races.

### Undo has zero steps

That entry recorded no typed inverse. Undo is not command replay in reverse and cannot infer how to reverse arbitrary external effects.

## Secrets

### `secret.get` cannot find a value set by `shoal-secret`

Check path mismatch:

```bash
printf 'SHOAL_SECRET_DIR=%s\n' "${SHOAL_SECRET_DIR-}"
printf 'XDG_DATA_HOME=%s\n' "${XDG_DATA_HOME-}"
```

The evaluator honors `SHOAL_SECRET_DIR`; the CLI ignores it and writes under XDG data. Align the stores as described in [Companion CLI reference](@/docs/companion-cli-reference.md#path-mismatch).

### Stored secret has an unexpected newline

`set` stores stdin byte-for-byte. This adds a newline:

```bash
echo "$TOKEN" | shoal-secret set token
```

Use:

```bash
printf %s "$TOKEN" | shoal-secret set token
```

### Secret store authentication/permission failure

Directory must be private and `master.key`/`secrets.json` must have no group/world permissions:

```bash
chmod 700 "${XDG_DATA_HOME:-$HOME/.local/share}/shoal/secrets"
chmod 600 "${XDG_DATA_HOME:-$HOME/.local/share}/shoal/secrets/master.key" \
          "${XDG_DATA_HOME:-$HOME/.local/share}/shoal/secrets/secrets.json"
```

An authentication failure can mean corruption/tampering/wrong key. Do not overwrite the store until you have backed it up and determined whether the key/data pair was mixed.

## Kernel startup and socket discovery

### `shoal-kernel: kernel already listening`

Another process accepts at that socket. Find the configured path/session and decide which lifecycle owner is authoritative. Do not delete a live socket:

```bash
ps -ef | rg '[s]hoal-kernel'
```

Stop the supervised/old process cleanly or point the new kernel at a different socket.

### Refuses to remove path

The existing path is unowned or not a socket. This is a safety check. Inspect without replacing:

```bash
ls -ld "$(dirname "$SOCKET")" "$SOCKET"
file "$SOCKET"
```

Choose a socket inside your owned runtime directory.

### MCP cannot connect, even though kernel is running

Compare exact paths. Kernel/MCP normal discovery order is:

```text
SHOAL_SOCKET
XDG_RUNTIME_DIR/shoal/SESSION.sock
TMPDIR/shoal-UID/shoal/SESSION.sock
/tmp/shoal-UID/shoal/SESSION.sock
```

Start both explicitly while debugging:

```bash
SOCKET="${XDG_RUNTIME_DIR:-/tmp}/shoal-debug.sock"
shoal-kernel --socket "$SOCKET" --state-dir "$STATE"
```

In another terminal:

```bash
SHOAL_NO_AUTOSTART=1 shoal-mcp --socket "$SOCKET" --session debug
```

### `shoal doctor` says socket missing but MCP works

Doctor uses a different no-XDG fallback (`std::env::temp_dir()/shoal/SESSION.sock`) and ignores `SHOAL_SOCKET`. On macOS especially, it may probe the wrong place. Compare its printed detail with kernel readiness; treat this as a doctor limitation.

### MCP autostarts an insecure/default kernel

Autostart passes only `--socket`, not `--policy`/`--state-dir`. For explicit policy, disable it:

```bash
SHOAL_NO_AUTOSTART=1 shoal-mcp ...
```

and start/supervise `shoal-kernel --policy ... --state-dir ...` yourself.

## Authentication and policy

### New token is rejected

Check store alignment and restart:

```bash
printf 'CLI store=%s\n' "${SHOAL_TOKEN_STORE:-${XDG_STATE_HOME:-$HOME/.local/state}/shoal/tokens.json}"
```

Kernel reads `<--state-dir>/tokens.json` once at startup. A token created afterward is invisible until kernel restart.

### Revoked token still works

Same reload limitation: restart the kernel, and stop existing MCP processes carrying the bearer. Expiry is checked live, but file revocation is not reloaded.

### Token lists `--cap`, but action denied

Token cap/profile fields are metadata. Add the token **principal** to the loaded Leash policy with actual grants. Confirm attachment's `principal`, `profile`, and policy principal. Never widen the local-human profile as a substitute.

### `LEASH_DENIED` (`-32010`)

Possible meanings:

- effect denied by policy;
- cross-session/principal plan access;
- direct `mode:"approved"` did not match an approved stored plan;
- spawn hash/name not allowlisted.

Inspect structured `data`, current principal/session, plan resource, and policy. Do not retry blindly.

### `APPROVAL_REQUIRED` (`-32011`)

Plan first, display source/effects, obtain authorized review, then apply. Current raw approval routing is not secure against an untrusted socket peer; keep the socket fully trusted.

### `UNKNOWN_PLAN` after planning

Causes:

- kernel restart;
- wrong kernel/socket/session;
- reference typo;
- same-shape plan collision overwrote the process-global map.

Derive a fresh plan, inspect it, and apply promptly. Do not persist plan refs as durable IDs.

### `caps_enforced` is false

It is false for the default permissive human, hosts without Landlock/Seatbelt, and principals whose policy resolves no nontrivial filesystem sandbox. Host `available_tier` is not active confinement. Network enforcement remains false even when filesystem enforcement is true.

## MCP request and tool errors

### Client says server emitted invalid JSON

MCP stdout must contain only JSON-RPC frames. Launch `shoal-mcp` directly and ensure wrappers/logging/profile scripts do not print to stdout. Put diagnostics on stderr. Do not wrap it in a shell that emits banners.

### `tools/call` succeeded but `isError` is true

Kernel RPC errors are wrapped as valid MCP tool results. Inspect:

```text
result.isError
result.structuredContent.code
result.structuredContent.data
```

Transport/schema errors instead fail the MCP request. Clients must handle both levels.

### `shoal_get` rejects `format`

The tool schema does not expose format. Use a resource:

```text
shoal://out/17?format=render
shoal://out/17?slice=0..4096&format=raw
```

Raw can be large; prefer slices.

### Unknown `out:N`

Transcript refs are named-session/live-kernel state. Verify the facade attached to the same socket/session and the kernel did not restart. Another standalone REPL has a separate transcript.

### Session cwd resource is stale

`shoal://session/cwd` is the attachment snapshot. Run:

```json
{"src":"pwd","position":"value"}
```

or reconnect.

## Tasks and events

### Timeout “did not stop” the command

By design, `timeout_ms` limits synchronous waiting and returns a task if work continues. Call `shoal_cancel`, then observe the task to terminal state. If you need a hard deadline, current Shoal requires client/service-level kill policy and side-effect reconciliation.

### Task output is empty/record-shaped while running

`shoal://task/N/out` returns the task record until `result_ref` exists. It is not incremental stdout. Subscribe/read task state, then fetch output after completion.

### Cancel response arrived but process still appears alive

Cancellation is requested/cooperative; descendant cleanup is not a universal transaction. Wait for task terminal state and inspect system processes/artifacts. Do not call the same effectful operation again until reconciled.

### Missing event sequence or `{dropped, latest_seq}`

The per-subscriber queue overflowed or ring history aged out. Pull from the last persisted cursor:

```text
shoal://events/CHANNEL?since=LAST
```

`journal`/`session.transcript` reconstruct durable history; other channels may require authoritative task/application state reconciliation.

### Unsubscribe does not stop notifications/resources

MCP `resources/unsubscribe` is currently a no-op success. End the MCP facade process to close subscription connections. Avoid duplicate subscribes in a long-lived process.

### Events and responses arrive “out of order”

Notifications are asynchronous; only per-channel `seq` defines event order. An event triggered by a request can arrive before/after its response. Deduplicate and correlate through refs/task IDs, not arrival position.

## PTYs

### PTY spawn failed

Check executable resolution, working directory, environment, policy/spawn pin, sandbox helper, and host PTY availability:

```bash
command -v PROGRAM
test -e /dev/ptmx && echo pty-present
```

Use ordinary `shoal_exec` first for a noninteractive diagnostic. A policy approval-required PTY spawn currently has no complete plan/apply handshake in the PTY tool surface.

### Sent input but screen did not change

- read before typing to confirm mode/prompt;
- send a named `{ "key":"Enter" }` rather than assuming newline text;
- wait/poll with 50–250 ms backoff;
- check `alive`/`exit`;
- resize may trigger delayed repaint;
- terminal application may be in alternate mode awaiting another key.

Do not tight-loop reads.

### Screen lacks earlier output

PTY reads return the current emulator grid, not raw bytes or complete scrollback. Use noninteractive exec for durable output, or make the child save/log explicitly when appropriate.

### Unknown PTY

The ID is closed, from another session, or lost on kernel restart. Do not replay queued keystrokes into a newly opened program without re-reading/confirming the screen.

## Platform-specific notes

### macOS `/tmp` differs from `/private/tmp`

macOS canonicalization can expose `/private/tmp` for paths entered as `/tmp`. Undo includes leading-prefix handling, but compare canonical paths when diagnosing scope errors and keep platform tests for symlink aliases.

### macOS has no `XDG_RUNTIME_DIR` by default

Kernel/MCP use `$TMPDIR/shoal-UID/...`; doctor may probe `$TMPDIR/shoal/...` instead. Explicit `SHOAL_SOCKET` or an owned service-defined runtime path avoids ambiguity.

### Linux reports tier B / Landlock unavailable

The running kernel may lack Landlock or usable ABI; there is no namespace fallback installed. `caps_enforced` remains false. Upgrade/use an appropriate kernel or contain the entire service externally; do not treat advisory policy as OS confinement.

### Network policy is not enforced by OS

Even with Landlock/Seatbelt filesystem enforcement, current `network_enforced` is false. Use an external network namespace/firewall/container/service sandbox for a hard network boundary.

### GNU/BSD command differences

Pin tools through Reef and use adapters that force portable/deterministic flags. Do not assume `sed`, `stat`, `date`, `find`, etc. share GNU syntax across Linux/macOS.

## When a retry is safe

| Failure point | Retry unchanged? |
| --- | --- |
| Shoal parse error | No; fix source. |
| Policy denial | No; inspect intent/policy. |
| Plan unknown | Re-plan; do not apply stale source assumptions. |
| Read-only `shoal_get`/resource read transport failure | Usually yes if ref/session still exists. |
| Event read | Yes with same exclusive cursor; deduplicate. |
| Effectful exec response lost | **Not until reconciled**; it may have completed. |
| PTY send response lost | **Not blindly**; read screen before resending. |
| Cancellation response lost | Read task/process state first. |
| Token create output lost | List metadata cannot recover bearer; revoke ID if known/create anew after review. |

## Minimal issue report

Include:

```text
Shoal commit/version:
OS + version + architecture:
Rust version (source build):
Invocation surface:
Exact minimal source/request (secrets removed):
Expected structured result:
Actual result/error code + data:
Relevant XDG/socket/session paths (no token):
Does it reproduce with a new temp state/config/session?:
```

For security issues, avoid a public proof containing real journal/source/token data. Share the smallest synthetic reproduction through the project's private security contact when one is published; until then, sanitize aggressively and state that the report is security-sensitive.
