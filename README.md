[![Build Status](https://github.com/adamtheturtle/strict-kwargs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/adamtheturtle/strict-kwargs/actions)
[![PyPI](https://badge.fury.io/py/strict-kwargs.svg)](https://badge.fury.io/py/strict-kwargs)

# strict-kwargs

Enforce using keyword arguments where possible.

`strict-kwargs` is a standalone CLI implemented in Rust.
It parses Python with Ruff's Python parser and AST crates, then uses its own resolver plus [ty](https://docs.astral.sh/ty/) for type-aware call resolution where static names alone are not enough.

For example, if we have a function which takes two regular arguments, there are three ways to call it.
With this tool, only the form where keyword arguments are used is accepted.

```python
"""Showcase errors when calling a function without naming the arguments."""


def add(a: int, b: int) -> int:
    """Add two numbers."""
    return a + b


add(a=1, b=2)  # OK
add(1, 2)  # strict-kwargs reports this
add(1, b=2)  # strict-kwargs reports this
```

## Why?

- In the same spirit as a formatter - think `black` or `ruff format` - this lets you stop spending time discussing whether a particular function call should use keyword arguments.
- Sometimes positional arguments are best at first, and then more and more are added and code becomes unclear, without anyone stopping to refactor to keyword arguments.
- Type checkers give better errors when keyword arguments are used.
  For example, with positional arguments, you may see, `Argument 5 to "add" has incompatible type "str"; expected "int"`.
  This requires that you count the arguments to see which one is wrong.
  With named arguments, you get `Argument "e" to "add" has incompatible type "str"; expected "int"`.

## How it works

`strict-kwargs` has two resolution layers:

- A built-in resolver parses checked files, first-party modules, vendored typeshed stubs, and discovered site-packages using Ruff's Python parser and AST crates.
- For calls that need richer inference, `strict-kwargs` asks ty's language server for hover and definition information.
  `ty` is a required dependency of the Python package so results do not depend on whether it happens to be installed separately.

The fixer uses the same detection path, but only rewrites calls when the target parameter names are known unambiguously.

## Installation

```shell
uv tool install strict-kwargs
```

or:

```shell
pip install strict-kwargs
```

This is tested on Python 3.11+.

## Usage

```shell
strict-kwargs .                 # check a directory
strict-kwargs fix .             # rewrite positional args to keyword args in place
strict-kwargs fix --diff .      # preview the rewrite, write nothing
strict-kwargs fix --fix-synthesized-constructors .  # opt into one declined category
strict-kwargs --python .venv .  # point type resolution at an environment
strict-kwargs --cache-dir .strict-kwargs-cache .  # enable the diagnostic cache
```

Exit codes are:

- `0`: clean
- `1`: violations found
- `2`: operational error

`fix` only rewrites calls it can name unambiguously. Ambiguous calls are
counted as declined.
Single-signature calls are rewritten by default, including calls that require deeper type inference.
Overloaded calls are rewritten by default only when analysis selects one precise overload arm and the rewritten argument types are precise enough.
Synthesized constructors are the only opt-in category, because generated constructor models can differ from runtime behaviour when class construction is customized.

- `--fix-synthesized-constructors`: rewrite dataclass and `NamedTuple` constructors whose signatures were synthesized from fields.
  These can differ from runtime behaviour when class construction is customized.

Use `--python` to point third-party resolution at an interpreter, virtual environment, or `sys.prefix`.
Missing paths are errors.
A missing `--python` path is warned about and ignored.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs-pre-commit
    rev: 2026.5.19.post3  # pin to a release tag
    hooks:
      - id: strict-kwargs
```

## Configuration

Configuration lives in `pyproject.toml`:

```toml
[tool.strict_kwargs]
required_version = ">=2026.5.19-post.3"
ignore_names = ["main.func", "builtins.str"]
cache_dir = ".strict-kwargs-cache"
fix_synthesized_constructors = true
```

Set `required_version` to make older or incompatible `strict-kwargs` binaries fail fast when they read this project configuration.
Supported specifiers are exact versions, such as `2026.5.19-post.3`, and minimum versions, such as `>=2026.5.19-post.3`.
Use the version reported by `strict-kwargs --version`.

This is useful especially for builtins which can look strange with keyword arguments.
For example, `str(object=1)` is not idiomatic.
Set `cache_dir` to enable the persistent diagnostic cache for `strict-kwargs`
checks. Relative `cache_dir` values in `pyproject.toml` are resolved against
the project root. The cache location precedence is:
`--cache-dir`, then `[tool.strict_kwargs].cache_dir`, then
`STRICT_KWARGS_CACHE_DIR`. If none are set, the cache is disabled.
Set `fix_synthesized_constructors = true` to make `strict-kwargs fix` rewrite dataclass and `NamedTuple` constructors without passing `--fix-synthesized-constructors` each time.

To find the name of a function to ignore, set the following configuration:

```toml
[tool.strict_kwargs]
debug = true
```

Then run `strict-kwargs` and look for the debug output.

## Comparison with mypy-strict-kwargs

[mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) is a `mypy` plugin that enforces the same rule during type checking.

Use `strict-kwargs` if you type-check with [ty](https://docs.astral.sh/ty/), if you prefer a standalone linter without plugins, or if you want automatic rewrites with `strict-kwargs fix`.
