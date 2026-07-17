+++
title = "Namespace reference"
description = "Every member of Shoal's json, yaml, toml, csv, math, http, os, and config namespaces, plus environment and secret access."
weight = 320
template = "docs/page.html"

[extra]
eyebrow = "Standard library reference"
group = "Reference"
audience = "Shoal programmers and integration authors"
status = "All namespace dispatch arms checked against shoal-eval"
toc = true
+++

Shoal exposes eight unbound root names as namespaces: `json`, `yaml`, `toml`, `csv`, `math`, `http`, `os`, and `config`. A user binding with the same name shadows the namespace. Functions must be called; only math constants and projected configuration fields support bare field access.

```shoal
json.parse('{"ready":true}')
math.pi
os.platform()
config.history
```

## Complete member inventory

| Namespace | Constants/fields | Functions |
| --- | --- | --- |
| `json` | none | `parse`, `stringify` |
| `yaml` | none | `parse`, `stringify` |
| `toml` | none | `parse`, `stringify` |
| `csv` | none | `parse`, `stringify` |
| `math` | `pi`, `e`, `tau`, `inf`, `nan`, `sqrt2` | `sqrt`, `cbrt`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `ln`, `log10`, `log2`, `log`, `exp`, `floor`, `ceil`, `round`, `trunc`, `abs`, `sign`, `pow`, `min`, `max`, `hypot`, `clamp` |
| `http` | none | `get`, `delete`, `post`, `put` |
| `os` | none | `platform`, `arch`, `pid`, `hostname`, `username`, `cpus`, `uptime`, `env` |
| `config` | resolved top-level keys | `all`, `get` |

## JSON

### `json.parse`

```text
json.parse(text: str) -> value
```

Parses one JSON document and maps it into Shoal values:

| JSON | Shoal |
| --- | --- |
| null | `null` |
| boolean | `bool` |
| integer in range | `int` |
| other number | `float` |
| string | `str` |
| array | `table` when non-empty and every item is an object; otherwise `list` |
| object | ordered `record` |

```shoal
let doc = json.parse('{"name":"shoal","ports":[80,443]}')
doc.ports.map(x => x + 1)
```

Missing/wrong argument and malformed input raise `arg_error`.

### `json.stringify`

```text
json.stringify(value, pretty: bool = false) -> str
json.stringify(value, true) -> str
```

The second positional boolean and named `pretty: true` both request indentation. Values without a direct JSON representation degrade through the runtime conversion rules: paths and temporal values become display strings; outcomes and special values use their safe representation rather than exposing secret content.

```shoal
json.stringify({name: "shoal", ready: true})
json.stringify((ls .).out, pretty: true)
```

No value is `arg_error`; serialization failure uses `custom`.

## YAML

### `yaml.parse`

```text
yaml.parse(text: str) -> value
```

Parses YAML through a JSON-compatible intermediate representation and returns native Shoal values. As with JSON, a non-empty array made entirely of objects becomes a `table`; empty, scalar, or mixed arrays remain lists.

```shoal
yaml.parse('name: shoal\nready: true\n')
```

Non-string/missing input and malformed YAML are `arg_error`.

### `yaml.stringify`

```text
yaml.stringify(value) -> str
```

Serializes the JSON-compatible form of a Shoal value.

```shoal
yaml.stringify({services: [{name: "api", port: 8080}]})
```

No value is `arg_error`; serializer failure uses `custom`.

## TOML

### `toml.parse`

```text
toml.parse(text: str) -> value
```

```shoal
let manifest = toml.parse((cat "Cargo.toml").out.str())
manifest.package.name
```

Malformed TOML is `arg_error`.

### `toml.stringify`

```text
toml.stringify(value) -> str
```

TOML requires a table-like top-level value. A scalar or incompatible nested value raises `arg_error` with that constraint in the message.

```shoal
toml.stringify({package: {name: "demo", version: "0.1.0"}})
```

## CSV

### `csv.parse`

```text
csv.parse(text: str) -> table<record<str>>
```

The first row supplies headers. Every field remains a string; CSV namespace parsing does not infer numbers, booleans, dates, or paths.

