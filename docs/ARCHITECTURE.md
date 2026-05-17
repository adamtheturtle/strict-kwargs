# Architecture & current state

This document describes how `strict-kwargs` resolves calls and what it can and
cannot do, as of the current code. It is intentionally honest about the
limits — the recurring caveats are collected in [Known limits](#known-limits).

## What it enforces

One rule: a call site must not pass an argument **positionally** when that
argument could be a keyword. Equivalent in spirit to the
[`mypy-strict-kwargs`](https://github.com/adamtheturtle/mypy-strict-kwargs)
plugin, but as a fast standalone tool.

The rule itself (`src/signature.rs`) understands: positional-only `/`,
keyword-only `*`, `*args`/`**kwargs`, defaults, `self`/`cls`, and the
constructor/descriptor dunders (`__init__`, `__new__`, `__call__`,
`__get__`, `__set__`). A call is flagged only when its positional-argument
count exceeds the maximum the signature allows.

## Resolution pipeline (per call)

```
call site
  │
  ├─ 1. built-in resolver (src/check.rs)
  │      name / import / scope resolution → fully-qualified name
  │      look up in the DefinitionIndex
  │      └─ found → apply the rule, emit diagnostic
  │
  └─ 2. not resolvable statically → defer to ty (if available)
         pipelined once per file:
           a. textDocument/hover  → parse the `def …(…)`/`bound method …`
              signature with our own parser, apply the rule
           b. hover gave no signature (constructors) →
              textDocument/definition → read & parse the target → apply rule
```

The built-in path is primary (fast, offline, deterministic). `ty` is an
**optional, additive fallback** for the cases static resolution structurally
cannot do. Every ty failure mode yields *no diagnostic* (fails closed), never
a wrong one.

### The DefinitionIndex (`src/index.rs`)

A map from fully-qualified name → **list** of signatures (a list, so
`@overload` stubs and redefinitions are handled *permissively*: a call is
flagged only if it exceeds *every* candidate — never a false positive).

Built once per run via a bounded recursive worklist over three sources, in
ty/pyright order:

1. **First-party** — the project's own files (and sibling modules resolved
   via the project root, so single-file checks still see the package).
2. **Standard library + builtins** — a pinned copy of
   [typeshed](https://github.com/python/typeshed) vendored under
   `vendored/typeshed/` and **embedded in the binary** (`include_dir!`). No
   Python environment required; fully offline and deterministic. Pinned
   commit is recorded in `vendored/typeshed/COMMIT`; update with
   `scripts/update-typeshed.sh` (see `vendored/typeshed/README.md`).
3. **Third-party** — the active environment's `site-packages`, honoring
   **PEP 561**: `*-stubs` distributions, inline-typed packages (`py.typed`),
   and bundled `.pyi`.

**Imports and re-exports** are followed: `import a.b [as m]`,
`from a.b import c [as d]`, relative imports (correctly anchored for
`__init__.py` packages), and re-exports — explicit `from .impl import name`,
`from x import *`, module-level assignment aliases (`helper = _impl.real`,
`alias = real`), and chains through package roots (e.g. `os.path` →
`posixpath`). Assignment aliases are followed only for pure name/attribute
references at true module scope (a call/literal RHS is a value, not an
alias; a function-local assignment binds in that scope, not the package's).
Builtins resolve via a synthetic `builtins` module plus a
bare-name fallback; `Class(...)` resolves to `Class.__init__`/`__new__`.

**Synthesized constructors.** `@dataclass` and `NamedTuple` classes have no
written `__init__`/`__new__`, so one is synthesized from the class's
annotated fields (each a positional-or-keyword parameter; `ClassVar` and
`field(init=False)` excluded, `@dataclass(init=False)` synthesizes nothing, a
hand-written constructor wins). Scoped to the class's *own* fields —
inherited base-class fields are not merged, so the auto-fixer declines
synthesized constructors (the position→name mapping is not guaranteed sound),
but the positional limit is `0` regardless so the diagnostic stays correct.

### The ty fallback (`src/ty_resolver.rs`)

A minimal JSON-RPC/LSP client that drives a `ty server` subprocess.

- **Hover-first.** `textDocument/hover` returns the *overload-matched,
  inheritance-resolved, `self`-stripped* signature even for stdlib/builtins
  (e.g. `bound method list[int].append(object: int, /) -> None`). We parse
  that with our own parser and apply the rule — this is what gives parity for
  inheritance/MRO, return-typed and annotation-typed receivers, locals bound
  from calls, and precise overloads.
- **Goto-definition** is the secondary path for constructors (hover yields
  `<class 'A'>`, not a signature).
- **Pipelined per file**: all requests for a file are sent, then collected —
  round-trip latency is hidden; out-of-order responses are buffered.
- **Robust / fails closed**: bounded timeouts (5 s request, 15 s init); the
  *first* failure latches ty OFF for the whole run (no timeout storms);
  server→client requests are answered so ty never blocks; a one-time stderr
  note is printed if `ty` is present but its server cannot start.
- **Explicit environment (`--python`)**: forwarded to `ty server` over LSP
  (see *Forwarding an explicit environment* below) so the fallback can
  resolve third-party imports in environments ty would not auto-discover.

## Capability matrix

| Target | Supported | How | Caveat |
| --- | --- | --- | --- |
| Your own code | ✅ | built-in resolver + ty for inference cases | — |
| Builtins (`str`, `len`, …) | ✅ | embedded typeshed (offline) | none |
| Stdlib (`os`, `json`, …) | ✅ | embedded typeshed + re-export following; ty for inferred receivers | none |
| Third-party libs | ✅ | `site-packages` (PEP 561) + re-export following; ty if env configured | **discovery**, below |

**Third-party discovery caveat.** The built-in resolver finds
`site-packages` only via `$VIRTUAL_ENV` or `<project_root>/.venv` (Unix +
Windows layouts). Other environments (Conda, a Poetry venv elsewhere, system
site-packages, `PYTHONPATH`) are *not* found by the built-in resolver. `ty`
covers them when present: it auto-discovers an activated virtualenv/Conda env
and `.venv`, reads `[tool.ty.environment] python = "…"` from
`pyproject.toml`/`ty.toml` (strict-kwargs launches `ty server` rooted at the
project, so that config applies automatically), **or** you point it at the
environment with `strict-kwargs --python <path>` (interpreter, venv dir, or
`sys.prefix`; mirrors `ty check --python`). `--python` only steers ty's
third-party discovery — the built-in resolver's env discovery and the
embedded builtins/stdlib are unaffected.

### Forwarding an explicit environment (`--python`)

`ty server` takes no CLI arguments, so the environment is delivered over
LSP. This client does not implement `workspace/configuration`, but ty also
accepts its dynamic options in the `initialize` request's
`initializationOptions`. strict-kwargs sends the inline-config channel that
mirrors ty's own config schema:

```jsonc
// initialize → params.initializationOptions
{ "configuration": { "environment": { "python": "<absolute path>" } } }
```

`configuration` is `ty`'s `WorkspaceOptions.configuration` map, deserialized
as `ty_project::metadata::Options`, so `environment.python` here is exactly
`[environment] python` from `ty.toml`. The path is made absolute before
sending (ty resolves a *relative* `environment.python` against its workspace
root, but a CLI value is relative to the user's cwd). When `--python` is
unset, no `initializationOptions` is sent and ty's auto-discovery is
untouched. An invalid/unknown path is not validated by strict-kwargs: ty
simply resolves nothing against it, so the fallback fails closed (no wrong
diagnostics) just as when no environment is configured.

**Stability.** `ty` is pre-1.0 and its LSP settings surface is undocumented
for embedding; the schema above was verified against the `ty_server` source
and the locally pinned `ty` (`0.0.23`) and is exercised by the
`ty_forwards_external_python_env` / `ty_invalid_python_env_fails_closed`
integration tests. If a future `ty` changes or rejects this channel, the
fail-closed behaviour means the fallback degrades to today's
auto-discovery-only behaviour rather than emitting wrong diagnostics.

## Parity with mypy-strict-kwargs

- All integration tests **ported from the plugin's `test_plugin.yaml` pass**.
- The major real-world gaps (inheritance, return/annotation-typed receivers,
  overload precision, stdlib via inferred receiver) are closed via the ty
  fallback → **effective parity for ordinary OO code when `ty` is present**.
- Not *provable* full parity: different engines, and the plugin is itself
  bounded by mypy. See [Known limits](#known-limits).

## Known limits

Structural (no static tool, including the mypy plugin via mypy, fully
handles these):

- Dynamic dispatch, `getattr`, runtime-computed `__all__`,
  decorator-rewritten signatures, metaclass magic.

Tool-specific:

- **ty is optional.** Without `ty` on `PATH`, inference-dependent cases
  (inheritance, return/annotation-typed receivers, locals from calls) are not
  resolved. Builtins/stdlib/own-code/first-party still work.
- **ty is pre-1.0.** Its hover/LSP behaviour can change between versions;
  hover parsing is best-effort and falls back to permissive (a miss, never a
  false positive).
- **Overloads** in the built-in path are permissive (flag only if *every*
  candidate is exceeded). ty's matched overload is precise when used.
- `sys.version_info` / `sys.platform` stub branches are not evaluated — all
  branches are indexed and treated as overloads.
- typeshed re-export following is structural; **runtime-computed** `__all__`
  is not followed.
- Synthesized constructors cover the **class form** of `@dataclass` and
  `NamedTuple` only. The functional `NamedTuple("N", [...])`/`namedtuple`
  forms, `attrs`, and `TypedDict` (keyword-only by definition) are out of
  scope; inherited base-class fields are not merged into the synthesized
  signature (limit is `0` regardless, so detection is unaffected).
- Cosmetic: module-qualified functions display as `"f" of "module"` (mypy
  wording differs slightly); detection is correct.

## Configuration & CLI

`pyproject.toml`:

```toml
[tool.strict_kwargs]
ignore_names = ["main.func", "builtins.str"]   # fully-qualified; class form
debug = false                                  # also covers Class.__init__
```

CLI: `strict-kwargs [PATHS...] [--project-root DIR]`.
Exit codes: `0` clean, `1` violations, `2` internal error.

## Source map

| File | Responsibility |
| --- | --- |
| `src/check.rs` | call visitor, name/import/scope resolution, rule application, ty deferral |
| `src/index.rs` | DefinitionIndex, worklist build, import/re-export following |
| `src/resolve.rs` | module resolver (first-party / embedded typeshed / site-packages, PEP 561) |
| `src/ty_resolver.rs` | LSP client, hover/definition, pipelining, robustness, URI handling |
| `src/signature.rs` | the positional/keyword rule and `max_positional` logic |
| `src/ast_util.rs` | AST → signature, argument counting, line/column |
| `src/config.rs` | `[tool.strict_kwargs]` loading, project-root discovery |
| `vendored/typeshed/` | pinned, embedded typeshed stdlib (see its README) |

## Testing & CI

- Unit and integration tests. Integration tests are ported from
  `mypy-strict-kwargs`; ty-backed tests are guarded by a `ty` availability
  check so the suite stays green for contributors without `ty`.
- Cross-platform URI handling has dedicated platform-independent unit tests.
- CI (`.github/workflows/`) runs on **`ubuntu-latest` and `windows-latest`**:
  `ci.yml` installs `ty` (via `uv`) with a `ty version` gate so the
  ty-backed tests actually execute on every platform; `lint.yml` runs
  `cargo fmt --check` and `cargo clippy -D warnings`.
