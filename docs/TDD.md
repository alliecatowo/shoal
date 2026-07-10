# shoal — Technical Design Document

**TDD v0.1 — the contract.** Companion to the PRD (formerly "hull"). Where the PRD argued *why*, this document locks *what*: the complete surface syntax, the desugaring rules, the semantics, the wire protocol, the enforcement mapping, the storage formats, and the build plan. Everything here is a decision, not an option. Open items are quarantined in §14 so the rest can be treated as stable.

---

## 0. The name

**shoal.** A shoal is a school of fish — many swimmers moving as one — and a shoal is the shallow water where careless vessels run aground. Both readings are the product: the first is the session thesis (a human and several agents schooling in one shared session; the multi-principal kernel *is* the shoal), the second is what happens to bash-era assumptions on contact with this design. It starts with `sh`, it's five characters, it's pleasant to type, and it carries the nautical lineage from `hull` — v0.1 named the structure of one vessel; v0.2 realized the point was never the vessel, it's the school. And yes: the most beloved alternative shell of the last twenty years is `fish`. A shoal is what you call more than one. The succession joke is free and it lands with exactly the audience we're recruiting.

Naming map used throughout: binary `shoal`; kernel process `shoal-kernel`; script extension `.shl`; shebang `#!/usr/bin/env shoal`; config `~/.config/shoal/shoal.toml`; state `~/.local/share/shoal/` (journal, CAS); runtime socket `$XDG_RUNTIME_DIR/shoal/<session>.sock`. The capability/policy engine gets its own name because it deserves one: **leash** — the one subsystem where a control metaphor is exactly right.

---

## 1. Deltas from the PRD — assumptions challenged, again

**1.1 The kernel is no longer mandatory.** The evaluator, value model, and exec engine live in an embeddable core; `shoal script.shl` and `shoal -c` run **kernel-less** (in-process journal buffer, flushed to a file or discarded; no socket, no daemon). Interactive sessions and agent sessions attach to `shoal-kernel`, which is the same core hosted long-lived. One engine, two hostings.

**1.2 The PTY position rule.** One rule keyed to syntax position:

- **Statement position** (the call is the whole statement, in an interactive tty session): the child gets a **real PTY, passed through raw** — colors, progress bars, cursor tricks, everything, byte-identical to bash — while the byte stream is **teed** into the CAS. `it` then binds to the outcome, with structured parsing (T1) applied lazily to the teed bytes on first structured access. `cargo build` at the prompt looks *exactly* like it does today, and is still a value afterward.
- **Value position** (the call feeds a chain, an assignment, an argument, or any expression): the child gets **pipes and no controlling tty**. It self-detects non-interactive, emits clean machine output, and shoal captures it structurally.

Scripts and non-tty sessions are all value-position. `interact <cmd>` forces a PTY anywhere; adapters can declare `class = "tui"` to get this automatically. Redirects force capture (the tee makes `it` work regardless).

**1.3 Muscle-memory sugar — reinstated.** Four bashisms earn their keep:

- `cmd args > file` and `>> file` (command mode only) — desugars to `.save(file)` / `.append(file)` on the outcome's stdout bytes. `< file` feeds stdin. No `2>`, no `&>`, no fd-number plumbing — `.stderr.save(...)` exists and is better.
- `cmd args &` — desugars to `spawn { cmd args }`; prints the task handle.
- `NAME=value cmd args` — leading `IDENT=word` tokens desugar to `with env: {NAME: "value"} { cmd args }`.
- `--` — end-of-flags marker, honored by adapters, passed through raw at T0.

**1.4 `|` gets a teaching error, and one asylum.** A lone `|` anywhere outside `sh { }` is a hard parse error with a curated diagnostic: *"shoal has no pipe operator — data composes with `.` (try `ls.where(.size > 1mb)`); raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`."* One exception: `|` is legal **inside `match` patterns** as alternation (`0 | 1 => "bit"`).

**1.5 Empty globs are empty lists — nullglob by construction.** A glob that matches nothing is an empty list of paths, never a literal string. Statement-level lint additionally flags globs that matched nothing.

**1.6 User functions ARE commands — full unification.** `fn deploy(env: str, dry: bool = false) { … }` is immediately invocable as `deploy staging --dry`, with tab completion, `--help` synthesis, typed coercion, and flag parsing derived from the signature.

