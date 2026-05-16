# strict-kwargs

Fast enforcement of **keyword arguments at call sites**, without mypy or ty plugins.

This is the companion to [mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) for teams that type-check with [ty](https://docs.astral.sh/ty/) (or only want a fast linter for this rule).

## Example

```python
def add(a: int, b: int) -> int:
    return a + b

add(a=1, b=2)  # OK
add(1, 2)  # strict-kwargs error: too many positional arguments
```

## Install

Build from source (PyPI packaging can be added later):

```bash
cargo install --path .
```

Or run without installing:

```bash
cargo run --bin strict-kwargs -- check .
```

## Usage

```bash
strict-kwargs .                 # check a directory
strict-kwargs src/foo.py        # check a file
strict-kwargs --project-root .  # explicit project root for config
```

Exit codes: `0` = clean, `1` = violations found, `2` = internal error.

## Configuration

In `pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
debug = false
```

The same `ignore_names` entries as mypy-strict-kwargs work (fully-qualified names like `package.module.func`).

## With ty

Run both tools in CI:

```bash
ty check
strict-kwargs .
```

## Limitations

This tool uses static analysis (Ruff’s Python parser), not a type checker. It resolves many calls within a project but will not catch every case mypy’s plugin handles (dynamic callables, complex overloads, etc.).

## License

MIT
