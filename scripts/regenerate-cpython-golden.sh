#!/usr/bin/env bash
#
# Regenerate the differential completeness golden baseline (issue #192):
# tests/data/cpython_completeness_golden.tsv.
#
# Usage:
#   scripts/regenerate-cpython-golden.sh            # use the pinned ref below
#   scripts/regenerate-cpython-golden.sh <git-ref>  # pin to a different ref
#
# The baseline is the exact set of `(path, line, column, callee)` diagnostics
# strict-kwargs produces for the pinned CPython checkout, restricted to the
# packages below. The completeness test (`tests/cpython_completeness.rs`)
# asserts the full-tree scan equals it exactly — no fewer (a dropped real
# violation, the issue #191 failure mode) and no more (a spurious one).
#
# Exact equality is possible because the ty fallback is deterministic: it warms
# up a full type-check before querying (issue #198) and runs ty in the project
# root (so resolution is independent of cwd). One scan therefore suffices.
#
# IMPORTANT: ty resolves the stdlib against the discovered interpreter, which
# differs by OS (Linux: the runtime `.py`; macOS: vendored typeshed stubs), so
# the baseline is OS-specific. The CI gate runs on Linux, so regenerate on
# Linux. On a macOS workstation, run this inside a Linux container:
#
#   docker run --rm -v "$PWD":/repo -e CARGO_TARGET_DIR=/tmp/t -w /repo \
#     rust:bookworm bash -c 'apt-get update -qq && \
#       apt-get install -y -qq python3 git curl ca-certificates && \
#       curl -LsSf https://astral.sh/uv/install.sh | sh && \
#       export PATH="$HOME/.local/bin:$PATH" && uv tool install ty==0.0.42 && \
#       export PATH="$(uv tool dir --bin):$PATH" && \
#       scripts/regenerate-cpython-golden.sh'
#
# Requires `ty` on PATH (the resolver backend) and python3.

set -euo pipefail

# Keep this ref in sync with `CPYTHON_REF` in .github/workflows/ci.yml and
# .github/workflows/full-dry-runs.yml so every CPython-backed job pins the same
# tree.
REF="${1:-8b31d08e62b9714cf8dd1d8b19afa5ecbad2414a}"
REPO="https://github.com/python/cpython.git"

# Packages the baseline (and the exact comparison) is restricted to, keeping
# the committed file reviewable. Must match `PACKAGES` in
# tests/cpython_completeness.rs.
PACKAGES="Lib/asyncio/ Lib/email/ Lib/http/ Lib/importlib/ Lib/multiprocessing/ Lib/unittest/"

# The baseline depends on ty's resolution output, so it is tied to a specific
# ty version. Keep in sync with `TY_VERSION` in .github/workflows/ci.yml so the
# gate uses the same ty the baseline was captured with. Bumping ty means
# regenerating the baseline.
TY_VERSION="0.0.42"

ROOT="$(git rev-parse --show-toplevel)"
GOLDEN="${ROOT}/tests/data/cpython_completeness_golden.tsv"

if ! command -v ty >/dev/null 2>&1; then
  echo "error: ty must be on PATH (the resolver backend)" >&2
  exit 1
fi

INSTALLED_TY="$(ty version | awk '{print $2}')"
if [ "${INSTALLED_TY}" != "${TY_VERSION}" ]; then
  echo "error: ty ${TY_VERSION} is required to regenerate the baseline, but" \
    "ty ${INSTALLED_TY} is on PATH. Install it with:" >&2
  echo "  uv tool install ty==${TY_VERSION}" >&2
  echo "(or bump TY_VERSION here and in ci.yml if you intend to move ty)." >&2
  exit 1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/cpython-golden.XXXXXX")"
cleanup() { rm -rf "${WORK}"; }
trap cleanup EXIT

CHECKOUT="${WORK}/cpython"
echo "==> Cloning CPython @ ${REF}"
mkdir -p "${CHECKOUT}"
git -C "${CHECKOUT}" init --quiet
git -C "${CHECKOUT}" remote add origin "${REPO}"
git -C "${CHECKOUT}" fetch --depth=1 --quiet origin "${REF}"
git -C "${CHECKOUT}" checkout --detach --quiet FETCH_HEAD
RESOLVED="$(git -C "${CHECKOUT}" rev-parse HEAD)"

echo "==> Building strict-kwargs (release)"
( cd "${ROOT}" && cargo build --release --locked --bin strict-kwargs )
TARGET_DIR="${CARGO_TARGET_DIR:-${ROOT}/target}"
BIN="${TARGET_DIR}/release/strict-kwargs"

echo "==> Scanning (one deterministic pass)"
# `--project-root` is the checkout so this matches how the test invokes
# `check_paths` (project root = the checkout). `check` exits non-zero when it
# finds violations; that is expected here.
"${BIN}" check --project-root "${CHECKOUT}" --output-format json "${CHECKOUT}" \
  > "${WORK}/scan.json" 2>/dev/null || true

echo "==> Restricting to the chosen packages and writing the baseline"
CHECKOUT="${CHECKOUT}" GOLDEN="${GOLDEN}" REF="${RESOLVED}" \
  PACKAGES="${PACKAGES}" SCAN="${WORK}/scan.json" \
  TY_VERSION="${TY_VERSION}" python3 - <<'PY'
import json
import os

checkout = os.environ["CHECKOUT"].rstrip("/") + "/"
packages = tuple(os.environ["PACKAGES"].split())
ref = os.environ["REF"]
ty_version = os.environ["TY_VERSION"]
golden = os.environ["GOLDEN"]

with open(os.environ["SCAN"]) as handle:
    data = json.load(handle)

subset = sorted(
    {
        (rel, item["location"]["row"], item["location"]["column"], item["callee"])
        for item in data
        if (rel := item["filename"].replace(checkout, "")).startswith(packages)
    }
)

# A tab separates fields, so a tab or newline in a callee would corrupt the
# format; fail loudly rather than write a corrupt baseline.
for _, _, _, callee in subset:
    if "\t" in callee or "\n" in callee:
        raise SystemExit(f"callee contains a tab/newline, cannot serialize: {callee!r}")

header = [
    "# Differential completeness golden baseline for issue #192.",
    "#",
    "# Each non-comment line is a tab-separated diagnostic strict-kwargs produces",
    "# for the pinned CPython checkout, restricted to the packages below:",
    "#     <relative-path>\\t<line>\\t<column>\\t<callee>",
    "#",
    "# The full-tree scan is asserted to EXACTLY equal these entries (within those",
    "# packages): no fewer (a dropped real violation) and no more (a spurious one).",
    "# This is possible because the ty fallback is deterministic (it warms up a",
    "# full type-check before querying, issue #198); the result is identical",
    "# run-to-run, so an exact match is stable.",
    "#",
    "# Generated on Linux (the CI gate platform); ty resolves the stdlib against",
    "# the runtime interpreter there. Regenerate with",
    "# scripts/regenerate-cpython-golden.sh.",
    "#",
    f"# Pinned CPython ref: {ref}",
    f"# Generated with ty: {ty_version} (Linux)",
]
with open(golden, "w") as handle:
    handle.write("\n".join(header) + "\n")
    for rel, row, col, callee in subset:
        handle.write(f"{rel}\t{row}\t{col}\t{callee}\n")

print(f"==> Wrote {len(subset)} entries to {golden}")
PY

echo
echo "Done. Baseline regenerated for CPython @ ${RESOLVED}."
echo "Review the diff:  git -C \"${ROOT}\" diff -- tests/data/cpython_completeness_golden.tsv"
