Changelog
=========

.. towncrier release notes start

2026.8.27-post.1
----------------

- Clear callable-class return metadata when a function is redefined.

- Clear callable iterator-item metadata when a factory function is redefined.

- Clear callable contextmanager-item metadata when a decorated factory is redefined.

- Ensure rebinding or deleting a specialized ``ContextVar`` clears its callable-value metadata.

- Clear stale receiver annotations when an ordinary assignment records a concrete new instance class.

- Clear generic parameter-to-result metadata when a function is redefined.

- Preserve concrete callable signatures returned by ``pop`` on literal sets and dictionaries.

- Preserve concrete callable item signatures through :func:`map` and
  :func:`filter` results.

- Preserve callable results from ``enterAsyncContext`` when an
  ``IsolatedAsyncioTestCase`` is stored in a local variable.

- Preserve callable results from a stored property's ``fget`` method.

- Resolve callable type aliases used as ``TypeIs`` or ``TypeGuard`` targets.

- Resolve named callable aliases when narrowing optional parameters after a non-``None`` guard.

- Preserve named callable aliases when an assertion narrows an optional parameter.

- Resolve named callable aliases used as the yield type of annotated generators.

- Resolve named callable aliases used as the yield type of annotated asynchronous generators.

- Preserve callable aliases returned by ``athrow`` on annotated asynchronous generators.

- Resolve named callable aliases used as the value type of annotated ``ContextVar`` instances.

- Resolve named callable aliases used as values in annotated ``MappingProxyType`` instances.

- Preserve homogeneous callable elements returned by popping a literal or sorted
  list.

- Preserve concrete callable elements through a literal list's ``copy`` method.

- Preserve concrete callable elements returned by an explicit
  ``list.__getitem__`` call.

- Preserve a concrete callable value returned by ``dict.get`` for an existing
  literal key.

- Preserve the concrete callable value returned by ``popitem`` on a literal
  dictionary.

- Preserve concrete callable values through a literal dictionary's ``copy``
  method.

- Preserve concrete callable values when subscripting a ``MappingProxyType``
  created from a literal dictionary.

- Preserve concrete callable values when subscripting a single-mapping
  ``ChainMap`` constructed from a literal dictionary.

- Preserve concrete callable values when subscripting a ``UserDict`` constructed
  from a literal dictionary.

- Preserve concrete callable values when subscripting an ``OrderedDict``
  constructed from a literal dictionary.

- Preserve concrete callable values returned by a zero-argument ``defaultdict``
  factory when subscripting an initially empty mapping.

- Preserve concrete callable elements through ``next(iter(...))`` for literal
  lists, tuples, sets, and dictionaries.

- Preserve concrete callable elements through ``next(reversed(...))`` for
  homogeneous literal iterables.

- Preserve concrete callable values returned by a mapping lambda through
  ``next(itertools.starmap(...))``.

- Preserve concrete callable elements through
  ``next(itertools.filterfalse(...))``.

- Preserve concrete callable elements when ``random.choice`` and
  ``secrets.choice`` receive their sequence through the ``seq`` keyword.

- Preserve concrete callable elements when subscripting a ``deque`` constructed
  from a literal list or tuple.

- Preserve concrete callable elements through subscripting after copying an
  immediately constructed ``deque``.

- Preserve concrete callable elements returned by ``next(reversed(deque(...)))``.

- Preserve concrete callable values selected from literal ``dict.items()``
  iterators.

- Preserve concrete callable keys from literal ``dict.keys()`` iterators.

- Preserve concrete callable keys from ``itertools.groupby`` calls without a
  key function.

- Preserve concrete callable elements through the group iterators returned by
  ``itertools.groupby``.

- Preserve concrete callable elements through subscripting the results of
  ``heapq.nsmallest`` and ``heapq.nlargest``.

- Preserve concrete callable elements through subscripting
  ``random.choices`` results.

- Preserve concrete callable elements returned by bound ``choice`` methods on
  immediate ``Random`` and ``SystemRandom`` instances.

- Preserve concrete callable elements returned by ``statistics.median_low`` and
  ``statistics.median_high`` for singleton literal data.

- Preserve concrete callable keys through ``next(Counter(...).elements())``.

- Preserve concrete callable keys selected from ``Counter.popitem()`` results.

- Preserve concrete callable values returned by ``get`` on literal-backed ``ChainMap``, ``UserDict``, and ``OrderedDict`` instances.

- Preserve existing concrete callable values returned by ``defaultdict.get``
  when initialized from a literal mapping.

- Preserve concrete callable elements captured by statically determined starred
  destructuring targets.

- Preserve concrete callable elements through ``next()`` for simple identity
  generator expressions over homogeneous literal iterables.

- Propagate a concrete callable through a local function whose body is a single unconditional return.

- Propagate a concrete callable when every branch of a local function returns the same target.

- Preserve concrete callable elements selected from statically bounded literal sequence slices.

- Preserve concrete callable elements selected from concatenated literal lists and tuples.

- Preserve concrete callable elements selected from repeated literal lists and tuples.

