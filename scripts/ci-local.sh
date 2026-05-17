#!/usr/bin/env bash
#
# Reproduce *every* CI job locally so failures are caught before pushing.
# Mirrors `.github/workflows/{lint,ci,coverage}.yml`:
#
#   - lint.yml `build`  : cargo fmt --check, clippy, docs
#   - ci.yml   `build`  : cargo test --all-features
#   - coverage.yml      : scripts/coverage.sh (100% line+branch+function)
#
# Run this before any push. Windows-only behaviour can't be reproduced
# here, but the logic is identical.
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n=== %s ===\n' "$1"; }

step "rustfmt (lint.yml)"
cargo fmt --all -- --check

step "clippy (lint.yml)"
cargo clippy --all-targets --all-features -- -D warnings

step "docs (lint.yml)"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --document-private-items

step "tests (ci.yml)"
cargo test --all-features

step "coverage gate (coverage.yml)"
./scripts/coverage.sh

printf '\nAll local CI checks passed.\n'
