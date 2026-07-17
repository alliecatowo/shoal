+++
title = "Syntax and literals"
description = "A grammar-oriented tour of statements, command words, strings, numbers, units, collections, operators, chaining, and continuation."
weight = 50
template = "docs/page.html"

[extra]
eyebrow = "Language guide"
group = "Language"
audience = "Shoal script authors"
status = "Current parser"
toc = true
+++

Shoal's parser is modal: it tokenizes command arguments differently from expressions, based on grammar position. The mode is determined while parsing—not by a runtime guess after names have executed. Read [the command/expression model](@/docs/mental-model.md) first if that distinction is new.

## Statements and terminators

A newline or semicolon ends a statement. A closing brace or end of file also closes the final statement in its scope.

```text
let a = 1
let b = 2; a + b
```

Statements include declarations, assignments, function and alias definitions, module imports, loops, control-flow statements, and expression/command statements.

```text
let immutable = 1
var mutable = 2
mutable += 3

fn double(n: int) -> int { n * 2 }
alias ll = ls --all
use ./lib/report

for item in [1, 2, 3] { echo (item) }
while mutable < 10 { mutable += 1 }
```

## Continuation across lines

Input continues when syntax is visibly incomplete:

- an opening `(`, `[`, or `{` has not closed;
- the previous line ends in an operator or comma;
- the following line begins with `.`, `catch`, or `else` where it attaches;
- the previous line ends in a backslash-newline continuation.

```text
let names = (ls .)
  .where(.type == "file")
  .map(.name)
  .sort()

let total = 10 +
  20 +
  12
```

Prefer delimiter- or operator-driven continuation. Backslash continuation exists for command-shaped input but is easier to break while editing.

## Comments

`#` starts a comment only at a token boundary. This allows ordinary command words containing `#`.

```text
let n = 3 # comment
echo ver#2 # `ver#2` is one argument, then a comment
```

## Names and reserved words

Language constructs reserve these leading words:

```text
let var fn alias use export return break continue
if else match for in while try catch with spawn true false null
```

Names bound by `let`, `var`, parameters, and functions affect statement dispatch. To reach a same-named external program through a non-callable value shadow, prefix the command with `^`. Callable bindings and builtins still resolve before `^`; use `run("name", ...)` to reach an executable sharing one of those names.

```text
let test = "value"
test
^test -d .git
```

`it` and `out` are reserved for the interactive transcript and are rejected when referenced in ordinary scripts.

## Strings

Double-quoted strings interpolate `{expression}` and process escapes:

```text
let name = "Shoal"
"hello, {name.upper()}"
"line one\nline two\tindented"
"literal braces: \{value\}"
"unicode: \u{1f41a}"
```

Supported escapes include `\n`, `\t`, `\r`, `\0`, `\\`, `\"`, escaped braces, and `\u{...}`.

Single-quoted strings are raw: they do not interpolate or interpret backslash escapes.

```text
'$HOME stays text'
'\n is two characters'
```

Triple strings are multiline and dedent common indentation. Both interpolating and raw forms exist:

```text
let rendered = """
  project: {name}
  status: ready
  """

let exact = '''
  {nothing interpolates here}
  backslashes\stay\literal
  '''
```

Interpolation must contain exactly one expression, not a sequence of statements.

## Numbers and units

Integers support decimal, hexadecimal, octal, and binary notation; underscores are separators. Floats support decimals and scientific notation.

```text
42
1_000_000
0xff
0o755
0b1010
3.14159
6.02e23
```

Sizes are base-aware values, not decorated integers:

```text
500b
4kb
2.5mb
8gb
1kib
32mib
```

Decimal suffixes use powers of 1000; `kib`, `mib`, `gib`, and `tib` use powers of 1024. Rendering chooses a compact size form, while arithmetic retains the exact byte count.

Durations support nanoseconds through weeks and compound text where a typed command parameter is expected:

```text
250ms
5s
2h
7d
1w
```

Duration values can be negative through unary `-`. Relative fields anchor them to the current wall clock:

```text
30d.ago
15m.from_now
```

## Time, date, regex, and constructors

Time-of-day literals include 12- and 24-hour forms:

```text
10:00am
23:15
14:30:05
```

Tagged datetime and regex literals make intent explicit:

```text
t"2026-07-16"
t"2026-07-16T09:30:00-07:00"
re"^[a-z][a-z0-9_-]+$"
```

