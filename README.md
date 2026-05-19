# strict-kwargs

Fast enforcement of **keyword arguments at call sites**, without mypy or ty plugins.
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

`fix` is conservative: it never rewrites a call it would not report, and leaves `*args`/`**kwargs` unpacking, synthesized constructors, unmatched overloads, and ambiguous `ty` resolutions untouched (reporting how many it declined). `ty`-resolved calls are rewritten only when `ty` reports one concrete callable signature with complete parameter names; overloaded callees are rewritten only when the call-site hover selects one indexed overload arm and the rewritten arguments have precise literal or annotation types.
`--python` accepts an interpreter, venv, or `sys.prefix` (mirrors `ty check --python`) for third-party packages outside an activated venv or `<project>/.venv`.
A path that does not exist is a hard error (exit 2), like `ruff`, rather than a silent "clean" result; a nonexistent `--python` is reported on stderr and the run falls back to `ty`'s own environment discovery.

## pre-commit

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs-pre-commit
    rev: 2026.5.16.post1  # pin to a release tag
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
debug = false                                  # log debug info and AST dumps to stderr
```

A missing `pyproject.toml`, or one without a `[tool.strict_kwargs]` table, is fine and uses the defaults.
A `pyproject.toml` that exists but cannot be parsed, or whose `[tool.strict_kwargs]` has the wrong shape or value types (e.g. `ignore_names` not a list), is a hard error (exit 2) rather than a silent fall back to defaults.

## Comparison with mypy-strict-kwargs

[mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) is a mypy plugin that enforces the same rule.
Use strict-kwargs if you type-check with [ty](https://docs.astral.sh/ty/) or prefer a standalone linter without plugins.
