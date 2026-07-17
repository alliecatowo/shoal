#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
cd -- "$repo_dir"

clean=false
check_only=false
case ${1-} in
  "") ;;
  --clean) clean=true ;;
  --check) check_only=true ;;
  *)
    echo "usage: $0 [--clean|--check]" >&2
    exit 2
    ;;
esac

install_dir=${SHOAL_INSTALL_DIR:-${CARGO_HOME:-${HOME:?HOME is not set}/.cargo}/bin}
case $install_dir in
  ""|/)
    echo "refusing unsafe install directory: ${install_dir:-<empty>}" >&2
    exit 2
    ;;
esac

binaries=(
  shoal
  shoal-kernel
  shoal-mcp
  shoal-lsp
  shoal-token
  shoal-secret
  shoal-history
  shoal-doctor
  shoal-sandbox-exec
  shoal-landlock-helper
)

if ! $check_only; then
  if $clean; then
    cargo clean --release
  fi
  cargo build --workspace --release --locked
  install -d -m 0755 -- "$install_dir"
  for binary in "${binaries[@]}"; do
    install -m 0755 -- "target/release/$binary" "$install_dir/$binary"
  done
fi

for binary in "${binaries[@]}"; do
  release="target/release/$binary"
  installed="$install_dir/$binary"
  if [[ ! -x $release ]]; then
    echo "missing release artifact: $release" >&2
    exit 1
  fi
  if [[ ! -x $installed ]]; then
    echo "missing installed executable: $installed" >&2
    exit 1
  fi
  if ! cmp -s -- "$release" "$installed"; then
    echo "installed executable differs from release artifact: $installed" >&2
    exit 1
  fi
done

"$install_dir/shoal" --version
echo "installed ${#binaries[@]} Shoal executables in $install_dir"
echo "note: restart an already-running durable shoal-kernel to load the new executable"
