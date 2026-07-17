+++
title = "Functions, control flow, and modules"
description = "Declare bindings and typed functions, use command-call syntax, branch and match, scope state, build modules, and choose source versus script execution."
weight = 70
template = "docs/page.html"

[extra]
eyebrow = "Language guide"
group = "Language"
audience = "Shoal script and library authors"
status = "Current evaluator"
toc = true
+++

Shoal functions are closures that can be called in expression style or command style. Control forms return values, lexical bindings capture by shared cells, and modules expose an explicit record of exports.

## Immutable and mutable bindings

Use `let` by default and `var` only when state must change:

```text
let root = pwd
var attempts: int = 0
attempts += 1
```

Assignments support `=`, `+=`, `-=`, `*=`, and `/=`. Reassigning a `let` binding is an error.

The current parser binds a single name (or `_`) in declarations and `for` loops. Rich list/record destructuring is implemented for `match` patterns, but not yet for `let [a, b] = ...` or `for {name} in ...`; older design notes that show those declaration forms are ahead of the parser.

## Define a function

```text
fn greet(name: str, excited: bool = false) -> str {
  let suffix = if excited { "!" } else { "." }
  "Hello, {name}{suffix}"
}
```

Parameters and return annotations are optional. Defaults are evaluated when needed. A rest parameter collects surplus positional arguments:

```text
fn total(...nums: int) -> int {
  nums.sum()
}

total(1, 2, 3)
```

Calls reject extra positional arguments without a rest parameter, unknown named arguments, and missing required arguments. Type annotations are not yet sound runtime contracts: command-form calls use them mainly to coerce bare string words (and a non-string expression can pass through unchanged), expression-form calls do not enforce scalar annotations, and return annotations are stored but not checked. Validate important invariants explicitly with `assert` or `match`.

## Expression calls and command calls

The same function supports both surfaces:

```text
greet("Shoal", excited: true)
greet Shoal --excited
```

In command form, bare string words are coerced using declared parameter types. A boolean long flag means presence; a non-boolean declared flag consumes its next word or an inline `=value`. Already typed non-string values are generally left unchanged even when they disagree with an annotation.

```text
fn deploy(environment: str, replicas: int = 1, dry_run: bool = false) {
  { environment: environment, replicas: replicas, dry_run: dry_run }
}

deploy staging --replicas 3 --dry_run
deploy staging --replicas=3
```

Session-function flag names currently match parameter names exactly, including underscores. Adapter flags may map underscore schema names to hyphenated external flags, but do not assume that spelling conversion for a Shoal function.

Use `--help` on a function's command form to render its synthesized signature and documentation metadata:

```text
deploy --help
```

When a command-form call appears inside an expression, parenthesize it:

```text
let plan = (deploy staging --dry_run)
```

## Typed command binding

Command words can bind to `str`, `path`, `glob`, `int`, `float`, `size`, `duration`, `time`, `datetime`, `bool`, and `list<T>` parameters.

```text
fn older_than(limit: duration, files: list<path>) {
  files.where(.modified < limit.ago)
}

older_than 30d *.log
```

A `glob` parameter receives the pattern itself; a `list<path>` parameter receives expanded, sorted matches as one list. This distinction lets library authors choose whether expansion is caller- or callee-controlled.

## Lambdas and closures

```text
let twice = x => x * 2
let add = (a: int, b: int) => a + b
let summarize = rows => {
  let total = rows.map(.size).sum()
  { count: rows.len(), total: total }
}
```

Closures capture lexical binding cells, so a captured `var` observes later mutations. Prefer returning new values over relying on distant mutable captures in reusable code.

Collection methods accept explicit lambdas or implicit leading-dot forms:

```text
rows.where(r => r.status == "ready")
rows.where(.status == "ready")
```

## `if` is an expression

```text
let label = if score >= 90 {
  "excellent"
} else if score >= 70 {
  "good"
} else {
  "needs work"
}
```

