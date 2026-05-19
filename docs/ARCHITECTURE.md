# Architecture

`strict-kwargs` enforces one rule: do not pass an argument positionally when
the target callable accepts that argument as a keyword.

The implementation has two resolvers:

1. A built-in resolver that is fast, offline, and deterministic.
2. A required `ty` fallback for cases that need type inference.

Per-call fallback failures are permissive: no diagnostic is emitted. Backend
unavailability is not permissive: if `ty` cannot be found or its server cannot
start, the run exits with code 2.

## Call Checking

For each call site:

```text
call
  -> built-in name/import/scope resolution
  -> DefinitionIndex lookup
  -> apply the signature rule if a signature is found
  -> otherwise ask ty for hover/definition information
```

The signature rule lives in `src/signature.rs`. It understands
positional-only parameters, keyword-only parameters, defaults,
`*args`/`**kwargs`, `self`/`cls`, and the common descriptor and constructor
dunders.

Whole-project runs are split into two phases:

1. Parse and walk files in parallel with the built-in resolver.
2. Process deferred `ty` requests serially through one shared `ty server`.

Diagnostics are sorted before output so parallelism does not affect the
result.

## DefinitionIndex

`src/index.rs` stores discovered callable signatures by fully qualified name.
It eagerly indexes only builtins and files being checked. Other modules are
resolved and indexed on demand.

Resolution order is:

1. First-party files.
2. Vendored typeshed stdlib stubs.
3. Active environment `site-packages`, including PEP 561 packages and stubs.

The index follows normal imports, relative imports, package re-exports,
`from x import *`, and simple module-level assignment aliases. Re-export
resolution is lazy and bounded so unusual import graphs fail closed instead of
exploding.

Overloads are permissive in the built-in path: a call is reported only if it
exceeds every candidate signature. `ty` can be more precise because hover gives
the selected overload for a concrete call.

The index also synthesizes constructors for the class form of `@dataclass` and
`NamedTuple` when no explicit constructor is present.

## ty Fallback

`src/ty_resolver.rs` is a small JSON-RPC/LSP client for `ty server`.

It uses:

- `textDocument/hover` first, because hover usually contains the resolved
  callable signature with overloads, inheritance, and bound receivers handled.
- `textDocument/definition` as a secondary path, mainly for constructors.

Requests are pipelined per file. Timeouts and unparsable responses produce no
diagnostic for that call. The first in-run response failure disables further
fallback attempts to avoid repeated timeouts.

`ty` is located next to the `strict-kwargs` executable first, then on `PATH`.
The PyPI package depends on `ty`, so normal wheel installs provide it.

## Forwarding an explicit environment (`--python`)

`--python` accepts an interpreter, venv, or `sys.prefix` and is forwarded to
`ty server` through LSP `initialize` options:

```json
{ "configuration": { "environment": { "python": "<absolute path>" } } }
```

A nonexistent `--python` path is reported on stderr and ignored, so `ty` falls
back to its own environment discovery. Existing-but-invalid environments are
left for `ty` to interpret.

## Fixing

`strict-kwargs fix` runs the same detection path as `check`, then rewrites only
when the positional-to-keyword mapping is unambiguous.

It declines rewrites for unpacked calls, synthesized constructors,
goto-definition-only results, ambiguous callable displays, and unresolved or
multi-arm overloads. This keeps fixes conservative even when diagnostics can
still be reported.

## Limits

- Dynamic dispatch, `getattr`, runtime-computed `__all__`, decorator-rewritten
  signatures, and metaclass magic are out of scope.
- `ty` is pre-1.0, so hover/LSP behavior can change. The fallback is designed
  to miss diagnostics rather than create false positives.
- Stub branches guarded by `sys.version_info` or `sys.platform` are not
  evaluated; all branches are treated as possible signatures.
- The built-in third-party resolver discovers `$VIRTUAL_ENV` and
  `<project>/.venv`; use `--python` for other environments.
- Source decoding supports UTF-8, UTF-8 BOM, ASCII, and declared
  latin-1/iso-8859-1. Other encodings are warned about and skipped.
- Extremely deep expression nesting is rejected with exit code 2 instead of
  risking parser stack overflow.

## Configuration and CLI

`pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]
debug = false
```

`ignore_names` entries are fully qualified names. Set `debug = true` to print
resolved fully qualified names while diagnosing ignore patterns.

CLI:

```text
strict-kwargs [PATHS...] [--project-root DIR] [--python PATH]
strict-kwargs fix [PATHS...] [--project-root DIR] [--diff] [--python PATH]
```

Exit codes for `check`: `0` clean, `1` violations, `2` operational error.
`fix` exits `0` on success and `2` on operational error; declined rewrites are
reported on stderr.

## Source Map

| File | Responsibility |
| --- | --- |
| `src/check.rs` | call visitor, built-in resolution, rule application, ty deferral |
| `src/index.rs` | `DefinitionIndex`, lazy module and re-export resolution |
| `src/resolve.rs` | module discovery across first-party, typeshed, and site-packages |
| `src/source.rs` | source reading and decoding |
| `src/ty_resolver.rs` | LSP client for `ty server` |
| `src/signature.rs` | positional/keyword rule |
| `src/ast_util.rs` | AST to signature helpers |
| `src/config.rs` | `[tool.strict_kwargs]` loading and project-root discovery |
| `benches/resolver.rs` | resolver benchmark fixtures |
| `vendored/typeshed/` | pinned embedded stdlib stubs |

## Testing

Run the normal test suite with `cargo test`. Since `ty` is required, install it
first if it is not already available.

CI runs formatting, clippy, tests on Linux and Windows, pre-commit hooks, and a
non-gating CodSpeed benchmark job for resolver hot paths.
