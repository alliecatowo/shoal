+++
title = "Values, types, and methods"
description = "Shoal's runtime data model, access rules, equality, arithmetic, conversion boundaries, secrets, and the core collection method vocabulary."
weight = 60
template = "docs/page.html"

[extra]
eyebrow = "Language guide"
group = "Language"
audience = "Shoal script authors"
status = "Current evaluator"
toc = true
+++

Shoal values carry runtime types through command calls and transformations. Rendering is presentation; a path shown as text is still a path, and a table shown in columns is still structured records.

## Runtime type map

| Type | Purpose | Typical source |
|---|---|---|
| `null` | absence | optional lookup, missing optional field |
| `bool` | typed condition | comparison, `.ok` |
| `int`, `float` | numbers | literals, parsers, arithmetic |
| `str` | Unicode text | literals, command words, decoded output |
| `path` | filesystem path, byte-preserving internally | `pwd`, `path(...)`, path-shaped command words |
| `glob` | pattern plus expansion context | `glob(...)`, wildcard command word |
| `regex` | compiled regular expression | `re"..."`, `regex(...)` |
| `size` | byte count | `10mb`, `ls.size`, `stat.size` |
| `duration` | signed nanoseconds | `250ms`, outcome `.dur` |
| `datetime`, `time` | calendar instant / time of day | `now`, `today`, `t"..."`, `10:30am` |
| `bytes` | arbitrary byte sequence | outcome `.stdout`, file bytes |
| `list<T>` | ordered values | list literal, `map`, `collect` |
| `record` | ordered named fields | record literal, `stat` |
| `table` | record collection with tabular rendering | `ls`, CSV, adapters |
| `range` | integer interval | `1..10`, `1..=10` |
| `stream<T>` | single-consumer asynchronous sequence | `every`, `watch`, `tail`, channel events |
| `error` | caught evaluation error | `catch err { ... }` |
| `outcome<T>` | command status, output, and metadata | external and most builtin commands |
| `task<T>` | background computation | `spawn { ... }`, trailing `&` |
| `closure` | function/lambda plus captured bindings | `fn`, `=>` |
| `command` | parsed command reference | alias |
| `secret` | redacted sensitive material | `secret.get("name")` |

Large captured stdout may be represented by a content-addressed, lazy bytes value. It behaves as bytes for type-facing operations while avoiding immediate materialization.

## No general implicit conversion

Expression operators do not freely turn strings into numbers or paths into strings. Convert at an explicit boundary:

```text
"42".parse_int()
"3.5".parse_float()
path("./README.md")
value.str()
value.display()
value.json()
```

Command binding is the main controlled coercion site. A typed Shoal function or adapter can turn a command word into its declared `int`, `float`, `path`, `glob`, `size`, `duration`, `time`, `datetime`, `bool`, or collection type. Raw external programs receive argv strings.

```text
fn wait_for(delay: duration) { sleep (delay) }
wait_for 250ms
```

For paths, `.str()` is a fallible UTF-8 conversion. `.display()` is intentionally lossy and suitable for diagnostics, not round-tripping. The path value itself preserves non-UTF-8 platform bytes where the host supports them.

## Conditions are typed

Only `bool` and `outcome` can be used as conditions. A successful outcome is true; a failed outcome is false.

```text
if items.is_empty() { "empty" } else { "non-empty" }
if (^test -e ./Cargo.toml) { "found" } else { "missing" }
```

Zero, empty text, empty collections, and null are not silently truthy or falsey. This keeps absence, emptiness, and failure from collapsing into one control signal.

## Equality and identity

Most data values compare structurally. Mixed `int`/`float` equality promotes numerically, and a path can compare with a string by its display form. Tables compare as record lists as well as with equivalent `list<record>` values.

Streams and tasks compare by runtime identity. Outcomes and closures also use object identity rather than deep comparison. Content-addressed bytes compare cheaply by hash and length when both sides are ref-backed; materialize before comparing a ref-backed value with resident bytes.

Streams are single-consumer resources, so equality does not inspect their elements.

## Arithmetic is unit-aware

Regular numeric arithmetic supports integer/float promotion. Integer operations check overflow where applicable, and division by zero raises `div_zero`.

Unit values have meaningful combinations:

```text
10mb + 512kb
10mb / 2
10mb / 2mb          # ratio
5s * 3
2h / 30m            # ratio
now + 15m
now - 1d
now - 30d.ago       # duration
```

Lists concatenate with lists; strings concatenate with strings. Shoal does not guess that a number should become text for `+`—interpolate or call `.str()`.

## Field access is strict

Record fields use `.name` or a string index:

```text
let user = { name: "Allie", role: "maintainer" }
user.name
user["role"]
```

A missing field raises `field_missing`. Optional navigation only handles a null receiver:

```text
maybe_user?.profile?.name ?? "anonymous"
```

For a record, bare `.keys` means a field literally named `keys`. Call `.keys()` for the record method. This rule prevents method names from silently hiding real data fields.

