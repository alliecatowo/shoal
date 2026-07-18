+++
title = "Security and trust boundaries"
description = "Threat model, socket and token authentication, Leash policy, sandbox enforcement, secret handling, isolation gaps, and safe deployment guidance."
weight = 220
template = "docs/page.html"

[extra]
eyebrow = "Security"
group = "Agents & protocol"
audience = "Operators, agent integrators, and security reviewers"
status = "Preview; hardened local boundary with explicit remaining limits"
toc = true
+++

Shoal has useful policy, identity, and sandbox machinery, but one kernel process is not a hard multi-tenant security boundary. Run it as an unprivileged local service, keep its Unix socket private, and use separate OS users/processes/state roots for mutually hostile tenants.

The first deep-audit P0s are closed: journal reads and approvals require scoped attachments; approvals bind and durably audit a distinct authorized approver; plan objects use full caller/content-bound digests and non-overwriting IDs; child evaluators inherit one audited execution context; public sockets cannot assert local-human authority; visible Session names are principal-private; bearer changes are revalidated live; and raw/blob retrieval is owner-checked and byte-bounded. Remaining risk is concentrated in incomplete OS enforcement dimensions, same-process resource sharing, and native-code behavior beyond the planner's model.

Do not forward the raw socket casually through TCP, a web gateway, a shared container volume, an untrusted plugin, or another-user IPC bridge. Those transports change the threat model even though socket possession no longer grants local-human authority.

## Threat-model summary

```mermaid
flowchart TB
accTitle: Threat-model summary
accDescr: Shows the components and relationships described in Threat-model summary.
    subgraph Trusted["One trusted OS user boundary"]
        H["human / trusted local processes"]
        M["shoal-mcp"]
        K["shoal-kernel"]
        J["journal + CAS + tokens"]
        H -->|"private Unix socket"| K
        M -->|"private Unix socket"| K
        K --> J
    end
    A["scoped agent principal"] -->|"bearer token via trusted facade"| M
    K -->|"policy decision"| L["Leash"]
    L -->|"filesystem sandbox where available"| P["child process"]
    U["untrusted process with socket access"] -.->|"currently equivalent to full trust"| K
```

| Boundary | Current strength |
| --- | --- |
| Private socket filesystem permissions | Primary access boundary. Strong against other Unix users when path ownership/modes are correct. |
| Bearer token | Identifies an agent principal; `--require-token` makes it mandatory on the public listener. |
| Peer UID | Optional `--require-peer-uid` pre-worker gate for the kernel effective UID; same-UID peers remain equivalent. |
| Leash plan policy | Evaluates declared effects and approval rules. Useful, but declarations can be opaque/incomplete. |
| Linux Landlock / macOS Seatbelt | Real filesystem restriction for child spawns when a concrete sandbox is resolved. |
| Network restrictions | Coarse deny is enforced by supported Landlock/Seatbelt backends; hostname/port allowlists remain unavailable. |
| Spawn hash/name allowlist | Pre-exec check with a documented TOCTOU window. |
| Named session | Principal-private identity namespace; not a hostile-tenant process boundary. |
| MCP facade | Safer convenience surface, not an authorization proxy around a hostile kernel peer. |

## Safe deployment checklist

For the current release:

1. Run `shoal-kernel` as an unprivileged dedicated user or your own desktop user, never root.
2. Use the default per-user runtime directory or an explicitly owned `0700` directory.
3. Verify the socket is `0600` and never bind it inside a broadly shared/mounted directory.
4. Enable `--require-token` and `--require-peer-uid` for supervised named kernels unless compatibility requires the tokenless restricted principal.
5. Do not forward the socket over SSH/TCP or mount it into containers with untrusted workloads.
6. Give mutually untrusted agents separate kernel **processes**, state directories, sockets, and preferably OS users—not merely separate session names.
7. Configure an explicit Leash policy for every token principal.
8. Nested evaluators inherit semantic principal/policy context, but they do not create a new OS isolation boundary; isolate the whole kernel process at the OS/service layer for hostile code.
9. Read `caps_enforced` and the detailed platform limitations; approval is not equivalent to sandboxing.
10. Treat revocation as immediate: the kernel revalidates bearer authority from the locked store before every request.
11. Keep journal/state/secret directories private and back them up as sensitive data.
12. Avoid `format=raw` on untrusted large values without a client-side size limit.

