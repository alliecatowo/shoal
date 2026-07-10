# shoal

Shoal is a structured shell for humans and agents. It keeps ordinary terminal
programs alive on a real PTY at the prompt, turns captured results into typed
values when they participate in expressions, and exposes the same session over
a capability-aware protocol.

This repository is an early v0.1 implementation of the contract in
[`docs/TDD.md`](docs/TDD.md). It is not ready to replace your login shell yet,
but the end-to-end language, command runner, interactive REPL, journal/CAS,
adapter catalog, policy engine, and kernel protocol are under active
construction.

## Try it

```sh
cargo run -p shoal
cargo run -p shoal -- -c $'let answer = 6 * 7\nanswer'
cargo run -p shoal -- examples/example.shl
```

Inside the REPL, statement-position commands use a PTY so colors, progress
bars, password prompts, and TUIs behave normally. Expressions and assignments
capture clean output instead. The editor includes persistent history,
multiline input, completion, highlighting, and a cwd/Git-aware prompt.

Some language examples:

```text
let names = ["ada", "grace", "linus"]
names.where(.len() > 3).map(.upper())

fn deploy(env: str, dry: bool = false) {
    echo deploying (env) --dry=(dry)
}

let result = sh { printf '{"ok":true}' }
result.out.ok
```

## Verify it

```sh
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

The normative language corpus lives in [`spec/cases`](spec/cases). Architecture
and pinned inter-crate APIs are documented in
[`docs/CONTRACTS.md`](docs/CONTRACTS.md).

## Workspace

- `shoal-syntax`, `shoal-ast`: modal syntax, canonical AST, formatting
- `shoal-value`, `shoal-eval`: typed values, methods, evaluator
- `shoal-exec`: capture and PTY execution with process-tree cancellation
- `shoal-adapters`: declarative external-command schemas
- `shoal-journal`: SQLite journal and content-addressed output store
- `shoal-leash`: effects, plans, grants, and enforcement honesty
- `shoal-proto`, `shoal-kernel`: JSON-RPC protocol and shared sessions
- `shoal`: script runner and interactive terminal client

Rust edition 2024. License: MIT OR Apache-2.0.
