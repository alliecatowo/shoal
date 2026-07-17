+++
title = "System map"
description = "The composition roots, dependency boundaries, data paths, and state ownership that define Shoal as it exists today."
weight = 10
template = "docs/page.html"

[extra]
group = "Orientation"
eyebrow = "Architecture atlas"
status = "Source-grounded"
audience = "Maintainers and reviewers"
wide = true
+++

Shoal is not one process with one runtime path. It is a workspace of narrow libraries assembled by
two principal hosts:

- `shoal` is the human-facing local shell. It owns Reedline, terminal state, prompt collection,
  configuration loading, bundled adapters, and init files. Its default REPL uses a listener-free
  private kernel child over an inherited anonymous descriptor; `--standalone` embeds the evaluator.
- `shoal-kernel` is a JSON-RPC daemon/private child. It owns principal-private named sessions, authentication,
  transcripts, plans, tasks, long-lived PTYs, event delivery, and an evaluator per session.
  `shoal-mcp` is a stdio MCP facade over that daemon.

This distinction is the first fact to preserve when changing Shoal. “The evaluator supports it” is
not sufficient evidence that the local shell, kernel, and MCP surface all expose it in the same way.

## System context

```mermaid
flowchart LR
accTitle: System context
accDescr: Shows the components and relationships described in System context.
  Human["Human at a terminal"] --> Shell["shoal\nReedline host"]
  Editor["Editor"] --> LSP["shoal-lsp"]
  Agent["MCP client / agent"] --> MCP["shoal-mcp\nstdio facade"]
  MCP -->|"newline JSON-RPC over Unix socket"| Kernel["shoal-kernel"]

  Shell -->|"default: inherited private descriptor"| Kernel
  Shell -->|"--standalone"| LocalEval["embedded Evaluator"]
  Kernel --> Sessions["named Session objects"]
  Sessions --> KernelEval["one Evaluator per session"]

  LocalEval --> Exec["shoal-exec"]
  KernelEval --> Exec
  LocalEval --> Journal["shoal-journal + CAS"]
  KernelEval --> Journal
  Kernel --> Journal

  LocalEval --> OS["filesystem / processes / terminal"]
  KernelEval --> OS
  Exec --> OS
  LSP --> Syntax["shoal-syntax"]
  LocalEval --> Syntax
  KernelEval --> Syntax
```

The CLI can also launch companion binaries (`shoal lsp`, `shoal mcp`). The default REPL does route
normal commands through its private kernel, but it never binds or joins the durable public socket;
standalone mode remains a separate embedded composition root.

