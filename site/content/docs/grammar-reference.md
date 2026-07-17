+++
title = "Grammar, operators, patterns, and interpolation"
description = "A parser-level reference for Shoal source: dispatch modes, complete expression precedence, statement forms, command arguments, patterns, literals, and continuation."
weight = 300
template = "docs/page.html"

[extra]
eyebrow = "Language reference"
group = "Reference"
audience = "Language users, tooling authors, and reviewers"
status = "Mechanically checked against shoal-syntax and shoal-ast"
toc = true
+++

This chapter describes the grammar the current parser accepts. It is intentionally more exact than the tutorial in [Syntax and literals](@/docs/language-syntax.md): it documents precedence, parser mode transitions, patterns, continuations, and deliberately unsupported shell spellings.

## Source unit and statement boundary

A source unit is a sequence of statements. Newline and `;` are statement terminators. A closing `}` and end of file also terminate the final statement in their scope.

```text
program   := TERM* (statement TERM*)* EOF
TERM      := NEWLINE | ";"
block     := "{" TERM* (statement TERM*)* "}"
```

Comments begin with `#` outside a string and extend to the end of the line.

```shoal
# one statement
let n = 3

# two statements on one line
let a = 1; let b = 2
```

An ordinary newline ends an expression. It is ignored in these continuation positions:

- inside `()`, `[]`, or a delimited record;
- after a binary operator;
- before a leading `.` or `?.` continuing an existing postfix chain;
- before `else` after an `if` block;
- before postfix `catch`.

```shoal
let total = 10 +
    20 +
    30

let names = (ls .)
    .where(.type == "file")
    .map(.name)
    .sort()
```

A line beginning with `.` is special only in the interactive REPL: it continues from `it`. It is a parse error in a script.

## Statement dispatch algorithm

Shoal has expression tokens and command words, but no separate “command substitution language.” The parser decides the mode from syntax and its lexical binding scope.


The rules, in order, are:

1. `let`, `var`, `export`, `fn`, `alias`, `use`, `return`, `break`, `continue`, `for`, and `while` start their statement constructs.
2. `true`, `false`, `null`, `if`, `match`, `try`, `with`, and `spawn` start expressions.
3. An explicit path head (`./x`, `../x`, `/x`, or `~/x`) starts a command.
4. `^head` always starts a forced command.
5. An identifier immediately followed by `.`, `?.`, `(`, or `[` starts an expression.
6. A value-bound identifier starts an expression. An unbound or command-bound identifier starts a command.
7. Any other token starts an expression.

This makes both forms intentional:

```shoal
git status --short              # command statement
let git = {name: "not a command"}
git.name                        # expression, because git is value-bound
^git status --short             # bypass a git adapter (unless git is a callable binding)
```

`^` has a deliberately narrow effect in the current evaluator. It bypasses a non-callable `let`/`var` shadow and skips adapter dispatch. Session functions, aliases, and other callable bindings still win, and every special/structured builtin still wins. Use `run("name", ...)` when an external executable shares a builtin or callable name.

## Statement forms

### Bindings and assignment

```text
let NAME (":" type)? "=" expr
var NAME (":" type)? "=" expr
export (let | var) NAME (":" type)? "=" expr
target ("=" | "+=" | "-=" | "*=" | "/=") expr
```

The current binding grammar accepts only an identifier or `_` after `let`, `var`, and `for`. Destructuring patterns are implemented for `match`, not for binding statements.

```shoal
let immutable = 1
var mutable: int = 2
mutable += 3
env.LOG_LEVEL = "debug"
```

An assignment target may be a mutable name, a field, an index, or `env.NAME`, subject to evaluator type and mutability checks.

### Function declarations

```text
fn NAME "(" params? ")" ("->" type)? block
export fn NAME "(" params? ")" ("->" type)? block

params    := param ("," param)* ("," "..." rest)?
param     := NAME (":" type)? ("=" expr)?
rest      := NAME (":" type)?
type      := NAME ("<" type ("," type)* ">")? "?"?
```

```shoal
fn greet(name: str, punctuation: str = "!") -> str {
    "hello {name}{punctuation}"
}

fn collect(first: str, ...rest: str) -> list<str> {
    [first] + rest
}
```

