+++
title = "Value types and every method"
description = "The complete runtime value and method surface, organized by receiver type with signatures, return shapes, consumption rules, and errors."
weight = 330
template = "docs/page.html"

[extra]
eyebrow = "Standard library reference"
group = "Reference"
audience = "Shoal programmers and tooling authors"
status = "Method inventory checked against shoal-value and evaluator-hosted dispatch"
toc = true
+++

Shoal's method library is receiver-oriented. Some methods are pure value transformations; others are explicit sinks; several evaluator-hosted methods need filesystem, process, event-bus, or terminal access. This chapter inventories both layers.

## Runtime type inventory

| Runtime name | Representation and role |
| --- | --- |
| `null` | absence |
| `bool` | `true` or `false`; no general truthiness |
| `int` | signed 64-bit integer |
| `float` | IEEE 64-bit floating point |
| `str` | Unicode UTF-8 text |
| `path` | platform path bytes through `PathBuf`; not synonymous with file content |
| `glob` | pattern plus origin directory and hidden-match flag |
| `regex` | compiled regular expression |
| `size` | unsigned byte quantity |
| `duration` | signed nanoseconds |
| `datetime` | zoned calendar instant |
| `time` | time of day |
| `bytes` | resident bytes or lazy content-addressed bytes |
| `list` | ordered heterogeneous values |
| `record` | insertion-ordered string keys to values |
| `table` | ordered records with tabular rendering |
| `range` | integer interval |
| `stream` | single-consumer bounded or live value source |
| `error` | caught evaluator failure |
| `outcome` | command status, raw output, parsed value, and metadata |
| `task` | asynchronous computation/job handle |
| `closure` | function/lambda with captured environment |
| `command` | parsed command reference used by aliases |
| `secret` | opaque secret value with redacted presentation |

## Access before methods

`value.field` first attempts direct field access. For most non-record values, a missing field falls back to calling a zero-argument method, making `name.upper` and `path.read` useful projection shorthands. Records are deliberately strict: a missing record field does not fall through, because user data might collide with method names. Call a record method with parentheses.

```shoal
["a", "b"].map(.upper)   # method projection
(ls .).map(.name)         # path/record field projection
row.keys                  # looks for a real field named keys; may fail
row.keys()                # record method
```

Optional `?.` protects only a `null` receiver. It does not turn an absent record field into null.

## Methods available on every receiver

### `tap` and `also`

```text
value.tap(f) -> value
value.also(f) -> value
```

These aliases call `f(value)`, discard the callback result, and return the original receiver unchanged.

```shoal
let result = expensive()
    .tap(x => x.json().append("trace.jsonl"))
    .map(transform)
```

If the callback errors, the method errors. The receiver is cloned for the callback, but resource identity/single-consumption rules still apply to streams and tasks.

### `json`

```text
value.json() -> str
```

Returns compact JSON from Shoal's generic value encoder. This is a representation helper, not a stable protocol schema for tasks, streams, errors, or outcomes. Secrets remain redacted. A content-addressed bytes value used as the top-level receiver is loaded only within the 16 MiB eager wall; a larger one raises `cas_materialization_limit`. The same value nested inside another record/table becomes a bounded preview/ref object without loading.

### `save` and `append`

```text
value.save(path: str|path) -> value
value.append(path: str|path) -> value
```

Strings and resident bytes write verbatim. Other values write compact JSON. `save` truncates/replaces; `append` appends. Both return the original receiver.

```shoal
report.save("report.json")
"next line\n".append("events.log")
```

Security boundary: these value methods write through the evaluator's injected `Fs` port, so an embedding host can observe or deny them. The default production port is still `StdFs`; port routing does not by itself apply Leash policy or install journal undo.

### `feed`

```text
value.feed(command) -> outcome
```

