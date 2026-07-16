# shoal — inter-crate contracts (pinned)

This file pins the public APIs between crates so work can proceed in parallel.
**Do not change a pinned signature without updating this file and every consumer.**
Read `docs/TDD.md` first — it is the semantic contract; this file is the Rust-level contract.

Crate dependency DAG (acyclic, enforced; verified against every `crates/*/Cargo.toml`
`shoal-*` dependency line — see reproduction command below), by tier:

```
Tier 0 — leaf (no shoal-* deps):
  shoal-ast  shoal-auth  shoal-config  shoal-journal  shoal-leash
  shoal-proto  shoal-reef  shoal-secret  shoal-wasm  shoal-prompt

Tier 1 — depend only on Tier 0:
  shoal-value   → ast
  shoal-syntax  → ast    (also owns the canonical builtin command-head registry —
                          `shoal_syntax::commands::builtin_names()` — see note below)
  shoal-exec    → leash    (NOT a leaf — see note below)
  shoal-history → journal
  shoal-lsp     → syntax

Tier 2 — depend on Tier 0/1:
  shoal-adapters → ast, value
  shoal-picker   → value

Tier 3 — the domain core:
  shoal-eval → adapters, ast, exec, journal, leash, picker, reef, secret, syntax, value

Tier 4 — composition roots (daemon + tools):
  shoal-doctor → adapters, journal, leash
  shoal-kernel → ast, auth, eval, exec, journal, leash, proto, syntax, value

Tier 5 — entrypoints (binaries):
  shoal        → adapters, ast, config, doctor, eval, prompt, syntax, value
  shoal-mcp, shoal-lsp  — spawned by `shoal` as companion subprocesses (`Action::Companion`,
                          crates/shoal/src/main.rs), NOT Cargo dependencies of `shoal`;
                          shoal-mcp itself has zero shoal-* deps in `[dependencies]` (talks to
                          shoal-kernel's socket over the wire protocol, not via Rust linkage —
                          shoal-kernel/shoal-proto appear only as `[dev-dependencies]`, for its
                          own integration tests). shoal-lsp links only shoal-syntax (Tier 1) —
                          parse/format/diagnostics plus the canonical `builtin_names()` command-head
                          registry that now lives in `shoal_syntax::commands`, so LSP completion
                          shares one source of truth with the evaluator without pulling shoal-eval
                          into the DAG. (The registry used to live in shoal-eval, which briefly
                          lifted shoal-lsp to Tier 4; hoisting the list into the leaf shoal-syntax
                          crate — which every consumer already links — dropped that edge and put
                          shoal-lsp back at Tier 1.)
  shoal-kernel  — the long-lived per-user daemon (TDD §10); MCP/LSP/agent/human clients
                  attach to its socket. Independent of the `shoal` binary at the Cargo-dep
                  level (`shoal` never depends on or spawns `shoal-kernel`) — the two are
                  alternative hostings of the same core (TDD §1.1: "kernel-less" via `shoal
                  script.shl` links shoal-eval in-process; kernel-hosted sessions attach over
                  the socket instead).
```

`shoal-prompt` (TDD-adjacent `design-prompt.md` §2.1) is a **pure** leaf: it renders a shoal
prompt string from a structured `PromptContext` snapshot with no IO and, deliberately, zero
`shoal-*` deps in `[dependencies]` (only `serde` + `toml`) — `shoal-value` appears strictly as a
`[dev-dependencies]` entry for a render-parity tripwire test and never ships in the compiled
graph. `shoal` (the binary) is the only in-tree consumer today and is responsible for gathering
the live context (cwd, git status, reef state, etc.) and feeding it in; `shoal-prompt` itself
never reaches out to git/fs/reef.