- Preserve concrete callable elements selected from statically expanded starred sequence literals.

- Preserve concrete callable values selected from literal dictionary unions.

- Ignore provably empty literal dictionary unpacking when resolving concrete callable values.

- Preserve identical concrete callable elements selected from both branches of a conditional container.

- Preserve concrete callable elements selected from statically determined boolean container expressions.

- Preserve concrete callable elements selected from literal containers passed through ``copy.copy`` or ``copy.deepcopy``.

- Preserve concrete callable elements selected from immediately constructed ``collections.UserList`` instances.

- Preserve concrete callable elements selected from copied ``collections.UserList`` instances.

- Preserve concrete callable elements returned by ``collections.UserList.pop`` on immediate literal-backed instances.

- Preserve concrete callable elements yielded by iterating immediate literal-backed ``collections.UserList`` instances.

- Preserve concrete callable elements yielded by iterating immediate literal-backed ``collections.deque`` instances.

- Preserve concrete callable elements returned by explicit ``collections.deque.__getitem__`` calls.

- Preserve concrete callable values from a ``ChainMap`` parent when subscripting an empty ``new_child()`` result.

- Preserve concrete callable items returned by ``itertools.chain.from_iterable`` over nested literals.

- Preserve concrete callable values selected from literal dictionaries by ``operator.itemgetter``.

- Preserve concrete callable elements when subscripting literal slices returned by ``operator.itemgetter``.

- Preserve concrete callable elements returned by ``operator.concat`` and ``operator.iconcat`` over literal sequences.

- Preserve concrete callable values when subscripting ``weakref.WeakValueDictionary`` instances initialized from literal mappings.

- Preserve concrete callable values while iterating ``weakref.WeakValueDictionary.values()`` initialized from a literal mapping.

- Preserve concrete callable keys while iterating ``weakref.WeakKeyDictionary`` instances initialized from literal mappings.

- Preserve concrete callable elements while iterating ``weakref.WeakSet`` instances initialized from literal iterables.

- Preserve concrete callable dataclass fields when subscripting ``dataclasses.astuple`` results.

- Preserve concrete callable dataclass fields when subscripting ``dataclasses.asdict`` results.

- Preserve concrete callable fields when subscripting ``NamedTuple._asdict()`` results.

- Preserve concrete callable fields when indexing ``NamedTuple._replace()`` results.

- Preserve concrete callable attributes when subscripting ``vars(SimpleNamespace(...))`` results.

- Preserve concrete callable attributes returned by ``getattr`` on inline ``SimpleNamespace`` instances.

- Preserve concrete callable results when awaiting items yielded by ``asyncio.as_completed``.

- Infer concrete callable results for tasks returned by ``asyncio.create_task``.

- Infer concrete callable results for futures returned by ``asyncio.ensure_future``.

- Preserve callable coroutine result signatures for tasks created by ``asyncio.TaskGroup.create_task``.

- Infer callable item signatures added to unannotated ``asyncio.Queue`` instances with ``put_nowait``.

- Preserve callable item signatures when iterating over annotated async iterator factory calls.

- Preserve callable item signatures in async comprehensions over annotated iterator factories.

- Preserve callable return signatures through ``Executor.submit(...).result()``.

- Preserve concrete callable results yielded by ``Executor.map``.

- Preserve callable future results through singleton ``concurrent.futures.as_completed`` iterators.

- Preserve callable future results through singleton ``concurrent.futures.wait`` done sets.

- Preserve Pool callable-result inference for context-manager target bindings.

- Infer callable item signatures added to unannotated ``queue.Queue`` instances with ``put``.

- Preserve callable item signatures in ``for`` loops over annotated generator factories.

- Preserve callable item signatures in comprehensions over annotated generator factories.

- Infer callable value signatures from unannotated ``ContextVar`` defaults.

- Preserve callable signatures selected through directly imported aliases of
  ``random.choice`` and ``secrets.choice``.

- Preserve callable signatures returned through directly imported aliases of
  ``heapq.heappop``.

- Preserve callable signatures returned through directly imported aliases of
  ``statistics.mode``.

- Preserve concrete callable items from directly imported aliases of
  ``itertools.chain``.

- Preserve concrete callable referents through directly imported aliases of
  ``weakref.ref``.

- Preserve registered callables returned through directly imported aliases of
  ``atexit.register``.

- Preserve wrapped callables returned through directly imported aliases of
  ``inspect.unwrap``.

- Preserve concrete callable defaults returned by ``dict.get`` when a literal
  key is known to be missing.

- Preserve concrete callable defaults returned by ``dict.pop`` when a literal
  key is known to be missing.

- Preserve concrete callable values from ``dict.fromkeys`` through literal-key
  subscripts.

- Preserve concrete callable values returned by explicit ``dict.__getitem__``
  calls on literal dictionaries.

- Preserve concrete callable values while iterating the values view of an
  immediately constructed ``OrderedDict``.

- Preserve concrete callable values selected from an immediately constructed
  ``OrderedDict`` items iterator.

- Preserve concrete callable values returned by ``OrderedDict.pop`` for a
  present literal key.