## Socket access is authentication

Default discovery puts the socket under a per-user directory. The kernel creates an owned directory as `0700` and binds the socket as `0600`. It refuses to delete an active listener, another user's stale socket, or a non-socket path.

This protects against other Unix users when the containing filesystem and ownership behave normally. It does not protect against:

- another process running as the same UID;
- root or a sufficiently privileged container host;
- accidental socket forwarding;
- a shared volume that changes ownership/mode semantics;
- a compromised MCP client launched under the same user;
- filesystem backup/snapshot readers with access to the state directory.

The public listener does not accept client-asserted human presence. An attachment with neither bearer nor `local_auth` becomes the restricted `agent:mcp` principal; `local_auth:"local-human"` is rejected on every public/named socket. The only local-human trust root is the server-selected inherited anonymous descriptor used by the default interactive REPL's private, listener-free child kernel. Bearer tokens select explicit machine identities; `supervisor` or `plan.approve` authority is required for cross-principal approval.

Named kernels can be hardened with `--require-peer-uid`, which compares the OS-reported peer UID
against the kernel's effective UID (`SO_PEERCRED` on Linux, `getpeereid` on supported BSD/macOS
targets) before allocating a connection worker. `--require-token` rejects every tokenless public
attachment. These are opt-in so existing local agent integrations remain compatible; unsupported
peer-credential platforms fail startup when the flag is requested. A same-UID process can still
reach a peer-bound socket, and a stolen bearer remains a credential. Socket possession alone never
upgrades a client to local human.

## State-directory sensitivity

The kernel state directory defaults to:

```text
$XDG_STATE_HOME/shoal
# otherwise
~/.local/state/shoal
```

It contains or anchors:

- SQLite/WAL journal entries with original source, AST, effects, paths, principals, statuses, and output descriptors;
- content-addressed blobs for captured output;
- transcript-event persistence;
- `tokens.json`, including the keyed-hash secret and token metadata/digests.

Journal redaction keeps `secret` values out of the typed wire/journal value encoding, but the journal is still sensitive. Source text may reveal filenames, URLs, user-provided literals, and commands; external program output may contain secrets unrelated to Shoal's `secret` type.

Use ordinary private-home permissions, encrypt backups where appropriate, and choose separate state directories when separating trust domains.

## Bearer tokens

Create an agent identity with the standalone companion:

```bash
shoal-token create agent:reviewer reviewer \
  --cap fs.read \
  --cap proc.spawn \
  --ttl 3600
```

The 32-byte random bearer is printed once on stdout. Only a keyed BLAKE3 digest is persisted; validation uses a constant-time digest comparison. Metadata includes:

- 16-hex token ID derived from the first eight digest bytes;
- principal string;
- profile label;
- capability-label array;
- creation/expiry/revocation nanosecond timestamps.

The store is written atomically through a `0600` temporary file and rename. Create/revoke hold an
exclusive interprocess lock across fresh load, mutation, and replace; validation uses a shared lock
and a fresh snapshot, so concurrent writers cannot lose updates.

The whole authority snapshot is fail-closed and bounded: 4 MiB total, 4,096 records, 256-byte
principals, 128-byte profiles, and 128 capability labels of 128 bytes each per token. Unknown JSON
fields, duplicates, noncanonical key/ID/digest encodings, or an exceeded limit invalidate the whole
snapshot. Shoal does not repair, truncate, or evict authority records automatically; it leaves the
file intact for operator diagnosis. Creation rejects invalid/capacity-exceeding input before
publishing any in-memory or on-disk change.

A durable kernel also exposes raw `auth.token.list`, `auth.token.create`, and `auth.token.revoke`
RPCs. They require either the server-established embedded-human trust root or a bearer created with
the exact `token.admin` capability. `supervisor` and `plan.approve` cannot mint credentials. Create
returns the bearer once; list exposes metadata only. Revoking an attached bearer invalidates that
connection's authority on its next request. MCP does not project these operator methods as tools.

### Profile and `--cap` mostly describe metadata