`now`, `today`, `now()`, and `today()` expose live calendar values. Datetimes provide component fields such as `.year`, `.month`, `.day`, `.hour`, `.minute`, and `.second`.

Expression constructors cover values that do not have a standalone expression literal:

```text
path("./README.md")
glob("src/**/*.rs", hidden: false)
regex("^v[0-9]+$")
```

In command mode, words beginning with `./`, `../`, `/`, or `~/` are path values, while wildcard-bearing words are globs.

## Lists, records, tables, and ranges

Lists preserve order and can contain any values:

```text
[1, 2, 3]
["name", 42, true, null]
```

Records preserve named fields in insertion order. Identifier or string keys are accepted:

```text
{ name: "shoal", ready: true }
{ "content-type": "application/json", count: 3 }
```

A table is the specialized value used for a collection of records with column rendering. Many builtins and parsers return one; `csv.parse(...)` is a direct constructor in practice.

Ranges can be exclusive or inclusive:

```text
1..5
1..=5
```

## Field access, indexing, and calls

Postfix forms bind most tightly:

```text
user.name
user?.profile?.name
items[0]
record["dynamic-key"]
function(1, mode: "fast")
value.method(arg)
```

`?.` short-circuits only when its receiver is `null`; it does not swallow arbitrary errors. Record field access is strict. A missing field raises `field_missing` unless optional navigation has already encountered null.

Lists and strings accept negative indices (`-1` means last). Record indexing requires a string key. Tables intentionally do not support integer indexing; use `.first()`, `.last()`, or collection transforms.

## Lambdas and implicit item expressions

Lambdas have single- or multi-parameter forms and expression or block bodies:

```text
x => x * 2
(a, b) => a + b
(acc, item) => { acc + item }
```

Inside a collection method argument, a leading dot is shorthand for a one-item lambda:

```text
rows.where(.status == "ready")
rows.map(.name)
paths.map(.read)
```

It behaves like a generated parameter receiving each item. Use a named lambda when the expression is nontrivial or refers to multiple inputs.

## Operators and precedence

From tightest to loosest:

| Level | Forms | Meaning |
|---:|---|---|
| 1 | `.`, `?.`, `[]`, `()` | postfix access/call |
| 2 | `!`, unary `-` | condition negation, numeric/duration negation |
| 3 | `*`, `/`, `%` | multiplicative |
| 4 | `+`, `-` | additive |
| 5 | `..`, `..=` | ranges |
| 6 | `==`, `!=`, `<`, `<=`, `>`, `>=`, `in` | comparison/membership |
| 7 | `&&` | conditional and |
| 8 | `||` | conditional or |
| 9 | `??` | null coalescing |
| 10 | postfix `catch` | error recovery |
| statement | `=`, `+=`, `-=`, `*=`, `/=` | assignment |

Comparisons cannot chain. Write `a < b && b < c`, not `a < b < c`.

`??` only replaces `null`; it does not catch an error:

```text
config.get("port") ?? 8080
parse() catch 8080
```

`&&` and `||` accept only booleans and outcomes as conditions, short-circuit, and return the deciding operand itself.

## Command syntax

A command statement has an optional environment prefix, a head, command arguments, redirects, and optional background marker:

```text
RUST_LOG=debug cargo test --workspace > ./test.log &
```

Command arguments include:

```text
echo plain-word
cat ./README.md
rm *.tmp
echo (1 + 2)
deploy --replicas 3 --dry_run
command -- --literal
```

`(expression)` is the universal bridge from expression values into command arguments. Globs expand in stable order for ordinary command parameters; a `glob`-typed function parameter can receive the pattern itself.

Redirections are command-only:

```text
command > ./out.txt
command >> ./out.txt
command < ./input.txt
```

Append `&` to produce a background task. Redirection and task lifecycle are covered in [Filesystem, jobs, history, and undo](@/docs/filesystem-jobs-history.md).

## Syntax intentionally not accepted

Shoal emits teaching diagnostics for several familiar shell forms:

| Form | Why | Shoal form |
|---|---|---|
| `$name` | variables are values, not text expansion | `name`; environment is `env.NAME` |
| `` `cmd` `` | no textual command substitution | `(cmd args)` |
| `a \| b` | no byte-pipeline operator | method chains or `.feed(command)` |
| implicit truthiness | conditions are typed | compare or call `.is_empty()` |

A lone `|` is legal only between alternatives in a `match` pattern. Verbatim POSIX source belongs in an interpreter block such as `sh { ... }`; see [External commands and data exchange](@/docs/external-commands.md).
