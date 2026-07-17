+++
title = "Companion CLI reference"
description = "Install and use shoal-kernel, shoal-mcp, shoal-lsp, shoal-token, shoal-secret, shoal-history, shoal-doctor, and sandbox helpers."
weight = 230
template = "docs/page.html"

[extra]
eyebrow = "Operations"
group = "Agents & protocol"
audience = "Shoal users, operators, and editor/agent integrators"
status = "Current command-line behavior"
toc = true
+++

The repository ships several executables around the main `shoal` shell. They are separate Cargo packages: installing `crates/shoal` alone does not automatically install the kernel, MCP facade, LSP server, token/history/secret utilities, or sandbox launcher.

## Binary map

| Binary | Package directory | Role |
| --- | --- | --- |
| `shoal` | `crates/shoal` | Interactive shell, scripts, formatter, prompt tools, dispatchers. |
| `shoal-kernel` | `crates/shoal-kernel` | Long-lived named evaluator service. |
| `shoal-mcp` | `crates/shoal-mcp` | MCP stdio facade over the kernel socket. |
| `shoal-lsp` | `crates/shoal-lsp` | Language Server Protocol over stdio. |
| `shoal-token` | `crates/shoal-auth` | Create/list/revoke agent bearer tokens. |
| `shoal-secret` | `crates/shoal-secret` | Set/list/delete the encrypted local secret store. |
| `shoal-history` | `crates/shoal-history` | Query journal/CAS, pin blobs, GC, and replay undo. |
| `shoal-doctor` | `crates/shoal-doctor` | Installation/environment diagnostics. |
| `shoal-sandbox-exec` | `crates/shoal-exec` | Internal child launcher that applies filesystem sandbox rules. |
| `shoal-landlock-helper` | `crates/shoal-leash` | Low-level enforcement test/helper, not a normal user command. |

The main CLI dispatches only two external companions:

```text
shoal lsp  -> finds and runs shoal-lsp through PATH
shoal mcp  -> finds and runs shoal-mcp through PATH
```

`shoal doctor` uses the doctor library inside the main process; it does not spawn `shoal-doctor`.

## Build or install the complete set

Build every workspace target into one sibling directory:

```bash
cargo build --release --workspace
ls target/release/shoal \
   target/release/shoal-kernel \
   target/release/shoal-mcp \
   target/release/shoal-lsp
```

Run directly by prepending the release directory:

```bash
export PATH="$PWD/target/release:$PATH"
```

For Cargo's user bin directory, install packages explicitly:

```bash
cargo install --path crates/shoal
cargo install --path crates/shoal-kernel
cargo install --path crates/shoal-mcp
cargo install --path crates/shoal-lsp
cargo install --path crates/shoal-auth
cargo install --path crates/shoal-secret
cargo install --path crates/shoal-history
cargo install --path crates/shoal-doctor
cargo install --path crates/shoal-exec
```

Installing the sandbox helper beside the other executables matters when Leash resolves a concrete OS sandbox: the spawn layer searches beside the current executable, not arbitrary `PATH`, for `shoal-sandbox-exec`.

Check:

```bash
for bin in shoal shoal-kernel shoal-mcp shoal-lsp \
           shoal-token shoal-secret shoal-history shoal-doctor \
           shoal-sandbox-exec
do
  command -v "$bin" || printf 'missing: %s\n' "$bin"
done
```

## XDG path matrix

The companions do not yet use one perfectly consistent root. This table is operationally important:

| Component | Default state/data path |
| --- | --- |
| `shoal-kernel` journal/tokens | `$XDG_STATE_HOME/shoal`, else `~/.local/state/shoal` |
| main `shoal` journal/history | state-rooted (`XDG_STATE_HOME`) |
| `shoal-history` | `$XDG_DATA_HOME/shoal`, else `~/.local/share/shoal` |
| evaluator secrets | `$SHOAL_SECRET_DIR`, else `$XDG_DATA_HOME/shoal/secrets`, else `~/.local/share/shoal/secrets` |
| `shoal-secret` | `$XDG_DATA_HOME/shoal/secrets`, else `~/.local/share/shoal/secrets` (ignores `SHOAL_SECRET_DIR`) |
| `shoal-doctor` “state dir” probe | `$XDG_DATA_HOME/shoal`, else `~/.local/share/shoal` |
| user config/policy/adapters | `$XDG_CONFIG_HOME/shoal`, else `~/.config/shoal` |

