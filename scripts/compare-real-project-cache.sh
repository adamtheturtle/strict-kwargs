#!/usr/bin/env bash
#
# Compare two strict-kwargs binaries on a real project's warm-cache path.
#
# Usage:
#   scripts/compare-real-project-cache.sh \
#     BASELINE_BINARY CANDIDATE_BINARY PROJECT_ROOT [PYTHON_ENV]
#
# The script primes an isolated cache for each binary, proves that their exit
# status, JSON diagnostics, and warnings are byte-identical, then benchmarks
# the same end-to-end command in both orders to expose order effects. Set
# BENCH_RUNS or BENCH_WARMUP to override the per-order Hyperfine sample counts.

set -euo pipefail

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
  printf 'usage: %s BASELINE_BINARY CANDIDATE_BINARY PROJECT_ROOT [PYTHON_ENV]\n' "$0" >&2
  exit 2
fi

if ! command -v hyperfine >/dev/null 2>&1; then
  printf 'error: hyperfine is required (https://github.com/sharkdp/hyperfine)\n' >&2
  exit 2
fi

absolute_file() {
  local path="$1"
  local directory
  directory="$(cd "$(dirname "$path")" && pwd -P)"
  printf '%s/%s\n' "$directory" "$(basename "$path")"
}

absolute_directory() {
  (cd "$1" && pwd -P)
}

baseline_binary="$(absolute_file "$1")"
candidate_binary="$(absolute_file "$2")"
project_root="$(absolute_directory "$3")"
python_env=""
if [ "$#" -eq 4 ]; then
  python_env="$(absolute_directory "$4")"
fi

for binary in "$baseline_binary" "$candidate_binary"; do
  if [ ! -x "$binary" ]; then
    printf 'error: binary is not executable: %s\n' "$binary" >&2
    exit 2
  fi
done

bench_runs="${BENCH_RUNS:-20}"
bench_warmup="${BENCH_WARMUP:-3}"
temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/strict-kwargs-cache-bench.XXXXXX")"
trap 'rm -rf "$temp_dir"' EXIT

baseline_cache="$temp_dir/baseline-cache"
candidate_cache="$temp_dir/candidate-cache"
mkdir "$baseline_cache" "$candidate_cache"

run_check() {
  local binary="$1"
  local cache="$2"
  local stdout_file="$3"
  local stderr_file="$4"
  local status=0

  if [ -n "$python_env" ]; then
    "$binary" check \
      --project-root "$project_root" \
      --cache-dir "$cache" \
      --output-format json \
      --python "$python_env" \
      "$project_root" \
      >"$stdout_file" 2>"$stderr_file" || status=$?
  else
    "$binary" check \
      --project-root "$project_root" \
      --cache-dir "$cache" \
      --output-format json \
      "$project_root" \
      >"$stdout_file" 2>"$stderr_file" || status=$?
  fi

  if [ "$status" -gt 1 ]; then
    printf 'error: cache priming failed with status %s\n' "$status" >&2
    sed -n '1,80p' "$stderr_file" >&2
    return "$status"
  fi
  printf '%s\n' "$status"
}

baseline_status="$(
  run_check \
    "$baseline_binary" \
    "$baseline_cache" \
    "$temp_dir/baseline.json" \
    "$temp_dir/baseline.stderr"
)"
candidate_status="$(
  run_check \
    "$candidate_binary" \
    "$candidate_cache" \
    "$temp_dir/candidate.json" \
    "$temp_dir/candidate.stderr"
)"

if [ "$baseline_status" -ne "$candidate_status" ]; then
  printf 'error: exit status differs: baseline=%s candidate=%s\n' \
    "$baseline_status" "$candidate_status" >&2
  exit 1
fi
cmp "$temp_dir/baseline.json" "$temp_dir/candidate.json"
cmp "$temp_dir/baseline.stderr" "$temp_dir/candidate.stderr"

printf 'Validated identical status (%s), JSON diagnostics, and warnings.\n' \
  "$baseline_status"
printf 'Project: %s\n' "$project_root"
if [ -n "$python_env" ]; then
  printf 'Python environment: %s\n' "$python_env"
fi

shell_command() {
  local binary="$1"
  local cache="$2"
  if [ -n "$python_env" ]; then
    printf '%q ' \
      "$binary" check \
      --project-root "$project_root" \
      --cache-dir "$cache" \
      --output-format json \
      --python "$python_env" \
      "$project_root"
  else
    printf '%q ' \
      "$binary" check \
      --project-root "$project_root" \
      --cache-dir "$cache" \
      --output-format json \
      "$project_root"
  fi
  # shellcheck disable=SC2016  # Expanded later by Hyperfine's benchmark shell.
  printf '>/dev/null 2>/dev/null; status=$?; test "$status" -le 1'
}

baseline_command="$(shell_command "$baseline_binary" "$baseline_cache")"
candidate_command="$(shell_command "$candidate_binary" "$candidate_cache")"

printf '\nForward order (baseline, candidate):\n'
hyperfine \
  --warmup "$bench_warmup" \
  --runs "$bench_runs" \
  --shell bash \
  --command-name baseline "$baseline_command" \
  --command-name candidate "$candidate_command"

printf '\nReverse order (candidate, baseline):\n'
hyperfine \
  --warmup "$bench_warmup" \
  --runs "$bench_runs" \
  --shell bash \
  --command-name candidate "$candidate_command" \
  --command-name baseline "$baseline_command"
