+++
title = "Operational programs and dogfooding"
description = "Use Shoal's repository programs for backups, journals, caches, release verification, data migration, adapters, benchmarks, and incident response."
weight = 135
template = "docs/page.html"

[extra]
eyebrow = "Operations"
group = "Shell & tools"
audience = "Operators and contributors"
status = "Executed by the repository gate"
toc = true
+++

Shoal's `programs/ops/` directory contains complete command-line programs, not a syntax catalogue.
They accept explicit inputs, validate preconditions, produce reviewable artifacts, and fail with a
nonzero status when an invariant is broken. The repository currently executes all 54 against
disposable fixtures in CI.

Run the complete functional workload with:

```text
mise run dogfood
```

Parse and formatting-check the collection without executing its effects:

```text
mise run dogfood:fmt
```

Each program is directly runnable with the installed shell. A bad argument count prints its exact
usage contract through an assertion, so operations remain scriptable rather than prompting midway:

```text
shoal programs/ops/backup-create.shl ./important ./backup-2026-07-18
shoal programs/ops/backup-verify.shl ./backup-2026-07-18
shoal programs/ops/backup-restore.shl ./backup-2026-07-18 ./restored
```

## Program inventory

| Workflow | Programs | Observable result |
| --- | --- | --- |
| artifact integrity | `artifact-inventory`, `release-manifest`, `release-verify` | deterministic paths, byte counts, and SHA-256 checksums |
| backup lifecycle | `backup-create`, `backup-verify`, `backup-restore` | copied data plus a versioned manifest and a post-copy checksum-verified restore |
| journal operations | `journal-export`, `journal-summary`, `journal-retention-plan` | bounded sequence export, outcome summary, non-destructive retention plan |
| cache operations | `cache-inventory`, `cache-prune-plan`, `cache-prune` | ownership/size/SHA-256 report, byte-budget plan, root-constrained conditional deletion |
| release communication | `changelog-generate` | categorized deterministic Markdown |
| state and configuration | `migration-plan`, `migration-apply`, `config-overlay` | reviewable migration actions, new-version output, layered resolution |
| datasets | `dataset-normalize`, `dataset-diff` | validated last-record-wins normalization and identifier-aware change set |
| dependency evidence | `sbom-generate`, `sbom-verify` | Cargo component inventory plus uniqueness/identity/license review report |
| adapter governance | `adapter-inventory`, `adapter-conformance` | executable/class/subcommand inventory and manifest contract validation |
| kernel and MCP lifecycle | `kernel-mcp-lifecycle` | credentialed managed start/stop, live socket, restricted tokenless status, and restricted MCP initialize |
| performance and CI | `benchmark-compare`, `benchmark-dashboard`, `test-shard-plan` | regression classification, Markdown dashboard, duration-balanced shards |
| recovery operations | `reef-repair-plan`, `incident-bundle` | non-mutating repair actions and collected diagnostic bundle |
| change and dependency review | `artifact-diff`, `dependency-diff`, `change-risk-report`, `feature-status-report` | classified drift, upgrade sets, risk score, and honest feature state |
| deployment and recovery | `deployment-plan`, `deployment-verify`, `rollback-plan`, `retry-plan`, `maintenance-window-plan` | ordered rollout, observed-version checks, reverse rollback, bounded retry, and time-boxed maintenance |
| operational health | `access-log-summary`, `service-health-summary`, `error-budget-report`, `quota-report`, `disk-usage-report` | route failures, service state, budget exhaustion, quota pressure, and attributed disk use |
| governance and incident response | `ownership-report`, `release-readiness-report`, `secret-rotation-plan`, `log-redact`, `incident-timeline`, `test-failure-triage` | ownership gaps, ship decision, rotation set, protected logs, ordered events, and grouped failures |
| storage planning | `backup-catalog`, `file-deduplicate-plan`, `config-drift` | backup coverage, non-mutating reclaim plan, and missing/unexpected/changed configuration |
| end-to-end continuity | `workspace-continuity` | source inventory, verified backup, clean restore, round-trip diff, catalog, and durable recovery decision |
| CI evidence | `ci-evidence-auditor` | owned test outcomes, performance deltas, artifact drift, required checks, blockers, and one ship decision |
| incident command | `incident-command-center` | health and traffic aggregation, ordered timeline, redacted logs, prioritized actions, and a durable incident workspace |

