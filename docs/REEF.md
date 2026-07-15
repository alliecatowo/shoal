# reef — tool resolution, ripped from the root

**Status:** implemented and wired as the live resolution path. `which`, `with reef:`, the lockfile,
and — the piece the previous revision of this line called still-landing — **project-scope
`.reef.toml` walking** are all real: a `.reef.toml` declaring `[tools] sh = "*"` in a scratch
directory changes `which sh`'s reported scope to `"reef"` and its resolution `chain` to name that
manifest file, verified directly against the binary. The conformance corpus (`spec/cases/reef.toml`)
still `skip`s the genuinely host-inventory-dependent cases (a real tool's resolved hash/version, the
live binding table) — those are testing-determinism gaps, not functionality gaps.

**Design contract v0.1.** Companion to `docs/TDD.md`; same rules: everything here is a decision,
the corpus decides disputes. Crate: `shoal-reef`.

## 0. Why reef exists

Every tool-version manager — mise, asdf, nvm, rbenv, pyenv, virtualenv, direnv, nix-shell — is a
workaround for one design fossil: **PATH**. A flat, ordered, mutable string of directories where
the first name-match wins. Its consequences are the daily misery of the terminal:

- Version selection = mutating ambient global state (env), inherited invisibly at fork.
- "Which binary am I actually running?" requires forensics (`which -a`, `type`, shim spelunking).
- Project-locality is reimplemented N times (`.tool-versions`, `.nvmrc`, `mise.toml`, `.envrc`),
  each with its own directory-walk and its own prompt-hook activation race.
- Shims add a fork to every invocation and lie to `which`.
- Reproducibility is unanswerable: the same command line does different things depending on
  invisible activation state, and nothing records what actually ran.
- Any writable dir on PATH is an injection vector.