This is evaluator-hosted rather than a pure method. It serializes the receiver and starts a child with those bytes as stdin. The complete serialization table appears under [Feedability](@/docs/value-methods-reference.md#feedability-by-type).

### `pick`

```text
value.pick(prompt: str = "> ", multi: bool = false) -> value|list
```

Opens an alternate-screen fuzzy picker. Lists/tables supply their items; a record or scalar is treated as one selectable item. Single mode returns the selected value or `null`; multi mode returns a list.

It requires both stdin and stdout to be terminals. Non-interactive use is `arg_error`; cancellation raises `custom`.

## Collection receiver set

The eager collection core accepts `list`, `table`, and `range`. Many operations convert a table row into a record and return an ordinary list rather than preserving table identity.

### Complete collection method index

| Method | Signature | Result |
| --- | --- | --- |
| `len`, `count` | `()` | integer element count |
| `is_empty` | `()` | boolean |
| `first` | `()` / `(n)` | item/null or list |
| `last` | `()` / `(n)` | item/null or list |
| `collect` | `()` | list/range materialization; table remains table |
| `stream` | `()` | finite stream |
| `tee` | `(n = 2)` | list of streams |
| `map` | `(item => value)` | list |
| `reduce`, `fold` | `(initial, (acc,item)=>acc)` | final accumulator |
| `where`, `filter` | `(item => bool)` | list |
| `each` | `(item => any)` | `null` |
| `any`, `all` | `(item => bool)` | bool |
| `find` | `(item => bool)` | item or null |
| `flat_map` | `(item => collection)` | flattened list; eager output is limited to 16,384 values / 16 MiB |
| `sort` | `()` or `(item => key)` | list |
| `sort_by` | `(item => key)` | list |
| `reverse` | `()` | list |
| `uniq` | `()` | list |
| `sum` | `()` | accumulated value; empty is integer 0 |
| `min`, `max` | `()` | item or null |
| `flatten` | `()` | one-level flattened list; eager output is limited to 16,384 values / 16 MiB |
| `enumerate` | `()` | list of `[index, item]` |
| `skip`, `take` | `(n)` | list |
| `chunks` | `(positive_n)` | list of lists |
| `zip` | `(other_collection)` | list of `[left,right]` |
| `group`, `group_by` | `(item => key)` | list of `{key,values}` records |
| `join` | `(separator = "")` | string; elements must be strings |
| `contains` | `(item)` | bool |
| `get` | see below | only list+int, not table/range |

Methods that produce an eager collection (`first(n)`, `last(n)`, `map`, `where`, `sort`,
`reverse`, `uniq`, `flat_map`, `flatten`, `enumerate`, `skip`, `take`, `chunks`, `zip`, `group`,
record projections, and collection `tee`) share a 16,384-value / 16 MiB retained-state wall.
They raise `collection_materialization_limit` before admitting the next result. Collection `tee`
stores one admitted replay vector shared by all forks; it does not clone the complete vector for
each fork.

### Size and endpoints

```shoal
items.len()
items.count()
items.is_empty()
items.first()
items.first(3)
items.last()
items.last(3)
```

`first()`/`last()` return one item or `null`. With a count they always return a list. Counts must be non-negative integers.

`len` also works on strings, bytes, and records. A range length is computed without first materializing it.

### `collect`

```shoal
(1..5).collect()   # [1,2,3,4]
```

On a list or table, eager `collect()` returns the receiver unchanged. It does not turn a table into a list. Streams have a separate consuming `collect()` described later.

### `stream`

```shoal
[1, 2, 3].stream()
(ls .).out.stream()
"a\nb\n".stream()
```

Collections stream their elements. A string or bytes value streams decoded lines; bytes use lossy UTF-8. The returned stream is bounded and single-consumer.

### `tee`

```shoal
let forks = items.tee(2)
let left = forks[0]
let right = forks[1]
```

For an eager collection, each fork replays a cloned list. `n` must be positive. Live stream tee behavior is different and described under streams.

### `map`

```text
collection.map(f) -> list
```

Calls `f(item)` in order and collects its result. Errors stop iteration.

```shoal
(ls .).map(row => {name: row.name, kib: row.size / 1kib})
```

### `reduce` and `fold`

```text
collection.reduce(initial, f) -> value
collection.fold(initial, f) -> value
```

These aliases perform a left fold: `acc = f(acc, item)` in input order.

```shoal
[1, 2, 3, 4].reduce(0, (acc, n) => acc + n)
```

### `where` and `filter`

```text
collection.where(predicate) -> list
collection.filter(predicate) -> list
```

The predicate must return `bool` or an outcome condition. Selected original values are preserved.

```shoal
(ls .).where(.type == "file")
```

### `each`

```text
collection.each(f) -> null
```

Calls `f(item)` for side effects and discards every result. It is eager and stops at the first error.

```shoal
paths.each(p => p.display().append("paths.log"))
```

### `any` and `all`

Both short-circuit and require condition results.

```shoal
rows.any(.status >= 500)
rows.all(.ready)
```

`any` on empty is false. `all` on empty is true.

### `find`

Returns the first item whose predicate succeeds, or `null`.

```shoal
services.find(.name == "api")
```

### `flat_map`

The callback must return a list, table, or range. Its elements are appended one level into the result.

```shoal
projects.flat_map(.members)
```

### `sort` and `sort_by`

```shoal
[3, 1, 2].sort()
rows.sort(.name)
rows.sort_by(.modified)
```

`sort(f)` is the projection alias of `sort_by(f)`. Directly comparable compatible types are int/float, string, path, size, duration, datetime, time, and bool. Heterogeneous or otherwise incomparable keys raise `type_error`; comparison errors are not silently converted to equality.

### `reverse`

Collections return a reversed list. A string receiver returns its Unicode characters in reverse order as a string.

### `uniq`

Keeps the first occurrence of each structurally equal value using a stable O(n²) scan. It does not sort and accepts no projection.

### `sum`

Starts with the first item and applies `+`, preserving homogeneous value types such as float, size, or duration. An empty collection returns integer `0`.

```shoal
(ls .).map(.size).sum()
```

`sum` takes no callback. Use `.map(f).sum()`.

### `min` and `max`

Return the selected item or `null` for empty. They take no callback; project with `.map(f)` first if you want the key itself, or sort records when you need the whole row.

### `flatten`

Each outer item must itself be a list/table/range. Only one level is flattened.

### `enumerate`

Returns `[[0,item0], [1,item1], ...]`. It does not create `{index,value}` records.

### `skip` and `take`

```shoal
items.skip(10)
items.take(10)
```

Return lists. Counts are non-negative. On strings these methods slice by Unicode scalar count and return a string.

### `chunks`

Splits into lists of at most `n`. Zero is `arg_error`.

### `zip`

Pairs elements until either side ends. Both receivers must be eager collection types.

### `group` and `group_by`

```shoal
rows.group_by(.type)
words.group(x => x.lower())
```

Both require a key function and return an ordinary list in first-key-seen order:

```text
[
  {key: KEY, values: [ITEM...]},
  ...
]
```

Keys use structural equality.

### `join`

Every element must be a string. The separator defaults to the empty string.

```shoal
["a", "b", "c"].join(",")
```

### `contains`

For a list/table/range it performs structural item membership. For a string it requires a string substring. For a record it requires a string key.

### `get`

The implemented signatures are exactly:

```text
list.get(index: int, default = null) -> value
table.get(index: int, default = null) -> record | value
range.get(index: int, default = null) -> int | value
record.get(key: str, default = null) -> value
```

Negative sequence indexes count from the end; out-of-range returns the default. Table lookup returns
the selected row as a record. Range lookup computes the integer directly without materializing the
range.

## String methods

| Method | Signature | Meaning |
| --- | --- | --- |
| `len`, `count` | `()` | Unicode scalar count |
| `is_empty` | `()` | no characters |
| `lines` | `()` | list of logical lines, CR removed at line end |
| `words` | `()` | Unicode whitespace split |
| `chars` | `()` | list of one-scalar strings |
| `trim` | `()` | trim surrounding whitespace |
| `upper` | `()` | Unicode uppercase |
| `lower` | `()` | Unicode lowercase |
| `split` | `(separator: str)` | literal split into list |
| `starts_with` | `(prefix: str)` | bool |
| `ends_with` | `(suffix: str)` | bool |
| `contains` | `(substring: str)` | bool |
| `replace` | `(str|regex, replacement: str)` | replace all |
| `matches` | `(regex)` | list of full match strings |
| `match` | `(regex)` | first full match string or null |
| `parse_int` | `()` | integer or `arg_error` |
| `parse_float` | `()` | float or `arg_error` |
| `take`, `skip` | `(n)` | substring by scalar count |
| `reverse` | `()` | reversed scalar order |
| `stream` | `()` | bounded stream of lines |
| `str`, `display` | `()` | identity |

`split`, `starts_with`, and `ends_with` require their text argument; a missing argument is not treated as `""`.

```shoal
" alpha beta ".trim().words()
"v1.2.3".starts_with("v")
"a-b-c".replace("-", "/")
"abc-123".replace(re"([a-z]+)-([0-9]+)", "$2:$1")
```

Regex replacement expands `$1` and named captures. `.matches()` returns every non-overlapping full match, not capture records. `.match()` returns only the first full match.

Eager string partitions and regex matches share the 16,384-value / 16 MiB collection wall.
Concatenation, `join`, Unicode case conversion, and replacement share a 16 MiB output wall. They
raise `collection_materialization_limit` or `string_materialization_limit` before retaining the
next item/chunk; use a stream or chunked processing for larger data.

## Numeric methods

`int` and `float` implement:

| Method | Signature | Result |
| --- | --- | --- |
| `abs` | `()` | same numeric kind; min-int overflow errors |
| `round` | `(decimal_places = 0)` | int unchanged; rounded float |
| `floor` | `(decimal_places = 0)` | int unchanged; floored float at precision |
| `ceil` | `(decimal_places = 0)` | int unchanged; ceiling float at precision |
| `str`, `display` | `()` | canonical number text |

Decimal places must be a non-negative integer. A scaling overflow leaves an already finite value unchanged rather than producing a spurious infinity/NaN.

```shoal
(-5).abs()
3.14159.round(2)
3.14159.floor(3)
```

The richer transcendental surface is under the [math namespace](@/docs/namespaces-reference.md#math-functions).

## Boolean conversion

`bool.str()` and `bool.display()` return `"true"` or `"false"`. Booleans also support generic JSON/save/append/feed/tap operations. They do not have numeric methods.

## Path fields and methods

| Member | Call form | Result |
| --- | --- | --- |
| `name` | field or `()` | basename string or null |
| `stem` | field or `()` | basename without final extension, or null |
| `ext` | field or `()` | extension string or null |
| `parent` | field or `()` | parent path or null |
| `join` | `(str|path)` | appended path |
| `abs` | `()` | absolute against session cwd |
| `read` | field or `()` | UTF-8 string |
| `read_bytes` | field or `()` | raw bytes |
| `lines` | field or `()` | UTF-8 line list |
| `exists` | field or `()` | bool |
| `is_dir` | field or `()` | bool |
| `is_file` | field or `()` | bool |
| `size` | field or `()` | size |
| `modified` | field or `()` | datetime or null |
| `str` | `()` | strict UTF-8 conversion |
| `display` | `()` | lossy display conversion |

```shoal
let p = path("src/main.rs")
{name: p.name, parent: p.parent, bytes: p.size, changed: p.modified}
p.parent.join("lib.rs")
p.read.lines()
```

Relative filesystem methods resolve against the session cwd. `read` and `read_bytes` stop at 16 MiB;
`lines` reads incrementally and stops at 16 MiB of input, 16,384 lines, or 16 MiB of retained line
values. Crossing any eager-read wall raises `path_read_limit` with a `.stream()`/`head` hint rather
than partially returning data. `read` and `lines` use strict UTF-8 and raise `utf8_error`;
`read_bytes` does not. `exists`/`is_dir`/`is_file` convert metadata errors to false, while `size`
raises. `modified` may return null when the timestamp cannot be represented.

A bare path is not feedable because it denotes a name, not contents:

```shoal
p.read.feed(^consumer)
p.read_bytes.feed(^consumer)
```

## Bytes methods

Resident bytes implement `len`, `count`, `is_empty`, `stream`, `str`, `display`, JSON/save/append/feed, and universal methods.

- `.str()` requires valid UTF-8 and raises `utf8_error` otherwise.
- `.display()` decodes lossily.
- `.stream()` decodes lossily and yields logical lines.
- `.len()` counts bytes, not characters.

### Lazy content-addressed bytes

Large command capture may spill into the journal CAS and retain only a resident preview. It still reports runtime type `bytes`, with additional host methods:

| Method | Behavior |
| --- | --- |
| `len`, `count` | true total length from metadata, no load |
| `is_empty` | metadata-only |
| `ref` | `val:blake3:HASH`, no load |
| `load`, `bytes` | explicitly load and return resident bytes |
| `stream` | lazily decode logical lines; one line may be at most 1 MiB |
| `save`, `append` | copy incrementally through the CAS reader and filesystem port |
| any other bytes method | load only when declared length is at most 16 MiB, then dispatch |

An implicit resident operation above the wall raises `cas_materialization_limit` before opening the
blob. An oversized unframed line raises `stream_line_limit`. `.load()` and `.bytes()` remain the
deliberate escape hatch when the caller truly wants the full allocation.

Writing the short ref as a string value in the same journal-aware evaluator can resolve it for subsequent methods. Unknown/unavailable refs raise `not_found` or `io_error`.

If capture itself exceeded the hard spill cap, `truncated` metadata means even CAS content is only a prefix; loading cannot reconstruct bytes never stored.

## Record methods

| Method | Signature | Result |
| --- | --- | --- |
| `keys` | `()` | ordered list of string keys |
| `values` | `()` | ordered list of values |
| `items` | `()` | ordered list of `[key,value]` lists |
| `set` | `(key: str, value)` | new record |
| `merge` | `(other: record)` | new record, right wins |
| `get` | `(key: str, default = null)` | value/default |
| `contains` | `(key: str)` | bool |
| `len`, `count`, `is_empty` | `()` | size predicates |

Records are immutable values. `set` does not mutate the receiver. Replacing an existing key keeps its position. `merge` overlays the right record; existing key positions remain and new keys append.

```shoal
let base = {a: 1, b: 2}
base.set("b", 20).merge({c: 3})
base.items()
```

Record field data wins over method shorthand. Use parentheses for these methods.

## Table-specific notes

A table is semantically an ordered record collection, but method outputs are usually lists:

- `map`, `where`, `filter`, `sort`, `reverse`, and grouping return `list`;
- `collect()` returns the table unchanged;
- `len` counts rows;
- `get(index, default = null)` returns a row record or the default;
- `table[index]` is also not implemented by strict index access;
- `.first()`, `.last()`, `.find()`, and `.take(n)` remain useful for selection by shape or predicate.

Strict bracket indexing and forgiving `.get()` deliberately remain different operations.

## Range methods

Ranges are compact. `1..5` yields 1,2,3,4; `1..=5` includes 5. Eager collection transforms, JSON conversion, and `collect` may materialize at most 16,384 integers and otherwise raise `range_materialization_limit`. `.stream()` and range expansions returned from stream `flat_map` iterate lazily, so use `.take(n)` before collecting a much larger range. Length and `.get(index, default = null)` remain non-materializing; an exact length outside Shoal's signed integer domain raises `range_length_overflow`.

## Glob fields and methods

| Member | Meaning |
| --- | --- |
| `.pattern` / `.pattern()` | source pattern string |
| `.expand()` | sorted `list<path>` |
| any collection method | expand, then dispatch on list |

```shoal
let rust = glob("crates/**/*.rs")
rust.pattern
rust.where(.name.ends_with("test.rs"))
```

The glob stores its creation cwd, so later session navigation does not change the expansion root. Passing a glob as a command argument expands at the callee boundary. `glob(..., hidden: true)` includes hidden entries. The accepted `follow:` constructor argument is currently not stored/applied.

## Regex values

Regex values are consumed by string `.match`, `.matches`, and `.replace`. There is no public regex field for its source and no capture-record method today. Generic rendering/JSON uses a safe representation. Feeding a regex is `feed_error`.

## Temporal fields

### Datetime

Direct fields:

```text
.year .month .day .hour .minute .second
```

Each returns an integer. Datetime values otherwise use comparison/operators and generic JSON/save/append/feed/tap.

### Duration

Direct relative-anchor fields:

```text
duration.ago
duration.from_now
```

They use the evaluator clock and return datetime, with `overflow` on an unrepresentable instant. Duration values can be added/subtracted where the operator table permits and summed in collections.

### Time and size

These quantity scalars use comparison/arithmetic where defined and generic JSON/save/append/feed. They do not implement `.str()`; interpolate for presentation:

```shoal
"limit={10mb} timeout={250ms} at={09:30}"
```

## Error fields

A caught `error` exposes exactly:

| Field | Type |
| --- | --- |
| `.code` | `str` |
| `.msg` | `str` |
| `.hint` | `str|null` |
| `.stderr` | `str|null` |
| `.status` | `int|null` |

No `.span` field is exposed, even though the internal diagnostic may carry one.

```shoal
try {
    risky()
} catch err {
    {code: err.code, message: err.msg, status: err.status}
}
```

## Outcome fields and forwarding

| Field | Type | Behavior |
| --- | --- | --- |
| `.status` | `int|null` | exit code |
| `.ok` | `bool` | success according to accepted codes |
| `.signal` | `str|null` | terminating signal |
| `.dur` | `duration` | elapsed time |
| `.pid` | `int` | child pid; builtin outcomes use 0 |
| `.cmd` | `str` | command head/description |
| `.stdout` | `bytes` | raw stdout, possibly CAS-backed |
| `.stderr` | `bytes` | raw stderr |
| `.out` | structured value or bytes/string | raises `cmd_failed` when non-ok |
| `.err` | stderr bytes | also raises `cmd_failed` when non-ok in current field path |

Unknown fields and methods on a successful outcome forward to `.out`:

```shoal
ls.where(.type == "file")
(stat Cargo.toml).size
```

On a failed outcome, `.out`, `.err`, and forwarded access raise `cmd_failed`. `.stdout`, `.stderr`, `.status`, `.ok`, and metadata remain directly readable.

Outcome method `.stdout()` and `.stderr()` are also accepted. Other methods forward to `.out`. Use explicit fields in protocol-oriented code to make failure behavior obvious.

## Task methods

| Method | Result | Effect |
| --- | --- | --- |
| `await`, `wait` | task result or its error | block until complete |
| `cancel` | `null` | set cancel state and run hooks |
| `is_done` | `bool` | completion query |
| `suspend` | same task | set suspended state and run process hook |
| `resume` | same task | clear state and run resume hook |
| `is_suspended` | `bool` | suspension query |

```shoal
let t = spawn { slow_work() }
if (!t.is_done()) { t.cancel() }
t.await()
```

Task handles compare by identity. Awaiting observes the stored completion. Local evaluator task
hooks and raw kernel task control can both signal owned process groups, but they use different task
identities; kernel evaluator-only work still returns `TASK_CONTROL_UNAVAILABLE`.

## Stream methods

A stream is single-consumer. Calling a lazy combinator moves its source into a new stream. Driving a sink consumes it. Reusing the original handle after either operation raises `stream_consumed`.

### Lazy combinators

| Method | Signature | Emission |
| --- | --- | --- |
| `map` | `(item => value)` | transformed item |
| `where`, `filter` | `(item => bool)` | selected original item |
| `scan` | `(initial, (acc,item)=>acc)` | each new accumulator |
| `flat_map` | `(item => collection|stream-compatible)` | each returned shape drained sequentially |
| `take` | `(n)` | first n, then end; makes bounded |
| `take_until` | `(predicate|other_stream)` | until predicate/other stream |
| `dedupe` | `()` | remove adjacent duplicates |
| `distinct` | `()` | remove all previously seen duplicates |
| `debounce` | `(duration)` | quiet-period emission |
| `throttle` | `(duration)` | rate-limited emission |
| `window` | `(positive_count|duration)` | list windows |
| `buffer` | `(capacity = 1)` | eager lossless queue; `0` is a rendezvous |
| `enumerate` | `()` | `[index,item]` |
| `merge` | `(stream)` | fair interleave; round-robin while both are ready |
| `zip` | `(stream)` | positional pairs, at most one pending item per side |

```shoal
every(1s)
    .map(t => {at: t, kind: "tick"})
    .take(10)
    .collect()
```

Durations must be non-negative. Window count must be positive. `merge`/`zip` require a stream.

### Consuming sinks

| Method | Result | Notes |
| --- | --- | --- |
| `each(f)` | `null` | run callback per item |
| `collect()` | list | requires bounded metadata; capped at 16,384 values / 16 MiB |
| `save(path)` | path | appends one encoded line per item |
| `append(path)` | path | same implementation as stream `save` today |
| `tee(n=2)` | list of streams | 1–64 forks; bounded replay or live shared queues |
| `into(channel)` | `null` | emit every item to named channel |
| `render()` | `null` | drive items to evaluator statement sink |

Stream `save` and `append` both open the file in append mode today; neither truncates. Strings/bytes become their bytes plus newline; other items become compact JSON plus newline.

Like value save, stream file sinks resolve relative paths against the evaluator cwd and open once
through the injected `Fs` port. A recording or denying adapter can therefore observe or refuse the
write, though these sinks still bypass journal undo and the production default remains `StdFs`.

For a bounded stream, unfamiliar eager methods such as `.sort()`, `.sum()`, or `.len()` first collect up to the shared 16,384-value / 16 MiB wall and then dispatch to collection logic. Exceeding it raises `stream_collect_limit`. For a live/unbounded stream this raises `stream_unbounded`; bound it explicitly with `.take(n)` or `.take_until(...)`.

`.feed(command)` incrementally pumps a finite or live stream into captured child stdin. Ordinary
values are line-framed; bytes and outcome output remain raw. The bounded stdin path holds 16 chunks
of at most 64 KiB, applies lossless backpressure, and stops on cancellation, child exit, closed
stdin, or a serialization/upstream error. Buffer and feed pumps share a maximum of 64 active pumps.

### Live `tee`

Bounded streams materialize once within the collection walls and create independent replay streams. A live stream uses one shared upstream with a queue capped at 64 items and 1 MiB per fork. If a fork is not driven, its queue fills, or a value exceeds the byte wall, later values for that fork are dropped and counted. The fork subsequently receives an in-order `{marker: "stream_gap", reason: "tee_overflow", dropped: n, from_seq: null, to_seq: null}` record once space is available or the queue drains; overflow does not raise. All forks still obey single-consumer semantics, and `n > 64` is rejected before source consumption.

## Channel handle methods

`channel(name)` returns a one-field record recognized specially by the evaluator. It implements:

| Method | Signature | Result |
| --- | --- | --- |
| `emit` | `(value)` | `null` |
| `events` | `(since: int?)` | stream of event records |
| `latest` | `()` | latest payload or null |
| `take` | `(timeout: duration?)` | next future payload |

Event stream items are `{channel, seq, ts, payload}`. Each subscriber holds at most 256 deliveries;
overflow and stale cursors yield explicit `stream_gap` records with dropped counts and sequence
ranges. `.events()` queues retained ring entries before going live; `.take()` subscribes only to
future events. Channel specifics are in [Streams and channels](@/docs/streams-channels.md).

## Closure, command, and secret values

These resource/capability-like types intentionally have no dedicated general method set beyond universal presentation/tap/save methods:

- a closure is invoked with call syntax;
- a command reference is invoked as a command/callable;
- a secret is passed only through explicitly supported secure injection paths.

Generic `.save()`/`.json()` on a secret writes only the redacted/tagged representation. `.feed()` on secret, closure, command, task, error, glob, or regex is rejected.

## Feedability by type

| Receiver | Bytes sent to stdin |
| --- | --- |
| `str` | UTF-8 verbatim, no added newline |
| resident/lazy `bytes` | raw full bytes |
| `int`, `float`, `bool`, `size`, `duration`, `datetime`, `time` | inline decimal/display text, no newline |
| `list<str>` | each string plus newline, including final newline |
| other `list`, `record`, `table` | compact JSON |
| `outcome` | encoded structured `.out`; raw stdout for ordinary text |
| `path` | rejected: name is not content |
| `stream` | incrementally serialized items; ordinary values line-framed, bytes/outcomes raw |
| `secret` | `feed_error` |
| `task`, `closure`, `error`, `glob`, `regex` | `feed_error` |
| `null`, `command`, other unsupported | `type_error` |

```shoal
"abc".feed(^wc -c)
["a", "b"].feed(^wc -l)
{name: "api"}.feed(^jq .name)
path("input").read_bytes.feed(^consumer)
```

## Conversion matrix

### `.str()`

Accepted:

- string identity;
- path only when its bytes are valid UTF-8;
- bytes only when valid UTF-8;
- int, float, and bool canonical rendering.

Other types raise `type_error` with an interpolation hint.

### `.display()`

Same as `.str()`, except path/bytes use lossy UTF-8. It is not a universal “format anything” method; use interpolation for arbitrary values.

### Parse methods

Only strings implement `.parse_int()` and `.parse_float()`. There is no general `.parse_bool`, `.parse_duration`, or `.parse_size`; use literals, typed function/adapter word coercion, or explicit format parsing as appropriate.

## Method errors and suggestions

Unknown methods raise `field_missing` and may include a receiver-aware suggestion. Completion tables are harvested from the method dispatcher but currently have known over-approximations (`get` for table/range) and omit dynamic/evaluator-hosted surfaces such as stream combinators, `pick`, glob methods, channel methods, and CAS-specific methods.

Do not use completion presence as a runtime capability test. The definitive behavior is the tables in this chapter and a caught error during preview development.