- Preserve concrete callable defaults returned by ``OrderedDict.setdefault``
  when an immediate mapping is known to be empty.

- Preserve concrete callable values while iterating the values view of an
  immediately constructed ``UserDict``.

- Preserve concrete callable values selected from an immediately constructed
  ``UserDict.popitem`` result.

- Preserve concrete callable values while iterating the values view of an
  immediately constructed single-mapping ``ChainMap``.

- Preserve concrete callable values selected from an immediately constructed
  single-mapping ``ChainMap`` items iterator.

- Preserve concrete callable values returned by ``get`` on an inline
  ``MappingProxyType`` around a literal dictionary.

- Preserve concrete callable values when iterating an immediate ``MappingProxyType.values()`` view.

- Preserve concrete callable values when subscripting an immediate ``MappingProxyType.copy()`` result.

- Preserve concrete callable values returned by ``WeakValueDictionary.get`` on immediate literal mappings.

- Preserve concrete callable values returned by ``WeakValueDictionary.pop`` on immediate literal mappings.

- Preserve concrete callable values selected from immediate ``WeakValueDictionary.popitem()`` results.

- Preserve concrete callable values returned by ``WeakKeyDictionary.get`` on immediate mappings with a known object key.

- Preserve concrete callable values selected from immediate ``WeakKeyDictionary.popitem()`` results.

- Preserve concrete callable elements when iterating a copied immediate ``WeakSet``.

- Preserve concrete callable keys yielded by ``Counter.elements()`` after immediate mapping initialization.

- Preserve concrete callable keys returned by ``Counter.most_common()`` after immediate mapping initialization.

- Preserve positive-count concrete callable keys when iterating unary plus on an immediate ``Counter``.

- Preserve positive-count concrete callable keys when iterating addition of immediate ``Counter`` values.

- Preserve positive-count concrete callable keys when iterating subtraction of immediate ``Counter`` values.

- Preserve positive-count concrete callable keys when iterating intersections of immediate ``Counter`` values.

- Preserve positive-count concrete callable keys when iterating unions of immediate ``Counter`` values.

- Preserve concrete callable elements through explicit ``__iter__().__next__()`` calls on literal sequences and sets.

- Preserve concrete callable elements through explicit ``__next__()`` calls on built-in iterators over literal containers.

- Preserve concrete callable values through explicit iterator protocol calls on literal dictionary values views.

- Preserve concrete callable values through explicit iterator protocol calls on literal dictionary items views.

- Preserve concrete callable defaults returned by ``next`` for statically empty iterators.

- Preserve concrete callable defaults returned by ``min`` and ``max`` for statically empty iterables.

- Preserve concrete callable defaults returned by ``getattr`` for missing attributes on inline ``types.SimpleNamespace`` values.

- Preserve concrete callable elements when ``sum`` concatenates statically known list literals.

- Preserve concrete module-level functions retrieved through literal ``locals()`` and ``globals()`` lookups.

- Preserve wrapped callable signatures returned by direct ``staticmethod.__get__`` calls.

- Preserve bound callable signatures returned by direct ``classmethod.__get__`` calls.

- Preserve concrete callables returned by single-return property method bodies.

- Attribute callable-valued ``cached_property`` results to their returned callable and safely rewrite their positional calls.

- Preserve concrete callables returned by inline ``cached_property.__get__`` expressions.

- Preserve wrapped callable signatures recursively through nested ``functools.partial`` values.

- Preserve lambda wrapper signatures returned by positional ``functools.update_wrapper`` calls.

- Preserve default implementations returned by inline ``singledispatch.dispatch`` calls.

- Preserve implementations returned by inline two-argument ``singledispatch.register`` calls.

- Preserve concrete ``SimpleNamespace`` callables returned by ``inspect.getattr_static``.

- Preserve target function signatures enforced by callable ``unittest.mock.create_autospec`` results.

- Preserve concrete wrapped function signatures through callable ``unittest.mock.Mock`` values.

- Preserve concrete callable fields read from statically known ``NamedTuple._make`` results.

- Preserve concrete callable elements indexed from statically known ``NamedTuple._make`` results.

- Preserve concrete callable attributes supplied through literal dictionary unpacking to ``SimpleNamespace``.

- Preserve concrete callables yielded by zero-argument lambda factories in callable-sentinel ``iter`` values.

- Preserve concrete callable elements selected from statically known ``itertools.batched`` results.

- Preserve concrete callables returned by explicit ``__enter__`` calls on simple ``@contextmanager`` results.

- Stop treating a module-level function whose first parameter is ``self`` as an unbound method call, which under-counted its arguments and left one unnamed by ``--fix``.

- Index a module that resolved but ran out of one query's module budget, instead of marking it indexed without parsing it and skipping it for the rest of the run.

- Exercise the bound-hover path in the receiver-stripping unit test, which previously passed the unbound flag.

- Stop reporting calls made through an attribute whose only declared type is a bare ``Callable[...]``, which names no parameter and so has no keyword form. Fields whose value traces to a concrete named function are still reported.