`shoal-eval`'s internals are split across many files under `crates/shoal-eval/src/` (one
`impl Evaluator { .. }` block per file, following the `reef.rs` precedent) rather than one
monolithic `lib.rs` — current modules: `args, builtins, call, channels, coerce, command, expr,
helpers, host, journal, modules, namespaces, pattern, plan, reef, script, stmt, streams`. This is
purely an internal-organization detail (no public-API surface beyond what's pinned in this file)
and is expected to keep growing/reshuffling as eval-heavy waves land; the DAG tiers above are
unaffected since none of these are separate crates.

Notes / corrections vs the old (stale) diagram:
- `shoal-exec` is **not** a leaf — it depends on `shoal-leash` (sandboxing hooks around spawn).
- `shoal-eval` additionally depends on `shoal-adapters`, `shoal-leash`, `shoal-secret`,
  `shoal-picker`, `shoal-reef` (not just `shoal-value`/`shoal-syntax`/`shoal-exec`/`shoal-journal`
  as the old diagram implied).
- The daemon/tooling tier (`shoal-kernel`, `shoal-mcp`, `shoal-lsp`, plus `shoal-doctor`,
  `shoal-history`, `shoal-config`, `shoal-wasm`) previously had no place in the diagram at all.
  `shoal-kernel` is the real composition root for the daemon-hosted surfaces (MCP/LSP/agent
  clients all attach through it); `shoal` (binary) is the composition root for the kernel-less
  REPL/script-runner surface. `shoal-wasm` (WASM component-plugin host) and `shoal-mcp` currently
  have no other in-workspace crate depending on them as a library — `shoal-wasm` isn't wired into
  `shoal-eval`'s command dispatch yet, and `shoal-mcp` is a standalone binary that speaks the wire
  protocol to a running `shoal-kernel`, not a Rust dependency of anything.
- No cycles exist anywhere (Cargo would reject the workspace if one did); the one
  cross-tier-looking edge (`shoal-value` needing closure-eval from `shoal-eval`) is a proper
  dependency *inversion*, not a back-edge: `shoal-value` defines `trait CallCtx` (§7 below) and
  `shoal-eval` implements it, so the arrow in the DAG still points `shoal-eval → shoal-value`.

Reproduce: `grep -oE '^shoal-[a-z]+' crates/*/Cargo.toml` (per crate) to re-derive this table if
crates are added/moved; re-run whenever a crate's `Cargo.toml` `[dependencies]` changes.

Ownership map:
- `shoal-ast`, `shoal-value` (core types), `shoal-syntax`, `shoal-eval`, `shoal` (binary): owned by the integrator. Do not edit unless your task says so.
- `shoal-exec`, `shoal-journal`, `shoal-adapters`, `shoal-reef`, `shoal-value/src/methods.rs` + `render.rs`, `spec/cases/*.toml`: delegated modules — build to the contracts below.

Build hygiene for parallel work: only edit files inside your assigned crate/dir; never touch the workspace `Cargo.toml`; if you must add a dependency use `cargo add -p <your-crate> <dep>`; run your tests with `CARGO_TARGET_DIR=target-<yourcrate> cargo test -p <your-crate>` to avoid lock contention.

---

## 1. shoal-exec — public API (pinned)

Blocking, thread-based. No tokio. `libc` + `portable-pty` allowed.

```rust
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct ExecSpec {
    pub argv: Vec<OsString>,   // argv[0] = program; if it contains '/', run as-is, else resolve via `which` against env PATH
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,  // the COMPLETE child environment
    pub stdin: StdinSpec,
    pub mode: ExecMode,
    pub sandbox: Option<shoal_leash::SandboxPolicy>,  // None = existing unsandboxed behavior; Some = apply the
                                                      // strongest available OS enforcement pre-exec (TDD §8),
                                                      // reported honestly via ExecResult::enforcement
}

pub enum StdinSpec { Null, Inherit, Bytes(Vec<u8>), File(PathBuf) }

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Capture,   // stdout/stderr = pipes, stdin per spec, no controlling tty, child in its own process group
    PtyTee,    // child on a real PTY (its own session): bytes stream raw to the REAL terminal AND are teed
               // into the returned stdout buffer; real stdin is forwarded to the pty (terminal in raw mode
               // for the duration); window resizes propagated; stderr merged into stdout (pty semantics)
}

pub struct ExecResult {
    pub status: Option<i32>,     // Some(code) on normal exit
    pub signal: Option<String>,  // Some("SIGSEGV") etc. on signal death (never 128+n encoding)
    pub stdout: Vec<u8>,         // captured bytes (PtyTee: the teed merged stream)
    pub stderr: Vec<u8>,         // captured bytes (PtyTee: empty)
    pub truncated: bool,         // captured buffer hit capture_hard_cap() and was truncated (TDD §317);
                                 // stdout/stderr is a prefix — child still ran, PtyTee terminal saw all
    pub dur: std::time::Duration,
    pub pid: u32,
    pub enforcement: Option<shoal_leash::EnforcementStatus>,  // Some iff sandbox was requested: the tier
                                                              // ACTUALLY applied (TDD §8 tier honesty) —
                                                              // never `enforced: true` unless it really was
}

// Configurable in-memory cap on value-position capture (TDD §317). Default 64 MiB; env override
// SHOAL_CAPTURE_CAP_BYTES; set_capture_hard_cap for hosts/tests. Bounds RAM so `let x = (yes)` can't OOM.
pub const DEFAULT_CAPTURE_HARD_CAP: usize;
pub fn capture_hard_cap() -> usize;
pub fn set_capture_hard_cap(bytes: usize);

#[derive(Clone, Default)]
pub struct CancelToken(/* private */);
impl CancelToken {
    pub fn new() -> Self;
    pub fn cancel(&self);           // idempotent
    pub fn is_cancelled(&self) -> bool;
}

/// Run to completion. Cancellation: SIGINT to the child's process group, escalate SIGTERM after 3s,
/// SIGKILL after 3 more. Returns normally with `signal` recorded.
pub fn run(spec: ExecSpec, cancel: &CancelToken) -> std::io::Result<ExecResult>;

/// Spawn with piped stdout/stderr for streaming consumption (background tasks, `tail -f`).
pub struct StreamingChild {
    pub pid: u32,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
}
impl StreamingChild {
    /// Wait for exit; honors the same cancellation escalation.
    pub fn wait(self, cancel: &CancelToken) -> std::io::Result<ExecResult>; // stdout/stderr in result empty; caller drained the readers
}
pub fn spawn_capture(spec: ExecSpec, cancel: &CancelToken) -> std::io::Result<StreamingChild>;

/// PATH resolution (no shell involved, ever).
pub fn which(name: &OsStr, path_var: Option<&OsStr>) -> Option<PathBuf>;

// Source-compat wrappers around the ExecSpec::sandbox path (hard/fail-closed Landlock). Kept only
// for compatibility — exercised by shoal-exec's own tests, no other in-tree callers; new code sets
// ExecSpec::sandbox instead.
pub fn run_sandboxed(spec: ExecSpec, cancel: &CancelToken, sandbox: shoal_leash::FsSandbox,
                     verified: Option<&shoal_leash::SpawnPreflight>) -> std::io::Result<ExecResult>;
pub fn spawn_capture_sandboxed(spec: ExecSpec, cancel: &CancelToken, sandbox: shoal_leash::FsSandbox,
                     verified: Option<&shoal_leash::SpawnPreflight>) -> std::io::Result<StreamingChild>;
```

Requirements: no zombies (always reaped); parent terminal state always restored (PtyTee restores cooked mode even on panic — use a drop guard); Capture-mode children get `setpgid(0,0)`; E2BIG and spawn errors surface as `io::Error`. Unit tests must cover: echo capture, exit codes, signal death (`kill -SEGV` a child), stdin Bytes, cancellation kills a sleeping child, which() resolution, PtyTee against the `script`-style check `test -t 1` (child sees a tty) — PTY tests must be skipped gracefully when the test runner itself has no tty (CI): only assert what's assertable (child sees pty, bytes teed).

## 2. shoal-journal — public API (pinned)

SQLite (rusqlite bundled) + blake3 CAS. Schema per TDD §9 (entry/output/undo/pin/transcript_event
tables; WAL mode). `transcript_event` was added additively (`CREATE TABLE IF NOT EXISTS` inside
`init_schema`, which runs on every `open`/`open_with_options`/`in_memory` call) — a pre-existing
`journal.db` written before this table existed opens unchanged and gains the table the next time it
is opened; no versioned migration step exists or is needed for column/table additions in this crate.

```rust
use std::path::Path;

pub struct Journal { /* private */ }

pub struct EntryRecord {
    pub session: String,
    pub principal: String,     // "human" | "agent:<name>"
    pub ts_ns: i64,
    pub cwd: Vec<u8>,          // bytes of the cwd path
    pub src: String,           // source text as typed
    pub ast_json: String,      // canonical AST JSON
    pub effects_json: String,  // JSON array of effect instances; "[\"opaque\"]" for T0
    pub opaque: bool,
}

pub struct OutputRow { pub kind: String, pub hash: String, pub len: i64, pub meta: Option<OutputMeta> }
pub struct OutputMeta { pub truncated: bool, pub original_len: u64, pub stored_len: u64 }
// meta is Some iff the blob was truncated at the output hard cap before storage.
pub struct EntryRow {
    pub id: i64, pub session: String, pub principal: String,
    pub ts_ns: i64, pub dur_ns: Option<i64>, pub cwd: Vec<u8>,
    pub src: String, pub ast_json: String, pub effects_json: String,
    pub status: Option<i32>, pub ok: Option<bool>, pub opaque: bool,
    pub outputs: Vec<OutputRow>,
}

#[derive(Default)]
pub struct JournalQuery {
    pub since_ts_ns: Option<i64>,
    pub head: Option<String>,      // match entries whose src's first word == head
    pub principal: Option<String>,
    pub ok: Option<bool>,
    pub limit: usize,              // 0 = default 100
}

/// One durably-stored `session.transcript` channel event (AGENT-SURFACE §4),
/// keyed by the journal `entry_id` of the exec that produced it (at most one row per entry_id;
/// only a successful exec ever produces one).
pub struct TranscriptEventRow { pub entry_id: i64, pub ts_ns: i64, pub payload_json: String }

// JournalOptions { output_hard_cap: usize (default 256 MiB), busy_timeout: Duration (default 5s) }.
// busy_timeout is applied on every connection at open: the journal is shared across processes, and
// the journaling call sites swallow errors, so a 0 timeout silently drops a concurrent write + inverse.
impl Journal {
    pub fn open(state_dir: &Path) -> rusqlite::Result<Journal>; // creates <dir>/journal.db (WAL) + <dir>/cas/
    pub fn open_with_options(state_dir: &Path, options: JournalOptions) -> rusqlite::Result<Journal>;
    pub fn in_memory() -> rusqlite::Result<Journal>;            // CAS in a temp dir
    pub fn in_memory_with_options(options: JournalOptions) -> rusqlite::Result<Journal>;
    pub fn append(&self, e: &EntryRecord) -> rusqlite::Result<i64>;
    pub fn finish(&self, id: i64, status: Option<i32>, ok: bool, dur_ns: i64) -> rusqlite::Result<()>;
    /// Store bytes in CAS (zstd), link to entry. kind: "stdout" | "stderr" | "value" | "render" | "undo-snapshot". Returns blake3 hex.
    pub fn record_output(&self, id: i64, kind: &str, bytes: &[u8]) -> rusqlite::Result<String>;
    /// As record_output, but also returns Some(OutputMeta) when the blob was truncated to output_hard_cap
    /// (None when stored whole). Undo snapshots use this to refuse keying a replayable inverse on partial bytes.
    pub fn record_output_meta(&self, id: i64, kind: &str, bytes: &[u8]) -> rusqlite::Result<(String, Option<OutputMeta>)>;
    /// Content-addressed read: decompressed bytes are re-hashed against `hash`; a mismatch (corruption/bit-rot)
    /// is an integrity Err, never wrong bytes. Ok(None) only when the blob is absent / hash is malformed.
    pub fn read_blob(&self, hash: &str) -> rusqlite::Result<Option<Vec<u8>>>;
    pub fn query(&self, q: &JournalQuery) -> rusqlite::Result<Vec<EntryRow>>;
    /// Targeted fetch of entries by id, in the EXACT order requested (not database order); ids not
    /// found are simply absent, never an error. The cold-replay counterpart to `query`'s filtered
    /// newest-first scan, for a caller (e.g. shoal-kernel's `journal`-channel replay) that already
    /// knows precisely which rows it needs and wants to avoid a wide scan + in-memory filter.
    pub fn entries_by_id(&self, ids: &[i64]) -> rusqlite::Result<Vec<EntryRow>>;
    /// Record an undo inverse for an entry (op: "trash", "restore_bytes", ...; inverse: JSON).
    pub fn record_undo(&self, id: i64, op: &str, inverse_json: &str) -> rusqlite::Result<()>;
    pub fn undos_for(&self, id: i64) -> rusqlite::Result<Vec<(String, String)>>;
    /// Persist a `session.transcript` event's exact `$`-tagged payload JSON for `entry_id`
    /// (AGENT-SURFACE §4), so `shoal-kernel` can reconstruct it after it ages out of the in-memory
    /// ring. Called once, right when the live event is published (`handlers_exec.rs`).
    pub fn record_transcript_event(&self, entry_id: i64, ts_ns: i64, payload_json: &str) -> rusqlite::Result<()>;
    /// As `entries_by_id`, for `transcript_event` rows: exact requested order, missing ids absent.
    pub fn transcript_events_by_entry(&self, entry_ids: &[i64]) -> rusqlite::Result<Vec<TranscriptEventRow>>;
}
```

CAS layout: `<state_dir>/cas/<hex[0..2]>/<hex[2..4]>/<hex>.zst`. Dedup by hash. Tests: roundtrip, dedup, query filters, WAL crash-tolerance smoke (drop without finish → row visible with NULL status), `entries_by_id`/`transcript_events_by_entry` order-preserving + missing-id behavior, and an additive-migration smoke test (a hand-built pre-`transcript_event` journal.db still opens and gains the table).

## 3. shoal-value — Value enum (pinned; core types by integrator)

The `Value` enum, `Env`, `ErrorVal` etc. live in `crates/shoal-value/src/lib.rs` — READ THAT FILE, it is the source of truth. Delegated within this crate:

- `src/methods.rs`: the value-method stdlib — `pub fn call_method(ctx: &mut dyn CallCtx, recv: Value, name: &str, args: CallArgs, span: Span) -> VResult<Value>` covering the TDD §5 method set (`.where .sort .map .each .first .last .len .is_empty .sum .uniq .join .lines .words .chars .split .trim .starts_with .ends_with .contains .replace .matches .match .upper .lower .reverse .keys .values .get .items .str .display .parse_int .parse_float .json .save .append .collect .tee .count .any .all .find .filter .flatten .flat_map .zip .enumerate .skip .take .chunks .sort_by .group .reduce/.fold .min .max .abs .round .floor .ceil .set .merge` plus type-specific ones per TDD).
- `src/render.rs`: `pub fn render_inline(v: &Value) -> String` and `pub fn render_block(v: &Value, width: usize) -> String` (pretty tables via unicode-width, ANSI-colored headers).
- A caught `error` value (bound by `catch err { … }`) exposes `.code .msg .hint .stderr .status` as fields so a handler can branch on the failure (absent optionals read as `null`).
- `.replace(pat, rep)`, like `.matches`/`.match`, takes `pat` as either a literal `str` (all occurrences) or a `regex` (all matches; `$1`/`$name` in `rep` expand capture groups). `.take`/`.skip` slice a `str` by char (substring), not just collections. `.round(n)`/`.floor(n)`/`.ceil(n)` round a `float` to `n` decimal places (`n` optional, default `0` → nearest integer); ints pass through unchanged.
- Every zero-arg `path` accessor is field-reachable as well as method-reachable: `.name .stem .ext .parent` (pure) and `.read .read_bytes .lines .exists .is_dir .is_file .size .modified` (fs-backed, via the `Fs` port) resolve through plain field access, so the `.field` shorthand in implicit lambdas reaches them — `glob("*.rs").map(.name)`, `ls.where(.size > 1mb)`, `glob("*.toml").map(.read.parse_toml())`. Only the argument-taking path methods (`.join .abs .save .append`) stay method-only. (`crates/shoal-eval/src/expr_access.rs::field`.)
- `.sum()` folds a numeric/quantity `list` from its first element, so `list<size>`/`list<duration>` sum to a `size`/`duration` (not an int-vs-quantity `type_error`); an empty list sums to `0`.

Render rules (normative — the conformance corpus depends on these):
- `null` → `null`; bool → `true`/`false`; int → decimal; float → Rust `{}` Display.
- str: `render_inline` double-quotes (`"a b"`, control chars escaped); at top-level `render_block` prints contents verbatim, no quotes.
- path → lossy display, no quotes inline unless it contains spaces (then quoted); glob → its pattern; regex → `re"<src>"`.
- size → largest decimal unit with integer part ≥ 1, ≤2 decimals, trailing zeros trimmed: `237b`, `1.5mb`, `1.02kb`. (`kib` family only when constructed binary — not in v1 render.)
- duration → compound, no spaces, units `w d h m s ms us ns`, nonzero parts only: `1m30s`, `250ms`, `1s500ms`. Zero → `0s`.
- datetime → RFC3339; time → 24h `HH:MM` (`:SS` only when nonzero).
- list → `[1, "a", null]`; record → `{name: "x", n: 3}` (keys unquoted when identifier-shaped); table inline → same as list-of-records.
- outcome → `outcome(status: 0, ok: true)`; error → `error(<code>: <msg>)`; secret → `secret(<NAME>)`; stream → `stream<…>`; task → `task(<id>)`.

## 4. Error codes (pinned — corpus asserts these)

`parse_error type_error arg_error undefined_var not_found cmd_failed div_zero index_range field_missing utf8_error stream_consumed no_matches custom assert_failed permission recursion_limit overflow`

Extensions from the companion design docs (each pinned there; collected here per this file's own rule that every code the corpus can assert lives in one list):
- reef (REEF.md §7): `reef_unlocked reef_drift reef_conflict reef_not_found reef_provider`
- IO (IO.md §5): `feed_error lang_block_unbalanced runner_not_found`
- streams (STREAMS.md, alongside the already-pinned `stream_consumed`): `stream_unbounded`

## 5. Conformance corpus schema (`spec/cases/*.toml`)

```toml
[[case]]
name = "unique-kebab-name"
src  = """
let x = 2 + 3
x * 2
"""
value = "10"                    # render_inline of the FINAL statement's value
# OR error = "type_error"       # eval error code
#    error_contains = "substr"  # optional
# OR parse_error = true
#    parse_error_contains = "no pipe operator"
# Optional:
fixture = ["a.txt", "sub/b.log"]  # empty files created under a temp cwd before eval
stdin   = "..."                   # reserved
skip    = "reason"                # harness skips with reason
```

Harness semantics: each case runs in a fresh interpreter, cwd = fresh temp dir containing `fixture` entries (dirs auto-created), no journal, value-position capture for commands. A multi-statement `src` yields the last statement's value (`let`/`fn`/assignment yield `null`). Keep expected values to stable renders (ints, strs, bools, lists, records, sizes, durations); avoid environment-dependent output.

## 6. shoal-adapters — public API (pinned)

```rust
pub struct AdapterCatalog { /* cmds: HashMap<String, CmdAdapter> */ }
impl AdapterCatalog {
    pub fn empty() -> Self;
    pub fn load_dir(dir: &std::path::Path) -> (Self, Vec<String>);  // (catalog, warnings) — never fails hard
    pub fn lookup(&self, head: &str) -> Option<&CmdAdapter>;
}
pub struct CmdAdapter {
    pub name: String, pub bin: String, pub class: AdapterClass,
    pub ok_codes: Vec<i32>, pub top: SubSpec,
    pub subs: std::collections::HashMap<String, SubSpec>,
}
#[derive(PartialEq)] pub enum AdapterClass { Cli, Tui, Daemon }
pub struct SubSpec {
    pub params: Vec<ParamSpec>,               // typed flag params
    pub positional: Vec<String>,              // names of positional params, in order (subset of params)
    pub short_flags: std::collections::HashMap<String, String>, // "s" -> "short"
    pub invoke: Option<Vec<String>>,          // argv template replacing "<head> <sub>"
    pub parse: String,                        // "json"|"ndjson"|"csv"|"tsv"|"lines"|"kv"|"z-records"|"porcelain-v2"|"none"
    pub output_type: Option<String>,          // promised shape, informational in v1
    pub effects: Vec<String>,
    pub ok_codes: Option<Vec<i32>>,
}
pub struct ParamSpec { pub name: String, pub ty: String }  // ty: "str"|"bool"|"int"|"float"|"path"|"glob"|"size"|"duration" (+ "?" suffix = optional)

/// Structured-output parser strategies. Column names/types from `type_hint` like
/// "table<{hash: str, author: str, date: datetime, subject: str}>" when present.
pub fn parse_output(strategy: &str, bytes: &[u8], type_hint: Option<&str>) -> Option<shoal_value::Value>;
```

Ship an adapter pack under `adapters/`: git (status porcelain-v2, log z-records, branch, push, pull, diff, add, commit), cargo (build/test/run: ok_codes, effects), rg (json parse), ls→(unused; ls is a builtin), docker ps (tsv via invoke template), kubectl get (json), jq, curl, tar, fd, du. Verify with unit tests on canned fixture bytes (no network, no real binaries required).

## 7. Eval ↔ methods bridge (pinned)

```rust
// in shoal-value
pub struct CallArgs { pub pos: Vec<Value>, pub named: Vec<(String, Value)> }
pub trait CallCtx {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value>;
    fn cwd(&self) -> std::path::PathBuf;
}
pub type VResult<T> = Result<T, ErrorVal>;
```

## 8. Hexagonal ports (pinned; ROADMAP R4 — done)

The evaluator's domain logic depends on trait objects for every OS-facing effect family, not on
`std::fs`/`std::process`/`std::time` directly — this is what makes `shoal-eval` unit-testable
without touching a real filesystem/process/clock. Four traits live in `shoal-value/src/ports.rs`
(kept there so they're reachable from a leaf crate); `Exec` lives in `shoal-eval/src/ports.rs`
instead because its signature is stated in terms of `shoal-exec` types and `shoal-value` must stay
a leaf. Every trait has a `Std*` default adapter that performs the *exact* call the evaluator made
inline before the ports existed — installing the default is behavior-identical to the pre-ports
code, so this refactor was purely structural.

```rust
// shoal-value/src/ports.rs
pub trait Fs: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn append(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn touch(&self, path: &Path) -> io::Result<()>;
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn create_dir(&self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64>;
    fn hard_link(&self, src: &Path, dst: &Path) -> io::Result<()>;
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
}
pub trait Clock: Send + Sync { fn now_ns(&self) -> i64; }     // journal timestamps, deterministic under test
pub trait Opener: Send + Sync { fn open(&self, path: &Path) -> Result<(), String>; }  // the `open` builtin
pub trait SecretPort: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String>;   // backs `secret.get(name)`
}

// shoal-eval/src/ports.rs — needs shoal-exec types, so it can't live in shoal-value
pub trait Exec: Send + Sync {
    fn run(&self, spec: shoal_exec::ExecSpec, cancel: &shoal_exec::CancelToken)
        -> std::io::Result<shoal_exec::ExecResult>;
}
```

`Evaluator` holds each as a `Box<dyn Port>`; tests swap a fake to interpose on reads/writes/spawns/
time without touching the real OS. Do not reintroduce a direct `std::fs`/`std::process`/
`SystemTime::now()` call in `shoal-eval`'s domain logic — route it through the matching port, adding
a method to the trait (plus the `Std*` default) if the one you need doesn't exist yet.

`methods.rs` must be pure over these (no direct process spawning; `.save`/`.append` do fs IO relative to `ctx.cwd()`).