Other types allow selected zero-argument methods as field-like access, particularly inside implicit lambdas. Paths expose `.name`, `.stem`, `.ext`, `.parent`, `.read`, `.read_bytes`, `.lines`, `.exists`, `.is_dir`, `.is_file`, `.size`, and `.modified`; methods requiring arguments remain calls.

```text
glob("src/**/*.rs").map(.name)
path("README.md").read
path("src").join("main.rs")
```

## Indexing

| Receiver | Index | Behavior |
|---|---|---|
| `list` | integer | element; negative counts from end |
| `str` | integer | character; negative counts from end |
| `record` | string | named field |
| `table` | — | integer indexing is intentionally unsupported |

```text
[10, 20, 30][-1]
"shoal"[0]
record[dynamic_key]
table.first()
```

Out-of-range access raises `index_range`. An outcome forwards unknown field and method access to structured `.out`, but indexing does not forward; write `result.out[0]` explicitly when the payload is indexable.

## Collection transforms

Lists, tables, ranges, globs, and streams share much of a collection vocabulary. The eager methods below operate on finite collections; stream variants preserve incremental behavior where documented.

```text
items.map(x => transform(x))
items.where(.enabled)
items.reduce(0, (acc, x) => acc + x)
items.flat_map(.children)
items.each(x => echo (x))
```

Core selection and shape methods include:

| Family | Methods |
|---|---|
| Size/state | `len`, `count`, `is_empty`, `first`, `last` |
| Transform | `map`, `flat_map`, `flatten`, `enumerate`, `chunks`, `zip` |
| Select | `where`/`filter`, `find`, `any`, `all`, `skip`, `take` |
| Order/set-like | `sort`, `sort_by`, `reverse`, `uniq` |
| Aggregate | `reduce`/`fold`, `sum`, `min`, `max` |
| Group/combine | `group`, `group_by`, `join` |
| Runtime bridge | `stream`, `collect`, `tee`, `tap`/`also` |

Aggregates take zero arguments. Project first:

```text
rows.map(.size).sum()
```

Do not write `rows.sum(.size)`.

`tap`/`also` run a side action while preserving the receiver, which is useful for diagnostics inside a chain.

## String and regex methods

Common string operations include:

```text
text.lines()
text.words()
text.chars()
text.trim()
text.upper()
text.lower()
text.split(",")
text.starts_with("v")
text.ends_with(".rs")
text.contains("shoal")
text.replace("old", "new")
text.matches(re"[0-9]+")
text.match(re"^name=(.*)$")
```

Use `.parse_int()` and `.parse_float()` for numeric conversion. Regex values retain their source in `.pattern`-style representations and compile at construction, so invalid patterns fail early.

## Records and serialization

Record methods include `keys()`, `values()`, `items()`, `get(key)`, `set(key, value)`, and `merge(other)`.

```text
let base = { host: "localhost", port: 8080 }
base.merge({ port: 9090 })
base.get("missing")            # null
```

Use namespace encoders when a specific wire format matters:

```text
json.stringify(value, pretty: true)
yaml.stringify(value)
toml.stringify(value)
csv.stringify(rows)
```

`.json()` is a convenient generic JSON representation, but process stdin uses the more specific [feed serialization contract](@/docs/external-commands.md).

## Filesystem methods

Paths are values with filesystem-aware accessors:

```text
let p = path("./notes.txt")
p.exists
p.parent
p.read
p.read_bytes
p.lines
p.abs()
"replacement".save(p)
"more".append(p)
```

Language-visible file reads, probes, navigation, watchers, and mutations go through the evaluator's host filesystem port, which supports embedding and denial tests. The production host currently installs `StdFs`, so this seam is not itself an in-process sandbox. Mutation participates in journaling/undo only where the host installs those facilities and the operation has a recorded inverse.

## Interactive picking

Finite values can open the terminal picker:

```text
(ls .).pick(prompt: "file> ")
(ls .).pick(multi: true, prompt: "files> ")
```

`pick` needs a terminal. It raises an argument error in non-interactive hosts, and canceling it yields an error rather than inventing a selection.

## Secrets

A `secret` is a distinct value whose normal rendering and interpolation expose only its redacted name. Secret material cannot be converted to argv or fed to generic stdin. JSON-like serialization produces a redacted tag rather than the stored bytes. It is intended for explicit, policy-aware injection at approved boundaries such as an HTTP header.

```text
let token = secret.get("deploy_token")
token                         # redacted representation
```

This type reduces accidental disclosure; it does not make an untrusted process safe once the process is authorized to receive the secret. Kernel principals and leash policy remain the authorization boundary.

## Resource values need lifecycle awareness

Streams and tasks are not ordinary replayable collections:

- a stream is single-consumer and may be unbounded;
- a task continues until completion or cancellation;
- a large content-addressed value may load bytes lazily;
- an outcome retains process metadata and possibly captured storage refs.

Use explicit sinks (`collect`, `each`, `render`, `save`) and task methods (`await`, `cancel`) so ownership stays visible. [Streams and channels](@/docs/streams-channels.md) and [Outcomes and errors](@/docs/language-errors-outcomes.md) cover those protocols.