When operating a kernel or the main shell journal, pass `shoal-history --state-dir` explicitly. When using `SHOAL_SECRET_DIR` for the evaluator, the CLI cannot target it by flag or environment today; temporarily align `XDG_DATA_HOME`, or manage the store with a process/environment layout that points both at the same directory.

## `shoal-kernel`

```text
shoal-kernel [--session NAME] [--socket PATH] [--state-dir PATH] [--policy FILE]
```

| Option | Default | Notes |
| --- | --- | --- |
| `--session NAME` | `default` | Derives default socket filename only; clients choose attached session. |
| `--socket PATH` | runtime discovery | Explicit Unix socket path. |
| `--state-dir PATH` | XDG state | Journal/CAS/tokens. |
| `--policy FILE` | permissive local human | Explicit Leash TOML; load/parse failure is fatal. |

Example:

```bash
install -d -m 700 "$HOME/.local/state/shoal" "$HOME/.config/shoal"
shoal-kernel \
  --session default \
  --state-dir "$HOME/.local/state/shoal" \
  --policy "$HOME/.config/shoal/leash.toml"
```

Readiness is printed to stderr:

```text
shoal-kernel: ready /path/to/default.sock
```

SIGINT/SIGTERM asks the serve loop to stop and removes the bound socket on normal teardown. The kernel is foreground by default; use a user service manager for production lifecycle rather than shell backgrounding.

Socket discovery and protocol details are in [Agents, kernel, and MCP](@/docs/agents-kernel-mcp.md) and [Kernel protocol](@/docs/kernel-protocol.md).

## `shoal-mcp`

```text
shoal-mcp [--socket PATH] [--session NAME] [--token TOKEN]
```

Environment:

| Variable | Meaning |
| --- | --- |
| `SHOAL_SOCKET` | Explicit socket, used when `--socket` absent. |
| `SHOAL_SESSION` | Named attachment session. |
| `SHOAL_TOKEN` | Bearer passed to `session.attach`. |
| `SHOAL_NO_AUTOSTART` | Any nonempty value disables detached kernel autostart. |

Flags overwrite environment-derived fields. If neither flag nor environment selects a socket, it is derived from the selected/default session.

The process uses stdin/stdout exclusively for newline-framed MCP JSON-RPC. Do not pipe logging into stdout. Errors go to stderr; normal fatal exit is status 1, usage is status 2.

### Autostart

If the socket is not listening, the facade tries:

```text
shoal-kernel --socket <selected-path>
```

as a detached process with null standard streams and its own process group, then polls for roughly five seconds. It does not pass MCP's session, token, state directory, or policy as kernel flags. In particular, `--session` selects client attachment/socket derivation; the autostarted kernel still uses its own default state and permissive policy unless separately configured through lifecycle.

For an explicitly managed secure policy:

```bash
SHOAL_NO_AUTOSTART=1 \
SHOAL_SOCKET="$XDG_RUNTIME_DIR/shoal/work.sock" \
SHOAL_SESSION=work \
SHOAL_TOKEN="$TOKEN" \
shoal-mcp
```

Start the matching kernel first through your service manager.

### Main dispatcher difference

```bash
shoal mcp
```

looks up `shoal-mcp` through `PATH` and accepts **no trailing arguments**. This fails:

```bash
shoal mcp --session work
```

Use environment variables with the dispatcher:

```bash
SHOAL_SESSION=work shoal mcp
```

or call `shoal-mcp --session work` directly.

## `shoal-lsp`

```text
shoal-lsp
shoal lsp
```

Both forms run an LSP server over stdin/stdout; the dispatcher resolves the companion through `PATH` and accepts no trailing options.

Current server capabilities:

| Capability | Behavior |
| --- | --- |
| Text sync | Full-document changes. |
| Diagnostics | One syntax diagnostic for incomplete/error parse; cleared on valid document/close. |
| Formatting | Whole-document edit using the Shoal formatter, only when parse is complete. |
| Completion | Keywords, canonical builtin command heads, and earlier lexical `let`/`var`/`fn`/`alias` declarations. |
| Hover | Short help for `let`, `var`, `fn`, `match`, `with`, `spawn`, `sh`, and `it`. |

