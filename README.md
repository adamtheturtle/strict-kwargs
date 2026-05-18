# strict-kwargs

Fast enforcement of **keyword arguments at call sites**, without mypy or ty plugins.
Companion to [mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs); a fast standalone linter for teams that type-check with [ty](https://docs.astral.sh/ty/).

```python
def add(a: int, b: int) -> int: ...

add(a=1, b=2)  # OK
add(1, 2)      # error: too many positional args  ->  fix rewrites to add(a=1, b=2)
```

## Install

```bash
uv tool install strict-kwargs   # or: pip install strict-kwargs
```

[`ty`](https://docs.astral.sh/ty/) is a hard requirement and is pulled in automatically as a dependency of the PyPI package.
strict-kwargs locates it next to its own binary or on `PATH`; if it cannot be found it exits with an error rather than silently resolving fewer calls.
`cargo install` does not pull it — then install it yourself with `uv tool install ty`.

## Usage

```bash
strict-kwargs .                 # check a directory (exit 0 = clean, 1 = violations, 2 = error)
strict-kwargs fix .             # rewrite positional args to keyword args in place
strict-kwargs fix --diff .      # preview the rewrite, write nothing
strict-kwargs --python .venv .  # point the ty fallback at an environment
```

`fix` is conservative: it never rewrites a call it would not report, and leaves overloaded callees, `*args`/`**kwargs` unpacking, and `ty`-only-resolved calls untouched (reporting how many it declined).
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
ignore_names = ["main.func", "builtins.str"]  # fully-qualified, as in mypy-strict-kwargs
debug = false
```

A missing `pyproject.toml`, or one without a `[tool.strict_kwargs]` table, is fine and uses the defaults.
A `pyproject.toml` that exists but cannot be parsed, or whose `[tool.strict_kwargs]` has the wrong shape or value types (e.g. `ignore_names` not a list), is a hard error (exit 2) rather than a silent fall back to defaults.

## Architecture

A built-in resolver (Ruff parser + embedded [typeshed](https://github.com/python/typeshed), offline) with a required `ty` inference fallback for the cases static analysis cannot resolve (inheritance/MRO, return/annotation-typed receivers, locals bound from calls).
For the full pipeline, capability matrix, parity status, and limitations, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Development

```bash
prek install         # or: pre-commit install — runs the same hooks as CI
uv tool install ty   # the test suite needs `ty` on PATH
```

`prek run --all-files` runs the commit hooks; `--hook-stage pre-push` runs `cargo test`.

## License

MIT
