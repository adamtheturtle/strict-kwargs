#!/usr/bin/env bash
#
# Regenerate a completeness snapshot from a pinned external repository used by
# tests/completeness.rs.
set -euo pipefail

cd "$(dirname "$0")/.."

CASE="${1:-${STRICT_KWARGS_COMPLETENESS_CASE:-sphinx}}"
COMPLETENESS_PYTHON="${STRICT_KWARGS_COMPLETENESS_PYTHON:-3.13}"
COMPLETENESS_TY_VERSION="${STRICT_KWARGS_COMPLETENESS_TY_VERSION:-0.0.44}"
COMPLETENESS_CONSTRAINTS="${STRICT_KWARGS_COMPLETENESS_CONSTRAINTS:-tests/golden/completeness-requirements-constraints.txt}"
temp_dir="$(mktemp -d)"
trap 'rm -rf "$temp_dir"' EXIT

case "$CASE" in
  sphinx)
    REPOSITORY_NAME="${STRICT_KWARGS_COMPLETENESS_SPHINX_REPOSITORY_NAME:-${STRICT_KWARGS_COMPLETENESS_REPOSITORY_NAME:-sphinx}}"
    REPOSITORY_URL="${STRICT_KWARGS_COMPLETENESS_SPHINX_REPOSITORY_URL:-${STRICT_KWARGS_COMPLETENESS_REPOSITORY_URL:-https://github.com/sphinx-doc/sphinx.git}}"
    REPOSITORY_REF="${STRICT_KWARGS_COMPLETENESS_SPHINX_REPOSITORY_REF:-${STRICT_KWARGS_COMPLETENESS_REPOSITORY_REF:-cc7c6f435ad37bb12264f8118c8461b230e6830c}}"
    TEST_NAME="pinned_repository_diagnostics_match_golden_oracle"
    NEEDS_PYTHON_ENV=1
    INSTALLS_CHECKOUT=1
    ;;
  cpython)
    REPOSITORY_NAME="${STRICT_KWARGS_COMPLETENESS_CPYTHON_REPOSITORY_NAME:-cpython}"
    REPOSITORY_URL="${STRICT_KWARGS_COMPLETENESS_CPYTHON_REPOSITORY_URL:-https://github.com/python/cpython.git}"
    REPOSITORY_REF="${STRICT_KWARGS_COMPLETENESS_CPYTHON_REPOSITORY_REF:-8b31d08e62b9714cf8dd1d8b19afa5ecbad2414a}"
    TEST_NAME="cpython_repository_diagnostics_match_golden_oracle"
    # An *empty* pinned-version venv (no editable install): without an
    # explicit environment ty discovers whatever interpreter the host
    # exposes, and any third-party site-packages on the regenerating
    # machine would leak environment-dependent entries into the snapshot
    # that CI runners can never reproduce.
    NEEDS_PYTHON_ENV=1
    INSTALLS_CHECKOUT=0
    ;;
  *)
    printf 'error: unknown completeness case %q (expected sphinx or cpython)\n' "$CASE" >&2
    exit 2
    ;;
esac

case_prefix="STRICT_KWARGS_COMPLETENESS_$(printf '%s' "$CASE" | tr '[:lower:]' '[:upper:]')"
export "${case_prefix}_REPOSITORY_NAME=$REPOSITORY_NAME"
export "${case_prefix}_REPOSITORY_URL=$REPOSITORY_URL"
export "${case_prefix}_REPOSITORY_REF=$REPOSITORY_REF"

ty_bin_dir="$temp_dir/ty-bin"
mkdir -p "$ty_bin_dir"
cat > "$ty_bin_dir/ty" <<EOF
#!/usr/bin/env bash
exec uv tool run --from "ty==$COMPLETENESS_TY_VERSION" ty "\$@"
EOF
chmod +x "$ty_bin_dir/ty"
export PATH="$ty_bin_dir:$PATH"
ty version

checkout_var="${case_prefix}_CHECKOUT"
python_env_var="${case_prefix}_PYTHON_ENV"

if [ "$CASE" = "sphinx" ] && [ -z "${!checkout_var:-}" ] && [ -n "${STRICT_KWARGS_COMPLETENESS_CHECKOUT:-}" ]; then
  export "$checkout_var=$STRICT_KWARGS_COMPLETENESS_CHECKOUT"
fi

if [ "$CASE" = "sphinx" ] && [ -z "${!python_env_var:-}" ] && [ -n "${STRICT_KWARGS_COMPLETENESS_PYTHON_ENV:-}" ]; then
  export "$python_env_var=$STRICT_KWARGS_COMPLETENESS_PYTHON_ENV"
fi

if [ -z "${!checkout_var:-}" ]; then
  export "$checkout_var=$temp_dir/$REPOSITORY_NAME"
  mkdir -p "${!checkout_var}"
  git -C "${!checkout_var}" init --quiet
  git -C "${!checkout_var}" remote add origin "$REPOSITORY_URL"
  git -C "${!checkout_var}" fetch --depth=1 origin "$REPOSITORY_REF"
  git -C "${!checkout_var}" checkout --detach --quiet FETCH_HEAD
fi

if [ "$NEEDS_PYTHON_ENV" -eq 1 ] && [ -z "${!python_env_var:-}" ]; then
  export "$python_env_var=$temp_dir/completeness-venv"
  uv venv --python "$COMPLETENESS_PYTHON" "${!python_env_var}"
  if [ "$INSTALLS_CHECKOUT" -eq 1 ]; then
    uv pip install --python "${!python_env_var}/bin/python" \
      --constraint "$COMPLETENESS_CONSTRAINTS" \
      -e "${!checkout_var}"
  fi
fi

export STRICT_KWARGS_COMPLETENESS_RUNS="${STRICT_KWARGS_COMPLETENESS_RUNS:-3}"
export STRICT_KWARGS_COMPLETENESS_REGENERATE_GOLDEN=1
export INSTA_UPDATE=always

cargo test --locked --test completeness \
  "$TEST_NAME" -- --ignored --nocapture