The kernel copies token `profile` and `caps` into the `session.attach` result. Leash grants still come
from the token's **principal string** in `[principal."..."]`, not those labels. Exact administrative
exceptions exist: `plan.approve` enables cross-principal plan approval, and `token.admin` enables the
live management RPCs. The legacy `supervisor` profile enables approval/shutdown, not token creation.

This means:

- `--cap fs.read` does not itself grant filesystem reading;
- a token principal absent from the policy is denied by plan evaluation;
- two tokens with the same principal share the same Leash policy even if their metadata labels differ;
- operators must keep token metadata and policy entries consistent themselves.

Treat other fields as claims/labels for clients and auditing, not enforced capability objects.

### Live revocation and fail-closed reload

Initial attach validates against a fresh shared-locked disk snapshot. Every later request refreshes
the already-authenticated token's immutable identity fields against another fresh snapshot. A newly
created token is accepted without restart; revocation or expiry invalidates an existing attachment on
its next request. Store corruption, replacement, or I/O/lock failure also fails closed instead of
falling back to startup authority. Reattach remains available after the kernel clears the attachment.

`session.attach` bounds the caller-controlled Session name, client kind, and exact canonical
43-byte bearer representation before authority-store work. Unknown attach/client fields are
rejected. Authentication errors use fixed messages and never quote bearer contents.

### Store-path alignment

`shoal-token` uses:

```text
$SHOAL_TOKEN_STORE
# otherwise $XDG_STATE_HOME/shoal/tokens.json
# otherwise ~/.local/state/shoal/tokens.json
```

The CLI and kernel share the nonempty `SHOAL_TOKEN_STORE` override. The kernel additionally accepts `--token-store`, which wins over the environment; without either override it opens `<--state-dir>/tokens.json`. Empty overrides are ignored. Relative paths resolve from each process's startup directory, so use an absolute override for supervised deployments.

## Leash policy

Start a kernel with an explicit file:

```bash
shoal-kernel --policy "$HOME/.config/shoal/leash.toml"
```

An explicit missing or malformed policy is fatal to kernel startup. Without `--policy`, the kernel constructs a permissive policy for its local-human `uid:<euid>` principal; token principals are not implicitly added.

Example:

```toml
[principal."agent:reviewer"]
net_connect = ["github.com:443", "*.githubusercontent.com:443"]
net_listen = []
proc_spawn = ["git", "rg", "cargo"]
env_read = ["HOME", "PATH", "CARGO_HOME"]
env_write = []
secret_use = ["github-token"]
session_write = true
journal_read = true
time = true
auto_apply = "reversible"
opaque = "ask"
hermetic = false

[principal."agent:reviewer".fs]
read = ["~/develop/shoal/**", "~/.cargo/**", "/usr/**"]
write = ["~/develop/shoal/**", "~/.cache/shoal/**"]
delete = ["~/develop/shoal/target/**"]
```

TOML accepts the nested `[...fs]` form above. Dotted fields such as `fs.read = [...]` are flattened by the loader as well.

### Policy fields

| Field | Value | Matching |
| --- | --- | --- |
| `fs.read` | string array | Every planned path must match a glob. |
| `fs.write` | string array | Every planned path must match a glob. |
| `fs.delete` | string array | Every planned path must match a glob. |
| `net_connect` / alias `net` | `host-pattern:port` array | Glob host; exact port or `*`. |
| `net_listen` | port array | Exact port. |
| `proc_spawn` / alias `spawn` | string array | Exact full hash, argv0, or basename at spawn gate. |
| `env_read` | name array | Exact name or `*`. |
| `env_write` | name array | Exact name or `*`. |
| `secret_use` / alias `secrets` | name array | Exact name or `*`. |
| `session_write` | boolean | Session mutation effect. |
| `journal_read` | boolean | Declared journal-read effect. |
| `time` | boolean | Wall-clock access effect. |
| `auto_apply` | `never`, `in-grant`, `reversible` | Whether an otherwise allowed plan runs without approval. |
| `opaque` | `deny`, `ask`, `allow` | Treatment of unanalyzable effects. |
| `hermetic` | boolean | Ask spawn layer to fail rather than degrade requested sandbox dimensions. |

