# shoal — configuration reference

**Crate:** `shoal-config`. Companion to `docs/TDD.md`/`docs/CONTRACTS.md`; same rule as those
files: everything here is a decision the crate actually implements and tests, not aspiration.

`shoal-config` owns discovery, layering, validation, and the typed [`Config`] model. It does
**not** own every subsystem's runtime behavior — see [§6](#6-what-actually-consumes-each-key-today)
for exactly which keys the `shoal` binary reads today vs. which are schema-only, ready for a
consumer to wire up.

## 0. Quick start

Drop this at `~/.config/shoal/shoal.toml` and shoal picks it up on the next launch, no restart
of anything else required:

```toml
version = 1

[prompt]
template = "{cwd} $"

[history]
max_entries = 20000
dedup = true

[editor]
bracketed_paste = true

[aliases]
gs = "git status"
gl = "git log --oneline -20"

[env]
EDITOR = "hx"
```

A typo like `[historyy]` or `max_entriess` never silently vanishes — you get a warning naming the
exact key and, if one is close, what you probably meant (§4).

## 1. Discovery & layering

Four layers, lowest to highest precedence. Each layer **deep-merges** over the ones below it —
setting one key in a higher layer never blanks out sibling keys left unset in a lower one; only
the exact keys a layer sets are overridden.

| Order | Layer | Location | Notes |
|---|---|---|---|
| 1 (lowest) | system | `/etc/shoal/shoal.toml` | machine-wide default |
| 2 | user | `$XDG_CONFIG_HOME/shoal/shoal.toml`, else `~/.config/shoal/shoal.toml` | your personal config |
| 3 | project | nearest `.shoal.toml` walking **up** from `$cwd` to the filesystem root | one file wins ("nearest wins" — same rule `shoal-reef` uses for `.reef.toml`, REEF.md §1) |
| 4 (highest) | env | `NO_COLOR` and `SHOAL_*` (§3) | live process environment |

A missing file at any layer is not an error — every layer is optional, and `Config::default()` is
itself a fully usable configuration. Only a file that **exists but can't be read or parsed** is a
hard error (§4).

The project layer is a single **nearest** `.shoal.toml`, not every `.shoal.toml` from the
filesystem root down to `$cwd` — a subdirectory's `.shoal.toml` fully shadows one further up, it
doesn't merge with it. If you want inheritance between a monorepo root and a subproject, `include`
via a shared file convention on top (not built in today).

Rust API: [`LoadOptions::discover(cwd)`] builds this four-way plan; [`load(&options)`] executes it
and returns a [`Loaded { config, warnings, sources }`]. `sources` lists exactly which files were
found and merged, in precedence order — useful for a `shoal doctor`-style "here's what I actually
read" report.

## 2. Precedence example

```toml
# system: /etc/shoal/shoal.toml
[history]
enabled = true
max_entries = 1000

# user: ~/.config/shoal/shoal.toml
[history]
max_entries = 10000

# project: ./.shoal.toml
[history]
max_entries = 50000
```