Not currently advertised: goto definition, references, rename, code actions, workspace symbols, semantic tokens, signature help, incremental sync, workspace configuration, or file watching.

Generic editor configuration should launch one of:

```json
{"command":"shoal-lsp","args":[]}
```

```json
{"command":"shoal","args":["lsp"]}
```

Associate it with `*.shl`. The process emits no user logging on stdout beyond LSP frames.

### Formatting caveat

Formatting returns no edits when the current document is incomplete or invalid. Fix the parse diagnostic first. Formatting replaces the entire document; editors should apply it as one text edit.

### Completion caveat

Local declaration discovery is intentionally lexical and simple. It scans text before the cursor, not the full semantic scope graph. Expect suggestions for declarations that may be shadowed/out of scope, and no project-wide symbol index.

## `shoal-token`

```text
shoal-token create PRINCIPAL [PROFILE] [--cap VALUE]... [--ttl SECONDS]
shoal-token list
shoal-token revoke ID
```

Store path:

```text
$SHOAL_TOKEN_STORE
$XDG_STATE_HOME/shoal/tokens.json
~/.local/state/shoal/tokens.json
```

### Create

```bash
TOKEN="$({ shoal-token create agent:ci ci \
  --cap fs.read --cap proc.spawn --ttl 3600; } 2>token-meta.log)"
```

The bearer secret is the only stdout line. Creation metadata (`created ID (secret shown once)`) goes to stderr. Keep those streams separate in automation.

If PROFILE is omitted, it is `default`. `--cap` is repeatable. `--ttl` is parsed as signed seconds and converted to nanoseconds; zero or negative values create immediately expired tokens and are not useful.

Important: profile/capability values are metadata reported at attachment, not grants. Leash authorization uses the principal's policy entry. See [Security and trust boundaries](@/docs/security.md#profile-and-cap-are-metadata-today).

### List

```bash
shoal-token list
```

One tab-separated row per token:

```text
ID<TAB>PRINCIPAL<TAB>PROFILE<TAB>active|revoked
```

The list does not print bearer secrets, capabilities, creation/expiry times, or an `expired` label. A token past expiry but not revoked can still display `active`; validation rejects it.

### Revoke

```bash
shoal-token revoke 0123456789abcdef
```

Unknown IDs fail. Revocation writes a timestamp rather than deleting metadata.

### Kernel reload warning

The daemon loads its token store once. Create/revoke through this CLI does not update a running kernel's in-memory copy. Restart every affected kernel after token changes. Expiry is still checked against current time on each validation.

The kernel always uses `<--state-dir>/tokens.json`, ignoring `SHOAL_TOKEN_STORE`; align the paths.

## `shoal-secret`

```text
shoal-secret set NAME < value
shoal-secret list
shoal-secret delete NAME
```

There is deliberately no CLI `get` command that prints material. Shoal source retrieves an opaque typed value:

```text
let token = secret.get("github-token")
```

### Set exact bytes

`set` reads stdin to EOF and stores every byte. `echo` commonly adds a newline, so use `printf` for an exact text secret:

```bash
printf %s "$GITHUB_TOKEN" | shoal-secret set github-token
```

Or from a protected file:

```bash
shoal-secret set signing-key < "$HOME/.private/signing-key"
```

Names must be nonempty ASCII letters, digits, `_`, or `-`.

### List and delete

```bash
shoal-secret list
shoal-secret delete github-token
```

Names are printed sorted. Deleting a missing name is a successful no-op from the CLI's perspective.

### Storage and encryption

Directory:

```text
$XDG_DATA_HOME/shoal/secrets
~/.local/share/shoal/secrets
```

The directory is set to `0700`. `master.key` is 32 random bytes and `secrets.json` is an AES-256-GCM authenticated envelope with a new 12-byte nonce for each save; both files must have no group/world bits (effectively `0600`) or open fails. Writes use a synced temporary file and atomic persist.

