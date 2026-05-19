# strict-kwargs

Fast enforcement of keyword arguments at call sites, without a mypy plugin.
Detects positional arguments and rewrites them to keyword arguments automatically.

```python
def add(a: int, b: int) -> int: ...

add(a=1, b=2)  # OK
add(1, 2)      # error: too many positional args  ->  fix rewrites to add(a=1, b=2)
```

## Install

```bash
uv tool install strict-kwargs   # or: pip install strict-kwargs
```

## Usage

```bash
strict-kwargs .                 # check a directory (exit 0 = clean, 1 = violations, 2 = error)
strict-kwargs fix .             # rewrite positional args to keyword args in place
strict-kwargs fix --diff .      # preview the rewrite, write nothing
strict-kwargs --python .venv .  # point the ty fallback at an environment
```

- `fix` only rewrites calls it can name unambiguously; ambiguous calls are counted as declined.
- Use `--python` to point third-party resolution at an interpreter, venv, or `sys.prefix`.
- Missing paths are errors. A missing `--python` path is warned about and ignored.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs-pre-commit
    rev: 2026.5.19  # pin to a release tag
    hooks:
      - id: strict-kwargs
```

Use the [mirror](https://github.com/adamtheturtle/strict-kwargs-pre-commit) (prebuilt wheel, no Rust toolchain).
Pointing `repo:` at this repository works too but builds from source.

## Configuration

In `pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]  # fully-qualified names to ignore
debug = false                                  # set true to show resolved fully-qualified names
```

## Comparison with mypy-strict-kwargs

[mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) is a mypy plugin that enforces the same rule.
Use strict-kwargs if you type-check with [ty](https://docs.astral.sh/ty/) or prefer a standalone linter without plugins.