- Honor the PEP 484 convention that a parameter whose name has a leading but no trailing double underscore is positional-only, so calls to functions such as ``tomli.load`` are no longer reported.

- Stop reporting ``super().__init__`` calls into a positional-only base such as ``list`` or ``dict``, and never name the type-inference namespace ``ty`` as the callee.

- Treat the parameters of a bare ``Callable[...]`` annotation as positional-only, as the typing spec requires, so calls through a value typed only that way are no longer reported. Their arity is still checked, and callables traced to a concrete function are unaffected.

- Treat ``functools.cached_property`` as a property, so a call through one is no longer checked against the getter as the plain ``property`` spelling already was.

- Stop reporting calls through a ``Callable[...] | None`` value that has been narrowed to its callable arm.

- Assemble news fragments into the changelog again. Every release since the towncrier switch rendered an empty changelog because the configured fragment types omitted the suffix every fragment uses.

2026.8.27
---------

No significant changes.

2026.8.24
---------

- Honor annotated ``__all__``, keep unknown star-import sources from blocking
  ``ty``, preserve sibling-branch re-exports while superseding same-branch
  aliases, replace superseded defs in one conditional arm, and keep the
  descriptor protocol exemption on multi-level unbound ``__get__`` / ``__set__``
  calls.

2026.8.23
---------

- Allow positional binding arguments to CPython descriptor ``__get__`` methods
  (``classmethod``, ``staticmethod``, ``property``, functions, method descriptors,
  and wrapper descriptors) that reject keyword arguments.

- Flag positional ``instance`` arguments to ``functools.cached_property.__get__``,
  which accepts that parameter by keyword.

- Check paths that share a directory as one project when no ``pyproject.toml``
  is found, and compare discovered project roots by what they name rather than
  by how they are spelled.

- Resolve ``operator.getitem`` and ``operator.methodcaller`` through either
  import style, and let a nearer binding of the same name shadow the import.

- Apply the ``ignore_names`` and ``@singledispatch`` exemptions to a call
  encoded as ``operator.methodcaller("__call__", ...)``.

- Stop selecting an ``@overload`` arm that a later definition of the same name
  has replaced.

- Select numeric ``@overload`` arms for negative and explicitly positive number
  literals.

- Keep a parse failure fatal for a file named on the command line even when
  selection reached it under a different spelling.

2026.8.16
---------

- Avoid redundant cache-fingerprint walks for nested source roots and unused default Python environments.

- Add ``error_on_unused_noqa`` (and ``--error-on-unused-noqa``), which reports a new ``KW002`` error for every ``# noqa: KW001`` comment that suppressed nothing. Blanket ``# noqa`` comments, and comments naming only other tools' codes, are never reported.

- Avoid copying every ty-bound source file during no-cache checks.

- Avoid copying ty hover payloads and share cached group responses, reducing cold-check time on repositories with many inferred calls.

- Balance large no-cache checks by the ``ty`` hover requests remaining after grouped-answer reuse, reducing whole-repository fallback time.

- Index the embedded typeshed files once, speeding up no-cache checks that resolve many standard-library calls.

- Isolate reused CPython completeness checkouts so parent project metadata cannot change the golden oracle.

- Keep safe ``self`` hover reuse in unaffected methods when another method narrows or escapes its receiver, reducing no-cache ``ty`` fallback work.

- Reuse identical ``self`` hover responses across ordinary methods of the same class, reducing no-cache ``ty`` fallback work on test-heavy projects.

- Run large no-cache fix and diff passes across deterministic ``ty server`` shards, substantially reducing whole-repository runtime while keeping smaller projects on the lower-overhead serial path.

- Scan projects normally when an ancestor of the project root is a dot-directory, including macOS temporary directories used by generated benchmarks.

- Serialize newly computed cache diagnostics without cloning their strings into a second in-memory representation, cutting cold-cache population medians by about 5% on the pinned Sphinx and CPython benchmarks.

- Share cached ty definition responses between grouped calls instead of deep-cloning their JSON trees.

- Sort whole-project diagnostic results in parallel, cutting cold-check medians by about 8% on CPython and 11% on Sphinx in the pinned repository benchmarks.

- Store warm-cache diagnostics in one bounded manifest, avoiding one cache-file read and write per checked Python file.

- Use eight deterministic ``ty server`` shards for uncached whole-project checks. In balanced ``ty`` 0.0.64 benchmarks with a fresh strict-kwargs cache, median wall time fell by about 6% on Sphinx and 16% on CPython. The new partition also stops emitting three dynamic-call false positives from the completeness floors.

2026.7.24
---------

- Avoid false constructor diagnostics and unsafe fixes when a class call crosses
  multiple locally modeled runtime boundaries. Calls now remain unchanged when
  a local ``__new__`` competes with ``__init__`` or a custom metaclass
  ``__call__`` can impose a different positional-only contract.

