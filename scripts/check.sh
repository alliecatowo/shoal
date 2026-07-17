#!/usr/bin/env bash
set -euo pipefail
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked
cargo audit
cargo audit --file fuzz/Cargo.lock
./site/scripts/check-diagrams.sh
zola --root site check
actionlint
