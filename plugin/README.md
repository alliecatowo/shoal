# shoal — Claude Code plugin

Gives a Claude Code agent two things:

1. **The shoal MCP tools** — the surface exposed by `shoal-mcp`, which bridges MCP to a running
   `shoal-kernel` over its JSON-RPC/Unix-socket wire protocol. At minimum: `shoal_exec`, `shoal_plan`,
   `shoal_apply`, `shoal_get`, `shoal_journal`, `shoal_cap_request`, plus `shoal_cancel` and an MCP
   `resources/*` layer once they land (see `skills/shoal/SKILL.md` §0 for exactly what's confirmed vs.
   intended as of any given build — the tool count on this surface is moving).
2. **A skill** (`skills/shoal/SKILL.md`) — a complete, corpus-grounded language card so the agent
   never guesses at shoal's syntax, coercion rules, or MCP protocol quirks. It documents what is real
   today, what is only *intended* pending an in-flight change to `shoal-mcp`/`shoal-kernel`, and what
   is spec'd but not implemented at all — each called out explicitly.

**Both macOS and Linux are first-class here.** The only asymmetry below is the `XDG_RUNTIME_DIR`
convention, which is a Linux/systemd default that macOS doesn't share — see "macOS socket path" below
for the current fix and its fallback.

## Prerequisites

shoal is not one binary you point at — it's (at least) three cooperating pieces, and **all of them
must be on `PATH`** (or referenced by absolute path, see below):

1. **`shoal-kernel`** — the long-lived daemon that actually holds a session, the evaluator, and the
   journal. **Must already be running before Claude Code starts** — nothing in this plugin starts it
   for you.
2. **`shoal-mcp`** — the stdio↔JSON-RPC bridge that speaks MCP on one side and shoal's wire protocol
   on the other. This is what the plugin's MCP server config actually launches.
3. **`shoal`** — the main binary; `shoal mcp` is a thin companion-launcher that execs `shoal-mcp` on
   your behalf (inheriting stdio, so it works as a stdio MCP transport). You can point the plugin
   directly at `shoal-mcp` instead if you prefer to skip the indirection.

Install from this repository (either works; pick one per binary or just build everything):

```sh
cd /path/to/shoal
cargo install --path crates/shoal          # installs `shoal` to ~/.cargo/bin
cargo install --path crates/shoal-kernel    # installs `shoal-kernel`
cargo install --path crates/shoal-mcp       # installs `shoal-mcp`
# make sure ~/.cargo/bin is on PATH (it usually already is once you have a Rust toolchain)
```

or build in place and put `target/release` on `PATH` yourself:

```sh
cd /path/to/shoal
cargo build --release
# binaries land in target/release/{shoal,shoal-mcp,shoal-kernel,...}
export PATH="/path/to/shoal/target/release:$PATH"
```

### Start the kernel

```sh
shoal-kernel &                     # default session "default", default socket location
```

By default `shoal-kernel` listens on `$XDG_RUNTIME_DIR/shoal/default.sock` if `XDG_RUNTIME_DIR` is
set, or falls back to `/tmp/shoal-<uid>/shoal/default.sock` if it isn't.

### macOS socket path