**1.7 `pick` — the native fuzzy picker.** `anything.pick()` opens a fuzzy-select overlay over a table/list/stream and returns the *selected rows as values*. Multi-select with Tab.

**1.8 `alias` is AST-level partial application, never text.** `alias gs = git status` binds `gs` to a partial call node; invoking `gs -sb` appends arguments to the AST. No textual splicing exists anywhere in the language.

**1.9 Streams are single-consumption.** A `stream<T>` may be consumed once; second consumption is a runtime error with a fix-it ("collect first, or `.tee(2)`").

**1.10 No truthiness.** `if list { }` is a type error; `.is_empty()`, `.is_some()`, `!= null` exist. The only non-bool admitted by `&&`/`||` is a command **outcome**, whose truth is *success*.

**1.11 Shared-session `it` is per-client.** Each attached client has a private `it` cursor; `out[n]` indexes the shared transcript globally.

**1.12 Adapters declare success codes.** Adapter field `ok_codes = [0, 1]` (optionally mapping code→meaning) keeps "non-zero raises" as the *default* without making it a lie.

---

## 2. Lexical structure

Source is UTF-8. The lexer is **modal**: the parser drives it between `CMD` mode (command word soup) and `EXPR` mode (conventional tokens). Mode is a property of grammar position, never of runtime state.

### 2.1 Tokens common to both modes

