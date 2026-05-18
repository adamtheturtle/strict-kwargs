#!/usr/bin/env bash
#
# Enforce 100% test coverage, including branch coverage.
#
# Branch coverage and the `coverage(off)` attribute both require unstable
# rustc features. CI runs this on a nightly toolchain; locally a stable
# toolchain works too because we set `RUSTC_BOOTSTRAP=1` (the exact same
# instrumentation either way, so local and CI results are identical).
#
# `cargo-llvm-cov` needs `llvm-cov`/`llvm-profdata`. Rustup's
# `llvm-tools-preview` component provides them on CI; for a Homebrew Rust
# install we fall back to Homebrew's LLVM.
set -euo pipefail

export RUSTC_BOOTSTRAP=1

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "error: cargo-llvm-cov is not installed (cargo install cargo-llvm-cov)" >&2
  exit 1
fi

# Use rustup's llvm-tools when present; otherwise try Homebrew's LLVM.
if [ -z "${LLVM_COV:-}" ]; then
  for prefix in /opt/homebrew/opt/llvm /usr/local/opt/llvm; do
    if [ -x "$prefix/bin/llvm-cov" ] && [ -x "$prefix/bin/llvm-profdata" ]; then
      export LLVM_COV="$prefix/bin/llvm-cov"
      export LLVM_PROFDATA="$prefix/bin/llvm-profdata"
      break
    fi
  done
fi

# Collect coverage once (runs the whole test suite, instrumented). This
# builds the library, the binary and every `tests/` integration crate, but
# deliberately *not* `benches/` — benchmarks are not tests and are not part
# of the coverage contract.
cargo llvm-cov --no-report --branch --workspace

# Enforce 100% line and function coverage on production code (test
# modules and documented-unreachable defensive helpers are
# `#[coverage(off)]`). Region coverage is not gated: LLVM "regions"
# subdivide a line at every short-circuit/`?` and double-count macro
# expansions — an instrumentation signal, not the contract.
cargo llvm-cov report --branch \
  --fail-under-lines 100 \
  --fail-under-functions 100

# Enforce 100% branch coverage from the JSON summary (`llvm-cov` has no
# `--fail-under-branches`).
branch_percent="$(
  cargo llvm-cov report --branch --json --summary-only |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["totals"]["branches"]["percent"])'
)"
echo "Branch coverage: ${branch_percent}%"
python3 -c "import sys; sys.exit(0 if float('${branch_percent}') >= 100.0 else 1)" || {
  echo "error: branch coverage ${branch_percent}% is below the required 100%" >&2
  exit 1
}

echo "Coverage gate passed: 100% lines, functions and branches."
