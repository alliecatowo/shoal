# shoal — Claude Code plugin

This plugin gives Claude Code a structured way to share a live shoal session. It installs the
`shoal` language skill and registers `shoal mcp`, which bridges MCP over stdio to a long-lived
`shoal-kernel` session. Results stay typed, large payloads become drillable references, and live
changes arrive through subscriptions instead of text scraping or polling.

The implementation is exercised alongside shoal's 1,310-case, 77-suite conformance corpus on
Linux and macOS. Shoal is still pre-release; read the [current status][status] before relying on it
as a login shell.

## What ships

The MCP server exposes 13 tools:

| Tool | Purpose |
|---|---|
| `shoal_exec` | Evaluate shoal source, synchronously or as a task. |
| `shoal_plan` | Derive effects and reversibility without spawning. |
| `shoal_apply` | Apply a stored, approved plan. |
| `shoal_get` | Read a field or slice from a transcript reference. |
| `shoal_journal` | Query structured execution history. |
| `shoal_cancel` | Cancel a background task. |
| `shoal_cap_request` | Request approval for a plan's effects. |
| `shoal_pty_open` | Start an interactive program on a real PTY. |
| `shoal_pty_send` | Send text, bytes, or named keys to a PTY. |
| `shoal_pty_read` | Read the bounded rendered terminal screen. |
| `shoal_pty_resize` | Resize the terminal and emulator grid. |
| `shoal_pty_close` | Terminate and reap a PTY session. |
| `shoal_pty_list` | List this session's open PTYs. |

The server implements MCP `resources/list`, `resources/templates/list`, `resources/read`, and
`resources/subscribe`; it acknowledges `resources/unsubscribe`, but that request does not yet stop
the dedicated forwarding connection/thread. Subscription cleanup is currently scoped to MCP
process exit. Shipped roots include `shoal://journal`,
`shoal://jobs`, `shoal://session/cwd`, `shoal://session/env`, `shoal://session/reef`, and
`shoal://pty`; open tasks, plans, and PTYs are listed dynamically. Templates cover transcript and
content-addressed values, task output, plans, session views, PTY screens, journal queries, and event
channels. Subscriptions are supported for `shoal://events/{channel}` and
`shoal://task/{id}[/out]`, delivered as `notifications/resources/updated`.

The bundled [`skills/shoal/SKILL.md`](skills/shoal/SKILL.md) is the operational language card. It
records exact syntax and wire behavior, including the two execution positions, forced commands,
aliases, undo, elision, resources, and PTY workflows.

## Install

Install the three binaries from this checkout:

```sh
cargo install --path crates/shoal
cargo install --path crates/shoal-kernel
cargo install --path crates/shoal-mcp
```

`~/.cargo/bin` must be on the `PATH` inherited by Claude Code. The plugin configuration launches
`shoal mcp`; that companion command execs `shoal-mcp`, which connects to the kernel.

Add this repository as a Claude Code marketplace and install the plugin:

```text
/plugin marketplace add /path/to/shoal
/plugin install shoal
```

After the repository is published, the marketplace source can instead be
`alliecatowo/shoal`.

### Kernel startup

Kernel startup is automatic by default. On the first MCP connection, `shoal-mcp` probes the target
socket and, if nothing is listening, starts a detached `shoal-kernel --socket <path>`. Concurrent
startup attempts are safe: the kernel refuses to replace a live listener, and each bridge connects
to the winner.

Set a non-empty `SHOAL_NO_AUTOSTART` when the kernel is supervised externally:

```sh
export SHOAL_NO_AUTOSTART=1
shoal-kernel &
```

Autostart is best-effort. If `shoal-kernel` is missing from `PATH` or never becomes ready, the MCP
bridge reports the underlying connection error after a bounded wait.

### Socket selection

Both the bridge and kernel use the same default:

1. `$XDG_RUNTIME_DIR/shoal/<session>.sock` when `XDG_RUNTIME_DIR` is set;
2. `$TMPDIR/shoal-<uid>/shoal/<session>.sock` when `TMPDIR` is set;
3. `/tmp/shoal-<uid>/shoal/<session>.sock` otherwise.

To select a socket explicitly, set `SHOAL_SOCKET` in the environment that launches Claude Code and
start a supervised kernel at the same path:

```sh
mkdir -p ~/.local/state/shoal
shoal-kernel --socket ~/.local/state/shoal/kernel.sock &
export SHOAL_SOCKET="$HOME/.local/state/shoal/kernel.sock"
export SHOAL_NO_AUTOSTART=1
```

Use a short path inside a directory you own. Unix-domain socket paths have a small platform limit,
and the kernel intentionally refuses insecure parent directories.

## Verify the connection

Call `shoal_exec` with:

```json
{"src":"1 + 2","position":"value"}
```

The structured result contains an `out:<n>` reference and the tagged integer value. Treat
`structuredContent` as data; `content[0].text` and `render` are bounded human previews.

`shoal_exec` defaults to `position: "value"` at the MCP facade. In value position, all leading
statements execute normally and a final expression is evaluated as a value, so a non-zero external
command outcome is returned for inspection. In statement position, a non-OK command raises
`cmd_failed`. Language errors such as `div_zero` raise in either position and still mint a
transcript ref in the error data.

## Interactive PTY workflow

Use PTY tools for programs whose terminal behavior matters; `shoal_exec` is intentionally headless.
A normal workflow is:

1. `shoal_pty_open` with `cmd`, optional string `args`, terminal dimensions, and environment.
2. `shoal_pty_read` to inspect `screen`, `cursor`, `changed`, `alive`, and `exit`.
3. `shoal_pty_send` with literal text, a named key such as `{"key":"Enter"}`, or an array that
   mixes both. Named keys include arrows, navigation keys, F1–F12, and `Ctrl-<letter>`.
4. `shoal_pty_resize` when the client layout changes.
5. `shoal_pty_close` when finished. Use `shoal_pty_list` or `shoal://pty` to recover open IDs.

PTY reads return a rendered screen, never raw escape bytes. PTY spawns pass through the same leash
and executable-pin gate as other process launches.

## Absolute-path configuration

If the binaries are not on `PATH`, edit `plugin/.mcp.json` to point at the built companion:

```json
{
  "mcpServers": {
    "shoal": {
      "type": "stdio",
      "command": "/path/to/shoal/target/release/shoal-mcp",
      "args": []
    }
  }
}
```

Autostart still needs `shoal-kernel` on `PATH`; otherwise supervise a kernel and set
`SHOAL_NO_AUTOSTART` plus `SHOAL_SOCKET`.

## Documentation

- [Agent and MCP manual][agent-mcp]
- [Kernel protocol atlas][kernel]
- [Language manual][manual]
- [Status and limits][status]
- [Roadmap][roadmap]

[agent-mcp]: ../site/content/internals/agent-mcp.md
[kernel]: ../site/content/internals/kernel-protocol.md
[manual]: https://alliecatowo.github.io/shoal/docs/
[status]: https://alliecatowo.github.io/shoal/docs/status-limits/
[roadmap]: https://alliecatowo.github.io/shoal/docs/roadmap/