The key is stored beside the ciphertext. Encryption prevents accidental plaintext inspection and detects tampering; it does not protect against an attacker who can read the whole directory. Filesystem permissions and OS-user isolation remain the boundary.

Every set/delete decrypts and rewrites the complete map. This store is appropriate for a modest number of local secrets, not a high-concurrency remote vault.

### Path mismatch

The evaluator honors `SHOAL_SECRET_DIR` before XDG/HOME. This CLI does not. If `SHOAL_SECRET_DIR=/custom` is set, `shoal-secret set` still writes to the XDG location while `secret.get` reads `/custom`.

Until the CLI gains a directory option, align by choosing `XDG_DATA_HOME` such that its `shoal/secrets` equals the intended store, avoid the override, or populate the custom store using a trusted program built on the library.

Exit codes: usage/store-open failure 2, set/list/delete operation failure 1, success 0.

## `shoal-history`

```text
shoal-history [--state-dir PATH] [--json] [COMMAND] [COMMAND OPTIONS]
```

Global flags are removed before command parsing and may appear before or after the command. The omitted command is `query`.

### Always select the right journal

The utility defaults to XDG **data**:

```text
$XDG_DATA_HOME/shoal
~/.local/share/shoal
```

The main shell and kernel normally write under XDG **state**. Use:

```bash
shoal-history --state-dir "${XDG_STATE_HOME:-$HOME/.local/state}/shoal" query
```

An empty result from the default location often means path mismatch, not absent history.

### Query

```text
query [--since NS] [--principal NAME] [--effects TEXT]
      [--head COMMAND] [--status ok|failed] [--limit N]
```

Example:

```bash
shoal-history --state-dir "$STATE" --json query \
  --since 1750000000000000000 \
  --principal agent:reviewer \
  --effects fs_write \
  --status failed \
  --limit 50
```

`--since` is a signed nanosecond timestamp. `--effects` is one substring-style effect matcher, despite the plural spelling; repeat/multiple all-of matching is not implemented. Default limit is 100; if an effect filter is present the library scans all rows, filters in memory, then truncates.

Human output prints ID, principal, verdict, and first source line. `--json` adds AST, effects, cwd, timing/status, and output descriptors. Treat it as sensitive.

### Show

```bash
shoal-history --state-dir "$STATE" show ENTRY_ID
shoal-history --state-dir "$STATE" --json show ENTRY_ID
```

Human mode includes each output's stored length, availability/aged-out state, truncation metadata, and hash. JSON contains the complete entry representation. Missing ID exits 1.

### Pin and unpin

```bash
shoal-history --state-dir "$STATE" pin BLAKE3_HEX
shoal-history --state-dir "$STATE" unpin BLAKE3_HEX
```

Pins exempt a CAS blob from garbage collection. Operations are idempotent at the storage layer; the CLI prints nothing. The hash must be valid even-length hexadecimal.

### Garbage collection

```text
gc [--ttl SECONDS] [--budget BYTES] [--apply]
```

Dry-run is the default:

```bash
shoal-history --state-dir "$STATE" gc --ttl 2592000 --budget 1073741824
```

Apply only after inspecting the compact JSON report:

```bash
shoal-history --state-dir "$STATE" gc \
  --ttl 2592000 \
  --budget 1073741824 \
  --apply
```

The report always prints JSON with `dry_run`, candidate/deleted counts, reclaimed and remaining bytes; global `--json` does not change this command.

TTL candidates are unpinned blobs whose last access is older than the cutoff. The budget pass additionally selects oldest/unreferenced-first blobs until projected bytes fit. Referenced output blobs are not absolutely protected unless pinned: budget/TTL GC can age them out while leaving journal metadata and hashes intact.

### Undo

```text
undo ENTRY_ID --root PATH
```

```bash
shoal-history --state-dir "$STATE" undo 419 --root "$PWD"
```

Undo replays recorded typed inverses newest-first. Supported inverse shapes restore a trash move, restore prior CAS bytes, or move a path back. The explicit root is a mandatory safety scope.

Safety checks:

- every target must be absolute and remain under the resolved root;
- symlink parents are rejected;
- expected file fingerprints detect changes since recording;
- missing CAS prior bytes fail;
- already-applied inverses report idempotent `already_applied`;
- stale or escaped state stops with exit 1 rather than overwriting.

