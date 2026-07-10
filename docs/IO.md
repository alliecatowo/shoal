# Input, interpreters, and running things: the anti-heredoc contract

**Status:** interpreter blocks + `.feed`: spec complete, implementation pending.

**Normative. The corpus/frame decides disputes.** Companion to `docs/TDD.md` (esp. §1.2, §1.3,
§1.9, §4.5, §13.13, §13.14), `docs/VISION.md` §1/§4, `docs/AGENT-SURFACE.md`, `docs/REEF.md` §5;
supersedes them where they conflict on stdin/interpreter/run surface. Everything here is a
decision; the corpus (`spec/cases/*.toml`) decides disputes.

## 0. Thesis

Three Unix positions are poison (VISION §1) and none of them exist in shoal, ever:

1. **Heredocs** (`<< EOF … EOF`, `<<- EOF`, `<<< "text"`) — a delimiter dance that smuggles a byte
   payload into a child's stdin through quoting hell. Not lexed, not desugared, not supported in
   any form. There is no `<<` token.
2. **`-c "…"` string-smuggling** (`python -c "..."`, `sh -c "..."`) — a program-as-a-string
   argument, unescaped by hand, invisible to the parser. Replaced by **interpreter blocks** (§2).
3. **`2>`, `&>`, and fd-number plumbing** — merging/redirecting streams by numeric convention.
   Replaced by structured `.stderr` on the outcome (TDD §4.1); `2>&1`-style merging is a type
   error with a teaching diagnostic (§5).

shoal replaces all three with two mechanisms: **values as stdin** (§1) and **interpreter blocks**
(§2). Getting output from running a file is its own surface (§3) because "just run this and show
me its output" must never require redirection ceremony.

---

## 1. Values as stdin

### 1.1 `.feed` — the value method

```ebnf
feed_call = value "." "feed" "(" command_expr ")" ;
command_expr = call_expr ;              (* a command/call node — NOT a string *)
```

`value.feed(cmd args…)` serializes `value` to bytes (§1.2), spawns `cmd args…` with those bytes as
its complete stdin (`StdinSpec::Bytes`, per the exec contract), and returns the resulting
**outcome** (TDD §4.1: `{status, ok, out, err, dur, pid, cmd}`). `.feed` is a normal postfix method
call — it composes with the rest of the dot-chain like any other value method (`.feed(cmd).out`,
`table.feed(jq{'.[] | .name'}).out.lines()`).

The equivalent inverted form exists for callers who start from the command: `cmd.feed(value)` is
exactly `value.feed(cmd)` — same spawn, same serialization, same result. Pick whichever reads
better at the call site; both desugar to the same AST node (`Expr::Feed{value, cmd}`, `cmd` and
`value` swapped for the inverted spelling). There is no third spelling.

`< file` is sugar (TDD §1.3) for feeding a **file's bytes** as stdin: `cmd args < file` desugars to
`path(file).bytes().feed(cmd args)` conceptually, but is implemented directly as
`StdinSpec::File(path)` at the exec layer (no intermediate read) — behaviorally identical, zero
extra I/O. `< file` is stdin-only sugar; it never means "read this file as an argument" and never
composes with `>`/`>>` chains beyond the single stdin slot per TDD's `redirect` grammar rule
(`command = { ENVPREFIX } head { arg } { redirect } …` — at most the redirects the grammar allows,
no repetition of `<`).

### 1.2 Serialization — exactly how each `Value` becomes stdin bytes

`.feed`'s first argument must be **feedable**. The mapping is exhaustive; anything not listed is
not feedable:

| Value type | Bytes fed |
|---|---|
| `str` | UTF-8 bytes of the string, verbatim, no trailing newline added |
| `bytes` | the raw bytes, verbatim |
| `path` | **not directly feedable** — feed `path.read()` (→ `bytes`) or `path.read_str()` (→ `str`); a bare `path` is a name, not content |
| `list<str>` | each element's UTF-8 bytes, joined with `\n`, **plus one trailing `\n`** (so `["a","b"].feed(cmd)` produces `a\nb\n`, matching what a human piping a here-list into a line-oriented tool expects) |
| `list<T>` (T not `str`) | JSON array via the same encoding as `.json()` (`$`-tagged elements per AGENT-SURFACE §2, at rest — i.e. `list.json().feed(cmd)`'s wire form, not the wire's elided form), UTF-8, no trailing newline |
| `record` | JSON object, `.json()` encoding, UTF-8, no trailing newline |
| `table` | JSON — `[{col: val, …}, …]` row-major (NOT the columnar wire encoding of AGENT-SURFACE §2, which is a transport optimization; stdin gets the row-major shape a downstream JSON consumer expects), UTF-8, no trailing newline |
| `int` / `float` / `bool` / `size` / `duration` / `datetime` / `time` | decimal text via the same rule as `render_inline` (shoal-value/render.rs), UTF-8, no trailing newline |
| `outcome` | the outcome's `.out` bytes if `.out` is structured-and-JSON-shaped (re-encoded JSON), else its raw stdout bytes — i.e. `outcome.feed(cmd)` ≡ `outcome.out.feed(cmd)` when `.out` exists, else `outcome.stdout_bytes.feed(cmd)` |
| `stream<T>` | consumes the stream (single-consumption, TDD §1.9) and feeds a *continuous* byte stream to the child's stdin as items arrive: `str`/`bytes` items per the rules above, each followed by the `list<str>` join rule (`\n`-joined) applied incrementally |
| `null`, `bool` alone as a bare feed target only through the table above (bool has a rule) | — |
| `secret` | **never feedable** — always `type_error` with hint "secrets are injected at spawn time, not fed as data" (secrets exist only as kernel-injected env/args, TDD §4.1) |
| `task`, `closure`, `error`, `glob`, `regex` | **not feedable** |

Feeding a value not covered above (or explicitly marked not-feedable) is `type_error`, code
`feed_error` if the value's type is one of the enumerated "never feedable" cases and the caller is
clearly trying to feed content (secret, task, closure) — see the error table (§6) for the precise
split between generic `type_error` and the dedicated `feed_error`.

**Rule of thumb, stated exactly**: if a type has an unambiguous canonical textual or JSON
representation used elsewhere in the language (`.json()`, `render_inline`), `.feed` uses that exact
representation. `.feed` never invents a new serialization.

### 1.3 Worked examples — replacing heredoc uses

```shoal
# box era: cat <<EOF | psql mydb
#   SELECT 1;
# EOF
"SELECT 1;".feed(psql "mydb")

# box era: python3 -c "$(cat build_report.py)" < data.json
data.json.read_str().feed(python { ... })     # see §2 for the block form instead

# box era: docker run -i myimage <<< "$CONFIG_JSON"
config.feed(docker "run" "-i" "myimage")       # config: record — JSON body over stdin

# box era: some_tool < input.txt > output.txt
< input.txt some_tool > output.txt             # `<` desugars to stdin-from-file; `>` to .save()
# canonical:
path("input.txt").read().feed(some_tool).save("output.txt")
```

---

## 2. Interpreter blocks

### 2.1 What generalizes

TDD §13.13 fixes one raw block, `sh { … }`, parsed by a hardcoded keyword. IO.md generalizes this
to **any interpreter-class tool**: `python { … }`, `node { … }`, `jq { … }`, `ruby { … }`, and any
future adapter that opts in. The mechanism, lexing rule, and AST shape are identical to `sh { }`;
only the *tool* varies, and which tools qualify is **declarative**, not hardcoded in the parser.

### 2.2 Declaring interpreter class

An adapter (TDD §6) or reef runner entry (REEF §5) opts a tool into raw-block parsing by declaring:

```toml
[cmd.python]
bin   = "python3"
class = "interpreter"        # new adapter class value, alongside cli | tui | daemon
```

`class = "interpreter"` is sufficient on its own — no separate `block` field is needed for the
common case. An adapter that wants a *different* trailing-block behavior than "raw payload handed
to the interpreter as its program" (there is no such case in v1) would instead declare
`block = "raw"` explicitly; `class = "interpreter"` implies `block = "raw"` by default. Tools
resolved purely through reef `[runners]` (no adapter TOML at all — e.g. a bare `ruby` with no
adapter installed) are **not** interpreter-class by this mechanism; interpreter-class is an
adapter-catalog property, looked up the same way adapters are looked up for any other head (TDD §6
resolution order: session `fn`/`alias` → adapter → PATH).

