+++
title = "Builtin command reference"
description = "Every canonical Shoal builtin command head, with accepted arguments, options, result shape, examples, effects, and important errors."
weight = 310
template = "docs/page.html"

[extra]
eyebrow = "Command reference"
group = "Reference"
audience = "Interactive users, script authors, and adapter authors"
status = "All 37 canonical heads checked against the builtin registry"
toc = true
+++

Shoal currently registers exactly 37 canonical builtin command heads: 14 structured builtins and 23 special evaluator heads. This chapter names every one. It documents the current implementation, including argument-validation gaps that a future release may tighten.

## How to read signatures

Signatures below use these conventions:

- `[x]` means optional;
- `x...` means zero or more values;
- `A | B` means either type;
- `-> T` names the structured result before ordinary top-level rendering;
- `outcome<T>` means a successful command outcome whose `.out` is `T`;
- an error code in backticks is catchable with `catch`.

The generic filesystem/environment builtins and Reef-aware `which`/`reef` are wrapped as outcomes. Several session-control heads return their value directly. In normal use, outcome forwarding lets both feel similar:

```shoal
(ls .).out
(ls .).where(.type == "file")   # forwards .where to .out
pwd                              # direct path value
```

Use `^name` to bypass a non-callable value shadow and an adapter. It does **not** bypass a function, alias, other callable binding, or any builtin head; those resolve before the forced flag is consulted. To invoke an external executable that shares a builtin/callable name, use dynamic `run("name", ...)`.

Every canonical head supports `-h` and `--help`. Help includes usage, typed arguments, options,
subcommands where applicable, result behavior, errors, and examples. It is dispatched only after
callable-shadow resolution and before expansion or effects, so `rm --help FILE`, `reef --help`, and
`ls --help > output &` only render help. Use `--` to treat a later `--help` spelling as an operand.

## Complete inventory

| Head | Signature summary | Primary result |
| --- | --- | --- |
| `apply` | `apply PLAN_OR_ID` | executed plan result |
| `assert` | `assert CONDITION [MESSAGE]` | `null` or `assert_failed` |
| `cat` | `cat PATH...` | `outcome<bytes>` |
| `cd` | `cd [PATH | -]` | `path` |
| `cp` | `cp [-r|--recursive] SRC... DEST` | `outcome<list<path>>` |
| `dirs` | `dirs` | `list<path>` |
| `echo` | `echo VALUE...` | `outcome<str>` |
| `env` | `env [NAME]` | `outcome<record|str|null>` |
| `exit` | `exit [STATUS]` | host exit request |
| `explain` | `explain SOURCE` | effect-plan record |
| `head` | `head PATH [COUNT]` | `outcome<list<str>>` |
| `history` | `history [filters]` | journal table |
| `interact` | `interact COMMAND...` | external outcome |
| `j` | `j [QUERY]` | `path` |
| `jobs` | `jobs` | task table |
| `journal` | `journal [filters]` | journal table |
| `jump` | `jump [QUERY]` | `path` |
| `ln` | `ln [-s|--symbolic] TARGET LINK` | `outcome<record>` |
| `ls` | `ls [-a|--all] [PATH...]` | `outcome<table>` |
| `mkdir` | `mkdir [-p|--parents] PATH...` | `outcome<list<path>>` |
| `mv` | `mv SRC... DEST` | `outcome<list<path>>` |
| `open` | `open PATH` | `null` |
| `plan` | `plan COMMAND...` or `plan { ... }` | plan record |
| `popd` | `popd` | `list<path>` |
| `pushd` | `pushd [PATH]` | `list<path>` |
| `pwd` | `pwd` | `path` |
| `quit` | `quit [STATUS]` | host exit request |
| `reef` | `reef [SUBCOMMAND]` | `outcome<table|record>` |
| `rm` | `rm [--permanent] [-r|--recursive] PATH...` | `outcome<list>` |
| `run` | `run TARGET [ARG...]` | script or command result |
| `save` | `save PATH VALUE` | original value |
| `sleep` | `sleep DURATION` | `outcome<null>` |
| `source` | `source PATH` | sourced program result |
| `stat` | `stat PATH...` | `outcome<record|table>` |
| `touch` | `touch PATH...` | `outcome<list<path>>` |
| `undo` | `undo [ENTRY_ID]` | undo report record |
| `which` | `which [-a|--all] NAME` | `outcome<record|table|null>` |