```shoal
let rows = csv.parse("name,count\napi,3\nweb,2\n")
rows.map(r => {name: r.name, count: r.count.parse_int()})
```

Malformed CSV is `arg_error`.

### `csv.stringify`

```text
csv.stringify(table_or_records) -> str
```

Accepted inputs are a table, a list containing only records, or one record. Header order comes from the first record. Later missing keys become empty fields. Later extra keys not present in the first record are not emitted. Strings write as text; other values use compact inline rendering.

```shoal
csv.stringify([
    {name: "api", count: 3},
    {name: "web", count: 2},
])
```

A wrong receiver shape is `type_error`; writer errors use `custom`; an impossible UTF-8 result uses `utf8_error`.

## Math constants

Constants are fields, not calls:

| Member | Value |
| --- | --- |
| `math.pi` | π |
| `math.e` | Euler's number |
| `math.tau` | 2π |
| `math.inf` | positive infinity |
| `math.nan` | IEEE NaN |
| `math.sqrt2` | √2 |

```shoal
2.0 * math.pi
math.sqrt2 * math.sqrt2
```

Calling a constant is not the namespace contract. Accessing a function without `()` produces a `field_missing` teaching error.

## Math functions

Every math function accepts `int` or `float`, converts to floating point, and returns `float`.

### One-argument functions

| Function | Operation |
| --- | --- |
| `math.sqrt(x)` | square root |
| `math.cbrt(x)` | cube root |
| `math.sin(x)` | sine, radians |
| `math.cos(x)` | cosine, radians |
| `math.tan(x)` | tangent, radians |
| `math.asin(x)` | inverse sine |
| `math.acos(x)` | inverse cosine |
| `math.atan(x)` | inverse tangent |
| `math.ln(x)` | natural logarithm |
| `math.log10(x)` | base-10 logarithm |
| `math.log2(x)` | base-2 logarithm |
| `math.exp(x)` | eˣ |
| `math.floor(x)` | floor |
| `math.ceil(x)` | ceiling |
| `math.round(x)` | nearest integer-valued float |
| `math.trunc(x)` | truncate fractional part |
| `math.abs(x)` | absolute value |
| `math.sign(x)` | signum |

### Two-argument functions

| Function | Operation |
| --- | --- |
| `math.atan2(y, x)` | quadrant-aware arctangent |
| `math.log(x, base)` | logarithm in arbitrary base |
| `math.pow(x, exponent)` | floating exponentiation |
| `math.min(a, b)` | IEEE floating minimum |
| `math.max(a, b)` | IEEE floating maximum |
| `math.hypot(x, y)` | √(x²+y²) |

### Three-argument function

```text
math.clamp(x, lo, hi) -> float
```

`lo > hi` is `arg_error`. Missing arguments are `arg_error`; non-numbers are `type_error`. Floating-domain conditions follow the Rust/IEEE operation: for example, a negative square root produces NaN rather than a Shoal error.

```shoal
math.sqrt(81)
math.pow(2, 10)
math.clamp(cpu, 0, 100)
```

## HTTP

The HTTP namespace performs synchronous requests with a 30-second global timeout and a 64 MiB response-body read cap.

### Signatures

```text
http.get(url: str|path, headers: record = {}) -> response
http.delete(url: str|path, headers: record = {}) -> response
http.post(url: str|path, body = "", headers: record = {}) -> response
http.put(url: str|path, body = "", headers: record = {}) -> response
```

Headers may be supplied as `headers:` or in the method-specific final positional record.

```shoal
let response = http.get(
    "https://api.example.test/v1/items",
    headers: {Accept: "application/json"},
)

let created = http.post(
    "https://api.example.test/v1/items",
    json.stringify({name: "demo"}),
    headers: {"Content-Type": "application/json"},
)
```

### Response record

Every HTTP call returns:

| Field | Type | Meaning |
| --- | --- | --- |
| `status` | `int` | HTTP status code |
| `ok` | `bool` | true for 200–299 |
| `body` | `str` | decoded response body |
| `json` | value or `null` | parsed JSON when the trimmed body is valid JSON |
| `headers` | `record<str>` | response headers |