Without an `else`, a false branch produces `null`. Conditions must be boolean or an outcome.

## Loops

```text
for path in glob("src/**/*.rs") {
  echo (path.name)
}

var remaining = 3
while remaining > 0 {
  echo (remaining)
  remaining -= 1
}
```

`break` exits the nearest loop and `continue` advances it. `return` exits the current function, optionally with a value.

Loops currently bind one name per iteration. Use an inner `match` or field access for structured items.

## Pattern matching

`match` supports literal, inclusive/exclusive integer range, type, record, list, rest, wildcard, binder, alternation, and guarded patterns.

```text
let description = match value {
  null => "missing"
  0 | 1 => "bit"
  2..=9 => "single digit"
  int n if n < 0 => "negative {n}"
  str s => "text: {s}"
  { status, body: { ready } } if ready => "status {status}: ready"
  [first, ...rest] => "first={first}, remaining={rest.len()}"
  _ => "other"
}
```

Record patterns are open: extra fields on the value are ignored. A bare identifier binds; a type pattern requires a known type name followed by a binder, such as `int n`. Patterns in one arm can be separated by `|`; this is the only general language context where a single pipe token is legal.

## Scoped context with `with`

`with` dynamically overrides current directory, environment, and/or Reef constraints for one block, then restores the prior state even if evaluation raises an error.

```text
let result = with cwd: ./crates/shoal,
                  env: { RUST_LOG: "debug" },
                  reef: { rust: "stable" } {
  cargo test
}
```

The accepted keys are exactly `cwd`, `env`, and `reef`. Direct `cd` and session environment writes are allowed at session/module top level but rejected inside function bodies; use `with`, parameters, and returned values so functions do not secretly mutate their caller's host context.

## Aliases are parsed commands

```text
alias gs = git status
alias recent = git log --oneline -20

gs --short
```

An alias stores an AST-level partial command. Later arguments and flags append structurally; Shoal does not paste strings and reparse them. For branching or typed parameters, define a function instead.

## Modules with `use`

Suppose `lib/release.shl` contains:

```text
let prefix = "release"

export let version = "1.0"

export fn tag(name: str) -> str {
  "{prefix}/{name}/{version}"
}
```

Import it from a caller:

```text
use ./lib/release
release.version
release.tag("candidate")
```

`use` tries the supplied path and an added `.shl` extension. The module:

- resolves relative to the evaluator's current directory;
- evaluates once per session and is memoized by canonical path;
- receives a fresh root scope and cannot see caller locals;
- uses its own containing directory as cwd while loading;
- returns an export record bound under the file stem;
- keeps private declarations available to exported closures;
- rejects circular imports with a cycle diagnostic.

`export let`, `export var`, and `export fn` are accepted at module top level. Exporting mutable state does not turn the module record into a general package manager; prefer function-mediated state where ownership matters.


## `source`, direct `.shl`, and `run`

These are deliberately different:

| Form | Scope | Intended use |
|---|---|---|
| `use ./module` | fresh, cached module; explicit exports | libraries |
| `source ./setup.shl` | current evaluator scope | intentional session initialization |
| `./script.shl` or `run ./script.shl` | separate child evaluator | program execution |
| `run ./tool.py` | extension/shebang runner, Reef-aware | polyglot scripts |

`source` can mutate the current scope and is therefore less composable than `use`. A directly invoked `.shl` file does not leak its bindings into the caller.

## Recursion and planning

Runtime callable dispatch has a hard maximum depth of 128. The 129th nested call raises `recursion_limit` with `maximum call depth of 128 exceeded`; plan derivation uses a separate, smaller internal guard. The ceiling counts nested callable values, including mutual recursion, and is a safety boundary rather than a tail-recursion feature. Use iterative collection operations or loops for larger traversals while the preview runtime lacks tail-call optimization.

Continue with [Outcomes and errors](@/docs/language-errors-outcomes.md) for recovery and [Reef environments](@/docs/reef.md) for runner/tool resolution.