All listed path/name/host requirements use all-of semantics: one missing grant denies the effect. Unknown principals are denied by plan evaluation.

### Path matching and sandbox roots differ

The plan layer matches normalized planned paths against full glob patterns. The OS sandbox lowers each grant to its longest concrete prefix:

```text
~/work/project/**  ->  ~/work/project
/etc/hosts         ->  /etc/hosts
/**                ->  /
**/secrets         ->  no concrete root
```

Only existing roots are installed. Nonexistent roots are dropped rather than causing sandbox setup to fail. This is fail-closed for access to that root, but it means a policy intended to permit creation under a path must have an existing concrete ancestor grant.

Parent components are lexically normalized; this is not a proof against every symlink/race edge. OS sandbox behavior remains the final filesystem boundary when active.

### Network grant syntax

Each `net_connect` item must contain a final colon:

```toml
net_connect = [
  "api.example.com:443",
  "*.example.net:*",
]
```

The host side is a glob. The port is exact `u16` or `*`. This representation does not naturally express raw IPv6 literals containing colons without additional convention; verify actual planner output before relying on an IPv6 policy.

### Auto-apply and opaque behavior

Effect evaluation first applies individual grants. Deny dominates approval; approval dominates allow. If all effects are allowed:

- `auto_apply = "never"` still requires approval;
- `auto_apply = "in-grant"` allows immediately;
- `auto_apply = "reversible"` allows only plans marked fully reversible.

`opaque` controls an effect that analysis could not make concrete:

- `deny` rejects;
- `ask` requests explicit approval;
- `allow` permits it, subject to auto-apply.

Approving an opaque effect does not teach the OS sandbox what the program will do. Use `opaque = "deny"` for high-assurance agent profiles.

## Effect model

Shoal can derive these semantic effect variants:


| Effect | Concrete data |
| --- | --- |
| `fs_read` | path list |
| `fs_write` | path list |
| `fs_delete` | path list |
| `proc_spawn` | binary content hash and argv0 |
| `net_connect` | host and port |
| `net_listen` | port |
| `env_read` | name list |
| `env_write` | name list |
| `secret_use` | name list |
| `session_write` | marker |
| `journal_read` | marker |
| `time` | marker |
| `opaque` | analysis gap |

Effects describe the planner's understanding. They are not a complete behavior proof for arbitrary native programs. An adapter can declare that `curl URL` connects to a host and writes an output path, but a compromised `curl` binary can attempt more. OS enforcement is what constrains attempted filesystem operations; unimplemented dimensions remain policy/advisory.

## Plan/approval integrity

A plan record stores source, session, principal, effects, and approval state. `plan.get`, `plan.list`, `plan.apply`, and internal approved execution compare stored metadata against the attached caller. That is the useful part of the design.

The stored object identity and approval transition are bound together as follows:

```mermaid
flowchart LR
accTitle: Plan/approval integrity
accDescr: Shows immutable plan identity, authenticated approval, durable audit, and one-shot consumption.
    P["source + AST + effects + estimates"] --> H["full bound BLAKE3 digest"]
    O["principal + Session"] --> H
    H --> R["unique in-process object ref"]
    A["authenticated distinct approver"] --> G["Granting reservation"]
    R --> G
    G --> J["durable approval audit"]
    J --> C["Approved → Claimed → Consumed"]
```

Exact current behavior:

- the full source, canonical AST, effects, reversibility, estimates, Session, and requester feed a domain-separated full BLAKE3 plan hash;
- a monotonic per-kernel suffix makes repeated storage of identical content produce distinct non-overwriting object references;
- `cap.request` requires an authenticated attachment and binds requester, approver, plan/source hashes, Session, and exact effect scope into a durable journal audit;
- self-approval is denied by default; cross-principal approval requires the embedded-human trust root or a `supervisor`/`plan.approve` bearer;
- grant/apply transitions are reserved and one-shot, with rollback on audit/request failure and compare-and-set state checks;
- plans and references remain in-memory and disappear on kernel restart, so they are not durable capabilities or secrets.

## Journal query boundary

