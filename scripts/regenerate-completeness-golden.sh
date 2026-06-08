#!/usr/bin/env bash
#
# Regenerate the completeness snapshot from the pinned external repository
# used by tests/completeness.rs.
set -euo pipefail

cd "$(dirname "$0")/.."

REPOSITORY_NAME="${STRICT_KWARGS_COMPLETENESS_REPOSITORY_NAME:-sphinx}"
REPOSITORY_URL="${STRICT_KWARGS_COMPLETENESS_REPOSITORY_URL:-https://github.com/sphinx-doc/sphinx.git}"
REPOSITORY_REF="${STRICT_KWARGS_COMPLETENESS_REPOSITORY_REF:-cc7c6f435ad37bb12264f8118c8461b230e6830c}"
COMPLETENESS_PYTHON="${STRICT_KWARGS_COMPLETENESS_PYTHON:-3.13}"
COMPLETENESS_TY_VERSION="${STRICT_KWARGS_COMPLETENESS_TY_VERSION:-0.0.44}"
COMPLETENESS_CONSTRAINTS="${STRICT_KWARGS_COMPLETENESS_CONSTRAINTS:-tests/golden/completeness-requirements-constraints.txt}"
temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT

export STRICT_KWARGS_COMPLETENESS_REPOSITORY_NAME="$REPOSITORY_NAME"
export STRICT_KWARGS_COMPLETENESS_REPOSITORY_URL="$REPOSITORY_URL"
export STRICT_KWARGS_COMPLETENESS_REPOSITORY_REF="$REPOSITORY_REF"

ty_bin_dir="$temp_dir/ty-bin"
mkdir -p "$ty_bin_dir"
cat > "$ty_bin_dir/ty" <<EOF
#!/usr/bin/env bash
exec uv tool run --from "ty==$COMPLETENESS_TY_VERSION" ty "\$@"
EOF
chmod +x "$ty_bin_dir/ty"
export PATH="$ty_bin_dir:$PATH"
ty version

if [ -z "${STRICT_KWARGS_COMPLETENESS_CHECKOUT:-}" ]; then
  export STRICT_KWARGS_COMPLETENESS_CHECKOUT="$temp_dir/$REPOSITORY_NAME"
  mkdir -p "$STRICT_KWARGS_COMPLETENESS_CHECKOUT"
  git -C "$STRICT_KWARGS_COMPLETENESS_CHECKOUT" init --quiet
  git -C "$STRICT_KWARGS_COMPLETENESS_CHECKOUT" remote add origin "$REPOSITORY_URL"
  git -C "$STRICT_KWARGS_COMPLETENESS_CHECKOUT" fetch --depth=1 origin "$REPOSITORY_REF"
  git -C "$STRICT_KWARGS_COMPLETENESS_CHECKOUT" checkout --detach --quiet FETCH_HEAD
fi

if [ -z "${STRICT_KWARGS_COMPLETENESS_PYTHON_ENV:-}" ]; then
  export STRICT_KWARGS_COMPLETENESS_PYTHON_ENV="$temp_dir/completeness-venv"
  uv venv --python "$COMPLETENESS_PYTHON" "$STRICT_KWARGS_COMPLETENESS_PYTHON_ENV"
  uv pip install --python "$STRICT_KWARGS_COMPLETENESS_PYTHON_ENV/bin/python" \
    --constraint "$COMPLETENESS_CONSTRAINTS" \
    -e "$STRICT_KWARGS_COMPLETENESS_CHECKOUT"
fi

export STRICT_KWARGS_COMPLETENESS_RUNS="${STRICT_KWARGS_COMPLETENESS_RUNS:-3}"
export STRICT_KWARGS_COMPLETENESS_REGENERATE_GOLDEN=1
export INSTA_UPDATE=always

cargo test --locked --test completeness \
  pinned_repository_diagnostics_match_golden_oracle -- --ignored --nocapture