Optional type syntax is `T?`. Generic-looking annotations such as `list<path>` are parsed recursively. Runtime coercion and validation are documented in [Functions and control flow](@/docs/language-functions-control.md).

### Aliases and modules

```text
alias NAME "=" command
use PATHWORD
```

An alias stores a parsed command call, including its fixed arguments. `use ./lib/deploy` resolves `./lib/deploy.shl`, evaluates it once in a fresh module scope, and binds an exports record named `deploy`.

```shoal
alias gs = git status --short
use ./lib/deploy
deploy.build("staging")
```

### Loops and jumps

```text
for (NAME | "_") in expr block
while expr block
return expr?
break
continue
```

`for` accepts lists, tables, ranges, globs, and streams. A stream is driven to completion and therefore must be bounded. `return` is valid inside a function; `break` and `continue` apply to a loop.

## Expression grammar

The following compact grammar is descriptive; the implementation uses a Pratt parser for binary operators and a postfix loop for access and calls.

```text
expr       := unary (binary-op unary)* ("catch" NAME? handler)?
unary      := ("!" | "-") unary | postfix
postfix    := primary postfix-part*
postfix-part
           := ("." | "?.") NAME
            | ("." | "?.") NAME "(" args? ")" trailing-block?
            | "[" expr "]"
            | "(" args? ")" trailing-block?
primary    := literal
            | NAME
            | "(" expr-or-command ")"
            | list
            | record-or-block
            | lambda
            | if-expr
            | match-expr
            | try-expr
            | with-expr
            | spawn-expr
            | interpreter-block
```

Postfix operations bind more tightly than unary and every binary operator:

```shoal
-value.abs()          # -(value.abs())
items[0].name.upper()
user?.profile?.name ?? "anonymous"
```

### Operator precedence

From lowest binding power to highest:

| Level | Operators | Meaning | Associativity in the parser |
| ---: | --- | --- | --- |
| 1 | `??` | null coalescing | left |
| 2 | `||` | boolean or; operands may be commands | left, short-circuit |
| 3 | `&&` | boolean and; operands may be commands | left, short-circuit |
| 4 | `==`, `!=`, `<`, `<=`, `>`, `>=`, `in` | comparison and membership | non-chainable |
| 5 | `..`, `..=` | exclusive/inclusive integer range | left |
| 6 | `+`, `-` | addition, concatenation, subtraction | left |
| 7 | `*`, `/`, `%` | multiplication, division, remainder | left |
| 8 | prefix `!`, prefix `-` | negation | right |
| 9 | `.`, `?.`, `[]`, calls | access and invocation | left postfix chain |

Comparison chains are rejected:

```shoal
# parse error
1 < n < 10

# explicit and valid
1 < n && n < 10
```

`??` tests only for `null`. It does not catch an error and it does not reinterpret false, zero, or an empty collection as absent.

```shoal
null ?? "fallback"     # "fallback"
false ?? true           # false
[] ?? [1]               # []
```

There is no general truthiness. Conditions accept `bool`; command outcomes are accepted using their `.ok` state. Use `.is_empty()`, `!= null`, or a specific predicate for other values.

### Operator domains

The evaluator supplies these principal domains:

| Operator | Supported examples |
| --- | --- |
| `+` | numeric addition; string concatenation; list concatenation; duration/datetime combinations where defined |
| `-` | numeric subtraction; temporal subtraction where defined |
| `*` | numeric multiplication |
| `/` | numeric division; division by zero errors |
| `%` | integer/number remainder |
| `==`, `!=` | structural data equality; mixed int/float numeric equality; path/string display equality |
| ordering | int, float, string, path, size, duration, datetime, time, and bool of compatible types |
| `in` | collection membership, record key membership, substring membership where implemented |
| `&&`, `||` | boolean/outcome conditions with short circuit |

Streams and tasks compare by identity, not by consuming or awaiting them. Secrets do not reveal their content through rendering.

## Literals

### Null and booleans

```shoal
null
true
false
```

### Integers and floats

Decimal digits may contain `_`. Integers also accept binary, octal, and hexadecimal prefixes. Floats accept a decimal fraction and/or exponent.

```shoal
42
1_000_000
0xff
0o755
0b1010
3.14159
6.02e23
1.0e-9
```