A shell that already owns spawn (no `execvp`, no PATH search — shoal's exec resolves explicitly),
already records a journal, and already records a content hash for every resolved binary (this
document, §2) can delete the fossil instead of shimming it. **Resolution becomes a pure,
declarative, journaled function; the PATH becomes an *output* synthesized for legacy children,
never an input the shell lives in.** (Whether leash's *policy* actually consults that hash before a
spawn is a separate, narrower question — answered honestly, not assumed, in §2.)

reef is that resolver. The shoal swims; the reef is the stable structure it lives over.

## 1. The model

**A name resolves through scopes, not directories.** For a command head `X` (after session
`fn`/`alias`, per TDD §3.1.3):

```
session fn/alias  →  adapter bin pin  →  project reef  →  user reef  →  system reef  →  ambient
```

Each reef scope is a **manifest** mapping tool names to constraints/providers:

```toml
# .reef.toml at a project root (nearest wins), or [reef] in shoal.toml for user scope
[tools]
node   = "22"                      # semver-ish constraint
python = "3.12"
rg     = "*"                       # any; locked on first resolve
go     = { provider = "mise" }     # force a provider

[runners]                          # content-type resolution (same idea, keyed on extension)
py  = "python"                     # tool names, themselves reef-resolved
js  = "node"
ts  = { tool = "deno", args = ["run"] }
sh  = "sh"
shl = "self"

[options]
hermetic = false                   # child PATH: synthesized-only (true) or +system tail (false)
```

- **Project scope**: nearest `.reef.toml` walking up from cwd. Pure function of
  `(cwd, manifest files)` — **no activation, no hooks, no env mutation, ever**. `cd` re-scopes the
  next resolution; nothing else changes.
- **User scope**: `~/.config/shoal/shoal.toml` `[reef]`.
- **System reef**: discovered inventory of `/usr/bin` etc. (see providers). Ambient PATH survives
  only as the final fallback and is reported as scope `ambient` in diagnostics.
- **Foreign manifests**: the mise provider reads `mise.toml` / `.tool-versions` as read-only
  manifests, so existing repos work day one. `.reef.toml` wins when both exist. mise is not baked
  in — it is one provider among several; delete it and nothing else changes.

## 2. The lock — resolution you can trust

`reef.lock` (TOML, committed) records every resolved tool: name, exact version, provider,
absolute path, **blake3 of the binary**, resolved-at timestamp.

- Interactive sessions **auto-lock on first resolve** with a one-line notice (and a journal
  entry). Scripts/CI **error on unlocked constraints** (`reef_unlocked`) — deterministic artifacts
  don't get to guess. (Same interactive/script split the TDD already uses for `it` and watchdogs.)
- At spawn, the on-disk binary is re-hashed (cached by dev/inode/mtime): a mismatch against the
  lock is `reef_drift` — a hard error naming old/new hashes, with `reef lock --refresh` as the
  fix-it. This re-hash-and-compare is real and enforced today.
- **Not yet true, corrected from a previous revision of this line**: this section used to claim
  "leash's `proc.spawn{bin_hash}` pins against the lock, so policy and resolution verify the same
  chain." Verified against source, that chain is **not** wired: `Effect::ProcSpawn`'s `bin_hash`
  field is only ever constructed as an empty string (`crates/shoal-eval/src/plan_derive.rs`,
  hardcoded `String::new()`), and the real spawn path (`run_argv` → `resolve_sandbox` in
  `crates/shoal-eval/src/command.rs`) only resolves an OS-level Landlock/Seatbelt `SandboxPolicy` —
  it never calls `shoal-leash`'s `evaluate_effect`/`evaluate_plan` and never receives reef's
  computed hash. **A leash policy author writing `proc_spawn = ["<hash>"]` believing it pins against
  reef's blake3 lock gets zero enforcement from that hash today.** The *lock's* own drift detection
  (bullet above) is real and independent of this; it's specifically the **policy-time** name →
  version → hash → grant chain from the original design intent that is unbuilt. Treat "leash pins
  spawns against reef's locked hash" as a `docs/ROADMAP.md` item, not a shipped guarantee, until
  `plan_derive.rs` actually threads a real hash through and the spawn path actually consults leash's
  evaluator with it.
- Version conflicts (two scopes constraining one tool incompatibly) are values: the error lists
  both sources. No silent first-wins.

## 3. Providers — acquisition as adapters

```rust
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Enumerate available candidates for a tool (fast, cached).
    fn discover(&self, tool: &str) -> Vec<Candidate>;
    /// Optionally materialize a version that satisfies the constraint (may download).
    fn fetch(&self, tool: &str, req: &Constraint) -> Option<Result<Candidate, ProviderError>>;
}
pub struct Candidate { pub tool: String, pub version: Version, pub path: PathBuf, pub provider: &'static str }
```

v1 providers: `system` (scans canonical roots + ambient PATH, versions via `--version` probe,
cached), `mise` (reads `~/.local/share/mise/installs/**` directly — no shims, no `mise exec`,
no forks), `cargo` (`~/.cargo/bin`), `npm-local` (`node_modules/.bin` as a *declared, scoped*
provider — the thing everyone hacks PATH for), `venv` (`.venv/bin` when present). Providers
enumerate; only `fetch` (explicit `reef fetch node`) may install, and it delegates to the
provider's own machinery. reef never installs implicitly.

## 4. Legacy children — PATH as an output

Children that themselves search PATH (build scripts spawning `cc`, `npm` spawning `node`) must see
a coherent world. At spawn, exec receives a **synthesized PATH**: a per-resolution-context view
dir (`$XDG_RUNTIME_DIR/shoal/views/<hash-of-binding-set>/bin`, symlinks to locked binaries),
plus the system tail unless `hermetic = true`. Views are content-addressed and reused; building
one is O(bindings). The shell never *consults* PATH for its own resolution; it only *emits* one.
leash tier-A can force `hermetic` per grant.

## 5. Runners — the poly-runner question, dissolved

"How do I run `./script.py`" is the same question as "what does `node` denote" — resolution keyed
on content-type instead of name. So it lives here, **not in the language core**: the `[runners]`
table maps extension (falling back to shebang sniff) → tool + argv template. The explicit spelling,
`run ./x.py args` / `run x.py args`, resolves the runner, resolves its tool through reef, and spawns
— journaled like any spawn; verified directly against the binary. `run <name> args…` with a non-path
stays the TDD §3.1.4 dynamic command form. Defaults ship for `py js ts sh shl rb lua`; `rs`
intentionally has **no default** (compile-vs-script ambiguity) — configuring one is one TOML line.

**Not yet wired**: the bare-path-head ergonomics case (`./script.py args` with no leading `run`,
IO.md §3.1's "just typing the filename" case) for **non-`.shl` extensions**. Verified against the
binary: `run script.py` prints the script's output correctly, but a bare `./script.py` at command
head position currently attempts to exec the file directly (and fails with a permission error if
it isn't itself marked executable) rather than routing through `[runners]` resolution — only `.shl`
bare-path heads work today (they route through the `"self"` child-evaluator path, which predates and
is separate from the general runner table). If you're describing or relying on "just type the
filename" ergonomics for a `.py`/`.js`/`.rb`/etc. script, use the explicit `run` spelling until this
gap closes; don't assume bare-path dispatch generalizes past `.shl` yet.

The language core knows only "resolve this invocable"; delete `shoal-reef` and the language still
parses.

## 6. Surface

- `which node` → record: `{name, scope, constraint, version, path, hash, provider, chain: [...]}` —
  the full resolution chain, rendered as a table. `which -a`-style: `which node --all`.
- `reef` → the current binding table (name, constraint, version, hash8, provider, scope).
- `reef add node@22` (writes nearest manifest + locks), `reef lock [--refresh]`, `reef fetch <tool>`,
  `reef doctor` (drift, orphans, shadowed ambient tools).
- `with reef: {node: "20"} { … }` — dynamic scoping, restore-on-exit, like `with cwd:/env:`.
- Command-not-found did-you-mean gains: *"`node` is constrained (22) but not installed — `reef fetch node`"*
  and *"found in ambient PATH but shadowed by project reef"*.
- Journal: every entry records the resolution-context hash; every spawn records (tool, version,
  bin hash) actually used. "Which node built this artifact three weeks ago" is a journal query.

## 7. Error codes

`reef_unlocked reef_drift reef_conflict reef_not_found reef_provider` — added to CONTRACTS §4.

## 8. Non-goals (v1)

Not a package manager (fetch delegates to providers). Not an env manager (env stays session
state; reef only synthesizes child PATH). No shims, no hooks, no prompt integration, no implicit
installs. Windows resolution semantics deferred with the rest of the Windows port.
