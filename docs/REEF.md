# reef — tool resolution, ripped from the root

**Status:** crate built+tested, eval integration in progress.

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
already records a journal, and already pins binaries by content hash (leash) can delete the
fossil instead of shimming it. **Resolution becomes a pure, declarative, journaled function; the
PATH becomes an *output* synthesized for legacy children, never an input the shell lives in.**

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
  fix-it. This is supply-chain teeth: leash's `proc.spawn{bin_hash}` pins against the *lock*, so
  policy and resolution verify the same chain: **name → version → hash → grant**.
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
table maps extension (falling back to shebang sniff) → tool + argv template. `./x.py args` at a
command head, and `run ./x.py args`, both resolve the runner, resolve its tool through reef, and
spawn — journaled like any spawn. `run <name> args…` with a non-path stays the TDD §3.1.4 dynamic
command form. Defaults ship for `py js ts sh shl rb lua`; `rs` intentionally has **no default**
(compile-vs-script ambiguity) — configuring one is one TOML line. The language core knows only
"resolve this invocable"; delete `shoal-reef` and the language still parses.

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