`journal.query`, journal-backed `blob.get`, and `events.read`/`events.subscribe` for the `journal`
channel require an attachment and an allowed `JournalRead` policy effect. Authorization precedes
query/blob decoding and journal-event cursor decoding or durable access. Pages are capped server-side,
and every row/hash/event remains scoped to the exact attached principal/Session. An ungranted,
unattached, or cross-principal read is rejected. Journal rows and CAS remain sensitive persisted data, so
filesystem/state-directory permissions and bearer handling still matter.

## Named sessions are principal-private

A Session is keyed by both authenticated principal and visible name. Two principals requesting `default` receive different evaluators, bindings, cwd, environment, transcripts, tasks, PTYs, Reef state, and event ownership. References and quotas use the same exact owner key rather than the user-chosen name alone.

Current access checks:

| State | Scope check |
| --- | --- |
| Transcript `out:N` | principal + Session |
| Tasks | principal + Session |
| PTYs | principal + Session |
| Environment/cwd/bindings | principal-private evaluator |
| Plans | principal + Session + immutable content binding |
| Journal query | attached principal + exact Session |

This is identity isolation inside one process, not a complete hostile-tenant sandbox. Principals still share the kernel process, global resource budgets, journal/CAS files, and any process-wide failure boundary. Use separate OS users/processes/state directories for mutually hostile tenants.

## Sandbox enforcement

Shoal reports the strongest available platform tier and whether a concrete sandbox is active for the principal.

| Tier | Current meaning |
| --- | --- |
| A | Linux Landlock detected; filesystem rules can be fully installed. |
| B | Linux without usable Landlock; namespace fallback is not installed. |
| C | macOS Seatbelt filesystem profile available through the shipped backend. |
| D | No OS sandbox backend; policy is advisory. |

`available_tier` answers what the host could support. `caps_enforced` becomes true only when an A/C backend exists **and** the principal's grants lower to a nontrivial filesystem sandbox. A permissive `/**` local-human policy deliberately resolves to no sandbox and reports false.

Even when `caps_enforced` is true:

- filesystem access is enforced when scoped;
- coarse network denial is enforceable with Landlock ABI 4+ or Seatbelt, but host/port allowlists
  are not;
- spawn content hashing is a preflight, not exec-time pinning;
- the binary can change between hash and exec (TOCTOU);
- policy analysis can miss behavior and emit `opaque`;
- child programs can communicate through already-available resources not modeled by a declared path/host.

Never render a single “sandboxed” badge without the dimension details.

### Nested evaluator policy propagation

Every production child-evaluator route (`spawn`, `.shl` scripts, parallel closures, stream producers, and channel handlers) builds through one audited child-context constructor. It carries principal, Leash policy, Reef resolver/configuration, echo policy, filesystem port, and cancellation semantics; targeted tests and a production-site inventory pin that boundary. The outer parent statement owns journaling, so children do not silently create nested journal entries. This closes the earlier policy-loss gap, but it does not turn planner effects into a complete proof of arbitrary native code behavior; the concrete OS sandbox remains the enforcement boundary.

### Linux Landlock

The child applies read/write/delete path-beneath rules after fork and immediately before exec. With
ABI 4+, a requested coarse network denial also handles TCP bind/connect without adding allow rules.
The implementation requests a hard compatibility level and errors if Landlock is not fully active.
It does not install seccomp or a network namespace, so destination allowlists remain unavailable.

Landlock is unprivileged and useful, but its exact coverage depends on kernel ABI and filesystem behavior. Test the policy on the production kernel/filesystem combination.

### macOS Seatbelt

The child applies a generated profile with `sandbox_init`. The profile explicitly allows networking
for unrestricted requests and leaves it denied for coarse-deny requests. It reports active tier C
when successful. Apple considers this interface legacy/private for some contexts, so validate
behavior across target macOS releases.

### Hermetic intent

`hermetic = true` asks the spawn layer to refuse rather than proceed when the concrete requested sandbox cannot be fully applied. This is safer than best-effort for supported dimensions, but it is not currently a general hermetic build environment:

- network grants are not lowered into an enforced network sandbox;
- time, process tree, CPU/memory, device, and IPC isolation are not comprehensive;
- Reef's tool hermeticity and Leash's OS sandbox are related but different layers.

Test fail-closed behavior for every effect dimension your workload depends on.

## Process pinning

A nonempty `proc_spawn` list activates spawn pinning. A candidate matches when one entry equals:

- the full BLAKE3 content hash;
- the complete argv0 string;
- the executable basename.

Hash pins are stronger than names but currently use a preflight read followed by normal exec. The file can be replaced between those operations. Reef-provided hashes are reused when available; otherwise Shoal resolves through ambient `PATH` and hashes the binary.

For higher assurance:

- prefer root-owned/immutable tool locations;
- use Reef hash locks and Leash hash pins together;
- prevent write access to executable directories;
- do not claim exec-time content identity until a BPF-LSM/fd-exec-style mechanism exists.

## Secrets

Shoal's `secret` value is redacted by construction on the wire:

```json
{"$":"secret","name":"github-token"}
```

Material is held inside the evaluator value and may be converted to an OS argument only at the command boundary. The journal/value encoder records the name, not the secret bytes. Ordinary value coercion rejects accidental stringification in several contexts.

Limits still matter:

- a child process can print the secret, causing it to enter captured output/journal CAS;
- command-line arguments may be visible to same-user process inspection on some systems;
- downstream programs can write it to files or network;
- debug logging/crashes outside Shoal's typed encoder can leak it;
- secret-use policy is only as complete as effect derivation.

Prefer programs that accept secrets through protected stdin or dedicated file descriptors, avoid echoing them, and scope child filesystem/network access.

The encrypted store colocates `master.key` and ciphertext, so **directory permissions are the real
confidentiality boundary**; copying/read-access to the directory copies both. OS-keyring integration
was evaluated but deferred to avoid platform-dependent availability and migration semantics. Store
reads use a shared fd lock; key bootstrap and set/delete hold an exclusive lock across load/mutate/
save. Keys, decrypted buffers, serialized plaintext, ciphertext buffers, and stored map values use
zeroizing wrappers where practical. Final language `Arc<str>` values, environment copies, argv, and
child-process memory cannot be forcibly zeroized by the store.

The evaluator secret store resolves:

```text
$SHOAL_SECRET_DIR
$XDG_DATA_HOME/shoal/secrets
~/.local/share/shoal/secrets
```

The standalone `shoal-secret` CLI and evaluator use the same discovery contract. A nonempty `SHOAL_SECRET_DIR` wins over the XDG/HOME default; relative values resolve from each process's startup directory.

## Resource and denial-of-service limits

The protocol limits JSON content to 16 MiB and performs a fixed-stack lexical preflight before tree
allocation: depth 64, 65,536 values total, 16,384 items per container, 64 KiB decoded keys, and 1
KiB numeric tokens. Outbound frames use the same limits. Values are normally elided around 8 KiB
with a 64 KiB encoded hard cap. These are context protections, not comprehensive service quotas.

Remaining high-cost surfaces include:

- PTY child resource consumption;
- journal/CAS disk growth until garbage collection;
- CPU/memory consumed by evaluated source or child processes.

Connections, retained principal Sessions, active tasks, PTYs (per Session/principal/global), subscriptions, plan/source bytes, transcripts, stream cursors, frames, and event queues have explicit bounds. There is still no general per-principal rate, memory, CPU, or descendant-process-tree meter. Use OS service controls (cgroups/launchd limits/container quotas where appropriate), supervise the daemon, and keep hostile code outside a shared kernel process.

`task.await` no longer holds a connection worker indefinitely: it defaults to 30 seconds and has a
60-second server ceiling. A timed-out wait leaves the underlying task running for later poll,
subscription, cancellation, or another bounded await.

Callers can separately set `exec.deadline_ms` to request cancellation after a hard execution
budget. The kernel caps it at 24 hours and records whether the deadline actually fired; this uses
the existing cooperative/process-group escalation path and is not a transaction rollback guarantee.

## Security review priorities

Before describing Shoal as safe for mutually untrusted agents, the remaining minimum work is:

1. add stronger network/process/CPU/memory enforcement while preserving per-dimension truth;
2. add a portable OS-keyring backend only with explicit migration and unavailable-backend behavior;
3. extend adversarial multi-principal, fault-injection, and long-duration lifecycle testing.

Track implementation status in [Current status and limits](@/docs/status-limits.md) and [Roadmap](@/docs/roadmap.md).