## Result and error conventions

The 14 generic structured builtins accept command words, coerce declared path/duration/string arguments, use the evaluator's filesystem/process-environment abstractions for their principal operations, and return a successful outcome with:

```text
status = 0
ok = true
pid = 0
streamed = false
cmd = builtin head
out = structured builtin result
```

Their validation or filesystem failures raise an error rather than returning a non-ok outcome. File-operation failures currently often use the broad code `custom`; do not branch only on a platform-specific message.

Structured builtins that eagerly construct strings, bytes, lists, tables, or records share a
16,384-value / 16 MiB retained-output wall. They raise `builtin_output_limit` before retaining the
next value or byte. Narrow the input or move repeated processing into a stream when a result can be
larger. In particular, production `ls` stops directory iteration at the wall, `cat` reads only the
remaining aggregate byte budget, and `head` reads line prefixes rather than loading the whole file.

## Filesystem inspection builtins

### `ls`

```text
ls [-a | --all] [PATH...]
-> outcome<table<{path, name, type, size, modified}>>
```

With no path, `ls` lists the session current directory. A directory argument contributes its immediate children; a non-directory contributes one metadata row. Rows are sorted by path.

| Column | Type | Meaning |
| --- | --- | --- |
| `path` | `path` | full path used by the builtin |
| `name` | `str` | basename |
| `type` | `str` | `dir`, `symlink`, `file`, or `other` |
| `size` | `size` | metadata byte length |
| `modified` | `datetime|null` | modification time when readable |

```shoal
ls
ls --all .
(ls src).where(.type == "file").sort_by(.size)
```

Hidden entries are skipped unless `-a` or `--all` is present. `ls` is not recursive. A missing/unreadable path raises a filesystem error. More than 16,384 admitted rows or 16 MiB of retained row state raises `builtin_output_limit`; directory enumeration is stopped before an extra production entry is retained.

### `stat`

```text
stat PATH...
-> outcome<record>       # one path
-> outcome<table>        # two or more paths
```

`stat` uses the same row schema as `ls`. It requires at least one path.

```shoal
(stat Cargo.toml).size
(stat Cargo.toml Cargo.lock).map(.modified)
```

Errors include `arg_error` for no arguments and a filesystem error for an unreadable path.

### `cat`

```text
cat PATH...
-> outcome<bytes>
```

Reads every file and concatenates the bytes without inserting separators.

```shoal
(cat README.md).out.str()
(cat a.bin b.bin).stdout.save("combined.bin")
```

It requires at least one path. `.str()` requires valid UTF-8; `.display()` is the lossy conversion. Concatenated output above 16 MiB raises `builtin_output_limit`; files are read incrementally against the shared budget.

### `head`

```text
head PATH [COUNT = 10]
-> outcome<list<str>>
```

Reads one file and returns the first `COUNT` logical lines. Non-UTF-8 bytes are decoded lossily. `COUNT` may be an integer value or a decimal string.

```shoal
head README.md
head README.md 3
(head README.md 5).join(" | ")
```

Errors include `arg_error` for no path, negative or invalid count, `type_error` for a count of the wrong type, and `builtin_output_limit` above 16,384 lines or 16 MiB of retained line state. `head` streams the requested prefixes and does not load the whole file. Extra positional arguments beyond the count are currently ignored; do not rely on that leniency.

## Filesystem mutation builtins

### `mkdir`

```text
mkdir [-p | --parents] PATH...
-> outcome<list<path>>
```

Creates each directory and returns the requested paths. `-p`/`--parents` creates missing ancestors and accepts an existing directory.

```shoal
mkdir build
mkdir --parents build/cache/assets
```

No path is `arg_error`. Filesystem permission/collision failures are raised.

### `touch`

```text
touch PATH...
-> outcome<list<path>>
```

Creates a file if absent or updates its modification time through the host filesystem port.

```shoal
touch .ready
touch a.txt b.txt
```

No path is `arg_error`.

### `cp`

```text
cp [-r | -R | --recursive] SOURCE... DESTINATION
-> outcome<list<path>>
```

With one source, destination may be a new file path or a directory. With multiple sources, destination must already be a directory. Directory copying requires a recursive flag.

```shoal
cp config.toml config.backup.toml
cp a.txt b.txt archive
cp --recursive assets public/assets
```

Errors include:

- `arg_error` when fewer than two paths are provided;
- `arg_error` when multiple sources target a non-directory;
- `arg_error` when a directory is copied without recursion;
- `arg_error` when source and destination identify the same file (including a hard-link alias), or
  when a recursive destination resolves inside its source through lexical or symlinked parents;
- a filesystem error for read/write failures.

Recursive copy inventories every source before the first filesystem mutation. The shared plan is
limited to 16,384 pending/final operations, 16 MiB of retained path state, and 64 directory levels;
`builtin_work_limit` means the tree must be split into bounded subtrees. Directory iteration checks
both entry count and aggregate encoded path bytes while reading. A preflight failure leaves every
destination untouched. This is an allocation/effect-order guarantee, not an atomicity guarantee for
an I/O failure that occurs after execution begins.

When a journaled statement overwrites a file and the complete prior bytes fit the journal limit, Shoal records a restore inverse for `undo`. A too-large prior file is left non-reversible rather than storing a truncated inverse.

### `mv`

```text
mv SOURCE... DESTINATION
-> outcome<list<path>>
```

Uses filesystem rename semantics. Multiple sources require a directory destination.

```shoal
mv old.txt new.txt
mv a.txt b.txt archive
```

Successful journaled moves may record a move-back inverse. Cross-device or platform-specific rename limitations surface as filesystem errors.

### `rm`

```text
rm [--permanent] [-r | -R | --recursive] PATH...
-> outcome<list<record<{path, trash}>>>  # default
-> outcome<list<path>>                   # --permanent
```

Default `rm` is a session-temporary trash move. It renames each target into a process-specific directory beneath the host temporary directory and returns both original and trash paths.

```shoal
rm scratch.txt
rm --recursive build
rm --permanent --recursive build
```

Safety behavior:

- no paths—including an empty glob—raises `no_matches`;
- duplicate, relative, and intermediate-symlink aliases (plus hard-link aliases on Unix) raise
  `rm_path_duplicate` before any trash directory or deletion is created;
- a directory together with any descendant raises `rm_path_overlap` in either argument order;
- permanent directory removal requires a recursive flag;
- non-permanent directory removal is implemented as a rename and does not require recursion;
- journaled trash moves can be undone while the trash target is intact;
- trash storage is temporary, not a desktop trash protocol and not durable archival storage.

`--permanent` bypasses the trash and is normally irreversible.
Identity uses the injected filesystem port's canonicalization. The final component of a symbolic
link is deliberately not followed because `rm link` removes the link, while symbolic-link aliases
in parent components are resolved. These checks eliminate deterministic input overlap; they do not
eliminate a hostile filesystem race between preflight and rename/removal.

### `ln`

```text
ln [-s | --symbolic] TARGET LINK_NAME
-> outcome<{target: path, link: path, symbolic: bool}>
```

The default creates a hard link. Symbolic mode preserves a relative target verbatim so it remains relative to the link's directory.

```shoal
ln data.db data-copy.db
ln --symbolic ../shared/config.toml config.toml
```

Exactly two positional arguments are required. The target and link must be path/string values.

### `save`

```text
save PATH VALUE
-> VALUE
```

This command-form helper delegates to `VALUE.save(PATH)`. Strings and bytes write verbatim; other values write compact JSON. The original value is returned, which makes save composable.

```shoal
save "report.json" {ok: true, count: 3}
save path("raw.bin") (cat input.bin).stdout
```

Exactly two positional values are required. Existing complete content may be recorded for undo in a journaled statement.

Command-mode flags are omitted by the generic value collector for this special head. Prefer expression-call syntax when the value could be parsed as a flag:

```shoal
save("args.json", ["--verbose"])
```

### `open`

```text
open PATH
-> null
```

Launches `xdg-open PATH` detached with null stdio and returns after spawn. The current default opener is Linux-oriented; on macOS it does not automatically substitute `open`.

```shoal
open README.md
open(path("target/doc/index.html"))
```

Exactly one path/string is required. Spawn failure raises `custom`.

## Navigation and directory stack

### `pwd`

```text
pwd -> path
```

Returns the evaluator's session current directory as a typed path.

```shoal
pwd
pwd.name
```

The current implementation ignores extra arguments; treat that as a validation gap, not supported syntax.

### `cd`

```text
cd [PATH]
cd -
-> path
```