### Size literals

Size units are lowercase and are converted to an unsigned byte count.

| Unit | Multiplier |
| --- | ---: |
| `b` | 1 |
| `kb` | 1,000 |
| `mb` | 1,000,000 |
| `gb` | 1,000,000,000 |
| `tb` | 1,000,000,000,000 |
| `kib` | 1,024 |
| `mib` | 1,048,576 |
| `gib` | 1,073,741,824 |
| `tib` | 1,099,511,627,776 |

Fractions are allowed and round to the nearest byte:

```shoal
512b
1.5mb
2gib
```

Uppercase units are rejected to prevent decimal/binary ambiguity.

### Duration literals

Durations are stored as signed nanoseconds.

| Unit | Meaning |
| --- | --- |
| `ns` | nanoseconds |
| `us` | microseconds |
| `ms` | milliseconds |
| `s` | seconds |
| `m` | minutes |
| `h` | hours |
| `d` | days |
| `w` | weeks |

```shoal
250ms
1.5h
30d.ago
2h.from_now
```

`duration.ago` and `duration.from_now` are fields resolved against the host wall clock and produce `datetime` values.

### Time and datetime literals

Time-of-day accepts 24-hour time or an `am`/`pm` suffix:

```shoal
09:30
23:15:45
10:00am
7:05pm
```

A datetime is tagged with `t`:

```shoal
t"2026-07-16T09:30:00-07:00"
```

The evaluator validates and parses the tagged value. Datetime fields are `.year`, `.month`, `.day`, `.hour`, `.minute`, and `.second`.

### Regex and glob values

Regex literals are tagged strings:

```shoal
re"^(?<name>[a-z]+)-(?<n>[0-9]+)$"
```

Globs are usually recognized as command words or constructed explicitly:

```shoal
glob("src/**/*.rs")
glob("*.toml").expand()
```

### Lists

```text
list := "[" (expr ("," expr)* ","?)? "]"
```

```shoal
[]
[1, 2, 3]
[
    {name: "api", port: 8080},
    {name: "web", port: 3000},
]
```

### Records and expression blocks

Braces are disambiguated by their first entry. `{}` is an empty record. A first identifier or string key followed by `:` makes a record. Otherwise the braces are an expression block.

```shoal
{name: "shoal", ready: true}
{"Content-Type": "application/json"}

{
    let x = 20
    x + 22
}
```

The last statement's value is the block value. A record preserves insertion order.

## Strings and interpolation

### Interpolating strings

Double-quoted strings process escapes and `{expr}` interpolation.

```shoal
let name = "Shoal"
"hello {name.upper()}"
"count = {[1, 2, 3].len()}"
```

Supported escapes are:

| Escape | Value |
| --- | --- |
| `\n` | newline |
| `\t` | tab |
| `\r` | carriage return |
| `\0` | NUL |
| `\\` | backslash |
| `\"` | double quote |
| `\{` | literal `{` |
| `\}` | literal `}` |
| `\u{HEX}` | Unicode scalar value |

Interpolation accepts an expression, not a command statement. Parenthesize a command first when its result is needed:

```shoal
let branch = (^git branch --show-current).out.str().trim()
"branch: {branch}"
```

Every value has an interpolation render form, including types that `.str()` deliberately refuses to convert.

### Raw strings

Single-quoted strings have no escapes and no interpolation:

```shoal
'literal {braces} \n $HOME'
```

### Multiline strings

Triple double quotes interpolate; triple single quotes are raw. Both remove a leading newline and common indentation.

```shoal
let rendered = """
    name = {name}
    ready = true
    """

let literal = '''
    exactly {this}
    no interpolation
    '''
```

## Calls and lambdas

### Positional and named arguments

```text
args := argument ("," argument)* ","?
argument := expr | NAME ":" expr
```

```shoal
json.stringify(value, pretty: true)
retry(3, () => fetch(), delay: 250ms)
```

### Lambdas

```text
lambda := NAME "=>" expr
        | "(" params? ")" "=>" (expr | block)
```

Lambda declaration parameters may have type annotations but not defaults in the parenthesized lambda syntax.

```shoal
[1, 2, 3].map(x => x * 2)
[1, 2, 3].reduce(0, (acc: int, x: int) => acc + x)
(() => { 42 })()
```

