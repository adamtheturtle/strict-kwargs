#!/usr/bin/env bash
#
# Update the vendored typeshed stdlib stubs.
#
# Usage:
#   scripts/update-typeshed.sh             # pin to current typeshed `main`
#   scripts/update-typeshed.sh <git-ref>   # pin to a specific commit/tag/branch
#
# The script is idempotent and self-verifying: it replaces
# `crates/strict-kwargs/vendored/typeshed/{stdlib,LICENSE}`, records the
# resolved commit in `crates/strict-kwargs/vendored/typeshed/COMMIT`, then
# builds and tests so a bad sync fails loudly.

set -euo pipefail

REF="${1:-main}"
REPO="https://github.com/python/typeshed.git"

ROOT="$(git rev-parse --show-toplevel)"
DEST="${ROOT}/crates/strict-kwargs/vendored/typeshed"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/typeshed-sync.XXXXXX")"
cleanup() { rm -rf "${WORK}"; }
trap cleanup EXIT

echo "==> Fetching typeshed @ ${REF}"
git clone --quiet --filter=blob:none --no-checkout "${REPO}" "${WORK}/typeshed"
git -C "${WORK}/typeshed" sparse-checkout set --no-cone /stdlib /LICENSE
git -C "${WORK}/typeshed" checkout --quiet "${REF}"
COMMIT="$(git -C "${WORK}/typeshed" rev-parse HEAD)"

OLD_COMMIT="$(cat "${DEST}/COMMIT" 2>/dev/null || echo '(none)')"
if [ "${COMMIT}" = "${OLD_COMMIT}" ]; then
  echo "==> Already at ${COMMIT}; nothing to do."
  exit 0
fi

echo "==> Replacing vendored stubs"
rm -rf "${DEST}/stdlib" "${DEST}/LICENSE"
cp -R "${WORK}/typeshed/stdlib" "${DEST}/stdlib"
cp "${WORK}/typeshed/LICENSE" "${DEST}/LICENSE"
printf '%s\n' "${COMMIT}" > "${DEST}/COMMIT"

STUB_COUNT="$(find "${DEST}/stdlib" -name '*.pyi' | wc -l | tr -d ' ')"
echo "==> ${OLD_COMMIT} -> ${COMMIT} (${STUB_COUNT} .pyi files)"

echo "==> Verifying (cargo test)"
( cd "${ROOT}" && cargo test --quiet )

cat <<EOF

Done. Vendored typeshed updated to ${COMMIT}.

Next steps:
  - Review the diff:   git -C "${ROOT}" status --short crates/strict-kwargs/vendored/typeshed
  - Commit:            git add crates/strict-kwargs/vendored/typeshed && git commit -m "Bump vendored typeshed to ${COMMIT:0:12}"

If new stub syntax broke parsing or behavior changed, tests above would have
failed; investigate before committing.
EOF
