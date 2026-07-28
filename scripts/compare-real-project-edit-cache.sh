#!/usr/bin/env bash
#
# Compare two strict-kwargs binaries on the first check after one controlled
# project or dependency edit.
#
# Usage:
#   scripts/compare-real-project-edit-cache.sh \
#     BASELINE_BINARY CANDIDATE_BINARY PROJECT_ROOT \
#     EDIT_TARGET MUTATED_FILE [PYTHON_ENV]
#
# The target is restored when the script exits. Before every timed command,
# both isolated caches are primed with the target's original contents and the
# mutated file is then copied over it. Cache priming and mutation are outside
# the timed interval.

set -euo pipefail

if [ "$#" -lt 5 ] || [ "$#" -gt 6 ]; then
  printf 'usage: %s BASELINE_BINARY CANDIDATE_BINARY PROJECT_ROOT EDIT_TARGET MUTATED_FILE [PYTHON_ENV]\n' "$0" >&2
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
edit_target="$(absolute_file "$4")"
mutated_file="$(absolute_file "$5")"
python_env=""
if [ "$#" -eq 6 ]; then
  python_env="$(absolute_directory "$6")"
fi

for binary in "$baseline_binary" "$candidate_binary"; do
  if [ ! -x "$binary" ]; then
    printf 'error: binary is not executable: %s\n' "$binary" >&2
    exit 2
  fi
done
for file in "$edit_target" "$mutated_file"; do
  if [ ! -f "$file" ]; then
    printf 'error: file does not exist: %s\n' "$file" >&2
    exit 2
  fi
done

bench_runs="${BENCH_RUNS:-10}"
bench_warmup="${BENCH_WARMUP:-1}"
temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/strict-kwargs-edit-bench.XXXXXX")"
original_file="$temp_dir/original"
cp "$edit_target" "$original_file"
trap 'cp "$original_file" "$edit_target"; rm -rf "$temp_dir"' EXIT

baseline_cache="$temp_dir/baseline-cache"
candidate_cache="$temp_dir/candidate-cache"
mkdir "$baseline_cache" "$candidate_cache"

check_command() {
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
}

run_check() {
  local binary="$1"
  local cache="$2"
  local stdout_file="$3"
  local stderr_file="$4"
  local status=0
  local command
  command="$(check_command "$binary" "$cache")"
  bash -c "$command" >"$stdout_file" 2>"$stderr_file" || status=$?
  if [ "$status" -gt 1 ]; then
    printf 'error: check failed with status %s\n' "$status" >&2
    sed -n '1,80p' "$stderr_file" >&2
    return "$status"
  fi
  printf '%s\n' "$status"
}

prepare_transition() {
  cp "$original_file" "$edit_target"
  run_check \
    "$baseline_binary" \
    "$baseline_cache" \
    "$temp_dir/prime-baseline.json" \
    "$temp_dir/prime-baseline.stderr" \
    >/dev/null
  run_check \
    "$candidate_binary" \
    "$candidate_cache" \
    "$temp_dir/prime-candidate.json" \
    "$temp_dir/prime-candidate.stderr" \
    >/dev/null
  cp "$mutated_file" "$edit_target"
}

prepare_transition
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

printf 'Validated identical post-edit status (%s), JSON diagnostics, and warnings.\n' \
  "$baseline_status"
printf 'Project: %s\n' "$project_root"
printf 'Edited target: %s\n' "$edit_target"
if [ -n "$python_env" ]; then
  printf 'Python environment: %s\n' "$python_env"
fi

baseline_command="$(check_command "$baseline_binary" "$baseline_cache")"
candidate_command="$(check_command "$candidate_binary" "$candidate_cache")"
baseline_command+=" >/dev/null 2>/dev/null; status=\$?; test \"\$status\" -le 1"
candidate_command+=" >/dev/null 2>/dev/null; status=\$?; test \"\$status\" -le 1"

printf -v prepare_command \
  '%q %q %q; %q check --project-root %q --cache-dir %q --output-format json ' \
  cp "$original_file" "$edit_target" \
  "$baseline_binary" "$project_root" "$baseline_cache"
if [ -n "$python_env" ]; then
  printf -v prepare_command \
    '%s--python %q ' \
    "$prepare_command" "$python_env"
fi
printf -v prepare_command \
  '%s%q >/dev/null 2>/dev/null || test "$?" -eq 1; %q check --project-root %q --cache-dir %q --output-format json ' \
  "$prepare_command" "$project_root" "$candidate_binary" "$project_root" "$candidate_cache"
if [ -n "$python_env" ]; then
  printf -v prepare_command \
    '%s--python %q ' \
    "$prepare_command" "$python_env"
fi
printf -v prepare_command \
  '%s%q >/dev/null 2>/dev/null || test "$?" -eq 1; %q %q %q' \
  "$prepare_command" "$project_root" cp "$mutated_file" "$edit_target"

printf '\nForward order (baseline, candidate):\n'
hyperfine \
  --prepare "$prepare_command" \
  --warmup "$bench_warmup" \
  --runs "$bench_runs" \
  --shell bash \
  --command-name baseline "$baseline_command" \
  --command-name candidate "$candidate_command"

printf '\nReverse order (candidate, baseline):\n'
hyperfine \
  --prepare "$prepare_command" \
  --warmup "$bench_warmup" \
  --runs "$bench_runs" \
  --shell bash \
  --command-name candidate "$candidate_command" \
  --command-name baseline "$baseline_command"