- Extend ``ty`` hover-answer reuse from ``self``/``cls`` attribute calls to
  every call on a stable name binding: any parameter, a module-level
  ``import``/``def``/``class``, a single assignment (``f = open(...)``,
  ``with open(...) as f``), and bare-name calls on such bindings
  (``Decimal(...)``, ``check(...)``). Deferred calls that share a binding, an
  attribute, and a call-shape fingerprint now resolve from one hover/definition
  round trip instead of one per call site, cutting the CPython completeness
  run's ``ty`` request volume by about 17% (138k to 114k requests). Bindings
  stay grouped only while they are provably stable: any rebinding, ``del``,
  augmented assignment, ``global``/``nonlocal``, ``match`` subject or capture,
  narrowing test, or escape into a call poisons the group, a binding that
  shadows an enclosing name retroactively un-groups call sites recorded before
  it in the same scope (Python makes the name scope-local throughout), and
  class-body bindings never shadow names inside methods. The wider reuse also
  recovers nine CPython diagnostics that per-site queries had been losing to
  ``ty``'s answer instability (``LibraryLoader.LoadLibrary``,
  ``IdleConf.GetOption``, ``ExitStack.enter_context``, ...), with no entries
  lost on either completeness oracle.

  The whole-project pipeline now also streams: files needing the ``ty``
  fallback are dispatched to the eight shard servers in sorted order as soon as
  the parallel built-in pass finishes each file, so the servers work
  concurrently with the scan instead of idling until it drains, and a shard
  that never receives a query no longer starts a server at all. The
  first-party index build reads and parses candidate files in parallel as
  well (about 1.1s to 0.5s on a CPython checkout). Each server's request
  order remains deterministic, and the fixed shard count keeps the partition
  independent of the host's core count. The new partition also stops emitting
  three dynamic-call false positives that the completeness floors previously
  required (one Sphinx ``node.__class__`` call and two CPython loop-variable
  calls).

- Fix a regression in ``ty`` hover-answer reuse that dropped real diagnostics
  for calls grouped across a platform-specific branch. When a group's earliest
  call site sits in code ``ty`` considers unreachable (for example a
  ``sys.platform``-guarded branch that is dead on the host platform), ``ty``
  answers with the bottom type ``Never`` instead of the receiver's real
  signature. That answer was cached and reused for the whole group, suppressing
  the group's live members, which do resolve. Only a usable callable signature
  is now treated as the shared group answer, so a ``Never`` (or absent) answer
  at one member no longer hides violations at the others.

- Include Python environment modules and stubs in the persistent diagnostic
  cache fingerprint. Changes below a project ``.venv``, ``VIRTUAL_ENV``, or an
  explicit ``--python`` environment now invalidate cached results instead of
  returning diagnostics based on stale dependency signatures.

- Preserve a Python file's PEP 263 encoding when applying ``check --fix``.
  Legacy Latin-1 source is now written back as Latin-1 instead of silently
  changing non-ASCII bytes to UTF-8 while leaving the original encoding
  declaration in place. UTF-8 byte-order marks are preserved as well.

- Raise the required ``ty`` floor from ``0.0.46`` to ``0.0.52``, the release the
  LSP/hover integration is now verified against. The full test suite (including
  the hover and goto-definition goldens) passes unchanged on ``0.0.52``, so the
  hover/LSP surface this project parses is unaffected. On the completeness
  oracles the new ``ty`` is effectively neutral versus ``0.0.46``: a handful of
  diagnostics churn either way, the only systematic change being ty's
  ``dict.pop`` overload fix, which drops three ``pop`` call sites that are no
  longer flagged.

- Reject an explicit ``--project-root`` that is missing or is not a directory instead of silently ignoring project configuration.

- Reuse one ``ty`` hover/definition answer for repeated same-shape
  ``self.method(...)``/``cls.method(...)`` calls, cutting the ty fallback
  roughly 40% on method-call-heavy projects (CPython completeness benchmark:
  ~24s to ~14.5s ty phase, ~29s to ~18s end to end; Sphinx: ~4s to ~2.1s).
  The built-in scan groups deferred calls that are proven to hover
  identically: the same un-rebound ``self``/``cls`` parameter binding, the
  same attribute, and the same call shape (argument arity, coarse argument
  kinds, keyword names; ty's hover is call-site sensitive for overloads and
  generics, so shape is part of the key).  The ty fallback asks once per
  group. Grouping is dropped conservatively whenever the receiver could be
  rebound or narrowed (assignment to the name, the bare name escaping into a
  call such as ``isinstance(self, T)``, a comparison/truthiness test, a
  ``match`` statement, an assignment to or non-call mention of the attribute),
  so every reused answer is exactly what ``ty`` would have returned at that
  site. Each server's request stream remains a pure function of the sorted
  work list, so diagnostics stay deterministic across runs and machines; the
  only observed output change on the pinned completeness repositories is four
  additional true positives on CPython where ty previously answered the same
  call sites inconsistently (``skipTest``/``fail``/``fspath``), with no
  entries lost.