HTTP 4xx/5xx statuses are not raised as errors; inspect `.ok` or `.status`. Transport, timeout, and body-read failures raise `net_error`.

Request body serialization uses the same feedability rules as `.feed`: strings are UTF-8 bytes, bytes are raw, scalar data renders to text, and records/tables/lists become compact JSON. A path is a name, not file content; use `path.read` or `path.read_bytes`.

Header values accept strings, secrets, and other renderable values. A secret is intentionally permitted here for authentication header injection, but remains redacted in ordinary output.

Current limitations:

- no custom timeout argument;
- no streaming request or response body;
- no explicit redirect/TLS/proxy configuration surface;
- response body must decode as text for the returned record;
- repeated response header names collapse into one record key.

## OS

Every `os` member is a nullary function. Passing any positional or named argument is `arg_error`.

| Function | Result | Source |
| --- | --- | --- |
| `os.platform()` | `str` | Rust target OS name, such as `linux` or `macos` |
| `os.arch()` | `str` | Rust target architecture, such as `x86_64` or `aarch64` |
| `os.pid()` | `int` | current host process id |
| `os.hostname()` | `str` | libc `gethostname`, or `unknown` |
| `os.username()` | `str` | session `USER`, `LOGNAME`, `USERNAME`, then libc user database, then `unknown` |
| `os.cpus()` | `int` | available parallelism, at least 1 |
| `os.uptime()` | `duration|null` | monotonic-clock time since boot when available |
| `os.env()` | `record<str>` | evaluator session environment, UTF-8 entries only |

```shoal
{platform: os.platform(), arch: os.arch(), cpus: os.cpus()}
os.env().get("CI", "false")
```

`os.uptime()` is best effort. The current implementation uses libc and Unix clock APIs and is not a Windows implementation.

## Configuration

The host injects a snapshot of the already layered and validated configuration. The namespace does not independently walk the filesystem, so it cannot disagree with configuration the host applied.

### `config.all`

Despite its name, this is a nullary function:

```text
config.all() -> record
```

```shoal
config.all()
```

Calling `config.all` without parentheses is field access and reads a top-level key literally named `all`; absent means `null`.

### `config.get`

```text
config.get(key: str) -> value|null
```

This performs one top-level lookup. It does not parse dotted paths.

```shoal
config.get("history")
config.get("missing") ?? {}
```

### Projected fields

Any top-level key can be read as a field:

```shoal
config.version
config.history
config.render
```

A missing top-level key returns `null`, unlike ordinary record field access, which raises `field_missing`.

If no host snapshot was injected, `config.all()` is `{}` and every lookup is `null`. The normal `shoal` interactive, script, and `-c` host inject the resolved configuration; embedded evaluator tests and some kernel-less integrations may not.

See [Configuration and prompt](@/docs/configuration-prompt.md) for every schema key and wiring status.

## Environment root value

`env` is also a canonical builtin command, but `env.NAME` and assignment have dedicated language behavior:

```shoal
env.HOME
env.BUILD_MODE = "release"
env.BUILD_MODE
```

Reads return `str|null`. Writes update the evaluator's session environment and therefore later child processes. The root environment is not one of the eight namespace-dispatch names; it is evaluator-provided state.

## Secret access

`secret` is another special root receiver, with exactly one implemented member:

```text
secret.get(name: str) -> secret
```

```shoal
let token = secret.get("api-token")
http.get(url, headers: {Authorization: token})
```

The value renders redacted and cannot be fed as generic stdin. Store access errors use `permission`; an unknown name uses `not_found`; a non-UTF-8 stored value uses `utf8_error`; wrong arguments use `arg_error`.

## Shadowing and diagnostics

A lexical binding wins over a namespace:

```shoal
let json = {parse: "shadowed"}
json.parse
```

Namespace functions accessed as fields produce a `field_missing` message that tells you to call them. Unknown called members also use `field_missing`:

```shoal
json.parse     # function must be called
json.decode(x) # unknown method
```

Use a local binding only when shadowing is deliberate; otherwise choose names such as `json_doc`, `http_response`, or `os_info`.
