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

From PyPI (recommended — [`ty`](https://docs.astral.sh/ty/) is pulled in automatically as a dependency, so there is nothing else to install):

```bash
pip install strict-kwargs
# or
uv tool install strict-kwargs
```

From source:

```bash
pip install .            # also installs the bundled `ty` dependency
# or
cargo install --path .   # Cargo does not resolve the `ty` dependency —
                         # install it yourself: `uv tool install ty`
```

`ty` is a hard requirement: strict-kwargs locates it next to its own binary (where the PyPI install places it) or, failing that, on `PATH`.
If it cannot be found, strict-kwargs exits with an error rather than silently resolving fewer calls (see [Type-inference resolution via `ty`](#type-inference-resolution-via-ty)).

## Usage

```bash
strict-kwargs .                 # check a directory
strict-kwargs src/foo.py        # check a file
strict-kwargs --project-root .  # explicit project root for config
strict-kwargs --python .venv    # point the ty fallback at an environment
```

### Auto-fix

`strict-kwargs fix` rewrites positional call arguments to keyword arguments
for every violation it can resolve to a single known signature — project code
*and* the embedded typeshed builtins — much like `ruff check --fix`:

```bash
strict-kwargs fix .             # rewrite files in place
strict-kwargs fix src/foo.py    # fix a single file
strict-kwargs fix --diff .      # print the unified diff, write nothing
strict-kwargs fix --python .venv .   # detect ty-resolved violations too
```

```python
add(1, 2)     # ->  add(a=1, b=2)
add(1, b=2)   # ->  add(a=1, b=2)
```

The fixer is deliberately conservative: it never rewrites a call it would not report, and it leaves alone overloaded callees (so an overloaded builtin like `str` is reported but not rewritten), `*args` / `**kwargs` unpacking at the call site, and anything resolved only through the `ty` fallback (a wrong parameter name would corrupt source).
Positional-only parameters and arguments absorbed by `*args` stay positional.
Run `strict-kwargs fix` before `ty check`.

When `fix` declines a violation it does not stay silent: it prints how many violations it detected but left untouched, so `fix` then `check` is predictable — that count is exactly what a following `strict-kwargs` run (with the same `--python`) still reports.
`fix` accepts `--python` for the same reason `check` does: it steers the `ty` fallback so `fix` *detects* the same violations `check` would, making the reported count complete.
The rewrite itself never edits a `ty`-resolved call regardless.

`--python` accepts a Python interpreter, a virtualenv directory, or a `sys.prefix` directory (mirrors `ty check --python`).
Use it when third-party packages live in an environment strict-kwargs/`ty` would not otherwise find (Conda, a venv outside the project, system site-packages).
It is forwarded to the `ty` fallback only; the built-in resolver and embedded builtins/stdlib are unaffected, and an invalid path simply disables the fallback rather than producing wrong diagnostics.

With ty:

```bash
ty check
strict-kwargs .
```

Exit codes: `0` = clean, `1` = violations found, `2` = internal error.

## pre-commit

Run strict-kwargs automatically with [pre-commit](https://pre-commit.com/).
Use the [`strict-kwargs-pre-commit`](https://github.com/adamtheturtle/strict-kwargs-pre-commit) mirror, which installs the prebuilt wheel from PyPI (**no Rust toolchain required**).
Add this to your project's `.pre-commit-config.yaml`:

```yaml
repos:
  - repo: https://github.com/adamtheturtle/strict-kwargs-pre-commit
    rev: 2026.5.16.post1  # pin to the latest release tag
    hooks:
      - id: strict-kwargs
```

Then:

```bash
pre-commit install
pre-commit run --all-files
```

Each mirror tag installs the identically-versioned `strict-kwargs` release; pin `rev` to a published tag (see the mirror's [Releases](https://github.com/adamtheturtle/strict-kwargs-pre-commit/releases)) and let [`pre-commit autoupdate`](https://pre-commit.com/#pre-commit-autoupdate) bump it.
Pass extra arguments (config flags, paths) with `args:` as usual; by default the hook checks the staged Python files.

This repo also ships a `.pre-commit-hooks.yaml`, so pointing `repo:` at `https://github.com/adamtheturtle/strict-kwargs` directly works too — but that path builds the maturin wheel from source and needs a [Rust toolchain](https://rustup.rs/) on the machine running the hook.
Prefer the mirror unless you specifically want to track unreleased revisions.

## Configuration

In `pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
debug = false
```

The same `ignore_names` entries as mypy-strict-kwargs work (fully-qualified names like `package.module.func`).

## Standard library & third-party resolution

Builtins and the standard library resolve against a pinned copy of [typeshed](https://github.com/python/typeshed) vendored under `vendored/typeshed/` and embedded in the binary (no Python environment required).
Third-party packages resolve from the active virtualenv's `site-packages` (PEP 561), like `ty`/pyright.
Re-exports (`from .impl import name` in a package `__init__`, `from x import *`, and module-level assignment aliases like `helper = _impl.real`) are followed, including chains, so APIs exposed through a package root resolve correctly.

This indexing runs once per invocation and walks the import closure of the checked files (capped for safety), so checking files that import large portions of the stdlib costs a fraction of a second of one-time work.

Maintainers: see [`vendored/typeshed/README.md`](vendored/typeshed/README.md) for the documented update process.
To bump the pinned stubs:

```bash
scripts/update-typeshed.sh            # latest typeshed main
scripts/update-typeshed.sh <git-ref>  # a specific commit/tag
```

## Type-inference resolution via `ty`

For calls the built-in resolver cannot resolve without type inference — methods reached through inheritance/MRO, receivers typed by a return annotation or a parameter annotation, locals bound from a call — strict-kwargs falls back to [`ty`](https://docs.astral.sh/ty/).
A `ty server` (LSP) subprocess is driven to resolve the callee's definition, and the strict-kwargs rule is applied to it.
This brings detection close to the mypy-strict-kwargs plugin for ordinary OO code.

`ty` is a **hard requirement**, declared as a dependency of the PyPI package (`ty>=0.0.23`, the version the integration is verified against) so a `pip`/`uv` install brings it along.
strict-kwargs looks for it next to its own binary first (where the wheel install places it — `uv tool install` does not put a dependency on `PATH`), then on `PATH` (for `cargo install` users, or an activated venv).
If it cannot be found, or its language server cannot be started, strict-kwargs exits with an error (code 2) instead of silently degrading — that keeps results deterministic, so the same source can never resolve fewer calls just because the machine running it happens to lack `ty`.
The server itself is still started **lazily** — only when a file has calls the built-in resolver could not resolve — so a fully-resolvable run does not pay ty's project-indexing startup cost.

`ty` is pre-1.0; its resolution/LSP behavior may change between versions.
Per-call resolution still fails closed — any error yields no diagnostic rather than a wrong one — and the fallback adds a `ty`-server round-trip per otherwise-unresolved call.

If your third-party packages are not in an activated virtualenv, a Conda env, or `<project>/.venv`, pass `--python <interpreter|venv|sys.prefix>`: it is forwarded to `ty server` over LSP so the fallback resolves those imports, without you editing `ty`'s own config.
See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) ("Forwarding an explicit environment") for the mechanism and stability notes.

## Limitations

This tool's built-in engine uses static analysis (Ruff's Python parser), not a type checker; the required `ty` fallback adds real inference for the common gaps but is not a full reimplementation of the mypy plugin.
Overloads are handled permissively (a call is flagged only if it exceeds every candidate signature), `sys.version_info`/`sys.platform` stub branches are not evaluated, and dynamic callables / runtime-computed `__all__` are still not caught.

## Architecture & current state

For the full resolution pipeline (built-in resolver + embedded typeshed + the required `ty` inference fallback), the support matrix, parity status, and the honest limitations, see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Development

This repo enforces a strict lint culture (pedantic + nursery Clippy with a small documented allow-list, `-D warnings`, doc coverage, spell-checking, and workflow hardening).
Install the git hooks once so you catch the same failures locally that CI enforces:

```bash
prek install        # or: pre-commit install
```

That wires up both the **pre-commit** stage (fast: `cargo fmt`, `cargo clippy -D warnings`, `typos`, `actionlint`, `zizmor`, file hygiene) and the **pre-push** stage (`cargo test`).
Run everything on demand:

```bash
prek run --all-files                          # commit-stage hooks
prek run --all-files --hook-stage pre-push    # cargo test
```

`ty` is a runtime requirement of strict-kwargs, so the test suite needs it on `PATH` too — install it once before running `cargo test` (or `git push`):

```bash
uv tool install ty
```

The lint configuration lives in `Cargo.toml` (`[lints]`), `clippy.toml`, `rustfmt.toml`, `_typos.toml`, and `.pre-commit-config.yaml`.
CI runs the same hooks (`.github/workflows/lint.yml`), so there is no local/CI drift.

## License

MIT