Sources: [`shoal` main and REPL](https://github.com/alliecatowo/shoal/tree/main/crates/shoal/src),
[`shoal-kernel`](https://github.com/alliecatowo/shoal/tree/main/crates/shoal-kernel/src), and
[`shoal-mcp`](https://github.com/alliecatowo/shoal/tree/main/crates/shoal-mcp/src).

## Dependency strata

The Cargo graph is deliberately mostly acyclic. The central inversion is that `shoal-value` does
not depend on `shoal-eval`: method callbacks cross that boundary through a `CallCtx`. Likewise,
the AST and syntax crates do not know about execution, policies, or hosts.

```mermaid
flowchart TB
accTitle: Dependency strata
accDescr: Shows the components and relationships described in Dependency strata.
  subgraph Hosts["Composition roots"]
    CLI["shoal"]
    Kernel["shoal-kernel"]
    MCP["shoal-mcp"]
    LSP["shoal-lsp"]
  end

  subgraph Semantics["Language semantics"]
    Eval["shoal-eval"]
    Syntax["shoal-syntax"]
    AST["shoal-ast"]
    Value["shoal-value"]
  end

  subgraph Runtime["Runtime services"]
    Exec["shoal-exec"]
    Leash["shoal-leash"]
    Journal["shoal-journal"]
    Reef["shoal-reef"]
    Adapters["shoal-adapters"]
    Secret["shoal-secret"]
    Picker["shoal-picker"]
  end

  subgraph Protocol["Protocol and identity"]
    Proto["shoal-proto"]
    Auth["shoal-auth"]
  end

  CLI --> Eval
  CLI --> Syntax
  CLI --> Journal
  Kernel --> Eval
  Kernel --> Proto
  Kernel --> Auth
  MCP -. "socket protocol; no normal Cargo edge" .-> Proto
  LSP --> Syntax
  Eval --> Syntax
  Eval --> Value
  Eval --> Exec
  Eval --> Leash
  Eval --> Journal
  Eval --> Reef
  Eval --> Adapters
  Eval --> Secret
  Eval --> Picker
  Exec --> Leash
  Adapters --> Value
  Adapters --> AST
  Value --> AST
  Syntax --> AST
```

`shoal-mcp` intentionally talks to the kernel as an external client and has no normal dependency on
`shoal-proto` or `shoal-kernel`; those are development dependencies for tests. This keeps the
transport boundary honest, at the cost of some duplicated client-side wire shapes.

## The source-to-effect path

The stable mental model is a sequence of representations, not a single “run command” operation.

```mermaid
flowchart LR
accTitle: The source-to-effect path
accDescr: Shows the components and relationships described in The source-to-effect path.
  Source["UTF-8 source"] --> Lex["mode-aware tokens"]
  Lex --> Parse["Program / Stmt / Expr AST"]
  Parse --> Plan["derived effects + reversibility"]
  Parse --> Eval["tree-walk evaluation"]
  Plan --> Policy["principal policy verdict"]
  Policy -->|allow / approved| Eval
  Policy -->|ask| Approval["stored plan + approval event"]
  Policy -->|deny| Error["stable error code"]
  Eval --> Values["typed Value graph"]
  Eval --> Spawn["ExecSpec"]
  Spawn --> Sandbox["lowered sandbox profile"]
  Sandbox --> Child["child process / process group / PTY"]
  Child --> Outcome["OutcomeVal"]
  Values --> Render["terminal rendering"]
  Values --> Wire["bounded WireValue + ref"]
  Values --> Persist["journal metadata + CAS bytes"]
```

Not every branch occurs in every host. The local evaluator can plan and apply through language
builtins. The kernel adds RPC-level plan storage, approval, reference scoping, bounded wire
rendering, and events.

## State ownership

State is intentionally split by lifetime. Confusing these lifetimes causes most restart, sharing,
and isolation bugs.

| State | Owner | Lifetime | Shared with |
|---|---|---|---|
| lexical environment, `cwd`, process env, `it`, functions, aliases | `Evaluator` | evaluator/session | callers of the same evaluator |
| local line editor state and filtered history | `shoal` REPL | process / history file | local user only |
| named session transcript and `out[n]` values | kernel `Session` | kernel process | clients with the exact principal+Session owner |
| per-connection `it` reference | kernel attachment/client | connection | no other connection |
| plans, task wrappers, open PTYs | kernel maps | kernel process | exact principal+Session lookup |
| event channel rings and subscribers | kernel `EventBus` | kernel process | permitted principal+Session clients |
| durable transcript/journal events | SQLite journal | filesystem | later kernel processes |
| output blobs | journal CAS | filesystem until GC | any authorized ref lookup |
| Reef lock and executable view | project/user filesystem | filesystem | processes using that scope |
| auth tokens and policy | user state directory | filesystem | kernel/policy loaders |
| secrets | encrypted secret store | filesystem | same-user callers with key access |

```mermaid
flowchart TB
accTitle: State ownership
accDescr: Shows the components and relationships described in State ownership.
  Disk["Persistent filesystem"]
  Kernel["Kernel process"]
  Session["Named session"]
  ClientA["Connection A"]
  ClientB["Connection B"]

  Disk -->|"journal rows / CAS / auth"| Kernel
  Kernel --> Session
  Session -->|"shared env + transcript"| ClientA
  Session -->|"shared env + transcript"| ClientB
  ClientA --> ItA["client_it A"]
  ClientB --> ItB["client_it B"]
```

Kernel restart currently loses named evaluator state, transcripts held as live `Value`s, plans,
task wrappers, PTYs, and in-memory event rings. The journal, CAS, auth store, policy, Reef manifests,
and Reef locks survive.

## Private-kernel default and explicit standalone path

The interactive CLI performs presentation/bootstrap assembly around either a private kernel-backed
protocol Session (the default) or an explicit standalone evaluator. A durable public kernel remains
a separate process/trust domain and does not share the private REPL's live state:

```mermaid
flowchart TD
accTitle: Private-kernel default and standalone path
accDescr: Shows the default isolated private kernel, the explicit local evaluator, and the separate durable machine kernel.
  Config["layered shoal config"] --> CLI["local shoal host"]
  Prompt["prompt config"] --> CLI
  Bundled["bundled + extra adapters"] --> CLI
  Init["init files, aliases, env"] --> CLI
  UserReef["user Reef manifest"] --> CLI
  Journal["journal + frecency"] --> CLI

  Journal --> Kernel["kernel session factory"]
  Channel["event channel forwarder"] --> Kernel
  Policy["per-principal leash policy"] --> Kernel

  CLI --> Private["listener-free private kernel (default)"]
  CLI --> EvalA["local Evaluator (--standalone)"]
  Private --> EvalP["principal-private REPL Session"]
  Kernel --> EvalB["durable machine Session"]
```

As implemented, a newly created kernel session installs journal/frecency support and a channel
forwarder, but does not load the CLI's layered config, aliases, environment overrides, init files,
bundled/extra adapter directories, or user Reef manifest. This is a parity boundary, not merely a
documentation omission. Test a feature through the intended host before describing it as universal.

There is a second parsing difference: the local REPL constructs a `ParseCtx` from session bindings,
while the kernel `exec` handler parses submitted source without that context. Evaluation can still
resolve command-shaped callable names, but statement-head classification of session-bound values can
differ across requests.

Sources: [`shoal/src/repl.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal/src/repl.rs),
[`shoal-kernel/src/session.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal-kernel/src/session.rs),
and [`handlers_exec.rs`](https://github.com/alliecatowo/shoal/blob/main/crates/shoal-kernel/src/handlers_exec.rs).

## Architectural rules worth defending

1. **ASTs are syntax, not authority.** Effects are derived and then checked by Leash before an
   authorized host crosses an OS boundary.
2. **Values remain structured until a boundary requires bytes.** Rendering, stdin feeding, JSON
   wire encoding, and CAS persistence are separate conversions.
3. **Paths preserve bytes.** Display strings are not treated as reversible path encodings.
4. **Session state is explicit.** Process-wide `cwd` or environment mutation would violate
   concurrent kernel sessions.
5. **External execution is centralized.** Process groups, cancellation, sandbox lowering, capture,
   and PTYs belong in `shoal-exec`, not ad-hoc evaluator builtins.
6. **Wire responses are bounded and recoverable.** Large results become previews plus typed refs;
   clients follow refs deliberately.
7. **Reversibility is evidence-based.** If the journal cannot safely snapshot or invert a mutation,
   the plan must not call it reversible.
8. **Durability is opt-in and named.** An in-memory transcript or event ring must not be mistaken for
   journal durability.

## Where to continue

- [Crate and module ledger](../crate-ledger/) for ownership and dependency direction.
- [Language engine](../language-engine/) for lexer, parser, AST, evaluator, and dispatch.
- [Kernel, protocol, and sessions](../kernel-protocol/) for RPC and concurrent state.
- [Change map and known debt](../change-map/) before making cross-cutting changes.
