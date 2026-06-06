[![Build Status](https://github.com/adamtheturtle/strict-kwargs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/adamtheturtle/strict-kwargs/actions)
[![PyPI](https://badge.fury.io/py/strict-kwargs.svg)](https://badge.fury.io/py/strict-kwargs)

# strict-kwargs

Enforce using keyword arguments where possible.

`strict-kwargs` is a standalone CLI implemented in Rust.

For example, if we have a function which takes two regular arguments, there are three ways to call it.
With this tool, only the form where keyword arguments are used is accepted.

```python
"""Showcase errors when calling a function without naming the arguments."""


def add(a: int, b: int) -> int:
    """Add two numbers."""
    return a + b


add(a=1, b=2)  # OK
add(1, 2)  # strict-kwargs reports this; strict-kwargs check --fix can rewrite it
add(1, b=2)  # strict-kwargs reports this; strict-kwargs check --fix can rewrite it
```

## Why?

- Like a formatter, such as `black` or `ruff format`, this lets you stop discussing whether a particular function call should use keyword arguments.
- Positional arguments can be fine at first.
  As more are added, calls can become unclear without anyone stopping to refactor them to keyword arguments.
- Type checkers give better errors when keyword arguments are used.
  For example, with positional arguments, you may see, `Argument 5 to "add" has incompatible type "str"; expected "int"`.
  This requires that you count the arguments to see which one is wrong.
  With named arguments, you get `Argument "e" to "add" has incompatible type "str"; expected "int"`.

## Installation

```shell
uv tool install strict-kwargs
```

or:

```shell
pip install strict-kwargs
```

or, as a Rust crate:

```shell
cargo install strict-kwargs
```

`cargo install` installs only the `strict-kwargs` binary; install `ty` separately and keep it on `PATH`.

This is tested on Python 3.11+.

## Usage

```shell
strict-kwargs check .                         # check a directory
strict-kwargs check --output-format json .    # emit check diagnostics as JSON
strict-kwargs check --output-format github .  # emit GitHub Actions annotations
strict-kwargs check --fix .                   # rewrite positional args in place
strict-kwargs check --diff .                  # preview fixes, write nothing
strict-kwargs check --fix --unsafe-fixes .    # include behavior-changing fixes
strict-kwargs check --python .venv .          # point type resolution at an environment
strict-kwargs check --cache-dir .strict-kwargs-cache .  # enable the diagnostic cache
```

Exit codes are:

- `0`: clean
- `1`: violations found
- `2`: operational error

### Output

- `full`, the default check output, writes Ruff-style diagnostics and summaries to stdout.
- `json` and `github` write diagnostics to stdout so machine consumers can read them without mixing in operational messages.
- Warnings and operational errors are always written to stderr.
- `check --diff` writes the unified diff to stdout and its summary to stderr.

### Fix behavior

`check --fix` only rewrites calls whose target parameter names are known unambiguously.
Ambiguous calls are counted as declined.

By default, `check --fix` rewrites:

- single-signature calls
- overloaded calls, when one precise overload arm can be selected and the rewritten argument types are precise enough

Synthesized constructors are treated as unsafe fixes because generated constructor models can differ from runtime behaviour when class construction is customized.

- `--unsafe-fixes`: include dataclass and `NamedTuple` constructor calls whose signatures were synthesized from fields.

### Python environment

Use `--python` to point third-party resolution at an interpreter, virtual environment, or `sys.prefix`.

- Missing paths are errors.
- A missing `--python` path is warned about and ignored.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs-pre-commit
    rev: 2026.6.4  # pin to a release tag
    hooks:
      - id: strict-kwargs
```

## Configuration

Configuration lives in `pyproject.toml`:

```toml
[tool.strict_kwargs]
required_version = ">=2026.5.19-post.3"
ignore_names = ["main.func", "builtins.str"]
src = ["src"]
namespace_packages = ["src/airflow/providers"]
extend_exclude = ["generated", "vendor"]
force_exclude = true
cache_dir = ".strict-kwargs-cache"
fix_synthesized_constructors = true
output_format = "full"  # or "json", "github"
```

Set `required_version` to make older or incompatible `strict-kwargs` binaries fail fast when they read this project configuration.
Supported specifiers are exact versions, such as `2026.5.19-post.3`, and minimum versions, such as `>=2026.5.19-post.3`.
Use the version reported by `strict-kwargs --version`.

### Ignored functions

Use `ignore_names` for functions that should still allow positional arguments.
This is useful especially for builtins which can look strange with keyword arguments.

For example, `str(object=1)` is not idiomatic.

### Suppressing individual findings

Add a Ruff-style `# noqa` comment to the line a diagnostic is reported on (the first line of the offending call) to suppress it:

```python
func(1, 2, 3)  # noqa: KW001
```

- `# noqa: KW001` suppresses only `KW001`. A directive naming other codes (for example `# noqa: E501`) leaves the call reported.
- A bare `# noqa` suppresses every finding on the line, matching Ruff.
- Suppressed calls are skipped by `--fix` too, so a `# noqa` call is never rewritten.

For a call spanning multiple lines, put the comment on the first line — the line the `path:line:col` output points at:

```python
func(  # noqa: KW001
    1,
    2,
    3,
)
```

#### Using `# noqa` alongside Ruff

If you also run Ruff with `RUF100` (unused `noqa`) enabled, prefer the coded form `# noqa: KW001`: Ruff leaves a directive whose only codes it does not recognise untouched, but it will remove a bare `# noqa` it considers unused. To keep `KW001` from being stripped when it shares a directive with a Ruff code (for example `# noqa: E501, KW001`), declare it as an external code:

```toml
[tool.ruff.lint]
external = ["KW001"]
```

### Source discovery

Set `src` to source-code directories that should be:

- searched for first-party imports
- stripped when deriving module names

Relative paths are resolved against the project root.
For example, `src = ["src"]` maps `src/pkg/mod.py` to `pkg.mod` while preserving the repository root as a fallback source root.

Set `namespace_packages` to directories that should be treated as namespace packages for module resolution even when they have no `__init__.py`.

### Exclusions

Use `extend_exclude` to skip generated or vendored Python files during directory runs.

- Patterns use `.gitignore`-style matching relative to the project root.
- By default, exclusions apply to directory traversal only.
- An explicitly passed file, such as `strict-kwargs check generated/api.py`, is still checked.
- Set `force_exclude = true` to apply exclusions to explicitly passed files too.
  This is useful when pre-commit passes changed files directly.
- The built-in skips for dot-directories, `venv`, and `__pycache__` remain enabled.

### Cache

Set `cache_dir` to enable the persistent diagnostic cache for `strict-kwargs` checks.
Relative `cache_dir` values in `pyproject.toml` are resolved against the project root.

The cache location precedence is:

1. `--cache-dir`
2. `[tool.strict_kwargs].cache_dir`
3. `STRICT_KWARGS_CACHE_DIR`

If none are set, the cache is disabled.

### Fix defaults

Set `fix_synthesized_constructors = true` to make `strict-kwargs check --fix` include dataclass and `NamedTuple` constructor rewrites without passing `--unsafe-fixes` each time.

To find the name of a function to ignore, set the following configuration:

```toml
[tool.strict_kwargs]
debug = true
```

Then run `strict-kwargs check` and look for the debug output.

## Comparison with mypy-strict-kwargs

[mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) is a `mypy` plugin that enforces the same rule during type checking.

Use `strict-kwargs` if you:

- type-check with [ty](https://docs.astral.sh/ty/)
- prefer a standalone linter without plugins
- want automatic rewrites with `strict-kwargs check --fix`
