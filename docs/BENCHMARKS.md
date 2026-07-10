# Performance gates

```sh
cargo bench -p shoal-syntax --bench syntax
cargo bench -p shoal-value --bench table
cargo bench -p shoal-journal --bench journal
cargo bench -p shoal-exec --bench spawn
```

The journal benchmark seeds 100,000 rows and the table benchmark retains one
million rows, so neither runs during normal tests.

Budgets from TDD §12:

- 10 kB keystroke reparse: p99 below 1 ms.
- One-million-row `where` plus sort: below 150 ms.
- Journal query over 100,000 entries: below 50 ms.
- Spawn overhead: within 5% of direct `execve`.
- Cold CLI start: below 15 ms.

Criterion results are reviewed against pinned-runner baselines rather than
hard assertions on noisy shared CI. CI gates formatting, workspace tests,
strict Clippy, and release compilation. Fuzz compilation is non-blocking
because it requires nightly and a C++ toolchain.
