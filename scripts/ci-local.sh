#!/usr/bin/env bash
#
# Reproduce *every* CI job locally so failures are caught before pushing.
# Mirrors `.github/workflows/{lint,ci,coverage}.yml`:
#
#   - lint.yml `build`  : cargo fmt --check, clippy, docs, cargo audit
#   - lint.yml `pre-commit`: uv lock --check
#   - ci.yml   `build`  : cargo test --locked --all-features
#   - coverage.yml      : cargo llvm-cov (100% line + branch coverage)
#
# Run this before any push. Windows-only behaviour can't be reproduced
# here, but the logic is identical.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

step "rustfmt (lint.yml)"
cargo fmt --all -- --check

step "Python lockfile (lint.yml)"
uv lock --check

step "clippy (lint.yml)"
cargo clippy --locked --all-targets --all-features -- -D warnings

step "docs (lint.yml)"
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features --document-private-items

step "Rust dependency audit (lint.yml)"
cargo audit --deny warnings

step "tests (ci.yml)"
cargo test --locked --all-features

step "coverage gate (coverage.yml)"
# CI runs this on nightly; locally `RUSTC_BOOTSTRAP=1` lets a stable
# toolchain compile the unstable `#[coverage(off)]` attribute, so local
# and CI results are identical. `cargo-llvm-cov` needs llvm-tools: use
# rustup's `llvm-tools-preview` when present, else fall back to a
# Homebrew LLVM install.
if [ -z "${LLVM_COV:-}" ]; then
  for prefix in /opt/homebrew/opt/llvm /usr/local/opt/llvm; do
    if [ -x "$prefix/bin/llvm-cov" ] && [ -x "$prefix/bin/llvm-profdata" ]; then
      export LLVM_COV="$prefix/bin/llvm-cov"
      export LLVM_PROFDATA="$prefix/bin/llvm-profdata"
      break
    fi
  done
fi
export RUSTC_BOOTSTRAP=1
cargo llvm-cov --locked --no-report --branch --workspace
cargo llvm-cov report --locked --branch --fail-under-lines 100
summary="$(cargo llvm-cov report --locked --branch --json --summary-only)"
echo "Branch coverage: $(jq -r '.data[0].totals.branches.percent' <<<"$summary")%"
jq -e '.data[0].totals.branches.percent >= 100' >/dev/null <<<"$summary" \
  || { echo "error: branch coverage is below the required 100%" >&2; exit 1; }

printf '\nAll local CI checks passed.\n'
