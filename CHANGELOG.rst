Changelog
=========

Next
----

- Fix a false positive on the explicit receiver of a first-party
  unbound-method call (``K.n(K())``): the receiver binds to ``self`` and is
  never keyword-passable, so it is no longer counted against the positional
  limit. ``K.m(K(), 1)`` now reports only the real argument and the fixer
  rewrites it to ``K.m(K(), a=1)``. This extends the typeshed/``ty``-path
  fix to the built-in resolver path (issue #27; companion to #15).
- Fix a bound-instance ``__call__`` off-by-one (issue #28): an explicit call
  through ``__call__`` now strips the receiver-bound ``self`` and grants no
  first-positional exemption, so ``C()(1, 2)`` reports ``maximum 0`` (was
  ``maximum 1``) and previously-missed cases such as ``C()(1, b=2)`` are
  flagged. The ``@C()`` decorator-application form is unaffected (it is never
  a checked call site).
- Performance: large import closures (e.g. files importing ``numpy``) no
  longer take many seconds. Re-export expansion was super-quadratic in the
  index size; it now scans only each alias's prefix range, with identical
  output (issue #31).
- Performance: ``ty server`` is started lazily — only when a file has calls
  the built-in resolver could not resolve. Runs the built-in resolver fully
  handles (the common editor-on-save / pre-commit case on first-party code)
  no longer pay ty's project-indexing startup cost (issue #31).
- Continuous benchmarking via `CodSpeed <https://codspeed.io>`_: a divan
  benchmark suite (``benches/resolver.rs``) covering a leaf file, a large
  stdlib import closure, an overload/special-form heavy file, and a
  generated first-party closure, plus the auto-fixer. A non-gating CI job
  reports an instruction-count delta against ``main`` on every PR. See
  ``docs/ARCHITECTURE.md``.
- ``strict-kwargs fix``: auto-rewrite surplus positional call arguments to
  keyword arguments (``--diff`` to preview). Conservative — only calls that
  resolve to a single known signature are rewritten (project code and the
  embedded typeshed builtins); overloaded callees, ``*args``/``**kwargs``
  unpacking, and ty-only resolutions are left untouched. The implicit
  receiver is skipped only for constructor/callable dunders and bound
  ``receiver.method(...)`` calls, so a standalone function whose first
  parameter is named ``self``/``cls`` is rewritten correctly.
- Flag positional construction of ``@dataclass`` and ``NamedTuple`` classes
  (issue #29): their compiler-synthesized ``__init__`` / ``__new__`` is now
  modeled from the annotated fields, so ``D(1, 2)`` is reported while
  ``D(x=1, y=2)`` is accepted. ``ClassVar`` and ``field(init=False)`` fields
  are excluded, ``@dataclass(init=False)`` synthesizes nothing, and a
  hand-written constructor still wins. The auto-fixer conservatively declines
  these (a synthesized signature omits inherited base-class fields). The
  functional ``NamedTuple("N", [...])``/``namedtuple`` forms, ``attrs``, and
  ``TypedDict`` remain out of scope.
- Ship a consumer-facing pre-commit hook (``id: strict-kwargs``) so projects
  can run strict-kwargs via `pre-commit <https://pre-commit.com/>`_. A
  `strict-kwargs-pre-commit
  <https://github.com/adamtheturtle/strict-kwargs-pre-commit>`_ mirror
  installs the prebuilt PyPI wheel (no Rust toolchain required); the in-repo
  hook builds from source. See the README "pre-commit" section.
- Resolve calls into builtins, the standard library, and third-party
  packages: a pinned typeshed copy is vendored and embedded in the binary;
  third-party resolves from ``site-packages`` (PEP 561).
- Follow imports and re-exports (relative imports, ``from x import *``,
  package-root re-export chains, and module-level assignment aliases such as
  ``helper = _impl.real``); overload-safe (permissive) signature model.
- Optional ``ty`` type-inference fallback (drives a ``ty server`` over LSP):
  resolves inheritance/MRO, return-typed and annotation-typed receivers,
  locals bound from calls, and precise overloads. Fails closed; pipelined;
  robust to ty being absent/slow/changing.
- Cross-platform ``file://`` URI handling; CI runs the ty-backed suite on
  Linux and Windows.
- See ``docs/ARCHITECTURE.md`` for the current state, capability matrix,
  parity status, and limitations.

2026.5.16-post.1
----------------


2026.05.16
----------

- Fast Rust linter enforcing keyword arguments at call sites (companion to `mypy-strict-kwargs`).
- Configuration via ``pyproject.toml`` (``[tool.strict_kwargs]``).
