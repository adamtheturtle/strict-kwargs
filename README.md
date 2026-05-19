[![Build Status](https://github.com/adamtheturtle/strict-kwargs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/adamtheturtle/strict-kwargs/actions)
[![PyPI](https://badge.fury.io/py/strict-kwargs.svg)](https://badge.fury.io/py/strict-kwargs)

# strict-kwargs

Enforce using keyword arguments where possible.

For example, if we have a function which takes two regular arguments, there
are three ways to call it. With this tool, only the form where keyword
arguments are used is accepted.

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

- In the same spirit as a formatter - think `black` or `ruff format` - this
  lets you stop spending time discussing whether a particular function call
  should use keyword arguments.
- Sometimes positional arguments are best at first, and then more and more are
  added and code becomes unclear, without anyone stopping to refactor to
  keyword arguments.
- Type checkers give better errors when keyword arguments are used. For
  example, with positional arguments, you may see,
  `Argument 5 to "add" has incompatible type "str"; expected "int"`. This
  requires that you count the arguments to see which one is wrong. With named
  arguments, you get
  `Argument "e" to "add" has incompatible type "str"; expected "int"`.

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
strict-kwargs --python .venv .  # point type resolution at an environment
```

Exit codes are:

- `0`: clean
- `1`: violations found
- `2`: operational error

`fix` only rewrites calls it can name unambiguously. Ambiguous calls are
counted as declined.

Use `--python` to point third-party resolution at an interpreter, virtual
environment, or `sys.prefix`. Missing paths are errors. A missing `--python`
path is warned about and ignored.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs-pre-commit
    rev: 2026.5.19 # pin to a release tag
    hooks:
      - id: strict-kwargs
```

## Configuration

Configuration lives in `pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
```

This is useful especially for builtins which can look strange with keyword
arguments. For example, `str(object=1)` is not idiomatic.

To find the name of a function to ignore, set the following configuration:

```toml
[tool.strict_kwargs]
debug = true
```

Then run `strict-kwargs` and look for the debug output.

## Comparison with mypy-strict-kwargs

[mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) is a
`mypy` plugin that enforces the same rule during type checking.

Use `strict-kwargs` if you type-check with [ty](https://docs.astral.sh/ty/), if
you prefer a standalone linter without plugins, or if you want automatic
rewrites with `strict-kwargs fix`.