The leading-dot shorthand is accepted when the entire call argument is a projection or predicate:

```shoal
(ls .).map(.name)
(ls .).where(.size > 1mb)
```

Outside that position, name the parameter explicitly.

### Trailing block arguments

A block after a call becomes a zero-argument lambda argument:

```shoal
retry(3) {
    ^curl --fail https://example.invalid
}
```

Likewise, `items.each { ... }` supplies a thunk. Use an explicit parameter lambda when the callee passes an item.

## Optional access and indexing

`?.` returns `null` when its receiver is `null`; otherwise it performs the same field or method access as `.`.

```shoal
user?.profile?.display_name ?? "anonymous"
```

Indexing currently supports:

| Receiver | Index | Behavior |
| --- | --- | --- |
| list | integer | item; negative indices count from the end |
| string | integer | Unicode scalar as a one-character string; negative allowed |
| record | string | value for that key |

An out-of-range list/string index raises `index_range`; a missing record key raises `field_missing`.

## Control expressions

### If

```text
if expr block (else if expr block | else block)?
```

```shoal
let label = if (result.ok) {
    "success"
} else {
    "failure"
}
```

### Match

```text
match expr "{" TERM*
    match-arm (TERM | ",")*
"}"

match-arm := pattern ("|" pattern)* ("if" expr)? "=>" (expr | block)
```

Patterns are tested in arm order. The first matching pattern whose guard succeeds selects the arm.

```shoal
match value {
    null => "missing"
    0 | 1 => "small"
    int n if n > 100 => "large int"
    {status: 200, body} => body
    [first, ...rest] => ({first: first, remaining: rest.len()})
    _ => "other"
}
```

### Try and postfix catch

```text
try block catch (NAME | "_")? block
expr catch NAME? (expr | block)
```

```shoal
let value = try {
    risky()
} catch err {
    {code: err.code, fallback: true}
}

let value2 = risky() catch err { err.msg }
```

The catch binder receives an `error` value with `.code`, `.msg`, `.hint`, `.stderr`, and `.status`. Its internal source span is not exposed as a field.

### Dynamic scope with `with`

```text
with scope-entry ("," scope-entry)* block
scope-entry := ("cwd" | "env" | "reef") ":" expr
```

```shoal
with cwd: path("./service"), env: {MODE: "test"}, reef: {node: "22"} {
    npm test
}
```

All selected scopes restore on normal return and error.

### Spawn

```shoal
let task = spawn {
    sleep 500ms
    "complete"
}
task.await()
```

`spawn block` returns a single-consumption task handle registered in the local jobs table.

## Match pattern reference

The current match grammar supports:

| Pattern | Example | Meaning |
| --- | --- | --- |
| wildcard | `_` | always matches, binds nothing |
| binding | `x` | always matches and binds the value |
| boolean literal | `true` | structural equality |
| integer literal | `42` | structural/numeric equality |
| string literal | `"ok"` | string equality |
| range | `1..10`, `1..=10` | integer exclusive/inclusive interval |
| type + binder | `int n`, `str text` | runtime type check and bind |
| record | `{name, size: n}` | requires named fields; extra fields allowed |
| list | `[a, b]` | exact length and recursive element patterns |
| list rest | `[head, ...tail]` | fixed prefix and remaining list |
| alternation | `404 | 410` | either pattern in the same arm |
| guard | `int n if n > 0` | additional boolean condition |

Type-pattern names mirror runtime names:

```text
null bool int float str path glob regex size duration datetime time bytes
list record table range stream error outcome task closure command secret
```

A lone type-looking identifier is a normal binder. `int` binds a variable named `int`; `int n` is the type pattern.

Record matching is open:

```shoal
match {name: "api", port: 8080, ready: true} {
    {name, port: p} => "{name}:{p}"
}
```

List matching without `...rest` is exact-arity.

## Command grammar

```text
command := env-prefix* "^"? command-head command-part* redirect* "&"? trailing-block?
env-prefix := NAME "=" WORD
command-head := WORD | PATHWORD
command-part := WORD
              | PATHWORD
              | GLOBWORD
              | string
              | "(" expr-or-command ")"
              | long-flag
              | short-flags
              | "--"
              | "-"
redirect := "<" command-arg | ">" command-arg | ">>" command-arg
```

