# strict-kwargs

Fast enforcement of **keyword arguments at call sites**, without mypy or ty plugins.

Companion to [mypy-strict-kwargs](https://github.com/adamtheturtle/mypy-strict-kwargs) for teams that type-check with [ty](https://docs.astral.sh/ty/) (or want a fast standalone linter).

## Example

```python
def add(a: int, b: int) -> int:
    return a + b

add(a=1, b=2)  # OK
add(1, 2)  # strict-kwargs error: too many positional arguments
```

## Install

From PyPI:

```bash
pip install strict-kwargs
# or
uv tool install strict-kwargs
```

From source:

```bash
cargo install --path .
# or
pip install .
```

## Usage

```bash
strict-kwargs .                 # check a directory
strict-kwargs src/foo.py        # check a file
strict-kwargs --project-root .  # explicit project root for config
strict-kwargs --python .venv    # point the ty fallback at an environment
```

`--python` accepts a Python interpreter, a virtualenv directory, or a
`sys.prefix` directory (mirrors `ty check --python`). Use it when third-party
packages live in an environment strict-kwargs/`ty` would not otherwise find
(Conda, a venv outside the project, system site-packages). It is forwarded to
the `ty` fallback only; the built-in resolver and embedded builtins/stdlib are
unaffected, and an invalid path simply disables the fallback rather than
producing wrong diagnostics.

With ty:

```bash
ty check
strict-kwargs .
```

Exit codes: `0` = clean, `1` = violations found, `2` = internal error.

## pre-commit

Run strict-kwargs automatically with [pre-commit](https://pre-commit.com/).
Add this to your project's `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs
    rev: 2026.5.16.post1  # pin to the latest release tag
    hooks:
      - id: strict-kwargs
```

Then:

```bash
pre-commit install
pre-commit run --all-files
```

Pin `rev` to a published release tag (see
[Releases](https://github.com/adamtheturtle/strict-kwargs/releases)) and let
[`pre-commit autoupdate`](https://pre-commit.com/#pre-commit-autoupdate) bump
it.

The hook builds strict-kwargs' maturin wheel in an isolated environment, so
the machine running the hook needs a [Rust
toolchain](https://rustup.rs/). Pass extra arguments (config flags, paths)
with `args:` as usual; by default the hook checks the staged Python files.

## Configuration

In `pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
debug = false
```

The same `ignore_names` entries as mypy-strict-kwargs work (fully-qualified names like `package.module.func`).

## Standard library & third-party resolution

Builtins and the standard library resolve against a pinned copy of
[typeshed](https://github.com/python/typeshed) vendored under
`vendored/typeshed/` and embedded in the binary (no Python environment
required). Third-party packages resolve from the active virtualenv's
`site-packages` (PEP 561), like `ty`/pyright. Re-exports (`from .impl import
name` in a package `__init__`, `from x import *`) are followed, including
chains, so APIs exposed through a package root resolve correctly.

This indexing runs once per invocation and walks the import closure of the
checked files (capped for safety), so checking files that import large
portions of the stdlib costs a fraction of a second of one-time work.

Maintainers: see [`vendored/typeshed/README.md`](vendored/typeshed/README.md)
for the documented update process. To bump the pinned stubs:

```bash
scripts/update-typeshed.sh            # latest typeshed main
scripts/update-typeshed.sh <git-ref>  # a specific commit/tag
```

## Type-inference resolution via `ty`

For calls the built-in resolver cannot resolve without type inference —
methods reached through inheritance/MRO, receivers typed by a return
annotation or a parameter annotation, locals bound from a call — strict-kwargs
falls back to [`ty`](https://docs.astral.sh/ty/). If a `ty` executable is on
`PATH`, a `ty server` (LSP) subprocess is driven to resolve the callee's
definition, and the strict-kwargs rule is applied to it. This brings
detection close to the mypy-strict-kwargs plugin for ordinary OO code.

This is **optional and additive**: with no `ty` installed the tool works
exactly as before (built-in resolver + vendored typeshed), just without the
inference-dependent cases. `ty` is pre-1.0; its resolution/LSP behavior may
change between versions. The fallback fails closed — any error yields no
diagnostic rather than a wrong one — and adds a `ty`-server round-trip per
otherwise-unresolved call.

If your third-party packages are not in an activated virtualenv, a Conda env,
or `<project>/.venv`, pass `--python <interpreter|venv|sys.prefix>`: it is
forwarded to `ty server` over LSP so the fallback resolves those imports,
without you editing `ty`'s own config. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) ("Forwarding an explicit
environment") for the mechanism and stability notes.

## Limitations

This tool's built-in engine uses static analysis (Ruff's Python parser), not a type checker; the optional `ty` fallback adds real inference for the common gaps but is not a full reimplementation of the mypy plugin. Overloads are handled permissively (a call is flagged only if it exceeds every candidate signature), `sys.version_info`/`sys.platform` stub branches are not evaluated, and dynamic callables / runtime-computed `__all__` are still not caught. Without `ty` on `PATH`, inheritance/return-type/annotation-typed receivers are not resolved.

## Architecture & current state

For the full resolution pipeline (built-in resolver + embedded typeshed + the
optional `ty` inference fallback), the support matrix, parity status, and the
honest limitations, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Development

This repo enforces a strict lint culture (pedantic + nursery Clippy with a
small documented allow-list, `-D warnings`, doc coverage, spell-checking, and
workflow hardening). Install the git hooks once so you catch the same failures
locally that CI enforces:

```bash
prek install        # or: pre-commit install
```

That wires up both the **pre-commit** stage (fast: `cargo fmt`, `cargo clippy
-D warnings`, `typos`, `actionlint`, `zizmor`, file hygiene) and the
**pre-push** stage (`cargo test`). Run everything on demand:

```bash
prek run --all-files                          # commit-stage hooks
prek run --all-files --hook-stage pre-push    # cargo test
```

The lint configuration lives in `Cargo.toml` (`[lints]`), `clippy.toml`,
`rustfmt.toml`, `_typos.toml`, and `.pre-commit-config.yaml`. CI runs the same
hooks (`.github/workflows/lint.yml`), so there is no local/CI drift.

## License

MIT
