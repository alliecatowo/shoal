+++
title = "Collections, tables, and data namespaces"
description = "Query records and tables, choose eager or streaming transforms, parse common formats, call HTTP, use math, and inspect host/config data."
weight = 90
template = "docs/page.html"

[extra]
eyebrow = "Structured data"
group = "Language"
audience = "Data-oriented shell users"
status = "Current evaluator"
toc = true
+++

Structured values are Shoal's main alternative to text pipelines. Builtins and adapters return records, tables, lists, sizes, paths, and outcomes; collection methods preserve those types while filtering or reshaping them.

## List, record, and table

These three values serve different roles:


A table is semantically a record collection. It compares equal to an equivalent `list<record>` and uses the same eager transform vocabulary, but renders as columns and intentionally does not support `table[0]`.

```text
let files = (ls .)
files.first()
files.where(.type == "file")
files.map(.name)
```

Use `.first()` or `.last()` to select a row. With a count, they return a list:

```text
files.first(5)
files.last(2).map(.name)
```

## Query pipeline

A typical query selects, projects, orders, and aggregates without rendering between stages:

```text
let largest = (ls .)
  .where(.type == "file")
  .sort_by(.size)
  .reverse()
  .first(10)

{
  files: largest.map(.name),
  bytes: largest.map(.size).sum(),
  count: largest.len(),
}
```


Aggregates have no projection parameter. Write `rows.map(.size).sum()`, not `rows.sum(.size)`.

## Selection methods

```text
rows.where(.enabled)
rows.filter(x => x.score >= 80)
rows.find(.name == "shoal")
rows.any(.failed)
rows.all(.ready)
rows.contains(target)
records.get(3, null)       # list only
row.get("name", "unknown") # record only
```

`where` and `filter` are aliases. `find` returns the first match or null. `any` and `all` return booleans. `get` accepts only list+integer or record+string, with an optional default; it does not currently accept a table or range even though completion metadata may suggest it. Strict `[]` indexing raises instead of returning a default.

Predicates must produce a boolean or outcome; there is no empty-value truthiness.

## Reshaping and grouping

```text
rows.map({ name: .name, size: .size })
rows.flat_map(.children)
nested.flatten()
rows.enumerate()
rows.chunks(100)
left.zip(right)
rows.group_by(.type)
words.group(x => x)
```

Use `flat_map` when each input produces a collection and you want a single level. `enumerate` adds stable positions. `zip` stops according to the collection implementation's paired extent; do not use it as a relational join with missing-key semantics.

`group` and `group_by` are aliases and always require a key function. They return a list of `{key, values}` records. Inspect that value before serializing rather than relying on the pretty renderer as a wire format.

## Ordering and uniqueness

```text
values.sort()
rows.sort_by(.modified)
values.reverse()
values.uniq()
```

Direct ordering is defined for comparable scalars including numbers, strings, paths, sizes, durations, datetimes, times, and booleans. `sort_by` evaluates a key per row. `uniq` removes repeated structural values; it does not apply a projection.

## Eager versus stream transforms

Lists, tables, and ranges are finite and eager. A `stream<T>` is single-consumer and may be unbounded. Several names overlap, but stream transforms pull incrementally:

| Need | Finite value | Stream |
|---|---|---|
| Transform | `.map(f)` | `.map(f)` |
| Select | `.where(f)` | `.where(f)` |
| Prefix | `.take(n)` | `.take(n)` marks a bounded stream |
| Gather | already materialized | `.collect()` only if bounded |
| Side effect | `.each(f)` | `.each(f)` drives until end/cancel |
| Sort/sum | immediate | materializes; rejected if unbounded |

Do not call a materializing method on an endless source without bounding it:

```text
every(1s).take(5).collect()
```

See [Streams and channels](@/docs/streams-channels.md) for lifecycle and overflow rules.

## JSON

```text
let value = json.parse('{"name":"shoal","ready":true}')
value.name
json.stringify(value)
json.stringify(value, pretty: true)
```

`json.parse(str)` maps JSON arrays/objects/scalars to Shoal values. `json.stringify(value, pretty: bool = false)` maps supported Shoal values back into JSON. Resource-like values use tagged or preview representations according to the value encoder; use explicit projections for stable external APIs.

## YAML and TOML

```text
let y = yaml.parse("name: shoal\nready: true\n")
yaml.stringify(y)

let t = toml.parse('[package]\nname = "shoal"\n')
toml.stringify(t)
```

Each namespace currently exposes only `parse` and `stringify`. TOML serialization needs a record/table-compatible top level; scalar top-level values may be rejected by the TOML encoder.

## CSV

```text
let rows = csv.parse("name,count\nalpha,2\nbeta,10\n")
rows.where(.name == "beta")
csv.stringify(rows)
```

CSV parsing treats the first row as headers and every cell as a `str`; it does not infer numeric types. Convert explicitly:

```text
rows.map({ name: .name, count: .count.parse_int() })
```

`csv.stringify` accepts a table, a record, or a list of records. The first record's keys define output column order; absent fields in later rows become empty cells, and non-string values use their inline rendering.

## Math

Constants are fields; functions require parentheses:

```text
math.pi
math.e
math.sqrt(2)
math.pow(2, 10)
math.clamp(value, 0, 100)
```

Current constants are `pi`, `e`, `tau`, `inf`, `nan`, and `sqrt2`. Current functions are:

```text
sqrt cbrt sin cos tan asin acos atan atan2
ln log10 log2 log exp floor ceil round trunc abs sign
pow min max hypot clamp
```

Inputs may be `int` or `float`; results are `float`. Domain errors follow floating-point behavior, so operations can return `nan` or infinity. `clamp` rejects `lo > hi`.

## HTTP

```text
let response = http.get("https://example.test/api")
if response.ok {
  response.json
} else {
  { status: response.status, body: response.body }
}
```

Current calls are:

```text
http.get(url, headers?)
http.delete(url, headers?)
http.post(url, body, headers?)
http.put(url, body, headers?)
```

Headers can be passed positionally in the final record slot or as `headers:`. Header values render to text; a secret value is injected without exposing it through ordinary rendering.

The returned record has `status`, `ok`, `body`, `json`, and `headers`. HTTP non-2xx responses are ordinary records with `ok = false`, not evaluator errors. Transport/read failures raise `net_error`. The current implementation uses a 30-second global request timeout and a 64 MiB response-body cap.

Request bodies use the same finite-value serialization rules as `feed`: strings/bytes are direct, records/tables become compact JSON, and unsupported resource values fail.

## Operating-system facts

The `os` namespace exposes nullary functions:

```text
os.platform()
os.arch()
os.pid()
os.hostname()
os.username()
os.cpus()
os.uptime()
os.env()
```

`os.uptime()` returns a duration or null when unavailable. `os.env()` returns the evaluator's session environment record, not an untracked second view of the parent process.

## Resolved configuration

The `config` namespace reads the host-injected, already-layered snapshot:

```text
config.all()
config.get("history")
config.history
```

`config.<key>` is field projection; `config.get(key)` returns null when absent. The namespace does not independently walk the filesystem, so it cannot disagree with the configuration the host applied. A bare embedded evaluator with no injected snapshot returns an empty record/nulls.

## Serialize at the boundary

Keep data typed until a consumer actually needs bytes:


This avoids the classic shell failure mode where a human-oriented rendering becomes an accidental protocol.