No argument selects the process home directory, falling back to `/` if unavailable. A relative path is resolved against the session current directory and canonicalized. `cd -` uses the evaluator's previous-directory slot and returns the new path.

```shoal
cd src
cd -
cd
```

Every successful session navigation updates `OLDPWD` internally and best-effort frecency data. `cd` is rejected inside a function body because it would mutate ambient session state; use `with cwd: path { ... }` for scoped changes.

Errors include `arg_error` for a non-path or unresolvable target and `custom` when `cd -` has no previous directory. Only the first positional argument is consumed currently.

### `pushd`

```text
pushd PATH
pushd
-> list<path>
```

With a path, pushes the current directory at the front of the stack and enters the target. With no argument, swaps the current directory with the most recent stack entry. The result is the same ordering as `dirs`: current directory first.

```shoal
pushd ../service
pushd
```

An empty-stack swap raises `custom`. Like `cd`, `pushd` is session-top-level only.

### `popd`

```text
popd -> list<path>
```

Removes the most recent stack entry and changes into it. Empty stack raises `custom`. It is session-top-level only. Extra arguments are currently ignored.

### `dirs`

```text
dirs -> list<path>
```

Returns `[current_directory] + saved_stack`.

```shoal
dirs
dirs.map(.name)
```

Extra arguments are currently ignored.

### `j` and `jump`

These are two canonical names for the same frecency navigation builtin.

```text
j [QUERY]
jump [QUERY]
-> path
```

An existing directory query is used directly. Otherwise the persistent frecency store ranks recorded destinations matching the query. No query selects the highest-ranked available destination.

```shoal
j shoal
jump ./crates
```

At most one text/path query is accepted. `not_found` indicates no valid target or a vanished selected path. Like `cd`, jumping inside a function is rejected.

## Text, environment, and time

### `echo`

```text
echo VALUE...
-> outcome<str>
```

Joins values with one space. Strings and paths render without quotes; `null` contributes an empty field; other values use compact inline rendering.

```shoal
echo hello world
echo {name: "api", ready: true}
echo ([1, 2, 3]) > values.txt
```

`echo` does not append a newline to its structured string value. The external `^printf` remains available for byte-exact formatting.
An eager result above 16 MiB raises `builtin_output_limit` while the destination string is being built.

### `env`

```text
env                 -> outcome<record<str>>
env NAME            -> outcome<str|null>
```

With no argument, returns the evaluator process environment as an ordered record, omitting names/values that are not valid UTF-8. One name returns its string value or `null`.

```shoal
env
env PATH
env.PATH
```

More than one name is `arg_error`. `env.NAME` field syntax reads the session environment through the same evaluator path, and assignment writes it.
The whole-record form admits entries incrementally and raises `builtin_output_limit` above 16,384
entries or 16 MiB of retained key/value state.

### `sleep`

```text
sleep DURATION
-> outcome<null>
```

Accepts a non-negative duration literal or a non-negative integer interpreted as seconds. It polls cancellation about every 50 ms, so Ctrl-C can shorten a foreground sleep.

```shoal
sleep 250ms
sleep 2
```

Wrong arity is `arg_error`; a negative/wrong type is `type_error`.

## Process and script execution

### `which`

```text
which NAME
which [-a | --all] NAME
-> outcome<record|null>  # singular
-> outcome<table>        # all
```

Singular `which` is Reef-aware. For a constrained tool it reports the selected path, version, provider, constraint/scope, and lock-related resolution facts. If no manifest constrains the name, it falls back to ambient `PATH`. A true miss returns `null`.

```shoal
which node
which --all python
```

`--all` enumerates raw candidates from every provider without making a lock/conflict decision. Exactly one name is required. Protection states such as conflict, drift, or unlocked are represented as an unresolved report rather than silently lying with an ambient path.
Candidate tables and nested scope/adapter lists use the shared 16,384-value / 16 MiB builtin result
wall and raise `builtin_output_limit` rather than returning a partial report.

See [Reef environments](@/docs/reef.md) for the report and lock model.

### `interact`

```text
interact COMMAND [ARG...]
-> external outcome
```

Forces a real inherited PTY-style interactive invocation even in a value-oriented context.

```shoal
interact ssh example.org
interact top
```

At least one command value is required. Current command-form collection discards flag AST nodes, so `interact tool --flag` may omit the flag. Until fixed, use a direct statement invocation for flag-heavy interactive programs:

```shoal
ssh -v example.org
```

There is no `interact(...)` expression-call form today. Use `interact` only when its simple argv shape suffices.

### `run`

```text
run TARGET [ARG...]
run(TARGET, ARG...)
```

`TARGET` may be a command name or a path. A path, or a known script extension present in the current directory, uses the script runner. A non-path name invokes dynamically as a command.

```shoal
run script.py "--mode" "test"
run("git", "status", "--short")
run ./tool.shl value
```

Runner selection is documented in [Reef environments](@/docs/reef.md). Errors include `arg_error` for no target, `type_error` for a target not string/path, `parse_error`/`io_error` for `.shl`, and `runner_not_found` for a file with no configured extension runner or shebang.

Prefer function-call form when arguments begin with `-`; command-form flag nodes are skipped by the special-head value collector.

### `source`

```text
source PATH
```

Reads and evaluates a Shoal file in the current evaluator and current lexical/session environment. Declarations can therefore remain visible after it returns.

```shoal
source ~/.config/shoal/helpers.shl
```

Contrast with `run file.shl`, which creates a child evaluator with a fresh root lexical scope and binds `args` and `script`. `source` errors with `arg_error` for a non-path, `io_error` for read failure, and `parse_error` for invalid source. Only the first argument is consumed currently.

### `exit` and `quit`

Both canonical heads have the same behavior:

```text
exit [STATUS = 0]
quit [STATUS = 0]
```

They request that the hosting REPL/script loop stop; the evaluator never calls process exit directly, so an embedded kernel is not killed.

```shoal
exit
exit 2
exit (result.status)
```

The first argument is coerced to an integer and clamped to the signed 32-bit range. A non-integer is `arg_error`. Extra arguments are currently ignored.

## Jobs and transcript

### `jobs`

```text
jobs -> table<{id, desc, state, done, suspended}>
```

Rows include spawned tasks and stopped foreground external commands in the local evaluator.

| Column | Type | Meaning |
| --- | --- | --- |
| `id` | `int` | evaluator-local task id |
| `desc` | `str` | task description |
| `state` | `str` | `running`, `stopped`, or `done` |
| `done` | `bool` | completion flag |
| `suspended` | `bool` | local suspension flag |

```shoal
jobs
jobs.where(.state == "running")
```

Extra arguments are ignored. Task operations are methods: `.await()`, `.cancel()`, `.suspend()`, `.resume()`, and state predicates. Kernel wire task control has different limits; see [Tasks and plans over MCP](@/docs/mcp-workflows.md).

### `journal` and `history`

These are two canonical views of the same durable execution journal.

```text
journal [--head=WORD] [--principal=NAME] [--limit=N]
history [--head=WORD] [--principal=NAME] [--limit=N]
-> table<{id, ts, principal, src, ok, status, effects}>
```

```shoal
journal --limit=20
history --head=git --principal=human
journal.where(.ok == false)
```

Without an installed journal they return an empty table. Filters are read only from long flags carrying literal values; dynamic expressions and short forms are not supported. An invalid numeric limit is silently left at the query default in the current implementation.

### `undo`

```text
undo [ENTRY_ID]
-> {entry: int, undone: int, actions: list<str>}
```

No id selects the newest journal entry with recorded inverse steps. An explicit integer/string id targets one entry. Inverses run newest-first and verify fingerprints before changing files.

```shoal
undo
undo 42
```

Errors include:

- `custom` if no journal is active or no reversible entry exists;
- `arg_error` for a non-integer target;
- `stale_undo` if the target changed since the inverse was recorded;
- a filesystem/journal error for replay failure.

Undo covers only recorded trash moves, move-back operations, and complete prior-byte snapshots. It is not a general transaction rollback.

## Planning and assertions

### `assert`

```text
assert CONDITION [MESSAGE]
assert(CONDITION, MESSAGE?)
-> null
```

The condition must be a boolean or an outcome. A false condition raises `assert_failed` with the supplied string or `"assertion failed"`.

```shoal
assert(config.get("version") == 1, "unsupported config")
assert((^git diff --quiet), "working tree changed")
```

Errors: `arg_error` for missing condition/more than two values/named args, `type_error` for a non-condition or non-string message, and `assert_failed` for a false condition.

### `plan`

```text
plan COMMAND [ARG...]
plan { STATEMENTS }
-> {id: int, effects: list<str>, reversible: bool, spawns: bool}
```

