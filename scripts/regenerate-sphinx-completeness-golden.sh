#!/usr/bin/env bash
#
# Regenerate the Sphinx completeness oracle files from the pinned checkout
# used by tests/sphinx_completeness.rs.
set -euo pipefail

cd "$(dirname "$0")/.."

SPHINX_REF="cc7c6f435ad37bb12264f8118c8461b230e6830c"
SPHINX_PYTHON="${STRICT_KWARGS_SPHINX_PYTHON:-3.13}"
SPHINX_TY_VERSION="${STRICT_KWARGS_SPHINX_TY_VERSION:-0.0.44}"
temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT

ty_bin_dir="$temp_dir/ty-bin"
mkdir -p "$ty_bin_dir"
cat > "$ty_bin_dir/ty" <<EOF
#!/usr/bin/env bash
exec uv tool run --from "ty==$SPHINX_TY_VERSION" ty "\$@"
EOF
chmod +x "$ty_bin_dir/ty"
export PATH="$ty_bin_dir:$PATH"
ty version

if [ -z "${STRICT_KWARGS_SPHINX_CHECKOUT:-}" ]; then
  export STRICT_KWARGS_SPHINX_CHECKOUT="$temp_dir/sphinx"
  mkdir -p "$STRICT_KWARGS_SPHINX_CHECKOUT"
  git -C "$STRICT_KWARGS_SPHINX_CHECKOUT" init --quiet
  git -C "$STRICT_KWARGS_SPHINX_CHECKOUT" remote add origin https://github.com/sphinx-doc/sphinx.git
  git -C "$STRICT_KWARGS_SPHINX_CHECKOUT" fetch --depth=1 origin "$SPHINX_REF"
  git -C "$STRICT_KWARGS_SPHINX_CHECKOUT" checkout --detach --quiet FETCH_HEAD
fi

if [ -z "${STRICT_KWARGS_SPHINX_PYTHON_ENV:-}" ]; then
  export STRICT_KWARGS_SPHINX_PYTHON_ENV="$temp_dir/sphinx-venv"
  uv venv --python "$SPHINX_PYTHON" "$STRICT_KWARGS_SPHINX_PYTHON_ENV"
  uv pip install --python "$STRICT_KWARGS_SPHINX_PYTHON_ENV/bin/python" \
    -e "$STRICT_KWARGS_SPHINX_CHECKOUT"
fi

export STRICT_KWARGS_REGENERATE_SPHINX_GOLDEN=1
export STRICT_KWARGS_SPHINX_RUNS="${STRICT_KWARGS_SPHINX_RUNS:-3}"

cargo test --locked --test sphinx_completeness \
  regenerate_pinned_sphinx_golden_baseline -- --ignored --nocapture