Result: `history.enabled = true` (only the system layer ever set it, and nothing overrode it),
`history.max_entries = 50000` (project wins — it's the highest layer that touched the key).
`SHOAL_HISTORY_MAX_ENTRIES=5` in the environment would win over all three.

## 3. Environment overrides

Env overrides only apply to **scalar leaves** (bool/int/string) — not to arrays, tables, or maps
(`aliases`, `env`, `adapters.dirs`, `history.ignore`, `editor.keybindings`, `reef.*`): those are
config-file-only. Each override is an explicit, individually documented variable (not a generic
`SHOAL_SECTION_FIELD` name-splitting scheme — several field names already contain underscores,
e.g. `max_entries`, so an automatic split would be ambiguous).

| Variable | Key | Type |
|---|---|---|
| `NO_COLOR` | `render.color` | presence-only; **any** value (including empty) forces `false`, and it wins over `SHOAL_RENDER_COLOR` too — the one no-color.org (<https://no-color.org>) rule nothing else is allowed to undo |
| `SHOAL_PROMPT_TEMPLATE` (alias: `SHOAL_PROMPT`) | `prompt.template` | string |
| `SHOAL_HISTORY_ENABLED` (alias: `SHOAL_HISTORY`) | `history.enabled` | bool |
| `SHOAL_HISTORY_MAX_ENTRIES` | `history.max_entries` | non-negative int |
| `SHOAL_HISTORY_FILE` | `history.path` | string (path) |
| `SHOAL_HISTORY_DEDUP` | `history.dedup` | bool |
| `SHOAL_RENDER_COLOR` | `render.color` | bool |
| `SHOAL_RENDER_WIDTH` | `render.width` | non-negative int |
| `SHOAL_EDITOR_MODE` | `editor.mode` | string (`emacs`\|`vi`) |
| `SHOAL_EDITOR_BRACKETED_PASTE` | `editor.bracketed_paste` | bool |
| `SHOAL_EDITOR_KEY_TIMEOUT_MS` | `editor.key_timeout_ms` | non-negative int |
| `SHOAL_KERNEL_ENABLED` (alias: `SHOAL_KERNEL`) | `kernel.enabled` | bool |
| `SHOAL_KERNEL_SESSION` | `kernel.session` | string |
| `SHOAL_JOURNAL_ENABLED` | `journal.enabled` | bool |
| `SHOAL_LEASH_POLICY` | `leash.policy` | string (path) |
| `SHOAL_COMPLETION_FUZZY` | `completion.fuzzy` | bool |
| `SHOAL_COMPLETION_CASE_INSENSITIVE` | `completion.case_insensitive` | bool |
| `SHOAL_COMPLETION_MAX_RESULTS` | `completion.max_results` | non-negative int |
| `SHOAL_COMPLETION_MENU` | `completion.menu` | bool |

Bool coercion accepts `1/true/TRUE/True/yes/on` and `0/false/FALSE/False/no/off`; anything else is
a hard `ConfigError::Env` naming the variable and the bad value — never a silent "treat as false".

## 4. Validation

Three distinct failure shapes, and only one of them (the first) is a warning rather than a hard
error:

**Unknown key → warning, never a silent drop.** Every key actually present anywhere in a layer is
checked against the schema. An unrecognized key doesn't fail the load and doesn't get quietly
dropped either — it's surfaced in `Loaded::warnings` with its exact dotted path, plus a
did-you-mean guess when one sibling key is close enough:

```
/home/dev/.config/shoal/shoal.toml: unknown config key `editor.bracketde_paste` (did you mean `editor.bracketed_paste`?)
```

No suggestion is offered when nothing is close (a wildly-off key gets an honest "no idea", not a
misleading guess).

**Type mismatch → precise error naming the key path and expected type**, before ever reaching a
generic deserialization error. This is `ConfigError`'s `Display` output verbatim (a caller
typically prints it as `eprintln!("error: {e}")` or similar):

```
/home/dev/.config/shoal/shoal.toml: history.max_entries: expected a non-negative integer, found string
```

Array elements and map entries are named down to the exact index/key (`adapters.dirs[1]`,
`aliases.gs`).

**Malformed TOML → structured parse error, never a panic.** Nothing in `shoal-config` panics on
attacker- or typo-controlled input — a syntax error, wrong-shaped value, or bad env override always
comes back as `Err(ConfigError)`, never an `unwrap`/`expect` on user data. The message reuses
`toml`'s own diagnostic (line/column pointer included), prefixed with the offending file:

```
/home/dev/.config/shoal/shoal.toml: TOML parse error at line 3, column 1
  |
3 | enabled = true
  | ^
invalid table header
```

**Semantic validation** (parses fine, type-checks fine, but is nonsense) — also a hard error,
`ConfigError::Value`:

| Rule | Message |
|---|---|
| `version` must be `1` | `version: unsupported config version <n> (expected 1)` |
| `history.max_entries` must be `> 0` | `history.max_entries: must be greater than 0` |
| `editor.mode` must be `emacs` or `vi` | `editor.mode: must be \`emacs\` or \`vi\`` |
| `editor.key_timeout_ms` must be `1..=60000` | `editor.key_timeout_ms: must be between 1 and 60000 (milliseconds)` |
| `completion.max_results` must be `> 0` | `completion.max_results: must be greater than 0` |
| an `aliases` name must be non-empty, no whitespace | `aliases: alias name \`g s\` must not contain whitespace` |
| an `env` name must be non-empty | `env: environment variable name must not be empty` |
| a `history.ignore` pattern must be non-empty | `history.ignore: pattern must not be empty` |

Every `ConfigError` implements `Display`/`std::error::Error` and also converts into `String` (so
`shoal_config::load(&opts)?` keeps compiling unchanged inside any `fn foo() -> Result<_, String>`).

## 5. Full key reference

Every key `Config::default()` sets, its type, default, and which layers it's meaningful in
(all of them, unless noted).

### `version`

| Key | Type | Default |
|---|---|---|
| `version` | integer | `1` |

The only supported value today; anything else is a hard error (§4) rather than a silent "best
guess" migration.

### `[prompt]`

| Key | Type | Default |
|---|---|---|
| `prompt.template` | string | `"{cwd}"` |

Legacy/simple prompt config. The crate that actually **renders** prompts, `shoal-prompt`, loads its
own considerably richer `[prompt]` schema (`format.left`, `format.right`, `transient`, git/reef
segments, …) directly from the same files, independent of `shoal-config`; this `template` field
exists mainly so `[prompt]` round-trips through `shoal-config` without tripping the unknown-key
scanner, and is what `shoal-prompt`'s loader migrates from when it sees an old-style config with no
`format` table. If you're writing new prompt config, use `shoal-prompt`'s schema, not this field.

### `[history]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `history.enabled` | bool | `true` | record command history at all |
| `history.max_entries` | integer | `10000` | cap on retained entries |
| `history.path` | string (path), optional | unset → host default | history file location |
| `history.dedup` | bool | `true` | drop a line identical to the immediately preceding one (`HISTCONTROL=ignoredups`) |
| `history.ignore` | array of strings | `[]` | patterns; a matching line is never recorded (`HISTIGNORE`-equivalent) |
| `history.ignore_space` | bool | `true` | a line typed with a **leading space** is never recorded (`HISTCONTROL=ignorespace`) |

### `[render]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `render.width` | integer, optional | unset → detect terminal width | force a render width |
| `render.color` | bool | `true` | ANSI color on rendered output; forced off by `NO_COLOR` (§3) regardless of this value |

### `[editor]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `editor.mode` | string | `"emacs"` | `emacs` or `vi` |
| `editor.bracketed_paste` | bool | `true` | enable terminal bracketed-paste mode |
| `editor.keybindings` | table of strings | `{}` | `chord -> action`, e.g. `"ctrl-r" = "history_search_backward"`; empty = the host's built-in bindings for `mode` |
| `editor.key_timeout_ms` | integer (milliseconds) | `25` | how long the line editor waits after a prefix key (`Esc`, `jk` in vi insert mode, …) before treating it as standalone |

### `[kernel]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `kernel.enabled` | bool | `true` | use the kernel-hosted session model (TDD §10) |
| `kernel.session` | string | `"default"` | session name |

### `[adapters]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `adapters.dirs` | array of strings (paths) | `[]` | extra adapter directories scanned in addition to the bundled pack, in order (later entries can shadow earlier ones for the same command name) |

### `[journal]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `journal.enabled` | bool | `true` | record the command journal (TDD §9) |
| `journal.state_dir` | string (path), optional | unset → host default | where the journal/CAS lives |

### `[leash]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `leash.policy` | string (path), optional | unset → unsandboxed | path to the leash policy file (`~/.config/shoal/leash.toml` by convention) |

### `[init]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `init.files` | array of strings (paths) | `[]` | script files run, in order, at the start of every interactive session |

### `[completion]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `completion.fuzzy` | bool | `true` | allow typo-tolerant / non-contiguous matches, not just prefix |
| `completion.case_insensitive` | bool | `true` | ignore case when matching |
| `completion.max_results` | integer | `100` | cap candidates computed/shown per completion |
| `completion.menu` | bool | `true` | show the interactive selection menu (vs. cycle-only) |

### `[aliases]`

| Key | Type | Default |
|---|---|---|
| `aliases.<name>` | string | (table is empty) |

`name -> expansion`, e.g. `gs = "git status"`. Semantically equivalent to running the session
statement `alias gs = git status` (TDD §1.8: AST-level partial application, never text) at
startup, just declared persistently instead of typed each session. Alias names must be non-empty
and contain no whitespace (§4).

### `[env]`

| Key | Type | Default |
|---|---|---|
| `env.<NAME>` | string | (table is empty) |

`NAME -> value`, set in the session environment at startup — a declarative `.profile`-equivalent,
e.g. `EDITOR = "hx"`.

### `[reef]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `reef.tools.<name>` | string or table | `{}` | a version constraint (`"22"`, `"*"`) or `{ version = "...", provider = "..." }` |
| `reef.runners.<ext>` | string or table | `{}` | a bare tool name (`"python"`) or `{ tool = "...", args = [...] }` |
| `reef.options.hermetic` | bool | `false` | child PATH is synthesized-only (no ambient system tail) when true |

`[reef]` in `shoal.toml` is reef's **user scope** (REEF.md §1) — project scope is instead the
nearest `.reef.toml`, resolved by `shoal-reef` itself, not by this crate. `shoal-config` is
deliberately loose here: `tools`/`runners` entries are validated only as "this is a table", not
against reef's full constraint/provider grammar — `shoal-reef` re-parses `[reef]` directly out of
the raw `shoal.toml` text with its own richer manifest schema (`ReefManifest::parse_shoal_reef`).
See REEF.md for the authoritative grammar.

## 6. What actually consumes each key today

`shoal-config`'s job is a correct, validated model of the file — not wiring every key into
runtime behavior (that's each consuming subsystem's job, largely in the `shoal` binary and
friends). As of this wave, the `shoal` binary's REPL/script-runner path reads:

- `adapters.dirs` — extra adapter directories, layered onto the bundled pack.
- `init.files` — run at interactive-session start.
- `editor.bracketed_paste` — passed to the line editor.
- `history.enabled`, `history.path`, `history.max_entries` — history file wiring.

Schema-complete, validated, and documented, but **not yet read by any in-tree consumer** as of
this wave (ready for a consumer to wire up — see the integrator note below):
`editor.mode`, `editor.keybindings`, `editor.key_timeout_ms`, `history.dedup`, `history.ignore`,
`history.ignore_space`, `render.width`, `render.color` (the `shoal` binary currently checks
`NO_COLOR` directly rather than through `Config`), `kernel.*`, `journal.state_dir` (the binary
resolves its own state directory independently of this field today), `leash.policy`,
`completion.*`, `aliases`, `env`, `reef.*` (`shoal-reef` re-parses `[reef]` independently, per §5).

Nothing here is a defect in `shoal-config` — the schema, defaults, validation, and layering are
all real and tested regardless of whether a given field already drives behavior; it's the
inventory an integrator wiring up aliases/completion/keybindings/reef would work from.

## 7. Rust API

```rust
// Discover the four-layer plan for a cwd, then load + validate it.
let cwd = std::env::current_dir()?;
let options = shoal_config::LoadOptions::discover(&cwd);
let loaded = shoal_config::load(&options)?; // Result<Loaded, ConfigError>

for warning in &loaded.warnings {
    eprintln!("warning: {warning}");
}
let config: shoal_config::Config = loaded.config;
```

- [`Config`] — the full typed model (§5), `Default`-able, `Serialize`/`Deserialize`.
- [`LoadOptions`] — the four layer paths + the env pairs to consult; `discover(cwd)` builds the
  standard plan, or construct one directly (tests do this to avoid touching the real filesystem/
  environment).
- [`find_project_config(start)`] — the standalone project-layer walk-up, if a caller wants it
  without going through `discover`.
- [`load(&LoadOptions)`] → `Result<Loaded, ConfigError>`.
- [`Loaded`] — `{ config, warnings: Vec<String>, sources: Vec<PathBuf> }`.
- [`ConfigError`] — `Io`/`Parse`/`Type`/`Value`/`Env` variants (§4); `Display` + `std::error::Error`
  + `From<ConfigError> for String`.