Derives conservative effects without spawning or mutating, stores the parsed program in the evaluator, and returns a monotonic session-local integer plan id. The evaluator retains the newest 256 executable plans. Eviction removes only the program: ids are never reused or retargeted.

```shoal
let p = (plan rm build.log)
plan {
    mkdir release
    cp artifact release/artifact
}
```

Unknown/opaque operations appear as opaque process effects rather than being executed. The local evaluator plan record is not the same reference format as a kernel MCP plan, though the workflow is analogous.

### `apply`

```text
apply PLAN_RECORD_OR_ID
```

Runs a previously derived local evaluator plan. Accepts the whole record returned by `plan`, an integer id, or a numeric string.

```shoal
let p = (plan { touch marker })
apply p
```

An invalid value is `arg_error`; an id never issued in this evaluator is `plan_not_found`; an issued id whose program aged out is `plan_expired` with a hint to derive a fresh plan. The stored program is scoped to that evaluator process and is not durable.

### `explain`

```text
explain SOURCE
-> {source: str, effects: list<str>, reversible: bool, spawns: bool}
```

Parses a source string and derives its effects without storing an applicable plan id.

```shoal
explain 'rm --permanent old.log'
```

A non-string/path value is `arg_error`; invalid source is `parse_error`.

## Reef management

### `reef`

```text
reef
reef add TOOL@CONSTRAINT
reef lock [--refresh]
reef fetch TOOL
reef doctor
```

Bare `reef` returns a binding table with `name`, `constraint`, `version`, `hash8`, `provider`, and `scope` fields. Subcommands return structured tables or records and are wrapped in a successful builtin outcome when dispatch succeeds.

```shoal
reef
reef add node@22
reef lock --refresh
reef fetch node
reef doctor
```

The detailed discovery, lock, provider, runner, drift, and error contract lives in [Reef environments](@/docs/reef.md).
Binding, lock, doctor, and candidate tables admit names and rows incrementally against the shared
16,384-value / 16 MiB builtin result wall. Overflow is `builtin_output_limit`; a mutating lock
command does not persist its staged lock when result admission fails.

## Canonical names versus callable builtins

The following evaluator call forms are built in but are **not** entries in the 37-head command registry:

| Call form | Result |
| --- | --- |
| `path(str)` | `path` |
| `glob(pattern, hidden: bool?, follow: bool?)` | `glob` (`follow` is accepted but not currently stored/applied) |
| `regex(str)` | compiled `regex` |
| `channel(name)` | channel handle |
| `every(duration)` | timer stream |
| `watch(path_or_glob, recursive: bool = true)` | filesystem-event stream |
| `tail(path, from_start: bool = false)` | file-tail stream |
| `now()` | current `datetime` |
| `today()` | local start-of-day `datetime` |
| `parallel(thunk...)` | list of results, optionally `settle: true` |
| `retry(attempts, thunk, delay: duration?)` | first successful result |
| `on(channel, handler)` | background task |

`secret.get(NAME)` is a special evaluator call returning an opaque `secret`, or `not_found`/`permission`/`utf8_error`.

## Builtin option handling caveats

The current builtin parsers are intentionally small, and several do not yet enforce a full CLI grammar:

- unknown flags on generic structured builtins are usually ignored;
- special heads using the generic value collector skip all flag nodes;
- `pwd`, `dirs`, `jobs`, and `popd` ignore extra arguments;
- `cd`, `source`, `exit`, `quit`, `apply`, and `explain` primarily consume the first value;
- `head` ignores values after its count;
- `journal` accepts only literal `--name=value`/recognized long-flag value shapes;
- runtime arity checks and completion are not yet fully generated from the canonical signature
  schema, so the remaining permissive cases above can still drift from displayed help.

Treat these as current gaps, not extension points. Write the documented signature so stricter validation will not break your scripts.

## Error handling template

```shoal
let result = try {
    stat path("missing.txt")
} catch err {
    match (err.code) {
        "arg_error" => {kind: "usage", message: err.msg}
        "not_found" => {kind: "missing", message: err.msg}
        _ => {kind: "other", message: err.msg, hint: err.hint}
    }
}
```

Filesystem builtins do not yet normalize every OS error to a precise stable code. Use operation-specific preconditions such as `path.exists()` when the distinction matters, and keep a default catch arm.