`XDG_RUNTIME_DIR` is a Linux/systemd convention — **it is typically not set on macOS**. A companion
change to `shoal-mcp` is landing a matching `/tmp/shoal-<uid>/shoal/<session>.sock` fallback (mirroring
`shoal-kernel`'s own default) so the two binaries' socket resolution can't drift apart even when
`XDG_RUNTIME_DIR` is unset. **Verify this landed** (`shoal-mcp --help` or a quick read of its socket
resolution in `crates/shoal-mcp`) before relying on it — if it hasn't yet, `shoal-mcp` only checks
`--socket`, `SHOAL_SOCKET`, and `XDG_RUNTIME_DIR`, in that order, and exits with an error if none
resolve. The robust, works-either-way fix is to pin an explicit socket path on both ends:

```sh
mkdir -p ~/.local/state/shoal
shoal-kernel --socket ~/.local/state/shoal/kernel.sock &
export SHOAL_SOCKET=~/.local/state/shoal/kernel.sock   # must be set in the shell that launches `claude`
```

**Keep `--socket` paths short.** Unix domain socket paths are capped at roughly **108 bytes**
(`SUN_LEN`/`sun_path`; ~104 on macOS), and the limit applies to the whole absolute path. A socket
buried in a deep project or temp directory fails to bind/connect with an unhelpful
`Invalid argument`-style error. Prefer short, stable locations like the examples here
(`~/.local/state/shoal/…`, `/tmp/shoal-<uid>/…`) over anything nested inside a checkout.

Claude Code's stdio MCP servers inherit the environment of the process that launched `claude` — so
whichever shell starts your Claude Code session needs `SHOAL_SOCKET` (or `XDG_RUNTIME_DIR`) exported
*before* `claude` starts, every time, on both macOS and Linux, **unless** the `shoal-mcp` `/tmp`
fallback above has landed and you're fine relying on it. Once a socket resolves consistently on both
ends, behavior is identical on macOS and Linux — these are plain Rust binaries with no OS-specific
code path in this plugin's usage.

## Install

This repository doubles as its own plugin marketplace (`.claude-plugin/marketplace.json` at the repo
root, listing this `plugin/` directory as the one plugin it carries):

```
/plugin marketplace add /path/to/shoal
/plugin install shoal
```

or, once this repo is pushed and you'd rather not clone it separately first:

```
/plugin marketplace add alliecatowo/shoal
/plugin install shoal
```

Either way, once enabled you should see:

- New tools in your tool list, named `shoal_*` (see `skills/shoal/SKILL.md` §0 for the current exact
  set — it is expanding).
- The `shoal` skill available, and auto-triggered when you ask the agent to do something in/with
  shoal.

## Verifying it's wired up

Ask the agent to run `shoal_exec` with `{"src": "1 + 2", "position": "value"}`. You should get back
`{"ref": "out:1", "value": {"$":"int","v":3}, "render": "3"}`. If instead you get a connection-refused
style error, the kernel isn't running or the socket path doesn't match — re-check the Prerequisites
section above before assuming anything about the plugin or the language.

## Where the actual language/protocol documentation lives

Don't re-derive shoal's syntax or wire protocol from memory — read `skills/shoal/SKILL.md`. It is
sourced directly from this repository's `docs/*.md` and `spec/cases/*.toml`, and from a direct read of
`crates/shoal-mcp`, `crates/shoal-proto`, `crates/shoal-kernel` — it documents what is real, what is
*intended* pending an in-flight MCP-surface change (marked **(P1)** throughout), what is spec'd-but-
not-implemented at all, and every rough edge in between.

## Absolute-path alternative

If you don't want anything on `PATH`, edit `.mcp.json`'s `command`/`args` to an absolute path, e.g.:

```json
{"mcpServers": {"shoal": {"type": "stdio", "command": "/path/to/shoal/target/release/shoal", "args": ["mcp"]}}}
```

or point directly at the bridge binary and skip the `shoal mcp` indirection entirely:

```json
{"mcpServers": {"shoal": {"type": "stdio", "command": "/path/to/shoal/target/release/shoal-mcp", "args": []}}}
```

Either way, `shoal-kernel` must still be started separately first.

## A note on plugin/marketplace schema fidelity

`.claude-plugin/plugin.json`, `.mcp.json`, and `../.claude-plugin/marketplace.json` (repo root) were
written to match Anthropic's documented Claude Code plugin/marketplace schema as closely as this pass
could confirm — see the maintainer notes accompanying this plugin's introduction for the specific
fields that are worth a final check against a live Claude Code build (in particular: whether
`.mcp.json` at the plugin root is auto-discovered vs. needing an explicit pointer from `plugin.json`,
and the exact optional-field set `plugin.json`/`marketplace.json` accept beyond the required core).