`--json` reports each step and inverse. Undo is available only when the original operation recorded an inverse and required CAS blobs survived/pins/GC. Zero steps can mean no inverse was recorded; it is not proof that an arbitrary command was reversed.

## `shoal-doctor`

```text
shoal doctor [--json]
shoal-doctor [--json]
```

The main dispatcher strictly accepts only `--json`. The standalone binary currently treats the presence of `--json` anywhere as JSON mode and otherwise ignores extra arguments; rely only on the documented form.

Checks:

| Check | Level rule |
| --- | --- |
| Leash | Warn unless detection itself reports an active enforcement instance. |
| stdin TTY | Ok for TTY, warn otherwise. |
| `/dev/ptmx` | Ok if present, fail otherwise. |
| runtime/state/config directories | Create a temporary file; fail if directory absent/unwritable. |
| kernel socket | Connect at derived path; warn if unreachable. |
| adapters | Load custom config adapter dir; warn on parse/load warnings. |
| `sh`, `git`, `rg`, `cargo` | Missing `sh` fails; others warn. |
| journal | Create temporary journal under selected state dir and write/finish an entry. |
| `shoal.toml`, `leash.toml` | Missing warns; read/parse failure fails. |

Exit status:

```text
0  all checks ok
1  at least one warning, no failures
2  at least one failure
```

Because Leash detection reports backend availability rather than an active per-child sandbox, the standalone doctor commonly warns even on a host where Landlock/Seatbelt is available. Read the detail, not only the status word.

### Environment inputs and current mismatches

| Variable | Doctor use |
| --- | --- |
| `XDG_RUNTIME_DIR` | Runtime root; otherwise `std::env::temp_dir()`. |
| `XDG_DATA_HOME` | The directory doctor labels `state dir`. |
| `XDG_CONFIG_HOME` | Config root. |
| `SHOAL_SESSION` | Socket filename, default `default`. |

The doctor does not honor `XDG_STATE_HOME`, `SHOAL_SOCKET`, or kernel's UID-qualified `$TMPDIR/shoal-<uid>` fallback. Therefore it can probe a different state directory or socket than a healthy kernel, especially on macOS/no-`XDG_RUNTIME_DIR`. A socket warning may be a probe-path mismatch.

For an accurate current-kernel diagnosis:

1. create the directories doctor will probe, or interpret absence failures accordingly;
2. temporarily set `XDG_RUNTIME_DIR` to the kernel's actual runtime parent when possible;
3. compare the printed socket with `shoal-kernel: ready ...`;
4. inspect the actual kernel state path separately;
5. use `session.attach`'s `caps_enforced` for the active principal rather than doctor alone.

Example machine-readable invocation:

```bash
shoal doctor --json >doctor.json
status=$?
jq '.checks[] | select(.level != "ok")' doctor.json
exit "$status"
```

## `shoal-sandbox-exec`

This is an internal launcher, but packaging failures mention it, so its boundary matters:

```text
shoal-sandbox-exec [--read PATH]... [--write PATH]... [--delete PATH]... -- COMMAND [ARG]...
```

It applies the strongest platform filesystem sandbox to its own process, then replaces itself with the command. Missing/invalid options, sandbox failure, or exec failure exits 126 and prints to stderr.

Do not use it as a general container runtime. It has filesystem grants only, no environment scrubbing, network namespace, CPU/memory limit, or parent policy/token integration. The evaluator constructs invocations for Leash-managed child spawns.

`shoal-landlock-helper` is a test-oriented two-path probe with fixed exit statuses, not the user-facing sandbox command.

## Troubleshooting companion discovery

If `shoal lsp` or `shoal mcp` says it cannot launch the companion:

```bash
command -v shoal
command -v shoal-lsp
command -v shoal-mcp
type -a shoal shoal-lsp shoal-mcp
```

The dispatcher does not search beside itself; it uses `PATH`. Conversely, the sandbox launcher is searched beside the current executable. A packaging layout can therefore satisfy one rule and fail the other. Installing all user-facing binaries into one bin directory on `PATH` satisfies both.

See [Troubleshooting](@/docs/troubleshooting.md) for symptom-oriented diagnosis.
