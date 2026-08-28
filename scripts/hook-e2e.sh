#!/bin/sh
# Fail if the published pre-commit hook does not actually check files.
#
# `.pre-commit-hooks.yaml` is consumed only by *other* projects, never by this
# repo's own `.pre-commit-config.yaml`, which invokes the binary directly. So
# nothing here exercised it: when the CLI gained a required `check`
# subcommand, `entry: strict-kwargs` started parsing pre-commit's filenames as
# a subcommand name and every run exited 2 without checking anything. That
# shipped, unnoticed, for eight releases (PR #1291).
#
# This drives the hook the way a consumer does -- `repo:` pointing at this
# checkout, pinned to HEAD -- so a regression in the hook definition fails CI
# instead of a stranger's commit.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
rev=$(git -C "$repo" rev-parse HEAD)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

git init -q .
git config user.email hook-e2e@example.com
git config user.name hook-e2e

cat > .pre-commit-config.yaml <<EOF
repos:
  - repo: $repo
    rev: $rev
    hooks:
      - id: strict-kwargs
EOF

cat > pyproject.toml <<'EOF'
[project]
name = "consumer"
version = "0"
EOF

cat > clean.py <<'EOF'
def greet(*, name: str) -> str:
    return f"hello {name}"


greet(name="world")
EOF

git add -A
git commit -qm consumer

echo "==> a compliant file must pass"
if ! output=$(pre-commit run --all-files 2>&1); then
    echo "$output"
    echo
    echo "FAIL: the hook rejected a file with no violations."
    echo "If the output says 'unrecognized subcommand', the entry: in"
    echo ".pre-commit-hooks.yaml is missing the 'check' subcommand."
    exit 1
fi

echo "==> a positional-argument violation must be reported"
cat > bad.py <<'EOF'
def greet(name: str) -> str:
    return f"hello {name}"


greet("world")
EOF
git add bad.py

if output=$(pre-commit run --all-files 2>&1); then
    echo "$output"
    echo
    echo "FAIL: the hook passed a file that calls greet() positionally."
    exit 1
fi

# A non-zero exit is necessary but not sufficient: a hook that crashes also
# exits non-zero. Insist on the diagnostic itself.
if ! echo "$output" | grep -q 'KW001'; then
    echo "$output"
    echo
    echo "FAIL: the hook failed without producing a KW001 diagnostic --"
    echo "it errored out rather than checking the file."
    exit 1
fi

echo "==> ok: clean file passed, violation reported as KW001"
