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
  └─ 2. not resolvable statically → defer to ty (required backend)
         pipelined once per file:
           a. textDocument/hover  → parse the `def …(…)`/`bound method …`
              signature with our own parser, apply the rule
           b. hover gave no signature (constructors) →
              textDocument/definition → read & parse the target → apply rule
```

The built-in path is primary (fast, offline, deterministic). `ty` is a
**required backend** for the cases static resolution structurally cannot do.
Every *per-call* ty failure (timeout, protocol hiccup, unparsable target)
still yields *no diagnostic* (fails closed), never a wrong one. But ty being
*unavailable at all* — the `ty` executable cannot be located, or `ty server`
will not start — is a fatal error (`CheckError::TyNotFound` /
`TyServerFailed`, exit 2), not a silent downgrade: a hard requirement is what
makes results deterministic across machines, so the same source can never
resolve fewer calls just because a machine lacks `ty`. `ty` is declared as a
dependency of the PyPI wheel (`ty>=0.0.23` — a floor, not a pin: that is the
version the integration was verified against; see `pyproject.toml`), so a
`pip`/`uv` install ships it; the binary is located **next to our own
executable** first
(maturin + the dependency land in the same venv `bin`/`Scripts`, and `uv
tool install` does *not* put a dependency's entry point on `PATH`), then via
`PATH` (`cargo install`, activated venv). See `ty_command`. Presence is
verified **up front** (a cheap `ty version`, independent of file content,
memoized per process). The `ty server`
itself is still started **lazily** — only once a file actually has calls the
built-in resolver could not resolve — so a run the built-in path fully
handles never pays ty's project-indexing startup cost; a server that fails
to start at that point is the fatal `TyServerFailed`.

### The DefinitionIndex (`src/index.rs`)

A map from fully-qualified name → **list** of signatures (a list, so
`@overload` stubs and redefinitions are handled *permissively*: a call is
flagged only if it exceeds *every* candidate — never a false positive).

**Lazy & demand-driven.** Only **builtins** and the **files being checked**
are indexed eagerly (they are small and their call sites are what we walk).
Every *other* module — sibling first-party, stdlib, third-party — is
resolved, parsed and indexed **on demand**, the first time a query needs a
name it could define or route. Module resolution still follows ty/pyright
order — first-party, then vendored, embedded
[typeshed](https://github.com/python/typeshed) stdlib (offline,
deterministic; pinned in `vendored/typeshed/COMMIT`, updated via
`scripts/update-typeshed.sh`), then the active environment's `site-packages`
honoring **PEP 561** (`*-stubs`, `py.typed`, bundled `.pyi`). The earlier
eager worklist walked the *entire transitive import closure* up front; on a
heavy third-party package (numpy/torch/scipy) that did not complete in any
practical time (issue #39). Now only the modules on a queried name's actual
re-export path are parsed.

**Imports and re-exports** are followed: `import a.b [as m]`,
`from a.b import c [as d]`, relative imports (correctly anchored for
`__init__.py` packages), and re-exports — explicit `from .impl import name`,
`from x import *`, module-level assignment aliases (`helper = _impl.real`,
`alias = real`), and chains through package roots (e.g. `os.path` →
`posixpath`). A re-export `(src, dst)` edge is resolved *backwards* on
demand — `dst.foo` is tried as `src.foo` — instead of eagerly materializing
the full alias cross-product (which was superlinear). Edges are indexed by
destination, so a hop costs O(name-depth), not O(total edges). A
self-referential `from pkg.sub import *` web (`src` inside `dst`'s subtree)
is followed only one segment at a time, so chained stars still resolve while
the unbounded `pkg.sub.sub…` rewrite family cannot form. Per-query module
and step backstops keep an unforeseen pathology fail-closed (the query
yields nothing → the call defers to `ty`, never a false positive).
Assignment aliases are followed only for pure name/attribute references at
true module scope (a call/literal RHS is a value, not an alias; a
function-local assignment binds in that scope, not the package's). Builtins
resolve via a synthetic `builtins` module plus a bare-name fallback;
`Class(...)` resolves to `Class.__init__`/`__new__`.

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
- **Required, verified up front**: the `ty` executable is located next to
  our own binary (the wheel dependency, `ty>=0.0.23`) or on `PATH`
  (`ty_command`);
  a cheap `ty version` probe runs before any file is read, memoized per
  process; an unlocatable binary aborts the run with
  `CheckError::TyNotFound` (exit 2) regardless of file content, so the
  outcome is deterministic.
- **Lazy start**: the *server* subprocess is still spawned only on the first
  file with deferred calls, not at the start of the run (a fully-resolvable
  run does not pay ty's project-indexing cost). If the server fails to start
  there — binary present, server won't run — the run aborts with
  `CheckError::TyServerFailed` rather than continuing degraded.
- **Robust / fails closed (per call)**: bounded timeouts (5 s request, 15 s
  init); the *first* in-run failure latches ty OFF for the rest of the run
  (no timeout storms) and yields no diagnostic for the remaining deferred
  calls; server→client requests are answered so ty never blocks. (Backend
  *unavailability* is fatal — above — but a flaky *response* never produces
  a wrong diagnostic.)
- **Explicit environment (`--python`)**: forwarded to `ty server` over LSP
  (see *Forwarding an explicit environment* below) so the fallback can
  resolve third-party imports in environments ty would not auto-discover.
  Accepted by both `check` and `fix`.
- **`fix` uses it for detection only**: `fix_paths` runs the same built-in +
  ty detection as `check_paths` (same lazy start), but the *rewrite* stays
  conservative — overloaded, synthesized, and ty-only-resolved calls are
  never edited (issue #7; a wrong parameter name would corrupt source, cf.
  issue #41). Running detection in full lets `fix` report a `declined`
  count equal to what a following `check` (same `--python`) still reports,
  so the two no longer silently disagree (issue #42).

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
(always available — it is a hard requirement) covers them: it auto-discovers
an activated virtualenv/Conda env
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
and `ty` `0.0.23` (the dependency floor) and is exercised by the
`ty_forwards_external_python_env` / `ty_invalid_python_env_fails_closed`
integration tests. If a future `ty` changes or rejects this channel, the
fail-closed behaviour means the fallback degrades to today's
auto-discovery-only behaviour rather than emitting wrong diagnostics.

## Parity with mypy-strict-kwargs

- All integration tests **ported from the plugin's `test_plugin.yaml` pass**.
- The major real-world gaps (inheritance, return/annotation-typed receivers,
  overload precision, stdlib via inferred receiver) are closed via the ty
  fallback (a hard requirement) → **effective parity for ordinary OO code**.
- Not *provable* full parity: different engines, and the plugin is itself
  bounded by mypy. See [Known limits](#known-limits).

## Known limits

Structural (no static tool, including the mypy plugin via mypy, fully
handles these):

- Dynamic dispatch, `getattr`, runtime-computed `__all__`,
  decorator-rewritten signatures, metaclass magic.

Tool-specific:

- **ty is required.** `ty` must be on `PATH` (and its server must start);
  otherwise strict-kwargs aborts (exit 2) instead of silently skipping the
  inference-dependent cases (inheritance, return/annotation-typed receivers,
  locals from calls). This is deliberate — it keeps results deterministic
  across machines rather than letting the same source resolve fewer calls
  where `ty` is absent.
- **ty is pre-1.0.** Its hover/LSP behaviour can change between versions;
  hover parsing is best-effort and falls back to permissive (a miss, never a
  false positive).
- **Overloads** in the built-in path are permissive (flag only if *every*
  candidate is exceeded). ty's matched overload is precise when used.
- `sys.version_info` / `sys.platform` stub branches are not evaluated — all
  branches are indexed and treated as overloads.
- typeshed re-export following is structural; **runtime-computed** `__all__`
  is not followed.
- Re-export resolution is lazy and bounded. A self-referential
  `from pkg.sub import *` web resolves names re-exported one segment at a
  time; a name reachable only by a *multi-segment* path through such a
  self-referential star (rare; `from pkg.sub import *` then
  `pkg.deep.attr`) is not built-in-resolved and defers to `ty`. Per-query
  module/step backstops likewise defer on an unforeseen pathology. All
  deferrals fail closed (never a false positive).
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

CLI: `strict-kwargs [PATHS...] [--project-root DIR] [--python PATH]`, plus
`strict-kwargs fix [PATHS...] [--project-root DIR] [--diff] [--python PATH]`.
`fix` writes in place (`--diff` previews instead) and reports a count of
violations it detected but declined to rewrite.
Exit codes (`check`): `0` clean, `1` violations, `2` internal error. `fix`
exits `0` on success (`2` on internal error); the declined count is a stderr
signal, not an exit status — run `strict-kwargs` for the gate.

## Source map

| File | Responsibility |
| --- | --- |
| `src/check.rs` | call visitor, name/import/scope resolution, rule application, ty deferral |
| `src/index.rs` | DefinitionIndex, lazy demand-driven module + re-export resolution |
| `src/resolve.rs` | module resolver (first-party / embedded typeshed / site-packages, PEP 561) |
| `src/ty_resolver.rs` | LSP client, hover/definition, pipelining, robustness, URI handling |
| `src/signature.rs` | the positional/keyword rule and `max_positional` logic |
| `src/ast_util.rs` | AST → signature, argument counting, line/column |
| `src/config.rs` | `[tool.strict_kwargs]` loading, project-root discovery |
| `benches/resolver.rs` | divan / CodSpeed benchmark suite for the resolver hot paths |
| `vendored/typeshed/` | pinned, embedded typeshed stdlib (see its README) |

## Testing & CI

- Unit and integration tests. Integration tests are ported from
  `mypy-strict-kwargs`. Since `ty` is now a hard requirement, the test
  suite needs `ty` on `PATH` too (`check_paths`/`fix_paths` error without
  it) — install it with `uv tool install ty` before `cargo test`.
- Cross-platform URI handling has dedicated platform-independent unit tests.
- CI (`.github/workflows/`) runs on **`ubuntu-latest` and `windows-latest`**:
  `ci.yml` installs `ty` (via `uv`) with a `ty version` gate so the
  ty-backed tests actually execute on every platform; `lint.yml` runs
  `cargo fmt --check` and `cargo clippy -D warnings`, and installs `ty`
  before its `prek` pre-push stage because that stage runs `cargo test`.
- **Continuous benchmarking** (`benches/resolver.rs`, issue #30): a
  divan suite run under [CodSpeed](https://codspeed.io) by a non-gating
  `benchmarks` job in `ci.yml`, reporting an instruction-count delta against
  `main` on every PR. It covers a leaf file, a large stdlib import closure,
  an overload/special-form heavy file, a generated first-party closure, a
  wide chained `import *` re-export closure (`reexport_closure`, the issue
  #39 regression shape), and the auto-fixer. The job does **not** install
  `ty`: CodSpeed counts
  instructions of the strict-kwargs process, so the ty subprocess fallback
  is out of scope, and every fixture is fully resolvable by the built-in
  resolver — keeping the numbers deterministic and focused on the
  parse / index / walk / resolve hot paths. Run locally with `cargo bench`.