Command words become strings unless a builtin, adapter, or function signature requests coercion. Path-shaped words and globs retain their shape. Parenthesized arguments are evaluated and may expand to multiple argv words when the value is an expandable collection.

```shoal
MODE=test ^printenv
echo (2 + 2)
cp (glob("src/*.rs")) path("backup")
```

### Flags

Command-mode flags remain structured in the AST:

```text
--flag
--flag=value
--flag value       # when the lexer recognizes a pending value form
-abc
--                 # explicit flag terminator node
-                  # ordinary dash argument node
```

Functions bind long flags to parameter names exactly as declared. A parameter `dry_run` is called with `--dry_run`; automatic underscore-to-hyphen mapping is an adapter behavior, not a universal function rule.

### Globs

A command glob expands before invocation. An empty glob yields no argv entries. Destructive builtins add their own safety checks; in particular, `rm` with no expanded paths raises `no_matches`.

Quote a metacharacter to pass it literally:

```shoal
echo *.rs       # expanded command argument
echo '*.rs'     # literal string
```

### Redirects

Shoal supports `<`, `>`, and `>>`. Builtin output is redirectable as rendered/serialized value bytes; external output uses the process boundary.

These Bourne spellings are deliberately rejected with teaching errors:

- `|` pipelines;
- `<<` heredocs;
- `<<<` here-strings;
- `2>` and other fd-numbered redirects;
- `&>` and `&>>` stream-merging redirects;
- backtick command substitution;
- `$name` variable expansion.

Use structured alternatives:

```shoal
let names = (^printf 'a\nb\n').out.str().lines()
"payload".feed(^cat)
(^tool).stderr
sh { printf '%s\n' "$HOME" | sort }
```

### Background marker

A final `&` desugars to background task execution. It is not a binary operator and cannot appear mid-command.

## Interpreter blocks

The parser recognizes interpreter-class heads before an immediately following raw block. Built-in adapter metadata currently declares common interpreters including `sh`, `bash`, `python`, `node`, `deno`, and `ruby`.

```shoal
python {
print("hello from Python")
}

sh '''
printf '%s\n' "$HOME"
'''
```

The block body is not Shoal source. Balanced braces are preserved for brace blocks; triple-raw form ends at the closing delimiter. The adapter decides whether source travels as one argv word or on stdin.

## Parsing traps and exact fixes

| Attempt | Why it fails | Current spelling |
| --- | --- | --- |
| `git log.len()` | `.len()` attaches to the last command word ambiguously | `(git log).len()` |
| `$HOME` | variables have no sigil | `env.HOME` |
| `` `pwd` `` | backticks are not syntax | `(pwd)` or the `pwd` value |
| `a | b` | no pipe operator | data method chain or `.feed(command)` |
| `{a}` as record | no `:` means expression block | `{a: a}` |
| `let [a, b] = xs` | binding destructuring is not implemented | index/get explicitly; use list patterns in `match` |
| `for [a, b] in xs` | loop binding accepts a name or `_` only | `for pair in xs { ... }` |
| `x?.missing ?? fallback` after a non-null record | optional access only protects null receiver, not absent field | use `.get("missing", fallback)` |
| `erroring() ?? fallback` | coalescing does not catch errors | `erroring() catch fallback` |
| `1 < n < 10` | comparisons do not chain | `1 < n && n < 10` |
| `.name` at top level | shorthand has no receiver | `x.name`, or `.name` as a complete callback argument |

## Formatter contract

`shoal fmt` parses source into the canonical AST and renders normalized source. `shoal fmt --check` exits `1` when formatting would change a file and `0` when it is already formatted. Formatting cannot preserve every trivia choice because comments and whitespace are not all represented in the current AST; review formatted output before applying it across a large codebase.

```bash
shoal fmt script.shl
shoal fmt --check script.shl
printf 'let x=1\n' | shoal fmt
```

For runtime behavior after parsing, continue with [Value types and every method](@/docs/value-methods-reference.md), [Builtin command reference](@/docs/builtins-reference.md), and [Outcomes and errors](@/docs/language-errors-outcomes.md).