Program filenames end in `.shl`; omit the suffixes shown in the table only when discussing the
workflow concept. The checked-in source is the command reference and starts with a one-line usage
assertion.

## Safety contracts

Planning and mutation are separate where an operator should review an effect. Cache pruning consumes
a schema-2 plan carrying each file's byte size and SHA-256 digest. Each removal uses `rm --permanent`
with both expectations: Shoal first moves the identity-checked entry into a private quarantine,
rehashes it there, restores it on content drift, and only unlinks matching content. A replacement is
never deleted merely because it reused a planned pathname or size. Migration writes a new destination
and refuses to replace an existing file. Backup restore likewise requires a new destination, verifies
the source manifest before copying, and rehashes the complete restored tree afterward. A post-copy
mismatch removes the new invalid destination and reports failure. Journal retention only emits a
plan; it never deletes journal state.

The programs use ordinary local fixtures and require no network service, ambient credentials,
pre-existing kernel, or user configuration. The lifecycle fixture creates a short-lived supervisor
bearer and a managed kernel under its disposable root. Its later tokenless `kernel status` and MCP
initialize calls attach as the public `agent:mcp` restricted-agent principal; only managed stop loads
the owner-only supervisor credential. The continuity drill is launched without `--standalone`, which
exercises the ordinary noninteractive CLI spelling; scripts intentionally use the local evaluator in
both modes. The PTY-driven `shoal` integration suite separately proves that only the default
interactive REPL selects its private embedded kernel. The dogfood runner checks final artifacts rather than accepting
exit status alone: restored bytes, journal boundaries, cache survivors, overlay precedence,
migration shape, dataset classifications, checksums, adapter counts, benchmark regression status,
shard membership, recovery evidence, CI blockers, redacted incident data, and response actions are
asserted.

The three orchestration programs are deliberately larger than the focused tools they compose. They
exercise the part that toy examples miss: carrying validated evidence across multiple phases and
leaving an operator-readable result behind. `workspace-continuity` runs eight real subprocess phases
and retains their manifests; `ci-evidence-auditor` joins four independent evidence domains without
hiding failures; `incident-command-center` converts heterogeneous observations into redacted evidence
and ordered response work. Their fixtures assert the semantic outputs, not their source length.

## Host bootstrap boundary

Repository logic is Shoal-native. A small boundary still invokes host executables through Shoal's
typed `run(...)` outcome:

- `mktemp -d` creates a collision-resistant disposable test root;
- `sha256sum`, or macOS `shasum -a 256`, supplies the platform checksum primitive;
- `install`, `test`, `ln`, and bounded `sleep` calls create private lifecycle directories, construct
  a hostile symlink fixture, and probe a Unix socket without a shell interpreter;
- the compiled `shoal`, `shoal-token`, `shoal-mcp`, and `shoal-kernel` executables launch each
  independent program and exercise the shipped protocol surfaces.

Those are explicit process dependencies, not hidden Bash programs or workflow shell blocks. The
source audit rejects tracked `.sh` files, Bash/Sh workflow blocks, and workflow commands outside the
documented Cargo bootstrap plus Shoal execution boundary. A repository-wide interpreter scan has no
tracked Bash, Zsh, Ksh, Fish, or POSIX-sh program. CI first
builds Shoal with Cargo because an unbuilt language cannot execute its own source. After that
bootstrap, `.github/workflows/ci.yml` enters `scripts/ci-test.shl`, which runs the dogfood workload
through Shoal. The exact repository gate in `scripts/check.shl` does the same.

## Adding an operational program

A new program should own a real operator outcome, accept paths and policy as arguments, reject
ambiguous mutation, and produce a stable artifact another tool can inspect. Add a hermetic fixture and
content assertion to `scripts/dogfood-test.shl`; merely checking that a tiny example exits zero does
not count as dogfood. Keep network calls, user state, ambient credentials, wall-clock timestamps, and
running daemons out of this gate unless the test supplies and tears down the dependency itself.