- **Comments**: `#` begins a comment *only at token start* (preceded by whitespace, line start, or an opening delimiter). `ver#2` in CMD mode is one word.
- **Terminators**: newline or `;`. A statement **continues** across newlines when the line ends with a binary operator, `,`, or an unclosed `( [ {`; when the *next* line begins with `.` (chain continuation) or `catch`/`else`; or after an explicit trailing `\`.
- **Strings**: `"…"` interpolating — `{expr}` embeds any expression; escapes `\n \t \r \0 \\ \" \{ \} \u{1F980}`. `'…'` raw — zero escapes, cannot contain `'`. Triple forms `"""…"""` / `'''…'''` are multiline with common-leading-whitespace stripping.
- **Numbers**: `123`, `1_000_000`, `0xFF`, `0o755`, `0b1010`, `3.14`, `1e9`. Maximal munch binds a following unit into a single literal:
  - **size**: `b kb mb gb tb` (decimal) and `kib mib gib tib` (binary) → `1.5gb`, `4kib`.
  - **duration**: `ns us ms s m h d w` → `250ms`, `30d`, `1.5h`.
- **time**: `\d{1,2}:\d{2}(:\d{2})?(am|pm)?` lexes as a single time literal (`10:00am`, `23:15`). Absolute dates use tagged form only — `t"2026-07-09"`, `t"2026-07-09T14:00Z"`. Relative anchors are functions: `now`, `today`; durations compose via `.ago` / `.from_now`.
- **regex**: `re"…"` — a tagged literal producing a compiled `regex` value; raw semantics inside.
- **Reserved words**: `let var fn alias use export return break continue if else match for in while try catch true false null`.
- **Illegal everywhere** (curated diagnostics): lone `|` outside match-patterns and `sh{}`; `$` (reserved, error says "shoal variables have no sigil"); backtick (error points at `sh { }` and `re"…"`/`t"…"`).

### 2.2 CMD-mode tokens

A **word** is a maximal run of characters excluding: whitespace, `( ) { } [ ] " ' ; & < >` and token-initial `#`. Then, by shape:

- Word begins `~/`, `./`, `../`, or `/` → **path literal** (type `path`; `~` expands at this point; bytes-backed).
- Word contains unquoted `*`, `?`, `[…]`, or `**` → **glob literal** (compiled; *expansion site is the callee*).
- Word matches `--ident(=…)?` or `-[A-Za-z0-9]+` → **flag token**.
- Word is `IDENT=rest` at head position → **env-prefix token**.
- Anything else → **bare word**, type `str`.
- `(expr)` embeds a full EXPR-mode expression as one argument. `>` `>>` `<` are redirect operators. `&&` `||` chain; a trailing `&` backgrounds. A `{` after the argument list opens a **trailing block** (thunk) — a literal-brace argument must be quoted.

### 2.3 EXPR-mode tokens

Conventional: identifiers `[A-Za-z_][A-Za-z0-9_]*` (no hyphens — hyphenated *command names* are CMD-mode words or quoted heads: `run("docker-compose", …)` / `^docker-compose up`), delimiters, literals above. `-` is always minus. Globs and bare paths do not lex in EXPR mode — use `glob("*.rs")`, `path("…")`, or string literals with coercion.

---

## 3. Grammar

### 3.1 Statement dispatch — the two-mode rule, made precise

For each statement, examine the first token:

1. A **reserved word** → parse that construct (`let`, `if`, `for`, `fn`, …).
2. A **non-identifier** (literal, `(`, `[`, `{`, `-`, `!`, `.` chain-continuation) → EXPR statement.
3. An **identifier** `X`. Peek one token:
   - next is `=` or a compound-assign → **assignment** (X must be a `var`).
   - X resolves to a **bound variable** in lexical scope → EXPR statement (rest of line lexes EXPR; `x - 1` is subtraction; a stray bare word after a variable is a parse error with a "did you mean the command? use `^x`" hint).
   - otherwise → **COMMAND statement** (rest of statement lexes CMD; `X` resolves through: session `fn`s/aliases → adapters → PATH; unresolved = command-not-found with did-you-mean across all three).
   - Refinement: identifier immediately followed by `.` (no whitespace) and then an identifier → EXPR statement (invoke-then-chain desugar: `ls.where(…)` ≡ `ls().where(…)`).
4. Escape hatches: `^X …` forces command interpretation regardless of shadowing; `run("name", args…)` is the fully dynamic form. Shadowing a resolvable command name with `let` produces a lint, not an error.

### 3.2 EBNF (normative)

```ebnf
program     = { statement TERM } ;
TERM        = NEWLINE | ";" ;

statement   = decl | ctrl | command | expr ;

decl        = ("let" | "var") pattern [":" type] "=" expr
            | "fn" IDENT "(" [params] ")" ["->" type] block
            | "alias" IDENT "=" command
            | "use" mod_path | "export" decl ;
params      = param { "," param } [ "," "..." IDENT [":" type] ] ;
param       = IDENT [":" type] ["=" expr] ;

ctrl        = "return" [expr] | "break" | "continue"
            | "for" pattern "in" expr block
            | "while" expr block ;

command     = { ENVPREFIX } head { arg } { redirect } ["&"] [trailing] ;
head        = WORD | "^" WORD ;
arg         = WORD | PATHLIT | GLOBLIT | STRING | flag | "(" expr ")" ;
flag        = "--" IDENT ["=" (WORD|STRING|"(" expr ")")] | "-" SHORTS ;
redirect    = (">" | ">>" | "<") (WORD | PATHLIT | STRING | "(" expr ")") ;
trailing    = block ;                        (* thunk: () => block *)

expr        = assign ;
assign      = lvalue ("=" | "+=" | "-=" | "*=" | "/=") assign | coalesce ;
coalesce    = orx { "??" orx } ;
orx         = andx { "||" andx } ;
andx        = cmp { "&&" cmp } ;
cmp         = rng { ("=="|"!="|"<"|"<="|">"|">="|"in") rng } ;   (* non-assoc chain *)
rng         = add [ (".." | "..=") add ] ;
add         = mul { ("+"|"-") mul } ;
mul         = unary { ("*"|"/"|"%") unary } ;
unary       = ("!" | "-") unary | postfix ;
postfix     = primary { "." IDENT [call] [trailing]
                      | "?." IDENT [call]
                      | "[" expr "]"
                      | call [trailing] } ;
call        = "(" [args] ")" ;
args        = posarg { "," posarg } { "," named } | named { "," named } ;
posarg      = expr | implicit ;
named       = IDENT ":" expr ;
implicit    = "." IDENT { chain_tail }        (* arg position only *) ;

primary     = literal | IDENT | "(" expr ")" | list | rec_or_blk
            | lambda | ifx | matchx | tryx | "sh" RAWBLOCK | "spawn" block ;
lambda      = (IDENT | "(" [params] ")") "=>" (expr | block) ;
ifx         = "if" expr block { "else" "if" expr block } ["else" block] ;
matchx      = "match" expr "{" { arm TERM } "}" ;
arm         = pat { "|" pat } ["if" expr] "=>" (expr | block) ;
pat         = literal | rangepat | "_" | IDENT | type IDENT
            | "{" fieldpats "}" | "[" listpats "]" ;
tryx        = "try" block "catch" [pat] block ;
postcatch   = expr "catch" [IDENT] (expr | block) ;   (* sugar for tryx *)
list        = "[" [expr {"," expr}] "]" ;
rec_or_blk  = "{" "}"                                  (* empty record *)
            | "{" (IDENT|STRING) ":" … "}"             (* record literal *)
            | block ;
block       = "{" { statement TERM } [expr] "}" ;      (* value = trailing expr *)
```

### 3.3 Operator precedence (tight → loose)

`.` `?.` `[]` `()` → unary `!` `-` → `* / %` → `+ -` → `.. ..=` → `== != < <= > >= in` → `&&` → `||` → `??` → `catch` (postfix) → `=`. Comparison operators do not chain (`a < b < c` is a parse error with a fix-it). `&&`/`||` short-circuit; operands are `bool` or command outcomes (success = true); anything else is a type error.

### 3.4 Desugaring table (sugar → canonical AST, all rules)

| Sugar | Canonical |
|---|---|
| `git push origin main` | `call(cmd:"git", [w"push", w"origin", w"main"])` |
| `--release` / `--jobs=4` / `-rf` | `named(release: true)` / `named(jobs: w"4")` / short-flag expansion per adapter; unresolvable flags at T0 remain positional words |
| `NAME=v cmd …` | `with(env: {NAME: "v"}) { call(…) }` |
| `cmd … &` | `spawn { call(…) }` |
| `cmd … > f` / `>> f` / `< f` | `.save(f)` / `.append(f)` on stdout / stdin-from-file capture opt |
| `f(a) { … }` | `f(a, () => { … })` |
| `.field <op> e` (arg pos) | `x => x.field <op> e` |
| `.method(args)` (arg pos) | `x => x.method(args)` |
| leading-`.` line | continuation of previous expression's postfix chain |
| `IDENT.…` where IDENT resolves to a command (EXPR pos) | `IDENT().…` (zero-arg invoke-then-chain; lint if shadow-adjacent) |
| `alias gs = git status` | binds `gs` → partial call node; later args append |
| `e catch h` | `try { e } catch { h }` (binding form binds the error) |
| `x?.f` | `if x == null { null } else { x.f }` |

---

## 4. Semantics

### 4.1 Types

`null bool int(i64) float(f64) str path glob regex size(u64 bytes) duration(i64 ns) datetime time bytes list<T> record table stream<T> error outcome task plan cmd secret`.

**`path` is bytes-backed** (OsString) — `path → str` is fallible (`.str()` errors on invalid UTF-8; `.display()` is lossy-with-replacement). **`secret`** is opaque: renders as `secret(NAME)`, cannot be interpolated into a `str` (type error), injected by the kernel at spawn time, only its *name* reaches the journal. **`outcome`**: `{status: int, ok: bool, out, err, dur, pid, cmd}` — `.out` structurally parsed lazily. **`table`** is a `list<record>` semantically.

Equality is structural for data types, identity for `task`/`stream`; comparing streams is an error.

### 4.2 Coercion — the whole matrix

Exactly two coercion sites:

1. **Arithmetic promotion**: `int` → `float` in mixed arithmetic; `size±size`, `duration±duration`, `datetime±duration`, `datetime-datetime → duration`, `size*int`, `size/int`, `size/size → float`. Everything else: type error.
2. **Word binding** (CMD-mode words → declared parameter types, at call bind): to `str` (identity), `path`, `glob`, `int/float/size/duration/time` (parse; failure = arg error), `bool` (flag presence), `datetime`, `list<T>` (repeated flags/positionals accumulate). Unknown-signature (T0) targets: all words pass as `str`, verbatim, always.

### 4.3 Globs

A `glob` value carries its pattern and origin cwd. **Expansion happens at the callee**: a parameter typed `glob` receives the compiled pattern, `list<path>` receives matches (lazy, sorted, origin cwd), `path` receives a type error. T0 externals receive the *expanded matches* as separate argv entries — zero matches yields zero argv entries plus the statement lint. `**` recurses; dotfiles excluded unless pattern starts with `.`. `glob("…", hidden: true, follow: false)` is the explicit constructor.

### 4.4 Calls, flags, named arguments

Positional args bind in order; `name: value` binds by name; flags are named-arg sugar resolved through the callee's signature: `--name` → `name: true`, `--name v` / `--name=v` → `name: v` coerced, `-abc` explodes per adapter short-flag table. Arity/type errors carry the exact source span. `...rest` captures variadic tails. Every `fn` auto-synthesizes `--help` and completion.

### 4.5 Evaluation and execution model

Strict left-to-right; blocks yield trailing expression; `let` immutable (shadowing-with-lint), `var` mutable. **External calls spawn at evaluation of the call node**.

Position semantics: statement position in interactive session = PTY passthrough + CAS tee, awaited, `Ctrl-C` cancels task tree, binds `it`; non-`ok` (per adapter `ok_codes`, default `{0}`) **raises**. Value position = captured; status realized when demanded, raise carries originating span. `try`/`catch` intercept; `outcome` in `&&`/`||`/`if` position = success-as-bool, no raise.

Streams: pull-based, bounded buffers, backpressure; cancellation propagates down-chain; sinks are render, `for`, `.each`, `.collect`, `.save`, `.feed`. Single-consumption.

### 4.6 Scope, cwd, env

Lexical scoping; modules are files (`use ./lib/deploy` binds `export`s under `deploy.`). The **session** owns `cwd` and `env`: `cd` and `env.NAME = v` legal at session top level, journaled, **illegal inside `fn` bodies** (error names `with cwd:`/`with env:` as the fix). `with cwd: p, env: {…} { }` scopes both dynamically, restoring on any exit path.

### 4.7 Concurrency, jobs, signals

`spawn { }` → `task` (structured: children cancelled with it). `task.await() / .cancel() / .suspend() / .resume()`; `parallel(a, b, …)` and `.parallel(f, max: n)` gather with fail-fast default (`settle: true` collects errors). `jobs` is the task table. `Ctrl-C` → cancel foreground task tree (SIGINT to process group; escalate TERM, KILL). `Ctrl-Z` → suspend, return to prompt; `fg <task>` re-fronts with PTY. Kernel is parent of children — client detach never kills jobs. Background task output buffered per-task, rendered as discrete blocks.

---

## 5. Builtin surface (namespaces, v1)

Core verbs: `ls cd pwd cp mv rm mkdir touch ln cat open save stat which env echo sleep kill ps du df tail head watch jump(j) pick interact explain run sh spawn parallel retry with sudo`. Namespaces: `str.* path.* list.* table.* stream.* math.* json/yaml/toml/csv.* http.*`, `os.*`, `journal`, `jobs`, `history`, `config`, `secret`, `re`. Value methods: `.where .sort .first .last .map .each .group .sum .len .uniq .join .lines .words .matches .replace .save .pick .tee .collect …`. Builtins obey the same signature/flag/coercion machinery as user `fn`s.

---

## 6. Adapter schema (T2)

Declarative TOML. Resolution order for a head: session `fn`/`alias` → adapter → PATH (T1 sniffing) → not-found.

```toml
# adapters/git.toml
[cmd.git]
bin = "git"
class = "cli"                    # cli | tui | daemon
ok_codes = [0]

[cmd.git.sub.status]
flags  = { short = { s = "short" } }
params = { short = "bool", branch = "bool" }
output = { parse = "porcelain-v2", type = "table<{status: str, path: path, orig: path?}>" }
effects = ["fs.read(cwd)"]

[cmd.git.sub.push]
params  = { remote = "str?", refspec = "str?", force = "bool", dry_run = "bool" }
effects = ["net.connect(remote)", "fs.read(cwd)"]

[cmd.git.sub.log]
params  = { path = "path?", follow = "bool", n = "int?" }
invoke  = ["log", "--pretty=format:%H%x00%an%x00%aI%x00%s", "-z"]
output  = { parse = "z-records", type = "table<{hash: str, author: str, date: datetime, subject: str}>" }
effects = ["fs.read(cwd)"]
```

Fields: `bin` (+ optional pinned `hash`), `class`, `ok_codes`, per-subcommand `params` (typed), `flags.short`, `invoke` argv template, `output.parse` (`json`, `ndjson`, `csv`, `tsv`, `z-records`, `porcelain-v2`, `lines`, `kv`, `none`), `output.type` (validated, mismatch degrades to bytes + warning), `effects` (parametric: `fs.delete($paths)`), `completions`.

---

## 7. Wire protocol — the agent contract

Transport: JSON-RPC 2.0 over Unix socket, newline-delimited. Auth: `SO_PEERCRED` same-uid; agent principals use bearer tokens minted by `shoal token create --caps <leash-profile>`. MCP facade (`shoal mcp`) exposes `shoal_exec`, `shoal_plan`, `shoal_apply`, `shoal_get`, `shoal_journal`.

| method | params | result |
|---|---|---|
| `session.attach` | `{session?, token?, client: {kind, tty: bool}}` | `{session, principal, caps, cwd, env_hash}` |
| `parse` | `{src}` | `{ast}` or diagnostic |
| `exec` | `{src \| ast, mode: "run"\|"plan", position, capture, timeout?}` | `{ref, value?\|plan?, render?}` |
| `plan.apply` | `{plan_ref}` | as `exec` run |
| `value.get` | `{ref, path?, slice?}` | re-query any transcript value |
| `task.list/await/cancel/suspend` | `{task?}` | task records |
| `journal.query` | `{since?, until?, principal?, effects?, head?, limit}` | entry stream |
| `complete` | `{src, cursor}` | typed completion items |
| `explain` | `{src \| ast}` | structured explanation |
| `cap.request` | `{effects}` | grant / denial / `approval_pending` |

Value encoding: JSON with `$`-tags — `{"$":"size","v":1500000}`, tables columnar, streams as ref + chunks, errors `{"$":"error","code","msg","span","stderr?","hint?"}`. Refs stable: `out:12`, `val:blake3:…`, `task:7`, `plan:8f2c`.

Canonical AST node kinds: `prog let var fn param alias use call word flag lit var_ref field method index lambda list record block if match arm for while range binop unop try catch with spawn lang_block` — versioned by `ast_version` at attach.

---

## 8. Effects and leash (enforcement)

**Effect instances**: `fs.read{paths} fs.write{paths} fs.delete{paths} proc.spawn{bin_hash, argv0} net.connect{host, port} net.listen{port} env.read{names} env.write{names} secret.use{names} session.write journal.read time`. Builtins declare exactly; adapters declare parametrically; T0/`sh{}` declare **opaque** (⊤).

**Plan derivation**: `mode: plan` evaluates the call's *pure prefix* (arg coercion, glob expansion, adapter binding) and stops before spawn, emitting a plan: concrete effect instances, reversibility verdict, size/count estimates, `plan_ref`.

**leash policy** (`~/.config/shoal/leash.toml`, per-principal): fs.read/write/delete path globs, net host:port allowlist, spawn binary allowlist (pinned content hashes), secrets, `auto_apply = "reversible" | "in-grant" | "never"`, `opaque = "deny" | "ask" | "allow"`.

**Enforcement tiers**: A (Linux ≥5.13): Landlock + seccomp + netns proxy; BPF-LSM content-hash spawn pinning. B (older Linux): namespaces + bind-mounts, inode checks. C (macOS): Seatbelt profiles. D: advisory — honesty surfaced at attach (`caps.enforced: false`).

---

## 9. Journal and CAS

**Store**: SQLite (WAL) at `~/.local/share/shoal/journal.db`; the `journal` value is a lazy table view over it.

```sql
entry(id INTEGER PK, session TEXT, principal TEXT, ts INT, dur_ns INT,
      cwd BLOB, env_hash BLOB, ast BLOB, effects TEXT, status INT, ok BOOL, opaque BOOL)
output(entry_id INT, kind TEXT, hash BLOB, len INT, meta TEXT)
undo(entry_id INT, op TEXT, inverse TEXT)
pin(hash BLOB PK)
```

**CAS**: `cas/aa/bb/<blake3>.zst`, zstd, sharded; bounded buffer, disk spill >64 MiB, truncation markers. GC: TTL + LRU, pins exempt; entry rows outlive blobs. **Undo**: fs builtins record inverses (delete → trash-CAS move; overwrite → prior-content hash); `undo out[7]` replays inverses newest-first, refuses when stale. **Redaction by construction**: secrets journaled as names only.

---

## 10. Kernel architecture

One `shoal-kernel` per user (systemd user unit), socket-activated; sessions kernel-internal (cwd, env, task tree, transcript, per-client `it`, leash context). Clients multiplex; kernel is parent of jobs; crash-safety = journal WAL + orphan adoption. Kernel-less mode links `shoal-core` in-process. TUI is thin: line editor (reedline lineage), renderer registry, picker.

---

## 11. Implementation plan

Workspace (Rust, edition 2024): `shoal-syntax` (modal lexer, recursive-descent + Pratt, error-recovering), `shoal-ast` (canonical AST, serde, desugarer, formatter), `shoal-value` (Value, Table, renderers), `shoal-eval` (tree-walk, scopes, coercion, adapter binding, effects), `shoal-exec` (spawn, PTY, tee, signals), `shoal-kernel` (sessions, journal, CAS, socket), `shoal-leash`, `shoal-proto`, `shoal-tui`, `shoal-adapters`, `shoal-lsp`/`shoal-wasm` post-M2.

Tree-walk interpreter forever-until-proven-otherwise; hand-rolled lexer; M0 exit: live in it for one real workday; `vim`, `git rebase -i`, `htop`, full `cargo` cycle, background server all behave. M1: kernel + protocol + plan/journal + Linux leash tier A; exit = agent harness completes nontrivial coding task through `shoal mcp` with fewer tool-errors and tokens than bash baseline.

---

## 12. Testing strategy — the spec *is* a corpus

- **Conformance corpus** (`spec/cases/*.toml`): each case = `{src, canonical_ast?, value|render|diagnostic}`. The corpus **is the normative spec**. Target ≥1,000 by M1.
- **Properties** (proptest): `parse(format(ast)) == ast`; `format` idempotent; glob expansion order-stable; codec roundtrip incl. non-UTF-8 paths.
- **Fuzzing**: lexer/parser never panic on arbitrary bytes.
- **Diagnostic snapshots**: error messages are product surface.
- **PTY end-to-end** (expectrl): Ctrl-C tree-kill, Ctrl-Z/fg, color passthrough in stmt position and NOT in value position, sudo prompt, tui handoff, background blocks.
- **leash matrix**: enforcement + honesty signals.
- **Benchmarks**: cold start <15 ms, keystroke reparse <1 ms p99 @10 kB, spawn overhead within 5% of execve, 1M-row where+sort <150 ms, journal query 100k <50 ms.
- **Adapter verification harness**: fixtures vs. real binaries, promised type diffed against parsed reality.

---

## 13. Edge-case register (normative rulings)

1. **Non-UTF-8 filenames**: bytes-backed `path` end-to-end; render U+FFFD + marker; `.str()` fails loudly.
2. **Filenames with newlines/leading `-`**: values, not text; fs builtins `--`-guard argv; `ls` output containing `-rf` cannot become a flag.
3. **`-` as stdin convention**: bare `-` passes through verbatim.
4. **Empty glob**: empty list + lint.
5. **E2BIG**: adapters may declare `argfile` (`@file` spill); else clean error suggesting `.chunks(n).each(…)`.
6. **Exit vs. signal death**: `.status` is the code; signal deaths surface as `signal: "SIGSEGV"`, `ok = false` — never 128+n.
7. **SIGPIPE analog**: downstream cancellation closes pipe; producer's early exit not an error when caused by intentional cancel.
8. **`sudo`**: stmt-position PTY just works; value-position errors with "needs a terminal — wrap in `interact`".
9. **Command-not-found**: unified did-you-mean across fns/aliases/adapters/PATH + adapter-catalog suggestion.
10. **CRLF**: `.lines()` strips `\r?\n`; `bytes` never translated.
11. **Unicode width**: `unicode-width`, ambiguous-width config; CJK columns don't shear.
12. **Recursion/loop limits**: depth 10k; interactive watchdog (5 s CPU soft interrupt); off in script mode.
13. **`sh { }` brace balance**: scans balanced `{}` outside quotes; unbalanced payloads use `sh''' … '''`.
14. **Statement `{` ambiguity**: after command args, `{` is always a trailing block; literal-brace argument must be quoted.
15. **Shadowing a command**: legal, linted, `^ls` always available.
16. **`it`/`out` in scripts**: parse error ("REPL-only — bind a variable").

---

## 14. Open for v0.3

Typed-effect inference; UFCS for user `fn`s; reactive values; Arrow/polars; Windows ConPTY; language editions; multi-human sessions.

---

*shoal TDD v0.1 — the corpus decides disputes. End.*