The shipped adapter pack (CONTRACTS §6) gains `class = "interpreter"` entries for `sh`, `python`,
`node`, `ruby`, `jq`. `sh` becomes a regular entry in this table — no more special-cased keyword.

### 2.3 Lexing rule

At **command-head position** (TDD §3.1: the identifier resolves through the head-resolution chain
and the parser is deciding what kind of statement/expression this is), if:

1. the head identifier resolves to an adapter whose `class` is `"interpreter"` (checked at parse
   time against the loaded `AdapterCatalog`, exactly as flag/param binding already consults it),
   **and**
2. the very next token, with no intervening whitespace-insensitive-but-token-adjacent argument, is
   `{` (the open brace) — i.e. the same "is this a trailing block or a literal-brace argument"
   decision TDD §13.14 already makes for every command head —

then the lexer performs a **balanced-brace raw scan**: starting just past the opening `{`, it
consumes bytes verbatim, tracking `{`/`}` depth, **skipping brace characters that occur inside `"…"`
or `'…'` string spans within the payload** (so a payload containing `printf("{}\n")` or a Python
f-string with braces doesn't false-terminate), until depth returns to zero. The bytes between the
outer braces (exclusive) are the raw source. This is byte-for-byte the same scanning routine `sh {
}` already uses (`Lexer::raw_brace_block`); it is not reimplemented per tool, it is **parameterized
by tool name**.

If condition 1 fails (the head is not interpreter-class) — for example `deploy { … }` where
`deploy` is a plain `fn` — the trailing `{` is a **thunk** per the existing TDD §13.14 rule
(`f(a) { … }` desugars to `f(a, () => { … })`) and lexes in normal `EXPR` mode, not raw mode. The
disambiguation is entirely upstream: adapter-class lookup happens before the brace is consumed, so
there is no ambiguity at parse time — the parser knows which mode to switch the lexer into before
reading a single byte of the block body.

### 2.4 The triple-raw form

For payloads whose brace-balance genuinely cannot be scanned this way (a script containing an
unbalanced or quote-adjacent `{`/`}` sequence that would defeat the scanner — e.g. shell code that
emits a lone `}` inside a heredoc-of-its-own or a string with an escaped quote adjacent to a brace),
the triple-raw form bypasses brace scanning entirely:

```
tool ''' … arbitrary bytes, no scanning at all … '''
```

Lexically this is: the head resolves as interpreter-class, immediately followed by `'''` (no
whitespace required, but permitted). The lexer then scans for the **literal three-character
sequence `'''`** with no nesting/depth logic — first occurrence terminates, full stop, exactly like
the raw-string triple-quote rule in TDD §2.1 (`'''…'''` multiline raw strings) but at command-head
position instead of expression position. Use this whenever the brace scanner would be wrong;
prefer the brace form (§2.3) otherwise since it reads better and matches `sh { }` muscle memory.

```shoal
sh ''' if [ "$X" = '{' ]; then echo done; fi '''
```

### 2.5 The AST node

TDD's `Expr::ShRaw { src, span }` (shoal-ast §, canonical AST kind `sh_raw`) is **renamed and
generalized** to:

```rust
Expr::LangBlock {
    tool: String,   // adapter/head name, e.g. "sh", "python", "jq" — resolved at eval time
    src: String,    // raw payload bytes, UTF-8 (invalid UTF-8 in the payload is a lex error —
                     // interpreter payloads are source text, not arbitrary bytes)
    span: Span,
}
```

Canonical AST kind renames `sh_raw` → `lang_block` (versioned by `ast_version` per AGENT-SURFACE
§5 `session.attach`; the wire protocol's AST node-kind enum, TDD §7, gains `lang_block` and retires
`sh_raw` — a breaking rename gated the same way any AST vocabulary change is). `sh { … }` becomes
sugar that constructs `Expr::LangBlock{tool: "sh", src, span}` — no separate AST node survives for
it. All prior `sh { }`-specific behavior (brace balance, `sh'''…'''`, dispatch as an expression that
spawns) is preserved verbatim under the general node; nothing about `sh`'s own semantics changes.

### 2.6 Desugar

```
tool { BODY }              →  Expr::LangBlock{ tool: "tool", src: "BODY", span }
tool ''' BODY '''          →  Expr::LangBlock{ tool: "tool", src: "BODY", span }
```

Evaluation of `Expr::LangBlock{tool, src, span}`:

1. Resolve `tool` through the normal head-resolution chain (TDD §3.1.3 / REEF §1: session
   `fn`/`alias` → adapter bin pin → project reef → user reef → system reef → ambient). This is the
   **same resolution a bare command head would get** — an interpreter-class tool is still a
   command, just one whose invocation carries a payload instead of/alongside argv words.
2. Build argv as: `[resolved_binary_path, ...adapter-declared invoke template args (if any),
   ...trailing CMD-mode args on the same statement, if the grammar permits args before the block —
   e.g. `python -O { … }` puts `-O` before the block and it flows through normal flag/arg binding]`.
   The raw `src` is **not** appended as an argv word by default — see step 3.
3. **How the payload reaches the tool** is adapter-declared, defaulting to "as a single argument
   with the interpreter's own `-c`-equivalent flag, resolved by adapter metadata, never by the
   shell hardcoding `-c`": each interpreter-class adapter declares `invoke_payload = "arg" |
   "stdin"` (default `"arg"`) and, if `"arg"`, the flag template (e.g. python's adapter declares
   `invoke = ["-c"]` so the effective argv is `[python3, -c, BODY]`; sh's declares `invoke = ["-c"]`
   too — same as today's hardcoded behavior, now data instead of code). This is the *only* place a
   `-c`-shaped flag exists in shoal, and it is never spelled by the user — it is an implementation
   detail of the adapter, invisible at the call site, which is precisely why interpreter blocks are
   not "string-smuggling": the user never types a quoted string containing a program; they type an
   unquoted raw block that the lexer, not the user, hands to the tool.
4. **`.feed` composes with a block** exactly as it composes with any other command call: `value.feed(python { ... })` and `python { ... }.feed(value)` are both legal — the block IS a `call_expr`-shaped node for grammar purposes (it sits in head position with the block as its distinguishing trailing syntax, but the resulting `Expr::LangBlock` is a first-class expression, feedable into and feedable-from like any command call). When both a payload-as-arg (step 3) and `.feed`-supplied stdin are present, they are independent channels: the block is still the program argument; `.feed`'s bytes still go to the child's stdin. A `python { import sys; print(sys.stdin.read()) }.feed("hello")` prints `hello` — the block is the program, the fed value is that program's stdin, exactly matching what `python -c '...'` with piped stdin does today, just without the quoting.
5. **Output** is an `outcome` (TDD §4.1), identical to any other command call's result, governed by the same statement/value position rule (TDD §1.2, §3): at statement position in an interactive session it's PTY-teed and bound to `it`; at value position it's captured. **The payoff**: if the tool's adapter declares `output.parse = "json"` (or the block explicitly requests structured capture — see the worked example below), `.out` is the parsed JSON value, not text — `python { print(json.dumps(x)) }.out` is a `record`/`table`/etc., navigable by field path, with zero text-scraping. Without a declared parser, `.out` falls back to the outcome's default lazy-structural-parse-attempt (TDD §1.2) same as any other adapter-less command.

### 2.7 Worked examples — replacing heredoc/`-c` uses

```shoal
# box era: python3 -c "$(cat <<'EOF'
# import json, sys
# print(json.dumps({"n": 2 + 2}))
# EOF
# )"
python { import json; print(json.dumps({"n": 2 + 2})) }.out
# → {n: 4}   (a record, field-addressable: (...).out.n)

# box era: node -e "console.log(JSON.stringify(require('./pkg.json').version))"
node { console.log(JSON.stringify(require('./pkg.json').version)) }.out

# box era: jq '.items[] | select(.active)' <<< "$JSON_BLOB"
blob.feed(jq { .items[] | select(.active) }).out

# box era: psql mydb <<'SQL'
#   SELECT count(*) FROM users;
# SQL
sh { psql mydb <<'inner heredocs remain box-era and are STILL illegal — rewrite as: }
"SELECT count(*) FROM users;".feed(psql "mydb")

# box era: ruby -e 'puts (1..5).map { |i| i * 2 }' — braces inside the payload are fine,
# the scanner tracks depth and this Ruby block's { } balances on its own:
ruby { puts (1..5).map { |i| i * 2 } }.out
```

---

## 3. Running files ergonomically

### 3.1 The surface

```ebnf
run_call = "run" [ "--capture" | "--quiet" ]* path { arg } ;
```

`run <path> [args…]` and, at command-head position, a **bare path literal** (`./deploy.shl args…`,
`script.py args…` — anything that lexes as a `path` literal per TDD §2.2, whether or not it starts
with an interpreter shebang or has a known extension) both resolve a **runner** and execute the
file with `args…` passed through as the child's argv tail. There is no `./file.shl` "ceremony"
distinct from `run file.shl` — they are the same operation through two head-resolution entry
points:

- `run <path> args…` — the explicit, always-available spelling (`run` is a builtin, TDD §5).
- `<path> args…` at command-head position — a bare path-literal head (TDD §2.2: a word beginning
  `~/`, `./`, `../`, or `/` lexes as a `path` literal) is, at head position specifically, resolved
  as "run this path" rather than "this is a positional path argument to something else" (there is
  nothing else it could be — a path literal cannot itself be a command name to look up in
  fn/alias/adapter/PATH, since those are name-keyed, not path-keyed). This is not a special case in
  the grammar; it's `command = head {arg}...` where `head` is allowed to be a path literal, and path
  literals dispatch to the runner resolver instead of the name resolver.
- A **plain filename with no path separator** (`deploy.shl` with no `./`) that fails name resolution
  (not a session `fn`/alias, not an adapter, not found via reef/PATH-as-output) but **does** exist
  as a file in cwd, and has a resolvable runner (§3.2), also resolves through the runner — this is
  the "just typing the filename" ergonomics case. If a name collides (both a PATH-resolved binary
  named `deploy.shl` and a cwd file `deploy.shl` — vanishingly rare since extensions rarely appear
  in binary names) name resolution wins per the existing TDD §3.1.3 order; `run ./deploy.shl` or
  `^./deploy.shl` disambiguates explicitly.

### 3.2 Runner resolution

Exactly REEF §5's algorithm, restated precisely for this surface:

1. Extension lookup: the path's extension (e.g. `.py`, `.js`, `.shl`) is looked up in the
   nearest-scope `[runners]` table (REEF §1 scope order: project `.reef.toml` → user
   `~/.config/shoal/shoal.toml [reef]` → shipped defaults `py js ts sh shl rb lua`). A `[runners]`
   entry is either a bare tool name (`py = "python"`) or `{ tool = "...", args = [...] }` (an argv
   template prefix before the file path, e.g. `ts = { tool = "deno", args = ["run"] }`).
2. **No extension, or extension not in any `[runners]` table**: shebang fallback — read the file's
   first line; if it matches `#!<path-or-word> [args...]`, the interpreter word (basename of the
   shebang target) is looked up as a **tool name** through the normal reef tool-resolution chain
   (REEF §1), *not* executed as a literal path (so `#!/usr/bin/env shoal` resolves shoal through
   reef, honoring any local pin, rather than trusting whatever `/usr/bin/env` finds) — except when
   the shebang path itself is an absolute path to an executable with no interpreter-name semantics
   (e.g. a compiled binary self-shebanging, rare); in that case the absolute path is exec'd
   directly, unresolved, exactly as the kernel of the file demands.
3. Neither an extension match nor a shebang: `runner_not_found` (§6), with a hint listing the
   configured `[runners]` extensions and suggesting `reef add`-style config or an explicit `#!`.
4. `.shl` is special: its runner is the literal string `"self"` (REEF §1's example manifest) —
   meaning "run in a **child Evaluator** of the current `shoal` process, not a subprocess." No
   binary is spawned; no reef tool resolution happens for the interpreter itself (there is no
   external `shoal` binary to resolve — `run x.shl` inside a running `shoal` uses the in-process
   `shoal-eval`/`shoal-core` embedding, TDD §1.1). Args are bound the same way a script's top-level
   would see `env.args` (or an equivalent script-args builtin); the child Evaluator gets a fresh
   lexical scope, its own `cwd`/`env` snapshot (inherits the parent's at spawn, mutations inside
   don't leak back — TDD §4.6 discipline applies to the child evaluator's session state exactly as
   it does to `with`), and reports errors with spans relative to the child file, chained to the
   `run` call's span in the parent's diagnostic if the child raises uncaught.
5. Every non-`.shl` runner resolves its `tool` through reef (REEF §1 full chain) exactly like any
   command head — locked, hashed, journaled identically. Running a `.py` file is: resolve `python`
   via reef → spawn `[resolved_python, ...runner args template, file_path, ...args…]`.

### 3.3 Output handling — the flags, exactly

Default behavior answers the brief directly: **`run script.shl` (or bare `./script.shl`) just
shows its output, live, with no ceremony.**

- **Statement position** (TDD §1.2, unchanged rule applied to `run`/bare-path calls): PTY
  passthrough — colors, progress bars, interactive prompts inside the script all work exactly as
  running the equivalent command directly would; bytes are simultaneously teed into the CAS so
  `it` still binds to a queryable outcome afterward. This is the "just see its output" default —
  no flag is needed to get it; it is what happens when you type `./script.shl` or `run script.shl`
  and press enter.
- **Value position** (assigned, chained, passed as an argument): captured per TDD §1.2, `it` not
  bound (nothing to bind — value position never uses `it`), an `outcome` value returned directly
  as the expression's result — this is already "capture" without needing a flag, because value
  position *means* capture.
- **`--capture`** (only meaningful at statement position, where the default is live-PTY): forces
  value-position-style capture even though the call is syntactically at statement position — no
  live PTY, no teletype passthrough; the script's output is fully captured and an `outcome` is
  bound to `it` (statement position still binds `it`; `--capture` changes *how* the bytes were
  obtained, not whether `it` gets set). Equivalent to writing `run script.shl args… ; it =
  <captured>` but atomic and without the live rendering side effect. Use when you want to run a
  script for its output value without watching it scroll by.
- **`--quiet`**: suppresses the live render entirely (no PTY passthrough, no captured-and-then-
  printed transcript) while still executing normally and still teeing to CAS/binding `it` — for
  when you want the effects and the recorded outcome but not the terminal noise. Composable with
  `--capture` (quiet capture is just "run it, get me the outcome, don't show me anything," the
  closest analog to bash's `output=$(script.sh 2>&1)` — without needing `2>&1` because `.err` was
  never split off the terminal in the first place; PTY-teed bytes are already the merged stream by
  construction, TDD's `ExecMode::PtyTee` doc: "stderr merged into stdout (pty semantics)").
- No other flags exist for this in v1. There is no flag that reintroduces redirection syntax
  (`--out file` does not exist — that's `.save(path)`, §4) and no flag that splits merged
  stdout/stderr back apart for a *live* PTY run (structured `.stderr` is only available from a
  **captured** run, i.e. value position or `--capture`, because a real PTY has already merged the
  streams at the terminal-emulation layer before shoal ever sees them — this is the honest
  consequence of PTY semantics, not a shoal limitation to work around with a flag).

### 3.4 Worked examples

```shoal
./deploy.shl staging          # bare path head; live PTY output; `it` bound afterward
run deploy.shl staging        # identical result, explicit spelling
deploy.shl staging            # plain filename in cwd, no `fn`/adapter named that; same result

let result = run --capture backup.shl --full   # outcome value, no live scroll
result.ok                                       # bool
result.out                                      # structured, if backup.shl's own last statement
                                                 # value was structured and the child captured it
run --quiet migrate.shl                         # runs silently; `it` still queryable afterward
```

---

## 4. Output side — reaffirmed, and what's forbidden

- `cmd args > file` / `>> file` (TDD §1.3) remain exactly what they are: **muscle-memory sugar**,
  command-mode only, desugaring to `.save(file)` / `.append(file)` on the outcome's stdout bytes
  (TDD §3.4 desugar table). They are not a separate redirection subsystem; they are literally
  syntax sugar for two value methods that also exist and are callable directly:
  `(cmd args).save(file)` and `(cmd args).append(file)` do the identical thing with no sugar at
  all. **The modern, canonical form is the value method** — `>`/`>>` exist purely so bash muscle
  memory doesn't have to be unlearned on day one; documentation, generated code, and `alias`
  expansions should prefer `.save`/`.append`.
- `< file` (§1.1) is the sole stdin sugar and the sole legal use of `<`. It has no numeric
  variant, no here-string variant (`<<<` does not exist — feed a `str` literal instead: see the
  psql example in §1.3), and does not compose with process substitution (`<(...)` does not exist —
  feed a value or an outcome's `.out` instead).

**Forbidden, permanently, with the teaching-error rationale**:

| Spelling | Status | Diagnostic |
|---|---|---|
| `<< EOF` / `<<- EOF` (any heredoc) | not lexed — `<<` is not a token in CMD mode | parse error: *"shoal has no heredocs — feed a string or multiline literal instead: `value.feed(cmd)`, or use an interpreter block: `python { … }`"* |
| `<<< "text"` (here-string) | not lexed | parse error: *"shoal has no here-strings — `\"text\".feed(cmd)`"* |
| `2>` | not lexed — `2` immediately before `>` with no space is still just the int literal `2` followed by a `>` comparison/redirect ambiguity resolved in favor of the teaching error, because a **bare command-mode statement never has a dangling comparison operand** at that position | parse error: *"shoal has no fd-numbered redirects — stderr is `.stderr` on the outcome: `cmd.stderr.save(file)`, or `try { cmd } catch e { e.stderr }`"* |
| `&>` / `>&` (merge stdout+stderr to a file) | not lexed — `&` immediately followed by `>` is rejected before the background-`&` desugar (TDD §1.3) can apply, because background-`&` requires end-of-statement, not mid-redirect | parse error: *"shoal has no stream-merging redirect — PTY runs already merge (`run`/statement position); a captured outcome's `.out` + `.stderr` are separate values on purpose: concatenate explicitly if you truly want one blob, e.g. `(cmd.out.bytes() + cmd.stderr.bytes())`"* |
| any bare fd number (`3>&1`, `exec 4<>file`, …) | not lexed — no fd-number token class exists in the grammar at all | parse error: *"shoal has no file-descriptor plumbing — there are no numbered fds in the language; use `.feed`, `.save`, `.stderr`, or an interpreter block"* |

These are not runtime errors to be caught — they are **parse-time, curated diagnostics**, the same
enforcement class as TDD §1.4's pipe-operator error and §2.1's illegal-token list (`$`, backtick).
The lexer/parser recognizes the *shape* of the box-era spelling specifically so the error can name
the modern replacement, rather than failing generically on "unexpected token `<`".

---

## 5. Error codes

New codes, extending CONTRACTS §4's pinned table (`parse_error type_error arg_error undefined_var
not_found cmd_failed div_zero index_range field_missing utf8_error stream_consumed no_matches
custom assert_failed permission recursion_limit`):

| code | raised when |
|---|---|
| `feed_error` | `.feed`'s value argument is a type explicitly marked never-feedable in §1.2's table (`secret`, `task`, `closure`, `error`, `glob`, `regex`) — distinct from a generic `type_error` because the message is specialized per type (e.g. secrets get the "injected at spawn time" hint, §1.2) |
| `lang_block_unbalanced` | brace-scanning an interpreter block (§2.3) reaches end-of-input with nonzero depth (an unterminated `{`) — hint: *"unbalanced braces in `<tool> { … }` — use the triple-raw form: `<tool> ''' … '''`"* |
| `runner_not_found` | `run <path>`/bare-path-head resolution (§3.2) exhausts extension lookup and shebang fallback with no match — hint lists configured `[runners]` extensions |

`type_error` continues to cover: feeding a value not in §1.2's table at all and not in the
never-feedable list either (there is no such case in v1 — the table is exhaustive over `Value`'s
variants, so this row is present for forward-compatibility as new `Value` variants are added: a
new variant is `type_error` by default until §1.2 is amended to give it a serialization).
`parse_error` continues to cover every forbidden spelling in §4's table, and unbalanced triple-raw
quoting (`tool ''' … ` with no closing `'''` before EOF) — a plain unterminated-string-literal
class of error, not `lang_block_unbalanced` (that code is specifically for the *brace* scanner).

---

*shoal IO v0.1 — the corpus decides disputes. Cross-refs: VISION §1 (the box), §4 (inversions
table); TDD §1.2 (position rule), §1.3 (sugar), §2.1–2.2 (lexical), §3.4 (desugar table), §4.1
(types/outcome), §13.13–13.14 (raw block, brace ambiguity); AGENT-SURFACE §2 (value encoding used
by `.feed`'s JSON rules); REEF §5 (runners), §1 (resolution chain); CONTRACTS §4 (error codes),
§1 (`StdinSpec`).*