- Run the ``ty`` inference fallback on four parallel ``ty server`` shards when
  more than one file needs it, making whole-project runs ~3.5x faster on large
  repositories (CPython: ~45s to ~13s; Sphinx: ~6.6s to ~1.9s). Files are
  partitioned deterministically (greedy least-loaded over the sorted file list,
  with a fixed shard count that never depends on the host's core count) and
  each shard keeps the serial one-request-in-flight discipline, so every run on
  every machine replays identical request streams per server. Each shard also
  replays the full sorted ``didOpen`` history (cheap under pull diagnostics),
  keeping per-query answers aligned with the previous single-server pass.

  The goto-definition fallback now tries every location ``ty`` returns, in
  order, instead of only the first: the relative order of a multi-location
  answer (re-exports, a class plus a local binding) depends on the answering
  server's open/query history, and the leading entry is often a local binding
  the definition parser cannot use. Trying each location makes the outcome
  independent of that ordering and recovers definitions a first-entry-only
  read silently dropped: roughly 200 new true positives on the CPython
  completeness oracle (``mock.patch.dict``, ``inspect.signature``, ``ctypes``
  constructors) and a handful on Sphinx, with no entries lost; one CPython
  call site is now attributed to the resolved class constructor instead of
  ``object.__class__``. The completeness snapshots were regenerated for these
  additions.

- Speed up the ``ty`` inference fallback dramatically on large projects and
  make its results reproducible. The LSP client now advertises
  pull-diagnostics support, so ``ty`` no longer eagerly type-checks (and
  pushes diagnostics for) every file the fallback opens: hover and definition
  answers are computed on demand instead. The full CPython completeness run
  drops from ~42 minutes to ~42 seconds. Requests sent to ``ty`` also always
  carry absolute ``file://`` URIs now: a relative CLI path (``strict-kwargs
  check .``) previously produced relative URIs whose diagnostics wait could
  never match ``ty``'s absolute ones, stalling for a 15-second timeout per
  file. Fallback results are also deterministic now: files are opened in
  sorted order and queries are issued one at a time, because ``ty`` answers
  multi-location definition requests differently depending on its internal
  thread scheduling when several requests are in flight. Calls whose hover
  previously got dropped under ``ty``'s diagnostics load fell through to the
  goto-definition path (which cannot evaluate ``sys.version_info``-gated
  typeshed signatures); they are now consistently resolved from the
  version-aware hover, and the completeness snapshots were regenerated to
  match the (more accurate, and substantially larger) stable result set.

- Treat filesystem errors encountered during directory traversal as operational
  errors. A check now exits with status 2 and names the unreadable path instead
  of silently skipping part of the requested tree and potentially reporting a
  false clean result.

2026.6.8-post.1
---------------

No significant changes.

2026.6.8
--------

