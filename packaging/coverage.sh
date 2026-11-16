#!/usr/bin/env bash
# Workspace line/region coverage summary (default, non-ignored tests only).
# Needs: cargo install cargo-llvm-cov --locked && rustup component add llvm-tools
# Extra args pass through, e.g. `packaging/coverage.sh --lcov --output-path lcov.info`.
set -euo pipefail
[ -d "$HOME/.cargo/bin" ] && export PATH="$HOME/.cargo/bin:$PATH" || true
cd "$(dirname "$0")/.."
if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "error: cargo-llvm-cov not found. Install: cargo install cargo-llvm-cov --locked" >&2
  echo "       and: rustup component add llvm-tools" >&2
  exit 1
fi
exec cargo llvm-cov --workspace --summary-only \
  --ignore-filename-regex '(/tests/|/benches/|/examples/|build\.rs|/target/)' "$@"