- Add an ignored completeness regression test that checks a pinned external
  repository against a committed conservative golden diagnostic subset, plus
  scheduled CI coverage and a documented baseline regeneration script (issue
  #192).

- Parse ty's ``class Name(...)`` constructor hover directly when resolving a call
  through the ``ty`` fallback, instead of falling back to goto-definition. ty's
  goto-definition for a re-exported standard-library class resolves into the
  runtime ``.py`` shim and lands on the ``from ... import ...`` statement rather
  than the class, so depending on which Python environment ty discovered the old
  path could silently drop the violation. The hover carries the constructor
  signature consistently, so these calls are now reported regardless of the
  environment (issue #195).

- Make the ``ty`` inference fallback deterministic. Before querying call sites,
  ``strict-kwargs`` now warms ``ty`` up by having it type-check the whole project,
  so hover/definition results no longer race ty's background indexing. This makes
  results reproducible run-to-run and removes a class of false positives on
  positional-only standard-library calls (e.g. ``str.split``, ``int.to_bytes``,
  ``Path.glob``) that the previous racing resolution flagged spuriously. The
  warm-up runs only when the ``ty`` fallback is actually needed, so runs the
  built-in resolver fully handles are unaffected (issue #198).

- Tighten the ignored pinned-repository completeness regression test so scheduled
  CI fails on missing stable diagnostics and on undocumented extra diagnostics,
  with a regenerated full golden oracle and a documented unstable-extra allowance
  file (issue #207).

- Move the completeness golden oracle to an `insta` snapshot and remove the
  allowed-extra diagnostic baseline.

2026.6.4
--------

- Add Ruff-style ``# noqa`` suppression for ``KW001``. A ``# noqa`` or
  ``# noqa: KW001`` comment on the line a diagnostic is reported on suppresses
  that finding and skips any auto-fix for the call; a directive naming other
  codes leaves the call reported (issue #185).

2026.5.20
---------


- Add ``[tool.strict_kwargs].src`` and ``namespace_packages`` settings for
  first-party module resolution. ``src`` roots are searched alongside the
  repository root and are used when deriving module names, so ``src/pkg/mod.py``
  can resolve as ``pkg.mod`` while namespace-package directories can be marked
  even without ``__init__.py`` (issue #142).
- Add configurable project-level file exclusions. ``[tool.strict_kwargs]`` now
  accepts ``extend_exclude`` patterns for directory runs and
  ``force_exclude = true`` to apply those exclusions to explicitly passed
  files, matching pre-commit workflows (issue #141).
- ``strict-kwargs`` checks can now enable the persistent diagnostic cache
  through ``[tool.strict_kwargs].cache_dir`` or the
  ``STRICT_KWARGS_CACHE_DIR`` environment variable. The effective cache
  location precedence is ``--cache-dir``, then pyproject configuration, then
  the environment variable. Relative pyproject paths resolve against the
  project root.

2026.5.19-post.3
----------------


- ``--fix-synthesized-constructors`` can now also be enabled in
  ``pyproject.toml`` with
  ``[tool.strict_kwargs].fix_synthesized_constructors = true``.

2026.5.19-post.2
----------------


2026.5.19-post.1
----------------


- Add category-specific ``strict-kwargs fix`` controls instead of a blanket
  unsafe mode. ``--fix-synthesized-constructors`` rewrites dataclass and
  ``NamedTuple`` calls from synthesized field models. Overload rewrites are
  default-on when analysis selects one precise arm. The synthesized
  constructor control can be used with or without ``--diff``; ordinary
  single-signature fixes remain default-on regardless of which resolver found
  the signature.

- ``strict-kwargs fix`` now reports declined rewrite reasons by category on
  stderr, including synthesized constructors, unresolved overloads,
  ambiguous ``ty`` hovers, goto-definition-only ``ty`` resolutions,
  unsafe call-site unpacking, and unsupported signature shapes. ``--diff``
  stdout remains patch-only.

2026.5.19
---------


- Synthesized ``@dataclass`` constructor models now include inherited
  dataclass fields in runtime order, while preserving exclusions such as
  ``ClassVar``, ``field(init=False)``, ``@dataclass(init=False)``, and
  hand-written constructors (issue #96). ``NamedTuple`` subclasses now reuse
  inherited tuple fields without treating newly annotated subclass attributes
  as constructor parameters. The auto-fixer still declines synthesized
  constructors.

2026.5.18-post.4
----------------

2026.5.18-post.3
----------------


2026.5.18-post.2
----------------


2026.5.18-post.1
----------------


- Whole-project and directory runs are faster (issue #46). The per-file
  built-in pass (read, parse, AST walk) now runs in parallel across files
  instead of sequentially: on a multicore machine it is the bulk of
  whole-project runtime once ignored directories are pruned. The ``ty``
  fallback still runs serially against a single shared server, and output
  is byte-identical and deterministic regardless of how the work is
  scheduled.

- A deeply nested file no longer crashes the process with a stack
  overflow (issue #54). ``f(f(f(…f(1)…)))`` thousands of levels deep
  (machine-generated code, a huge data literal, or hostile input) used to
  abort the whole run with ``SIGABRT`` (exit 134), taking every other file
  in a directory or pre-commit run down with it; the vendored Ruff parser
  fork enforces no recursion limit. The analysis now runs on a large
  dedicated stack so legitimate deep nesting is handled identically across
  platforms and build profiles (rather than depending on the host's default
  stack), and a file nested deeper than the supported limit is rejected up
  front with a clear ``expression nesting too deep`` message and exit code
  2 instead of crashing.

- Operational errors are no longer silently swallowed (issue #55).
  Previously a mistyped path made the run report "clean" (exit 0), a
  malformed or wrong-typed ``[tool.strict_kwargs]`` was ignored and the run
  proceeded with defaults, and an invalid ``--python`` silently disabled the
  explicit environment: each a false pass or a silent downgrade in exactly
  the automated contexts this tool targets. Now: a path that does not exist
  is a hard error (exit 2), like ``ruff``, instead of being skipped (an
  existing non-Python file passed directly is still a deliberate selection
  and is skipped); a ``pyproject.toml`` that exists but cannot be read or
  parsed, or whose ``[tool.strict_kwargs]`` has the wrong shape or value
  types (e.g. ``ignore_names`` not a list), is a hard error (exit 2) rather
  than a silent fall back to defaults (a missing ``pyproject.toml`` or one
  without the table is still fine); and a nonexistent ``--python`` is
  reported on stderr and dropped, so the run falls back to ``ty``'s own
  environment discovery instead of degrading detection with no signal. The
  library ``Config::load`` now returns ``Result<Config, CheckError>`` and
  there are two new ``CheckError`` variants (``PathNotFound``,
  ``ConfigInvalid``).

- A single non-UTF-8 file no longer aborts the whole run or masks
  violations in every other file (issue #53). Previously one stray byte (a
  binary fixture, vendored data, a legacy-encoded module) failed the run
  with exit 2 *and* suppressed real violations everywhere else. Now an
  undecodable file is reported as a warning and skipped while the rest of
  the run proceeds and still reports genuine violations, mirroring
  ruff/pyright. A UTF-8 BOM and a `PEP 263
  <https://peps.python.org/pep-0263/>`_ ``# -*- coding: <enc> -*-``
  declaration in the first two lines are now honored, so legacy-encoded but
  valid Python (``latin-1``/``iso-8859-1``, ``ascii``, explicit ``utf-8``)
  is decoded and checked rather than rejected. Any other *declared* encoding
  degrades to the same graceful skip (still no crash, no masking, just not
  analysed); no third-party codec dependency is added. A genuine filesystem
  error (missing file, permission denied) is still fatal: that is a real
  error, not a stray file.

2026.5.18
---------

- Fix a false negative where a call in **decorator** position was never
  analyzed (issue #51). Decorator-factory calls with surplus positional
  arguments (``@retry(3, 0.5)``, ``@functools.lru_cache(128)``,
  ``@app.route("/x", 200)``, including attribute-chain and method
  decorators) are now flagged exactly like the same call in statement
  position, and ``fix`` rewrites them (``@retry(times=3, delay=0.5)``)
  with the same conservative rules. The call-site walker previously
  descended only into function/class bodies and skipped their decorator
  lists entirely.

- ``ty`` is now a hard requirement instead of an optional fallback. When
  ``ty`` cannot be located (next to the ``strict-kwargs`` binary or on
  ``PATH``) or its language server will not start, the run aborts with exit
  code 2 instead of silently resolving fewer calls, so results are
  deterministic across machines rather than depending on whether ``ty``
  happens to be installed. ``ty`` is now declared as a PyPI dependency
  (``ty>=0.0.23``, the version the integration is verified against), so a
  ``pip``/``uv`` install brings it along automatically; ``cargo install``
  users still install ``ty`` themselves. Per-call resolution still fails
  closed (a miss, never a wrong diagnostic).

- ``strict-kwargs fix`` no longer silently disagrees with ``check``
  (issue #42). It now runs the same detection (the built-in resolver
  *and* the ``ty`` fallback) and accepts ``--python`` (mirroring
  ``check``) to steer that fallback. The rewrite stays conservative and, by
  design (issue #7), still never edits an overloaded, synthesized, or
  ``ty``-only-resolved call (a wrong parameter name would corrupt source,
  cf. issue #41); but ``fix`` now reports how many violations it detected
  and declined to rewrite. That count is exactly what a following
  ``strict-kwargs`` run (with the same ``--python``) still reports, so
  ``fix`` then ``check`` is predictable instead of leaving violations with
  no signal. The ``ty`` fallback still starts lazily, so the all-first-party
  common case pays nothing. The library ``fix_paths`` now takes a
  ``python_env`` argument and returns a ``FixOutcome`` (``files`` plus a
  ``declined`` count) instead of a bare ``Vec<FileFix>``.

- Fix ``strict-kwargs fix`` corrupting source on a redundantly
  parenthesized argument (issue #41). The Ruff parser drops redundant
  parentheses, so ``f((1), (2))`` used to rewrite to the unparsable
  ``f((a=1), (b=2))``; the ``name=`` prefix now lands *before* the
  parentheses (``f(a=(1), b=(2))``), so the result parses and the fix is
  idempotent. As an independent fail-safe, the rewritten module is parsed
  before anything is written: if it would not parse, the run aborts with a
  clear message and every file is left untouched rather than corrupted.

- Performance: a file importing a heavy third-party package
  (``numpy``/``torch``/``scipy``/``PIL``) is now checked in milliseconds
  instead of timing out (issue #39, follow-up to #31/#36). The eager
  re-export expansion is gone; the ``DefinitionIndex`` now resolves modules
  *and* re-export aliases lazily and on demand: only the modules a queried
  name's actual re-export path traverses are parsed, instead of the whole
  transitive import closure. Re-export edges are indexed by destination
  (O(name-depth) per hop, not O(total edges)), a self-referential
  ``from pkg.sub import *`` web resolves via single-segment hops without the
  unbounded ``pkg.sub.sub…`` blow-up, and per-query module/step backstops
  keep an unforeseen pathology fail-closed (defers to ``ty``; never a false
  positive). Resolution is otherwise unchanged: all existing behavior tests
  pass. A ``reexport_closure`` benchmark covers this shape (issue #30).

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
- Performance: ``ty server`` is started lazily, only when a file has calls
  the built-in resolver could not resolve. Runs the built-in resolver fully
  handles (the common editor-on-save / pre-commit case on first-party code)
  no longer pay ty's project-indexing startup cost (issue #31).
- Continuous benchmarking via `CodSpeed <https://codspeed.io>`_: a divan
  benchmark suite (``benches/resolver.rs``) covering a leaf file, a large
  stdlib import closure, an overload/special-form heavy file, and a
  generated first-party closure, plus the auto-fixer. A non-gating CI job
  reports an instruction-count delta against ``main`` on every PR.
- ``strict-kwargs fix``: auto-rewrite surplus positional call arguments to
  keyword arguments (``--diff`` to preview). Conservative: only calls that
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
  these. The functional ``NamedTuple("N", [...])``/``namedtuple`` forms,
  ``attrs``, and ``TypedDict`` remain out of scope.
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
2026.5.16-post.1
----------------


2026.05.16
----------

- Fast Rust linter enforcing keyword arguments at call sites (companion to `mypy-strict-kwargs`).
- Configuration via ``pyproject.toml`` (``[tool.strict_kwargs]``).
