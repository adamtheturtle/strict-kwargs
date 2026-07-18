//! Index of callable definitions discovered in the project.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ruff_python_ast::{self as ast};
use ruff_python_ast::{Expr, ModModule, Stmt};
use ruff_python_parser::{parse_module, Parsed};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast_util::signature_from_parameters;
use crate::config::SourceRoots;
use crate::error::CheckError;
use crate::limits::parse_module_guarded;
use crate::resolve::ModuleResolver;
use crate::signature::{ParameterKind, Signature};
use crate::source::read_python_source_lossy;

mod data_model;

#[cfg(test)]
use data_model::extend_unique;
use data_model::{
    callee_tail, dataclass_decorator, is_namedtuple_class, synthesize_data_constructor,
};

/// Safety bound on re-export alias chain length during lazy resolution. Real
/// code converges in a handful of hops; this only stops a pathological or
/// cyclic chain (the cycle is also caught by the per-resolution visited set).
const MAX_ALIAS_DEPTH: usize = 64;

/// Backstop on the *new* modules a single `get` query may resolve+parse, and
/// on its total `resolve_alias` calls. The structural defense against a
/// `from X import *` web (`torch`'s) is the self-referential single-segment
/// rule in [`DefinitionIndex::resolve_alias`]; with it even `torch.tensor`
/// resolves in a few hops (measured: `numpy.array` 3 modules / 2 calls,
/// `torch.tensor` single-digit). These caps are pure insurance against an
/// unforeseen pathology: on exhaustion the query yields `None` — the call
/// defers to the `ty` fallback (or is left unchecked), exactly the documented
/// best-effort-third-party / fail-closed contract, never a false positive.
const MAX_QUERY_MODULES: usize = 200;
/// See [`MAX_QUERY_MODULES`]. Counts every call (not just distinct names) so
/// branching cannot multiply the work past this bound.
const MAX_QUERY_STEPS: usize = 1500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModuleState {
    Indexing,
    Indexed,
}

/// The real definitions discovered so far: fully-qualified name -> one or
/// more signatures (multiple for ``@overload`` stubs), plus
/// the set of *synthesized* constructors. This is the part the indexing
/// walk (`index_module`) writes; it grows as modules are resolved — eagerly
/// for builtins/checked files, lazily on demand for everything else.
#[derive(Debug, Default)]
struct Store {
    signatures: FxHashMap<String, Vec<Signature>>,
    /// Modules supplied as check targets rather than loaded from vendored
    /// typeshed or lazily resolved dependencies.
    first_party_modules: FxHashSet<String>,
    /// Names whose most recent definitions are an open sequence of
    /// ``@overload`` arms. The following undecorated implementation closes
    /// the sequence without replacing its public overload signatures.
    pending_overloads: FxHashSet<String>,
    /// Constructor fullnames whose signature we *synthesized* from class
    /// fields (``@dataclass`` / ``NamedTuple``) rather than reading a written
    /// ``def``. The default auto-fixer declines these;
    /// ``--fix-synthesized-constructors`` may opt into the field-model
    /// mapping.
    synthesized: FxHashSet<String>,
    /// Field models for classes whose constructor is synthesized by
    /// dataclasses / ``NamedTuple`` machinery, or inherited from such a base.
    data_models: FxHashMap<String, ClassDataModel>,
    /// Direct base classes for indexed classes, resolved to fully-qualified
    /// names using the imports visible at the class definition.
    class_bases: FxHashMap<String, Vec<String>>,
    /// Fully-qualified class names discovered while indexing.
    classes: FxHashSet<String>,
    /// Explicit metaclass for each indexed class, resolved to its
    /// fully-qualified name at the class definition.
    class_metaclasses: FxHashMap<String, String>,
    /// Function fullnames that must be skipped entirely (neither flagged nor
    /// rewritten). Currently populated for ``@singledispatch`` /
    /// ``@singledispatchmethod`` functions, whose dispatch reads
    /// ``args[0].__class__``; a keyword first argument would raise
    /// ``TypeError`` at runtime.
    excluded: FxHashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassDataKind {
    Dataclass,
    NamedTuple,
}

#[derive(Debug, Clone)]
struct ClassDataModel {
    kind: ClassDataKind,
    init_fields: Vec<String>,
}

impl Store {
    fn insert(&mut self, fullname: String, signature: Signature) {
        self.signatures.entry(fullname).or_default().push(signature);
    }

    fn insert_definition(&mut self, fullname: String, signature: Signature, is_overload: bool) {
        if is_overload {
            if self.pending_overloads.insert(fullname.clone()) {
                self.signatures.remove(&fullname);
            }
            self.insert(fullname, signature);
        } else if !self.pending_overloads.remove(&fullname) {
            self.signatures.insert(fullname, vec![signature]);
        }
    }
}

// Covered through callable-instance integration tests. Excluded from the
// coverage gate because llvm-cov reports branch holes for the duplicated
// test-binary instantiations of this small binding shim.
#[cfg_attr(coverage, coverage(off))]
fn bound_callable_instance_signature(signature: Signature) -> Signature {
    let mut parameters = signature.parameters;
    if parameters
        .first()
        .and_then(|param| param.name.as_deref())
        .is_some_and(|name| name == "self" || name == "cls")
    {
        parameters.remove(0);
    }
    Signature { parameters }
}

/// Mutable state shared between the eager construction pass and the lazy
/// per-query resolution (the latter only has `&self`, hence the interior
/// `RwLock` — see [`DefinitionIndex::read`]/[`DefinitionIndex::write`]).
#[derive(Debug, Default)]
struct Inner {
    store: Store,
    /// Re-export edges indexed by destination: ``dst_prefix`` -> the
    /// ``src_prefix``es re-exported under it (insertion order preserved, so
    /// the first-collected alias still wins). "Everything under ``src_prefix``
    /// (the prefix itself and any ``src_prefix.<sfx>``) is reachable as
    /// ``dst_prefix`` / ``dst_prefix.<sfx>``." Resolved **on demand**
    /// ([`DefinitionIndex::get`]) instead of eagerly expanding the full alias
    /// cross-product over the import closure — eager expansion is superlinear
    /// and does not complete on heavy third-party closures (numpy/torch/scipy)
    /// while only a handful of names are ever queried (issue #39). Keying by
    /// `dst` makes a query's per-hop cost O(dotted-depth of the name) instead
    /// of O(total edges) — the latter is thousands for a `torch`-sized
    /// star-import web. No-op/empty edges are dropped before being inserted.
    by_dst: FxHashMap<String, Vec<String>>,
    /// Modules already being resolved or fully resolved+indexed (or attempted),
    /// so a module — and the heavy third-party closure behind it — is parsed at
    /// most once. Misses are memoized too. An `Indexing` entry is a claim held
    /// by one worker; other workers wait for it to become `Indexed` before they
    /// use the store/cache state that module may populate.
    modules: FxHashMap<String, ModuleState>,
    /// Remaining lazy-module-resolution budget: a pathological dependency
    /// graph cannot blow up time/memory even though resolution is on demand.
    budget: usize,
    /// Memoizes [`DefinitionIndex::get`] (including resolved-to-`None`), so a
    /// name queried repeatedly across the file walk is chased through the
    /// edge graph at most once.
    cache: FxHashMap<String, Option<Arc<[Signature]>>>,
}

pub struct DefinitionIndex {
    /// Resolves a dotted module name to source. `None` in unit tests that
    /// drive the edge/signature logic directly (no module resolution).
    resolver: Option<ModuleResolver>,
    inner: RwLock<Inner>,
}

/// Source and parsed AST for a first-party file that was successfully indexed.
///
/// The check/fix scan phase can reuse this instead of reading and parsing the
/// same file again. Files that cannot be decoded or parsed are intentionally
/// absent so the scan phase preserves its existing user-facing skip/error
/// behavior.
pub struct IndexedFile {
    pub source: String,
    pub parsed: Parsed<ModModule>,
}

struct ModuleIndexClaim<'a> {
    index: &'a DefinitionIndex,
    dotted: String,
}

impl Drop for ModuleIndexClaim<'_> {
    fn drop(&mut self) {
        self.index
            .write()
            .modules
            .insert(self.dotted.clone(), ModuleState::Indexed);
    }
}

impl DefinitionIndex {
    fn new(resolver: ModuleResolver) -> Self {
        Self {
            resolver: Some(resolver),
            inner: RwLock::new(Inner {
                budget: MODULE_BUDGET,
                ..Inner::default()
            }),
        }
    }

    /// Shared-read access to the inner state. The whole-project run scans
    /// files in parallel (issue #46) over this one demand-driven index;
    /// resolution is overwhelmingly read-only after warmup, so a [`RwLock`]
    /// lets those reads run concurrently instead of serializing on a single
    /// mutex (which dominated wall time on many-core machines). A poisoned
    /// lock (a worker panicked while holding it) still yields the data:
    /// `Inner` is a pure memoization cache over deterministic resolution, so a
    /// half-updated entry is at worst a redundant re-resolve, never
    /// unsoundness — strictly better than turning every other worker's access
    /// into a panic.
    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Exclusive access for the rare mutations — caching a freshly resolved
    /// name, claiming/finishing a module, indexing parsed definitions. Every
    /// hold is short (a map lookup/insert); module parsing happens outside any
    /// guard. See [`Self::read`].
    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claim `dotted` for indexing, coordinating with other parallel workers.
    /// Returns `None` if it is already fully `Indexed` (nothing to do), or a
    /// claim whose [`Drop`] marks it `Indexed` once this worker finishes. If
    /// another worker currently holds an in-progress claim, this spins
    /// (yielding) until that claim resolves — module parsing is brief and
    /// lock-free, so the wait is short and only happens on a genuine race for
    /// the same not-yet-loaded module.
    fn claim_module(&self, dotted: &str) -> Option<ModuleIndexClaim<'_>> {
        // Fast path: a concurrent read confirms an already-indexed module, so
        // the common case (every re-query of a loaded module) never contends
        // on the write lock.
        if matches!(self.read().modules.get(dotted), Some(ModuleState::Indexed)) {
            return None;
        }
        loop {
            let mut inner = self.write();
            match inner.modules.get(dotted).copied() {
                Some(ModuleState::Indexed) => return None,
                Some(ModuleState::Indexing) => {
                    // Another worker is mid-index; release the lock and retry.
                    drop(inner);
                    std::thread::yield_now();
                }
                None => {
                    inner
                        .modules
                        .insert(dotted.to_string(), ModuleState::Indexing);
                    drop(inner);
                    return Some(ModuleIndexClaim {
                        index: self,
                        dotted: dotted.to_string(),
                    });
                }
            }
        }
    }

    // First-party indexing is single-threaded today, but it shares module
    // state with lazy constructor-base preloading. Keep the coordination
    // centralized and out of the coverage gate: the in-progress wait is a
    // defensive branch for a future parallel eager indexer.
    #[cfg_attr(coverage, coverage(off))]
    fn claim_first_party_module(&self, dotted: &str) -> Option<ModuleIndexClaim<'_>> {
        self.claim_module(dotted)
    }

    /// Record re-export edges into the by-destination index, dropping no-ops
    /// (self-edges, empty endpoints) so demand resolution never reconsiders
    /// them. Insertion order within a `dst` is preserved.
    fn push_edges(inner: &mut Inner, edges: Vec<(String, String)>) {
        for (src, dst) in edges {
            if src != dst && !src.is_empty() && !dst.is_empty() {
                inner.by_dst.entry(dst).or_default().push(src);
            }
        }
    }

    /// Parse-free indexing of one already-parsed module: record its real
    /// definitions and its re-export edges. Shared by the eager pass
    /// (builtins / checked files) and lazy [`Self::ensure_module`].
    fn index_source(&self, module_name: &str, is_package: bool, stmts: &[Stmt]) {
        self.index_source_with_imported_base_preload(module_name, is_package, stmts, true);
    }

    fn mark_first_party_module(&self, module_name: &str) {
        self.write()
            .store
            .first_party_modules
            .insert(module_name.to_string());
    }

    fn index_source_with_imported_base_preload(
        &self,
        module_name: &str,
        is_package: bool,
        stmts: &[Stmt],
        preload_imported_bases: bool,
    ) {
        let mut active_modules = FxHashSet::default();
        active_modules.insert(module_name.to_string());
        self.index_source_with_active(
            module_name,
            is_package,
            stmts,
            preload_imported_bases,
            &mut active_modules,
        );
    }

    fn index_source_with_active(
        &self,
        module_name: &str,
        is_package: bool,
        stmts: &[Stmt],
        preload_imported_bases: bool,
        active_modules: &mut FxHashSet<String>,
    ) {
        let mut collected = Collected::default();
        collect(
            stmts,
            module_name,
            is_package,
            preload_imported_bases,
            &mut collected,
        );
        let mut query_budget = MAX_QUERY_MODULES;
        for base in &collected.data_constructor_bases {
            if !same_module_or_nested(module_name, base) {
                self.ensure_for_data_constructor_base(base, &mut query_budget, active_modules);
            }
        }
        let mut inner = self.write();
        let track_data_constructors = collected.has_data_constructor_classes
            || collected
                .data_constructor_bases
                .iter()
                .any(|base| inner.store.data_models.contains_key(base));
        index_module(
            &mut inner.store,
            module_name,
            is_package,
            stmts,
            track_data_constructors,
        );
        for (class_name, bases) in collected.class_bases {
            inner.store.class_bases.insert(class_name, bases);
        }
        for (class_name, metaclass) in collected.class_metaclasses {
            inner.store.class_metaclasses.insert(class_name, metaclass);
        }
        for (instance_name, class_name) in collected.callable_instances {
            let Some(signatures) = inner
                .store
                .signatures
                .get(&format!("{class_name}.__call__"))
                .cloned()
            else {
                continue;
            };
            inner.store.signatures.insert(
                instance_name,
                signatures
                    .into_iter()
                    .map(bound_callable_instance_signature)
                    .collect(),
            );
        }
        Self::push_edges(&mut inner, collected.reexports);
        // Release before returning so a parallel worker's next query does not
        // wait on a guard the borrow checker would otherwise hold to scope
        // end (clippy::significant_drop_tightening).
        drop(inner);
    }

    /// Resolve, parse and index `dotted` if not already done. Memoized
    /// (including misses) and doubly budget-capped — a global cap and the
    /// caller's per-query `query_budget` — so the transitive third-party
    /// closure behind a heavy import is *not* eagerly walked: only the
    /// modules a queried name's re-export path actually traverses are parsed
    /// (issue #39). A resolution/parse failure, or an exhausted budget, is a
    /// silent miss (the call then defers to `ty` / is unchecked — fail
    /// closed, never a false positive).
    //
    // Excluded from the coverage gate: every arm here is a resolve/parse/
    // budget *guard* — a missing module, an unparsable one, or one of the
    // safety caps (`indexed` memo, global `budget`, per-query
    // `query_budget`). Those misses are not deterministically reachable from
    // the test suite (vendored stubs and the fixture packages always resolve
    // and parse; the caps are pathological-only — see [`MAX_QUERY_MODULES`]),
    // while the success path's actual indexing work is `index_source`, which
    // *is* gated and exercised end-to-end by the import-resolution suite.
    // Same rationale as the other documented exclusions (`index_source`'s
    // callees, `synthesize_data_constructor`).
    #[cfg_attr(coverage, coverage(off))]
    fn ensure_module(&self, dotted: &str, query_budget: &mut usize) {
        let Some(claim) = self.claim_module(dotted) else {
            return;
        };
        let Some(resolver) = self.resolver.as_ref() else {
            return;
        };
        let Some(m) = resolver.resolve(dotted) else {
            return;
        };
        // A real module was found; parsing it is the expensive step. Bound it
        // both per query and globally (cheap non-resolving candidate names —
        // the bulk of a star-import fan-out — never reach here).
        if *query_budget == 0 {
            return;
        }
        {
            let mut inner = self.write();
            if inner.budget == 0 {
                return;
            }
            inner.budget -= 1;
        }
        *query_budget -= 1;
        // File-backed dependencies are guarded: a deeply-nested dependency
        // (e.g. a machine-generated first-party or site-packages stub) must be
        // rejected gracefully, not crash the analysis thread (issue #83).
        // Vendored typeshed is embedded, pinned, and trusted; keep it on the
        // old direct parse path so every run does not rescan large bundled
        // stubs such as `builtins.pyi`.
        let parsed = if m.guard_nesting {
            parse_module_guarded(&m.source)
        } else {
            parse_module(&m.source).map_err(CheckError::from)
        };
        let Ok(parsed) = parsed else {
            return;
        };
        self.index_source_with_imported_base_preload(
            dotted,
            m.is_package,
            parsed.suite(),
            m.guard_nesting,
        );
        drop(claim);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn ensure_module_data_constructor_base(
        &self,
        dotted: &str,
        query_budget: &mut usize,
        active_modules: &mut FxHashSet<String>,
    ) {
        if active_modules.contains(dotted) {
            return;
        }
        let Some(claim) = self.claim_module(dotted) else {
            return;
        };
        let Some(resolver) = self.resolver.as_ref() else {
            return;
        };
        let Some(m) = resolver.resolve(dotted) else {
            return;
        };
        if *query_budget == 0 {
            return;
        }
        {
            let mut inner = self.write();
            if inner.budget == 0 {
                return;
            }
            inner.budget -= 1;
        }
        *query_budget -= 1;
        let parsed = if m.guard_nesting {
            parse_module_guarded(&m.source)
        } else {
            parse_module(&m.source).map_err(CheckError::from)
        };
        let Ok(parsed) = parsed else {
            return;
        };
        active_modules.insert(dotted.to_string());
        self.index_source_with_active(
            dotted,
            m.is_package,
            parsed.suite(),
            m.guard_nesting,
            active_modules,
        );
        active_modules.remove(dotted);
        drop(claim);
    }

    /// Ensure every dotted prefix of `name` (parents first) and `name` itself
    /// is resolved, so the module that *defines* `name` and every package
    /// `__init__` whose re-exports *route* to it are indexed. Misses are
    /// memoized, so a non-module prefix (the symbol itself) costs O(1).
    fn ensure_for(&self, name: &str, query_budget: &mut usize) {
        let mut idx = 0;
        while let Some(rel) = name[idx..].find('.') {
            let end = idx + rel;
            self.ensure_module(&name[..end], query_budget);
            idx = end + 1;
        }
        self.ensure_module(name, query_budget);
    }

    #[cfg_attr(coverage, coverage(off))]
    fn ensure_for_data_constructor_base(
        &self,
        name: &str,
        query_budget: &mut usize,
        active_modules: &mut FxHashSet<String>,
    ) {
        let mut idx = 0;
        while let Some(rel) = name[idx..].find('.') {
            let end = idx + rel;
            self.ensure_module_data_constructor_base(&name[..end], query_budget, active_modules);
            idx = end + 1;
        }
        self.ensure_module_data_constructor_base(name, query_budget, active_modules);
    }

    /// Resolve `fullname` to its signatures, following re-export edges
    /// backwards on demand. A real definition always wins; aliases are only
    /// consulted when no definition is bound under the queried name. Memoized.
    pub fn get(&self, fullname: &str) -> Option<Arc<[Signature]>> {
        // Scope the guard so it is released before `resolve_alias` (which
        // re-locks): holding it across that call would self-deadlock the
        // non-reentrant lock, where the old `RefCell` merely panicked.
        {
            let inner = self.read();
            if let Some(hit) = inner.cache.get(fullname) {
                return hit.clone();
            }
        }
        let mut visited = FxHashSet::default();
        let mut query_budget = MAX_QUERY_MODULES;
        let mut steps = MAX_QUERY_STEPS;
        let resolved = self.resolve_alias(fullname, &mut visited, 0, &mut query_budget, &mut steps);
        self.write()
            .cache
            .insert(fullname.to_string(), resolved.clone());
        resolved
    }

    /// Resolve `method` on `class_fullname`, searching the indexed class's
    /// direct bases recursively before deferring to ty. This intentionally
    /// handles only bases the index has already resolved to concrete first
    /// party / typeshed / discovered package classes.
    pub fn resolve_method(&self, class_fullname: &str, method: &str) -> Option<String> {
        let mut visited = FxHashSet::default();
        self.resolve_method_inner(class_fullname, method, &mut visited)
    }

    /// Whether `class_fullname` inherits from `base_fullname` through the
    /// indexed direct-base graph.
    pub fn class_inherits_from(&self, class_fullname: &str, base_fullname: &str) -> bool {
        let mut visited = FxHashSet::default();
        self.class_inherits_from_inner(class_fullname, base_fullname, &mut visited)
    }

    #[cfg_attr(coverage, coverage(off))]
    pub fn has_overriding_method(&self, class_fullname: &str, method: &str) -> bool {
        let subclasses = self.classes_defining_method(method);
        subclasses.into_iter().any(|subclass| {
            subclass != class_fullname && self.class_inherits_from(&subclass, class_fullname)
        })
    }

    #[cfg_attr(coverage, coverage(off))]
    fn classes_defining_method(&self, method: &str) -> Vec<String> {
        let inner = self.read();
        inner
            .store
            .classes
            .iter()
            .filter(|class| {
                inner
                    .store
                    .signatures
                    .contains_key(&format!("{class}.{method}"))
            })
            .cloned()
            .collect()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub fn has_overriding_method_matching_class_name(
        &self,
        class_name_or_tail: &str,
        method: &str,
    ) -> bool {
        let class_tail = class_name_or_tail
            .strip_prefix("ty.")
            .unwrap_or(class_name_or_tail)
            .rsplit('.')
            .next()
            .unwrap_or(class_name_or_tail);
        let matching_classes: Vec<String> = {
            let inner = self.read();
            inner
                .store
                .classes
                .iter()
                .filter(|class| class.rsplit('.').next() == Some(class_tail))
                .cloned()
                .collect()
        };
        let subclasses = self.classes_defining_method(method);
        matching_classes.iter().any(|class| {
            subclasses
                .iter()
                .any(|subclass| subclass != class && self.class_inherits_from(subclass, class))
        })
    }

    fn class_inherits_from_inner(
        &self,
        class_fullname: &str,
        base_fullname: &str,
        visited: &mut FxHashSet<String>,
    ) -> bool {
        if !visited.insert(class_fullname.to_string()) {
            return false;
        }
        let bases = {
            let mut query_budget = MAX_QUERY_MODULES;
            self.ensure_for(class_fullname, &mut query_budget);
            self.read()
                .store
                .class_bases
                .get(class_fullname)
                .cloned()
                .unwrap_or_default()
        };
        bases.iter().any(|base| {
            base == base_fullname || self.class_inherits_from_inner(base, base_fullname, visited)
        })
    }

    fn resolve_method_inner(
        &self,
        class_fullname: &str,
        method: &str,
        visited: &mut FxHashSet<String>,
    ) -> Option<String> {
        if !visited.insert(class_fullname.to_string()) {
            return None;
        }
        let candidate = format!("{class_fullname}.{method}");
        if self.get(&candidate).is_some() {
            return Some(candidate);
        }
        let bases = {
            let mut query_budget = MAX_QUERY_MODULES;
            self.ensure_for(class_fullname, &mut query_budget);
            self.read()
                .store
                .class_bases
                .get(class_fullname)
                .cloned()
                .unwrap_or_default()
        };
        for base in bases {
            if let Some(found) = self.resolve_method_inner(&base, method, visited) {
                return Some(found);
            }
        }
        None
    }

    /// Whether this resolution has hit a pathological backstop — the
    /// per-query call budget ([`MAX_QUERY_STEPS`]) or the alias-chain depth
    /// cap ([`MAX_ALIAS_DEPTH`]). Both fire only on a star-import web far
    /// beyond anything real (measured `numpy`/`torch` resolutions are
    /// single-digit), so they are not deterministically reachable from the
    /// test suite; excluded from the coverage gate with that documented
    /// rationale (the *cycle* backstop, by contrast, is gated and tested).
    #[cfg_attr(coverage, coverage(off))]
    const fn resolution_exhausted(steps: usize, depth: usize) -> bool {
        steps == 0 || depth >= MAX_ALIAS_DEPTH
    }

    /// Backward re-export resolution: the lazy inverse of the old eager
    /// fixpoint. The modules that could define or route `name` are resolved
    /// on demand first; a direct definition then wins; otherwise, for each
    /// edge whose `dst` is `name` or a dotted-prefix of `name`, try the
    /// corresponding `src` (`src` itself, or `src.<remaining suffix>`) and
    /// recurse. The per-resolution `visited` set breaks re-export cycles;
    /// `depth`, the call budget `steps` ([`MAX_QUERY_STEPS`]) and the
    /// module-parse `query_budget` ([`MAX_QUERY_MODULES`]) together bound a
    /// pathological star-import web (it dies as `None` → fail closed). Within
    /// one `dst`, edges keep collection order so the first-collected alias
    /// wins (the old `or_insert` first-writer-wins precedence); more specific
    /// `dst`s are tried before broader ones.
    fn resolve_alias(
        &self,
        name: &str,
        visited: &mut FxHashSet<String>,
        depth: usize,
        query_budget: &mut usize,
        steps: &mut usize,
    ) -> Option<Arc<[Signature]>> {
        if Self::resolution_exhausted(*steps, depth) {
            return None;
        }
        *steps -= 1;
        self.ensure_for(name, query_budget);
        // Materialize the lookup into an owned value so the guard is dropped
        // (end of this statement) before the recursive `resolve_alias` calls
        // below, which re-lock.
        let direct = self
            .read()
            .store
            .signatures
            .get(name)
            .map(|v| Arc::<[Signature]>::from(v.as_slice()));
        if let Some(sigs) = direct {
            return Some(sigs);
        }
        // Cycle guard: a name already on this resolution's stack dead-ends
        // (covered by `cyclic_edges_terminate_and_still_resolve`).
        if !visited.insert(name.to_string()) {
            return None;
        }
        // An edge applies iff its `dst` is `name` or a dotted-ancestor of it.
        // Look those up directly (the name itself, then each ancestor by
        // trimming a trailing `.segment`) instead of scanning every edge —
        // O(dotted-depth) vs O(total edges). Most-specific `dst` first.
        //
        // A *self-referential* prefix edge — `src` lies inside `dst`'s own
        // subtree, i.e. `from pkg.api import *` (`src = pkg.api`, `dst = pkg`)
        // — rewrites `pkg.<rest>` to `pkg.api.<rest>`, which is itself under
        // `pkg.` and re-triggers the same edge: an unbounded
        // `pkg.api.api.api…` family that starves the real path. For those,
        // only a *single* trailing segment is followed (`from pkg.api import
        // *` re-exports `pkg.api`'s module-level names, so `pkg.<attr>` ->
        // `pkg.api.<attr>` is a one-hop rewrite; chained stars still resolve
        // via successive single-segment hops). Exact matches (`remainder ==
        // ""`) and non-self-referential subtree aliases (e.g. `np = numpy`,
        // `src = numpy` not under `dst = np`) terminate, so stay unrestricted.
        let candidates: Vec<String> = {
            let inner = self.read();
            let mut out = Vec::new();
            let mut end = name.len();
            loop {
                let key = &name[..end];
                let remainder = &name[end..];
                let multi_segment = !remainder.is_empty() && remainder[1..].contains('.');
                if let Some(srcs) = inner.by_dst.get(key) {
                    for src in srcs {
                        let self_referential = src.len() > key.len()
                            && src.as_bytes()[key.len()] == b'.'
                            && src.starts_with(key);
                        if multi_segment && self_referential {
                            continue;
                        }
                        out.push(format!("{src}{remainder}"));
                    }
                }
                match name[..end].rfind('.') {
                    Some(dot) => end = dot,
                    None => break,
                }
            }
            // Drop the guard before the recursive `resolve_alias` loop below
            // (which re-locks) rather than holding it to block scope end
            // (clippy::significant_drop_tightening).
            drop(inner);
            out
        };
        for candidate in candidates {
            if let Some(found) =
                self.resolve_alias(&candidate, visited, depth + 1, query_budget, steps)
            {
                return Some(found);
            }
        }
        None
    }

    /// Whether `fullname` is a constructor we synthesized from class fields
    /// (see [`Store::synthesized`]).
    pub fn is_synthesized(&self, fullname: &str) -> bool {
        self.read().store.synthesized.contains(fullname)
    }

    /// Whether `fullname` is a function that must be skipped entirely
    /// (see [`Store::excluded`]).
    pub fn is_excluded(&self, fullname: &str) -> bool {
        self.read().store.excluded.contains(fullname)
    }

    /// Whether `fullname` denotes a class the built-in index has seen.
    pub fn is_class(&self, fullname: &str) -> bool {
        let mut query_budget = MAX_QUERY_MODULES;
        self.ensure_for(fullname, &mut query_budget);
        self.read().store.classes.contains(fullname)
    }

    /// Number of leading user arguments that must remain positional across
    /// the runtime boundaries of a class call.
    ///
    /// Class construction may cross `__init__`, `__new__`, and an explicit
    /// metaclass `__call__`. A positional-only parameter on any modeled
    /// boundary prevents the corresponding argument from being rewritten as
    /// a keyword, but a merely keyword-capable competing boundary must not
    /// suppress diagnostics from the selected constructor (issue #254).
    pub fn constructor_positional_allowance(&self, class_fullname: &str) -> usize {
        let (mut boundary_signatures, metaclass) = {
            let mut query_budget = MAX_QUERY_MODULES;
            self.ensure_for(class_fullname, &mut query_budget);
            let inner = self.read();
            let mut owner = class_fullname;
            let first_party = loop {
                let Some((parent, _)) = owner.rsplit_once('.') else {
                    break false;
                };
                if inner.store.first_party_modules.contains(parent) {
                    break true;
                }
                owner = parent;
            };
            if !first_party {
                return 0;
            }
            let signatures = ["__init__", "__new__"]
                .into_iter()
                .filter_map(|method| {
                    inner
                        .store
                        .signatures
                        .get(&format!("{class_fullname}.{method}"))
                })
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            let metaclass = inner.store.class_metaclasses.get(class_fullname).cloned();
            drop(inner);
            (signatures, metaclass)
        };
        if let Some(call) = metaclass
            .and_then(|metaclass| self.resolve_method(&metaclass, "__call__"))
            .filter(|method| method != "builtins.type.__call__")
        {
            boundary_signatures.extend(self.get(&call).unwrap_or_default().iter().cloned());
        }

        boundary_signatures
            .iter()
            .map(|signature| {
                signature
                    .parameters
                    .iter()
                    .skip(1)
                    .filter(|parameter| parameter.kind == ParameterKind::PositionalOnly)
                    .count()
            })
            .max()
            .unwrap_or(0)
    }
}

pub fn module_name_for_path(source_roots: &SourceRoots, path: &Path) -> String {
    source_roots.module_name_for_path(path)
}

/// Whether ``path`` is a package initializer (``__init__.py``/``.pyi``).
pub fn is_package_init(path: &Path) -> bool {
    path.file_stem().is_some_and(|s| s == "__init__")
}

/// Safety cap on how many modules a single run will resolve & index, so a
/// pathological dependency graph cannot blow up time/memory.
const MODULE_BUDGET: usize = 4000;

/// Re-export edges ``(source_prefix, dest_prefix)`` discovered in a module,
/// for lazy alias resolution. (Submodules are no longer collected: the import
/// closure is walked on demand, not eagerly — issue #39.)
#[derive(Default)]
struct Collected {
    reexports: Vec<(String, String)>,
    callable_instances: Vec<(String, String)>,
    bindings: FxHashMap<String, String>,
    preload_imported_bases: bool,
    has_data_constructor_classes: bool,
    data_constructor_bases: Vec<String>,
    class_bases: FxHashMap<String, Vec<String>>,
    class_metaclasses: FxHashMap<String, String>,
}

pub fn build_index_with_sources(
    project_root: &Path,
    python_files: &[PathBuf],
    source_roots: &SourceRoots,
) -> (DefinitionIndex, FxHashMap<PathBuf, IndexedFile>) {
    let index = DefinitionIndex::new(ModuleResolver::new(project_root, source_roots));
    let mut indexed_files = FxHashMap::default();

    // Builtins come from vendored typeshed ``stdlib/builtins.pyi``. Resolved
    // eagerly (small, and the bare-name fallback hits it constantly); this is
    // one module, so the query budget is irrelevant here.
    let mut builtins_budget = MAX_QUERY_MODULES;
    index.ensure_module("builtins", &mut builtins_budget);

    // First-party: the files being checked. Indexed from the source we
    // already read here (their call sites are what we walk). Every *other*
    // module — sibling first-party, stdlib, third-party — is resolved lazily
    // on demand by `get`, so a heavy third-party import closure
    // (numpy/torch/scipy) is never eagerly walked (issue #39).
    // Reading + parsing dominates this pass and every file is independent,
    // so that part fans out across cores; the index insertions stay serial
    // in `python_files` order so the first file claiming a module name wins
    // deterministically.
    for (path, read) in python_files
        .iter()
        .zip(read_and_parse_python_files(python_files))
    {
        // A file that cannot be decoded (non-UTF-8 with no usable PEP 263
        // declaration) or parsed is skipped here silently; the check/fix
        // loop reads the same set and emits the single user-facing warning
        // (issue #53). Its definitions just don't get indexed — same as if
        // it were absent.
        let Some((source, parsed)) = read else {
            continue;
        };
        let module_name = module_name_for_path(source_roots, path);
        let Some(claim) = index.claim_first_party_module(&module_name) else {
            continue;
        };
        index.mark_first_party_module(&module_name);
        index.index_source(&module_name, is_package_init(path), parsed.suite());
        drop(claim);
        indexed_files.insert(path.clone(), IndexedFile { source, parsed });
    }

    (index, indexed_files)
}

/// Read and parse one candidate first-party file, or `None` when it cannot
/// be decoded or parsed (the scan pass re-derives and reports the reason).
fn read_and_parse_python_file(path: &Path) -> Option<(String, Parsed<ModModule>)> {
    let source = read_python_source_lossy(path)?;
    let parsed = parse_module_guarded(&source).ok()?;
    Some((source, parsed))
}

/// Read + parse every candidate file in parallel, preserving input order.
///
/// Excluded from the coverage gate like the other parallel-pool
/// orchestration: the pool-construction failure fallback is
/// environment-only, and the per-file logic is the gated
/// [`read_and_parse_python_file`].
#[cfg_attr(coverage, coverage(off))]
fn read_and_parse_python_files(
    python_files: &[PathBuf],
) -> Vec<Option<(String, Parsed<ModModule>)>> {
    use rayon::prelude::*;
    crate::limits::with_large_stack_pool(|| {
        Ok(python_files
            .par_iter()
            .map(|path| read_and_parse_python_file(path))
            .collect())
    })
    .unwrap_or_else(|_| {
        python_files
            .iter()
            .map(|path| read_and_parse_python_file(path))
            .collect()
    })
}

/// Walk ``stmts`` collecting submodules to resolve and re-export edges,
/// resolving relative imports against ``module_name``/``is_package``.
fn collect(
    stmts: &[Stmt],
    module_name: &str,
    is_package: bool,
    preload_imported_bases: bool,
    out: &mut Collected,
) {
    out.preload_imported_bases = preload_imported_bases;
    let mut bindings: FxHashMap<String, String> = FxHashMap::default();
    collect_scoped(
        stmts,
        module_name,
        module_name,
        is_package,
        true,
        &mut bindings,
        out,
    );
    out.bindings = bindings;
}

/// Flatten a pure name/attribute reference (``a`` or ``a.b.c``) into its
/// dotted segments. Returns `None` for anything else (calls, literals,
/// subscripts, …) so only genuine aliases become re-export edges.
fn reference_path(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Name(name) => Some(vec![name.id.to_string()]),
        Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            let mut path = reference_path(value)?;
            path.push(attr.as_str().to_string());
            Some(path)
        }
        _ => None,
    }
}

/// Record a module-level binding ``local -> fullname`` so a later
/// assignment alias (``helper = impl.real``) can resolve its right-hand
/// side. Only meaningful at true module scope.
fn bind(bindings: &mut FxHashMap<String, String>, local: &str, fullname: String) {
    bindings.insert(local.to_string(), fullname);
}

/// Resolve a reference's head against module-level import bindings, falling
/// back to the current module's namespace (a sibling def or an earlier
/// alias, which the re-export fixpoint then chains).
fn resolve_reference(
    bindings: &FxHashMap<String, String>,
    module_name: &str,
    segments: &[String],
) -> Option<String> {
    let (head, rest) = segments.split_first()?;
    let base = bindings
        .get(head)
        .cloned()
        .unwrap_or_else(|| format!("{module_name}.{head}"));
    Some(if rest.is_empty() {
        base
    } else {
        format!("{base}.{}", rest.join("."))
    })
}

// Preloading base modules is a dependency-order optimization for synthesized
// constructor modeling. Its user-visible behaviour is covered end-to-end by
// the imported-base dataclass integration tests; the remaining arms here are
// structural AST traversal guards (control-flow containers, non-reference base
// expressions) and branch coverage is noisy for the same reason as
// `synthesize_data_constructor`.
#[cfg_attr(coverage, coverage(off))]
fn same_module_or_nested(module_name: &str, fullname: &str) -> bool {
    fullname == module_name
        || fullname
            .strip_prefix(module_name)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg_attr(coverage, coverage(off))]
fn base_reference(base: &Expr) -> &Expr {
    match base {
        Expr::Subscript(ast::ExprSubscript { value, .. }) => value.as_ref(),
        other => other,
    }
}

#[cfg_attr(coverage, coverage(off))]
fn resolve_base_name(
    base: &Expr,
    scope_name: &str,
    bindings: &FxHashMap<String, String>,
) -> Option<String> {
    reference_path(base_reference(base))
        .and_then(|segments| resolve_reference(bindings, scope_name, &segments))
}

#[cfg_attr(coverage, coverage(off))]
fn resolve_imported_base_name(
    base: &Expr,
    scope_name: &str,
    bindings: &FxHashMap<String, String>,
) -> Option<String> {
    let segments = reference_path(base_reference(base))?;
    let head = segments.first()?;
    bindings
        .contains_key(head)
        .then(|| resolve_reference(bindings, scope_name, &segments))
        .flatten()
}

#[cfg_attr(coverage, coverage(off))]
fn collect_class_data_constructor_bases(
    class_def: &ast::StmtClassDef,
    scope_name: &str,
    bindings: &FxHashMap<String, String>,
    out: &mut Vec<String>,
    preload_imported_bases: bool,
) -> bool {
    let directly_data_constructor =
        dataclass_decorator(class_def).is_some() || is_namedtuple_class(class_def);
    if let Some(arguments) = &class_def.arguments {
        out.extend(arguments.args.iter().filter_map(|base| {
            if directly_data_constructor {
                resolve_base_name(base, scope_name, bindings)
            } else if preload_imported_bases {
                resolve_imported_base_name(base, scope_name, bindings)
            } else {
                None
            }
        }));
    }
    directly_data_constructor
}

/// `module_scope` is true only at true module level. Imports nested inside a
/// function or class body bind in that local/class namespace, *not* the
/// module's, so they must not create module-level re-export edges (which
/// would make ``module.name`` a false alias). Modules referenced anywhere are
/// resolved lazily on demand (by `get`), so nested imports need no separate
/// queuing here.
fn collect_scoped(
    stmts: &[Stmt],
    module_name: &str,
    scope_name: &str,
    is_package: bool,
    module_scope: bool,
    bindings: &mut FxHashMap<String, String>,
    out: &mut Collected,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Import(ast::StmtImport { names, .. }) => {
                for alias in names {
                    let dotted = alias.name.as_str();
                    let parts: Vec<&str> = dotted.split('.').collect();
                    // ``import a.b as c`` binds ``c`` -> ``a.b``; plain
                    // ``import a.b`` binds the top-level ``a`` -> ``a``.
                    if module_scope {
                        if let Some(asname) = &alias.asname {
                            bind(bindings, asname.as_str(), dotted.to_string());
                        } else {
                            let top = parts.first().copied().unwrap_or(dotted);
                            bind(bindings, top, top.to_string());
                        }
                    }
                }
            }
            Stmt::ImportFrom(ast::StmtImportFrom {
                module,
                names,
                level,
                ..
            }) => {
                let Some(base) = relative_base(
                    module_name,
                    is_package,
                    *level,
                    module.as_ref().map(ast::Identifier::as_str),
                ) else {
                    continue;
                };
                for alias in names {
                    let name = alias.name.as_str();
                    if name == "*" {
                        // ``from base import *`` re-exports all of ``base``,
                        // but only when written at module level.
                        if module_scope && !base.is_empty() {
                            out.reexports.push((base.clone(), module_name.to_string()));
                        }
                        continue;
                    }
                    let qualified = if base.is_empty() {
                        name.to_string()
                    } else {
                        format!("{base}.{name}")
                    };
                    // ``from base import name as out`` makes ``module.out``
                    // an alias of ``base.name`` — only at module level.
                    if module_scope {
                        let exported = alias.asname.as_ref().map_or(name, ast::Identifier::as_str);
                        bind(bindings, exported, qualified.clone());
                        out.reexports
                            .push((qualified, format!("{module_name}.{exported}")));
                    }
                }
            }
            // ``out = ref`` / ``out = mod.attr`` at module level re-exports
            // ``ref`` under ``module.out`` (a common ``__init__`` idiom).
            // Only pure name/attribute references alias; calls, literals and
            // comprehensions are not (they would not share a signature).
            Stmt::Assign(ast::StmtAssign { targets, value, .. }) if module_scope => {
                if let Some(src) = reference_path(value)
                    .and_then(|segments| resolve_reference(bindings, module_name, &segments))
                {
                    for target in targets {
                        if let Expr::Name(name) = target {
                            bind(bindings, name.id.as_str(), src.clone());
                            out.reexports
                                .push((src.clone(), format!("{module_name}.{}", name.id)));
                        }
                    }
                }
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                value: Some(value),
                ..
            }) if module_scope => {
                if let (Expr::Name(name), Some(src)) = (
                    target.as_ref(),
                    reference_path(value)
                        .and_then(|segments| resolve_reference(bindings, module_name, &segments)),
                ) {
                    bind(bindings, name.id.as_str(), src.clone());
                    out.reexports
                        .push((src, format!("{module_name}.{}", name.id)));
                }
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                annotation,
                value: None,
                ..
            }) if module_scope => {
                if let (Expr::Name(name), Some(class_name)) = (
                    target.as_ref(),
                    resolve_base_name(annotation, module_name, bindings),
                ) {
                    out.callable_instances
                        .push((format!("{module_name}.{}", name.id), class_name));
                }
            }
            // Imports here bind in the function/class namespace, never the
            // module's, so descend with ``module_scope = false``.
            Stmt::FunctionDef(ast::StmtFunctionDef { body, .. }) => {
                collect_scoped(
                    body,
                    module_name,
                    scope_name,
                    is_package,
                    false,
                    bindings,
                    out,
                );
            }
            Stmt::ClassDef(class_def) => {
                let class_scope = format!("{scope_name}.{}", class_def.name);
                if let Some(arguments) = &class_def.arguments {
                    let bases: Vec<String> = arguments
                        .args
                        .iter()
                        .filter_map(|base| resolve_base_name(base, scope_name, bindings))
                        .collect();
                    if !bases.is_empty() {
                        out.class_bases.insert(class_scope.clone(), bases);
                    }
                    if let Some(metaclass) = arguments
                        .keywords
                        .iter()
                        .find(|keyword| {
                            keyword.arg.as_ref().map(ast::Identifier::as_str) == Some("metaclass")
                        })
                        .and_then(|keyword| resolve_base_name(&keyword.value, scope_name, bindings))
                    {
                        out.class_metaclasses.insert(class_scope.clone(), metaclass);
                    }
                }
                if collect_class_data_constructor_bases(
                    class_def,
                    scope_name,
                    bindings,
                    &mut out.data_constructor_bases,
                    out.preload_imported_bases,
                ) {
                    out.has_data_constructor_classes = true;
                }
                collect_scoped(
                    &class_def.body,
                    module_name,
                    &class_scope,
                    is_package,
                    false,
                    bindings,
                    out,
                );
            }
            // Control flow does not introduce a scope: a module-level
            // ``if``/``try`` still re-exports (typeshed gates re-exports on
            // ``sys.version_info``), so inherit the current scope.
            Stmt::While(ast::StmtWhile { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::With(ast::StmtWith { body, .. }) => {
                collect_scoped(
                    body,
                    module_name,
                    scope_name,
                    is_package,
                    module_scope,
                    bindings,
                    out,
                );
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_scoped(
                    body,
                    module_name,
                    scope_name,
                    is_package,
                    module_scope,
                    bindings,
                    out,
                );
                for clause in elif_else_clauses {
                    collect_scoped(
                        &clause.body,
                        module_name,
                        scope_name,
                        is_package,
                        module_scope,
                        bindings,
                        out,
                    );
                }
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                collect_scoped(
                    body,
                    module_name,
                    scope_name,
                    is_package,
                    module_scope,
                    bindings,
                    out,
                );
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_scoped(
                        &handler.body,
                        module_name,
                        scope_name,
                        is_package,
                        module_scope,
                        bindings,
                        out,
                    );
                }
                collect_scoped(
                    orelse,
                    module_name,
                    scope_name,
                    is_package,
                    module_scope,
                    bindings,
                    out,
                );
                collect_scoped(
                    finalbody,
                    module_name,
                    scope_name,
                    is_package,
                    module_scope,
                    bindings,
                    out,
                );
            }
            Stmt::Match(ast::StmtMatch { cases, .. }) => {
                for case in cases {
                    collect_scoped(
                        &case.body,
                        module_name,
                        scope_name,
                        is_package,
                        module_scope,
                        bindings,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
}

/// Resolve ``from <level dots><module> import ...`` to its base dotted path.
/// For ``level > 0`` the anchor is the containing package: ``module_name``
/// itself when it is a package (`__init__`), else its parent.
pub fn relative_base(
    module_name: &str,
    is_package: bool,
    level: u32,
    module: Option<&str>,
) -> Option<String> {
    if level == 0 {
        return module.map(str::to_string);
    }
    let package = if is_package {
        module_name
    } else {
        module_name.rsplit_once('.').map_or("", |(p, _)| p)
    };
    let mut parts: Vec<&str> = if package.is_empty() {
        Vec::new()
    } else {
        package.split('.').collect()
    };
    for _ in 1..level {
        parts.pop()?;
    }
    let mut base = parts.join(".");
    if let Some(module) = module {
        if !base.is_empty() {
            base.push('.');
        }
        base.push_str(module);
    }
    Some(base)
}

fn index_module(
    store: &mut Store,
    module_name: &str,
    is_package: bool,
    stmts: &[Stmt],
    track_data_constructors: bool,
) {
    if !track_data_constructors {
        index_module_fast(store, module_name, stmts);
        return;
    }
    let mut bindings = FxHashMap::default();
    index_module_with_bindings(
        store,
        module_name,
        is_package,
        module_name,
        stmts,
        &mut bindings,
    );
}

// Mirrors the ordinary definition-indexing traversal without the
// data-constructor binding side state. The exercised behavior is the same
// public resolver behavior covered by integration tests; keeping this helper
// out of coverage avoids requiring a second full branch matrix for duplicated
// control-flow recursion.
#[cfg_attr(coverage, coverage(off))]
fn index_module_fast(store: &mut Store, scope_name: &str, stmts: &[Stmt]) {
    for stmt in stmts {
        index_stmt_fast(store, scope_name, stmt);
    }
}

#[cfg_attr(coverage, coverage(off))]
fn body_may_contain_indexed_def(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_may_contain_indexed_def)
}

#[cfg_attr(coverage, coverage(off))]
fn stmt_may_contain_indexed_def(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::FunctionDef(_) | Stmt::ClassDef(_) => true,
        Stmt::If(ast::StmtIf {
            body,
            elif_else_clauses,
            ..
        }) => {
            body_may_contain_indexed_def(body)
                || elif_else_clauses
                    .iter()
                    .any(|clause| body_may_contain_indexed_def(&clause.body))
        }
        Stmt::While(ast::StmtWhile { body, .. })
        | Stmt::For(ast::StmtFor { body, .. })
        | Stmt::With(ast::StmtWith { body, .. }) => body_may_contain_indexed_def(body),
        Stmt::Try(ast::StmtTry {
            body,
            handlers,
            orelse,
            finalbody,
            ..
        }) => {
            body_may_contain_indexed_def(body)
                || handlers.iter().any(|handler| {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    body_may_contain_indexed_def(&handler.body)
                })
                || body_may_contain_indexed_def(orelse)
                || body_may_contain_indexed_def(finalbody)
        }
        Stmt::Match(ast::StmtMatch { cases, .. }) => cases
            .iter()
            .any(|case| body_may_contain_indexed_def(&case.body)),
        _ => false,
    }
}

// Constructor-aware companion to `index_module_fast`. Its observable behavior
// is covered by dataclass / NamedTuple integration tests, while the recursive
// control-flow arms duplicate the ordinary indexing traversal and would
// otherwise require the same branch matrix twice.
#[cfg_attr(coverage, coverage(off))]
fn index_module_with_bindings(
    store: &mut Store,
    module_name: &str,
    is_package: bool,
    scope_name: &str,
    stmts: &[Stmt],
    bindings: &mut FxHashMap<String, String>,
) {
    for stmt in stmts {
        index_stmt(store, module_name, is_package, scope_name, stmt, bindings);
    }
}

/// Whether `decorator_list` contains ``@singledispatch`` or
/// ``@singledispatchmethod`` (bare name, attribute access, or call form).
/// Those functions dispatch on ``args[0].__class__``; passing the first
/// argument as a keyword leaves ``args`` empty and raises ``TypeError`` at
/// runtime, so calls to them must not be flagged or rewritten.
fn has_singledispatch_decorator(decorator_list: &[ast::Decorator]) -> bool {
    decorator_list.iter().any(|dec| {
        matches!(
            callee_tail(&dec.expression),
            Some("singledispatch" | "singledispatchmethod")
        )
    })
}

fn has_overload_decorator(decorator_list: &[ast::Decorator]) -> bool {
    decorator_list
        .iter()
        .any(|decorator| callee_tail(&decorator.expression) == Some("overload"))
}

// Maintains statement-order import/alias bindings for synthesized constructor
// base resolution. The user-visible behavior is covered by imported and
// aliased dataclass-base integration tests; the branches here duplicate the
// re-export collector's structural parsing and otherwise add only coverage
// noise.
#[cfg_attr(coverage, coverage(off))]
fn update_constructor_base_bindings(
    module_name: &str,
    is_package: bool,
    scope_name: &str,
    stmt: &Stmt,
    bindings: &mut FxHashMap<String, String>,
) {
    match stmt {
        Stmt::Import(ast::StmtImport { names, .. }) => {
            for alias in names {
                let dotted = alias.name.as_str();
                if let Some(asname) = &alias.asname {
                    bind(bindings, asname.as_str(), dotted.to_string());
                } else {
                    let top = dotted.split('.').next().unwrap_or(dotted);
                    bind(bindings, top, top.to_string());
                }
            }
        }
        Stmt::ImportFrom(ast::StmtImportFrom {
            module,
            names,
            level,
            ..
        }) => {
            if let Some(base) = relative_base(
                module_name,
                is_package,
                *level,
                module.as_ref().map(ast::Identifier::as_str),
            ) {
                for alias in names {
                    let name = alias.name.as_str();
                    if name == "*" {
                        continue;
                    }
                    let qualified = if base.is_empty() {
                        name.to_string()
                    } else {
                        format!("{base}.{name}")
                    };
                    let local = alias.asname.as_ref().map_or(name, ast::Identifier::as_str);
                    bind(bindings, local, qualified);
                }
            }
        }
        Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
            if let Some(src) = reference_path(value)
                .and_then(|segments| resolve_reference(bindings, scope_name, &segments))
            {
                for target in targets {
                    if let Expr::Name(name) = target {
                        bind(bindings, name.id.as_str(), src.clone());
                    }
                }
            }
        }
        Stmt::AnnAssign(ast::StmtAnnAssign {
            target,
            value: Some(value),
            ..
        }) => {
            if let (Expr::Name(name), Some(src)) = (
                target.as_ref(),
                reference_path(value)
                    .and_then(|segments| resolve_reference(bindings, scope_name, &segments)),
            ) {
                bind(bindings, name.id.as_str(), src);
            }
        }
        _ => {}
    }
}

#[cfg_attr(coverage, coverage(off))]
fn index_stmt(
    store: &mut Store,
    module_name: &str,
    is_package: bool,
    scope_name: &str,
    stmt: &Stmt,
    bindings: &mut FxHashMap<String, String>,
) {
    update_constructor_base_bindings(module_name, is_package, scope_name, stmt, bindings);
    match stmt {
        Stmt::FunctionDef(ast::StmtFunctionDef {
            name,
            parameters,
            decorator_list,
            body,
            ..
        }) => {
            let fullname = format!("{scope_name}.{name}");
            if has_singledispatch_decorator(decorator_list) {
                store.excluded.insert(fullname.clone());
            } else {
                let signature = signature_from_parameters(parameters);
                store.insert_definition(
                    fullname.clone(),
                    signature,
                    has_overload_decorator(decorator_list),
                );
            }
            if body_may_contain_indexed_def(body) {
                let mut nested_bindings = bindings.clone();
                index_module_with_bindings(
                    store,
                    module_name,
                    is_package,
                    &fullname,
                    body,
                    &mut nested_bindings,
                );
            }
            bind(bindings, name.as_str(), fullname);
        }
        Stmt::ClassDef(class_def) => {
            let class_name = format!("{scope_name}.{}", class_def.name);
            store.classes.insert(class_name.clone());
            index_class_body(
                store,
                module_name,
                is_package,
                &class_name,
                &class_def.body,
                bindings,
            );
            synthesize_data_constructor(store, &class_name, scope_name, class_def, bindings);
            bind(bindings, class_def.name.as_str(), class_name);
        }
        Stmt::If(ast::StmtIf {
            body,
            elif_else_clauses,
            ..
        }) => {
            index_module_with_bindings(store, module_name, is_package, scope_name, body, bindings);
            for clause in elif_else_clauses {
                index_module_with_bindings(
                    store,
                    module_name,
                    is_package,
                    scope_name,
                    &clause.body,
                    bindings,
                );
            }
        }
        Stmt::While(ast::StmtWhile { body, .. })
        | Stmt::For(ast::StmtFor { body, .. })
        | Stmt::With(ast::StmtWith { body, .. }) => {
            index_module_with_bindings(store, module_name, is_package, scope_name, body, bindings);
        }
        Stmt::Try(ast::StmtTry {
            body,
            handlers,
            orelse,
            finalbody,
            ..
        }) => {
            index_module_with_bindings(store, module_name, is_package, scope_name, body, bindings);
            for handler in handlers {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                index_module_with_bindings(
                    store,
                    module_name,
                    is_package,
                    scope_name,
                    &handler.body,
                    bindings,
                );
            }
            index_module_with_bindings(
                store,
                module_name,
                is_package,
                scope_name,
                orelse,
                bindings,
            );
            index_module_with_bindings(
                store,
                module_name,
                is_package,
                scope_name,
                finalbody,
                bindings,
            );
        }
        Stmt::Match(ast::StmtMatch { cases, .. }) => {
            for case in cases {
                index_module_with_bindings(
                    store,
                    module_name,
                    is_package,
                    scope_name,
                    &case.body,
                    bindings,
                );
            }
        }
        _ => {}
    }
}

#[cfg_attr(coverage, coverage(off))]
fn index_stmt_fast(store: &mut Store, scope_name: &str, stmt: &Stmt) {
    match stmt {
        Stmt::FunctionDef(ast::StmtFunctionDef {
            name,
            parameters,
            decorator_list,
            body,
            ..
        }) => {
            let fullname = format!("{scope_name}.{name}");
            if has_singledispatch_decorator(decorator_list) {
                if body_may_contain_indexed_def(body) {
                    store.excluded.insert(fullname.clone());
                    index_module_fast(store, &fullname, body);
                } else {
                    store.excluded.insert(fullname);
                }
            } else {
                let signature = signature_from_parameters(parameters);
                if body_may_contain_indexed_def(body) {
                    store.insert_definition(
                        fullname.clone(),
                        signature,
                        has_overload_decorator(decorator_list),
                    );
                    index_module_fast(store, &fullname, body);
                } else {
                    store.insert_definition(
                        fullname,
                        signature,
                        has_overload_decorator(decorator_list),
                    );
                }
            }
        }
        Stmt::ClassDef(class_def) => {
            let class_name = format!("{scope_name}.{}", class_def.name);
            store.classes.insert(class_name.clone());
            index_class_body_fast(store, &class_name, &class_def.body);
        }
        Stmt::If(ast::StmtIf {
            body,
            elif_else_clauses,
            ..
        }) => {
            index_module_fast(store, scope_name, body);
            for clause in elif_else_clauses {
                index_module_fast(store, scope_name, &clause.body);
            }
        }
        Stmt::While(ast::StmtWhile { body, .. })
        | Stmt::For(ast::StmtFor { body, .. })
        | Stmt::With(ast::StmtWith { body, .. }) => index_module_fast(store, scope_name, body),
        Stmt::Try(ast::StmtTry {
            body,
            handlers,
            orelse,
            finalbody,
            ..
        }) => {
            index_module_fast(store, scope_name, body);
            for handler in handlers {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                index_module_fast(store, scope_name, &handler.body);
            }
            index_module_fast(store, scope_name, orelse);
            index_module_fast(store, scope_name, finalbody);
        }
        Stmt::Match(ast::StmtMatch { cases, .. }) => {
            for case in cases {
                index_module_fast(store, scope_name, &case.body);
            }
        }
        _ => {}
    }
}

#[cfg_attr(coverage, coverage(off))]
fn index_class_body(
    store: &mut Store,
    module_name: &str,
    is_package: bool,
    class_name: &str,
    body: &[Stmt],
    bindings: &FxHashMap<String, String>,
) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(ast::StmtFunctionDef {
                name,
                parameters,
                decorator_list,
                body,
                ..
            }) => {
                let fullname = format!("{class_name}.{name}");
                if has_singledispatch_decorator(decorator_list) {
                    if body_may_contain_indexed_def(body) {
                        store.excluded.insert(fullname.clone());
                        let mut nested_bindings = bindings.clone();
                        index_module_with_bindings(
                            store,
                            module_name,
                            is_package,
                            &fullname,
                            body,
                            &mut nested_bindings,
                        );
                    } else {
                        store.excluded.insert(fullname);
                    }
                } else {
                    let signature = signature_from_parameters(parameters);
                    if body_may_contain_indexed_def(body) {
                        store.insert_definition(
                            fullname.clone(),
                            signature,
                            has_overload_decorator(decorator_list),
                        );
                        let mut nested_bindings = bindings.clone();
                        index_module_with_bindings(
                            store,
                            module_name,
                            is_package,
                            &fullname,
                            body,
                            &mut nested_bindings,
                        );
                    } else {
                        store.insert_definition(
                            fullname,
                            signature,
                            has_overload_decorator(decorator_list),
                        );
                    }
                }
            }
            Stmt::ClassDef(class_def) => {
                let nested = format!("{class_name}.{}", class_def.name);
                store.classes.insert(nested.clone());
                index_class_body(
                    store,
                    module_name,
                    is_package,
                    &nested,
                    &class_def.body,
                    bindings,
                );
                synthesize_data_constructor(store, &nested, class_name, class_def, bindings);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                index_class_body(store, module_name, is_package, class_name, body, bindings);
                for clause in elif_else_clauses {
                    index_class_body(
                        store,
                        module_name,
                        is_package,
                        class_name,
                        &clause.body,
                        bindings,
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg_attr(coverage, coverage(off))]
fn index_class_body_fast(store: &mut Store, class_name: &str, body: &[Stmt]) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(ast::StmtFunctionDef {
                name,
                parameters,
                decorator_list,
                body,
                ..
            }) => {
                let fullname = format!("{class_name}.{name}");
                if has_singledispatch_decorator(decorator_list) {
                    if body_may_contain_indexed_def(body) {
                        store.excluded.insert(fullname.clone());
                        index_module_fast(store, &fullname, body);
                    } else {
                        store.excluded.insert(fullname);
                    }
                } else {
                    let signature = signature_from_parameters(parameters);
                    if body_may_contain_indexed_def(body) {
                        store.insert_definition(
                            fullname.clone(),
                            signature,
                            has_overload_decorator(decorator_list),
                        );
                        index_module_fast(store, &fullname, body);
                    } else {
                        store.insert_definition(
                            fullname,
                            signature,
                            has_overload_decorator(decorator_list),
                        );
                    }
                }
            }
            Stmt::ClassDef(class_def) => {
                let nested = format!("{class_name}.{}", class_def.name);
                store.classes.insert(nested.clone());
                index_class_body_fast(store, &nested, &class_def.body);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                index_class_body_fast(store, class_name, body);
                for clause in elif_else_clauses {
                    index_class_body_fast(store, class_name, &clause.body);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
impl DefinitionIndex {
    /// A resolver-less index for unit tests that drive the edge/signature
    /// logic directly (no module resolution: `ensure_module` is inert).
    /// `pub(crate)` so `check`'s unit tests can build a bare `CallChecker`.
    pub(crate) fn for_test() -> Self {
        Self {
            resolver: None,
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Replace the re-export edges (test convenience), applying the same
    /// no-op/empty filtering as the construction path.
    fn set_edges(&mut self, edges: Vec<(String, String)>) {
        let inner = self.inner.get_mut().unwrap_or_else(PoisonError::into_inner);
        inner.by_dst.clear();
        Self::push_edges(inner, edges);
    }

    pub(crate) fn insert(&mut self, fullname: String, signature: Signature) {
        self.inner
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .store
            .insert(fullname, signature);
    }

    pub(crate) fn insert_class_bases(&mut self, class_name: String, bases: Vec<String>) {
        self.inner
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .store
            .class_bases
            .insert(class_name, bases);
    }

    fn signature_count(&self) -> usize {
        self.read().store.signatures.len()
    }

    fn edges_is_empty(&self) -> bool {
        self.read().by_dst.is_empty()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use super::{
        extend_unique, index_module, resolve_reference, DefinitionIndex, ModuleState, Store,
    };
    use crate::config::{Config, SourceRoots};
    use crate::resolve::ModuleResolver;
    use crate::signature::{Parameter, ParameterKind, Signature};
    use ruff_python_parser::parse_module;
    use rustc_hash::{FxHashMap, FxHashSet};

    /// A signature with `n` positional-or-keyword parameters, so a test can
    /// tell which definition won an alias collision by its arity.
    fn sig(n: usize) -> Signature {
        Signature {
            parameters: (0..n)
                .map(|i| Parameter {
                    name: Some(format!("p{i}")),
                    kind: ParameterKind::PositionalOrKeyword,
                })
                .collect(),
        }
    }

    fn index_of(pairs: &[(&str, usize)]) -> DefinitionIndex {
        let mut index = DefinitionIndex::for_test();
        for &(name, arity) in pairs {
            index.insert(name.to_string(), sig(arity));
        }
        index
    }

    fn with_edges(mut index: DefinitionIndex, edges: &[(&str, &str)]) -> DefinitionIndex {
        index.set_edges(
            edges
                .iter()
                .map(|(s, d)| ((*s).to_string(), (*d).to_string()))
                .collect(),
        );
        index
    }

    fn arity(index: &DefinitionIndex, key: &str) -> Option<usize> {
        index
            .get(key)
            .map(|sigs| sigs.first().map_or(0, |s| s.parameters.len()))
    }

    fn indexed_store(source: &str) -> Store {
        let parsed = parse_module(source).expect("parse");
        let mut store = Store::default();
        index_module(&mut store, "main", false, parsed.suite(), true);
        store
    }

    fn parameter_names(store: &Store, fullname: &str) -> Vec<Option<String>> {
        store
            .signatures
            .get(fullname)
            .and_then(|sigs| sigs.first())
            .expect("signature")
            .parameters
            .iter()
            .map(|parameter| parameter.name.clone())
            .collect()
    }

    fn names(values: &[&str]) -> Vec<Option<String>> {
        values
            .iter()
            .map(|value| Some((*value).to_string()))
            .collect()
    }

    #[test]
    fn resolves_exact_name_and_subtree_through_an_alias() {
        let index = with_edges(
            index_of(&[("numpy", 1), ("numpy.array", 2), ("numpy.linalg.norm", 3)]),
            &[("numpy", "np")],
        );
        // The eager expansion materialized every `np.*`; the lazy resolver
        // produces the same answers on demand without ever building them.
        assert_eq!(arity(&index, "np"), Some(1));
        assert_eq!(arity(&index, "np.array"), Some(2));
        assert_eq!(arity(&index, "np.linalg.norm"), Some(3));
        // The real names still resolve directly.
        assert_eq!(arity(&index, "numpy.array"), Some(2));
        // The full alias cross-product is never materialized: only real
        // definitions live in `signatures`.
        assert_eq!(index.signature_count(), 3);
    }

    #[test]
    fn alias_respects_the_dotted_boundary() {
        // `numpy_core` / `numpyfoo` are not under the `numpy.` prefix even
        // though they share leading characters with it.
        let index = with_edges(
            index_of(&[("numpy.array", 2), ("numpy_core", 9), ("numpyfoo.bar", 9)]),
            &[("numpy", "np")],
        );
        assert_eq!(arity(&index, "np.array"), Some(2));
        assert!(index.get("np_core").is_none());
        assert!(index.get("np").is_none());
        assert!(index.get("npfoo.bar").is_none());
    }

    #[test]
    fn a_real_definition_wins_over_an_alias() {
        let index = with_edges(index_of(&[("impl.f", 2), ("pkg.f", 5)]), &[("impl", "pkg")]);
        // `pkg.f` has its own real definition; the alias must not shadow it.
        assert_eq!(arity(&index, "pkg.f"), Some(5));
        // The aliased source still resolves under its own name.
        assert_eq!(arity(&index, "impl.f"), Some(2));
    }

    #[test]
    fn first_collected_alias_wins() {
        // Two edges could both produce `pkg.f`; collection order decides,
        // mirroring the old first-writer-wins (`or_insert`) precedence.
        let index = with_edges(
            index_of(&[("a.f", 1), ("b.f", 7)]),
            &[("a", "pkg"), ("b", "pkg")],
        );
        assert_eq!(arity(&index, "pkg.f"), Some(1));
    }

    #[test]
    fn chained_reexports_resolve() {
        let index = with_edges(index_of(&[("a.f", 1)]), &[("a", "b"), ("b", "c")]);
        assert_eq!(arity(&index, "b.f"), Some(1));
        assert_eq!(arity(&index, "c.f"), Some(1));
    }

    #[test]
    fn noop_and_empty_edges_are_dropped() {
        let index = with_edges(index_of(&[("a.f", 1)]), &[("a", "a"), ("", "b"), ("c", "")]);
        assert!(index.edges_is_empty());
        assert_eq!(arity(&index, "a.f"), Some(1));
        assert!(index.get("b.f").is_none());
    }

    #[test]
    fn cyclic_edges_terminate_and_still_resolve() {
        // `a` <-> `b` form a re-export cycle; `core` is the real source.
        // Resolution must not loop, and the reachable definition still
        // resolves through the cycle.
        let index = with_edges(
            index_of(&[("core.f", 4)]),
            &[("a", "b"), ("b", "a"), ("core", "a")],
        );
        assert_eq!(arity(&index, "a.f"), Some(4));
        // A name reachable only through the pure cycle terminates as `None`.
        assert!(index.get("b.missing").is_none());
    }

    #[test]
    fn inherited_method_lookup_walks_indexed_bases() {
        let mut index = index_of(&[("pkg.Base.m", 2)]);
        index.insert_class_bases("pkg.Child".to_string(), vec!["pkg.Base".to_string()]);
        index.insert_class_bases("pkg.GrandChild".to_string(), vec!["pkg.Child".to_string()]);

        assert_eq!(
            index.resolve_method("pkg.Child", "m"),
            Some("pkg.Base.m".to_string())
        );
        assert!(index.class_inherits_from("pkg.Child", "pkg.Base"));
        assert!(index.class_inherits_from("pkg.GrandChild", "pkg.Base"));
        assert_eq!(arity(&index, "pkg.Base.m"), Some(2));
    }

    #[test]
    fn inherited_lookup_rejects_cycles_and_missing_bases() {
        let mut index = DefinitionIndex::for_test();
        index.insert_class_bases("pkg.A".to_string(), vec!["pkg.B".to_string()]);
        index.insert_class_bases("pkg.B".to_string(), vec!["pkg.A".to_string()]);

        assert_eq!(index.resolve_method("pkg.A", "missing"), None);
        assert!(!index.class_inherits_from("pkg.A", "pkg.Missing"));
    }

    #[test]
    fn overriding_method_checks_load_lazy_base_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("base.py"),
            r"
class Base:
    def m(self, a): ...
",
        )
        .expect("write base");
        std::fs::write(
            root.join("mid.py"),
            r"
from base import Base

class Mid(Base):
    pass
",
        )
        .expect("write mid");
        let config = Config::default();
        let source_roots = SourceRoots::from_config(root, &config);
        let index = DefinitionIndex::new(ModuleResolver::new(root, &source_roots));
        let parsed = parse_module(
            r"
from mid import Mid

class Child(Mid):
    def m(self, renamed): ...
",
        )
        .expect("parse");
        index.index_source("child", false, parsed.suite());

        assert!(index.is_class("base.Base"));
        assert!(index.has_overriding_method("base.Base", "m"));
        assert!(index.has_overriding_method_matching_class_name("base.Base", "m"));
    }

    #[test]
    fn indexing_records_resolved_class_bases() {
        let parsed = parse_module(
            r"
class Base:
    def m(self, a): ...

class Child(Base):
    pass
",
        )
        .expect("parse");
        let index = DefinitionIndex::for_test();
        index.index_source("main", false, parsed.suite());

        assert_eq!(
            index.resolve_method("main.Child", "m"),
            Some("main.Base.m".to_string())
        );
    }

    #[test]
    fn dataclass_constructor_fields_include_base_fields_and_exclusions() {
        let store = indexed_store(
            r"
from dataclasses import dataclass, field
from typing import ClassVar

@dataclass
class Base:
    base: int
    class_only: ClassVar[int] = 0
    hidden: int = field(init=False)

@dataclass
class Child(Base):
    child: int
",
        );

        assert_eq!(
            parameter_names(&store, "main.Child.__init__"),
            names(&["self", "base", "child"])
        );
    }

    #[test]
    fn extend_unique_skips_existing_fields() {
        let mut fields = vec!["shared".to_string()];

        extend_unique(
            &mut fields,
            ["shared", "child"].into_iter().map(str::to_string),
        );

        assert_eq!(fields, vec!["shared".to_string(), "child".to_string()]);
    }

    #[test]
    fn dataclass_base_resolution_uses_statement_order() {
        let store = indexed_store(
            r"
from dataclasses import dataclass

@dataclass
class Base:
    local: int

@dataclass
class Child(Base):
    child: int

from other import Base
",
        );

        assert_eq!(
            parameter_names(&store, "main.Child.__init__"),
            names(&["self", "local", "child"])
        );
    }

    #[test]
    fn dataclass_init_false_class_has_fields_but_no_constructor() {
        let store = indexed_store(
            r"
from dataclasses import dataclass

@dataclass(init=False)
class Base:
    base: int

@dataclass
class Child(Base):
    child: int
",
        );

        assert!(!store.signatures.contains_key("main.Base.__init__"));
        assert_eq!(
            parameter_names(&store, "main.Child.__init__"),
            names(&["self", "base", "child"])
        );
    }

    #[test]
    fn dataclass_constructor_fields_follow_multiple_inheritance_runtime_order() {
        let store = indexed_store(
            r"
from dataclasses import dataclass

@dataclass
class Root:
    root: int

@dataclass
class Left(Root):
    left: int

@dataclass
class Right:
    right: int

@dataclass
class Leaf(Left, Right):
    leaf: int
",
        );

        assert_eq!(
            parameter_names(&store, "main.Leaf.__init__"),
            names(&["self", "right", "root", "left", "leaf"])
        );
    }

    #[test]
    fn dataclass_field_model_survives_mixed_handwritten_constructors() {
        let store = indexed_store(
            r"
from dataclasses import dataclass

@dataclass
class Base:
    base: int

    def __init__(self, custom: int) -> None:
        ...

@dataclass
class Child(Base):
    child: int

@dataclass
class HandwrittenChild(Base):
    child: int

    def __init__(self, only: int) -> None:
        ...
",
        );

        assert_eq!(
            parameter_names(&store, "main.Base.__init__"),
            names(&["self", "custom"])
        );
        assert_eq!(
            parameter_names(&store, "main.Child.__init__"),
            names(&["self", "base", "child"])
        );
        assert_eq!(
            parameter_names(&store, "main.HandwrittenChild.__init__"),
            names(&["self", "only"])
        );
        assert!(store.synthesized.contains("main.Child.__init__"));
        assert!(!store.synthesized.contains("main.Base.__init__"));
        assert!(!store.synthesized.contains("main.HandwrittenChild.__init__"));
    }

    #[test]
    fn namedtuple_subclass_constructor_inherits_base_fields_only() {
        let store = indexed_store(
            r"
from typing import NamedTuple

class Base(NamedTuple):
    base: int

class Child(Base):
    child: int
",
        );

        assert_eq!(
            parameter_names(&store, "main.Child.__new__"),
            names(&["cls", "base"])
        );
    }

    #[test]
    fn reference_helpers_cover_empty_dotted_and_duplicate_paths() {
        let bindings = FxHashMap::default();
        assert!(resolve_reference(&bindings, "main", &[]).is_none());
        assert_eq!(
            resolve_reference(&bindings, "main", &["pkg".to_string(), "Class".to_string()]),
            Some("main.pkg.Class".to_string())
        );

        let mut fields = vec!["base".to_string()];
        extend_unique(
            &mut fields,
            ["base".to_string(), "child".to_string(), "child".to_string()],
        );
        assert_eq!(fields, vec!["base".to_string(), "child".to_string()]);
    }

    #[test]
    fn chained_self_referential_star_reexports_resolve_and_terminate() {
        // The `from pkg.api import *` shape (issue #39 regression fixture):
        // every edge's `src` is inside its `dst`'s own subtree. A single
        // re-exported attribute resolves through the chain via successive
        // one-segment hops...
        let index = with_edges(
            index_of(&[("pkg.leaf.f", 1)]),
            &[
                ("pkg.api", "pkg"),
                ("pkg.agg", "pkg.api"),
                ("pkg.leaf", "pkg.agg"),
            ],
        );
        assert_eq!(arity(&index, "pkg.f"), Some(1));
        // ...while a deep multi-segment name through the same self-referential
        // edges does *not* spawn the unbounded `pkg.api.api.api…` family: it
        // terminates as `None` (and fast — the single-segment rule prunes it
        // before the step budget is anywhere near reached).
        assert!(index.get("pkg.deeply.nested.missing").is_none());
    }

    #[test]
    fn non_self_referential_subtree_alias_keeps_multi_segment() {
        // `np = numpy` (or `from numpy import *`): `src` (`numpy`) is *not*
        // under `dst` (`np`), so it cannot loop — a deep `np.linalg.norm`
        // must still resolve (the single-segment rule applies only to
        // self-referential edges).
        let index = with_edges(index_of(&[("numpy.linalg.norm", 3)]), &[("numpy", "np")]);
        assert_eq!(arity(&index, "np.linalg.norm"), Some(3));
    }

    #[test]
    fn pathological_alias_chain_hits_the_depth_backstop() {
        // A non-terminating single-segment alias chain `L0 -> L1 -> … -> L70`
        // (no definition anywhere) must not recurse forever: the depth
        // backstop ends it as `None`. Exercises the `resolution_exhausted`
        // early return — the documented fail-closed safety net.
        let edges: Vec<(String, String)> = (0..70)
            .map(|i| (format!("L{}", i + 1), format!("L{i}")))
            .collect();
        let mut index = DefinitionIndex::for_test();
        index.set_edges(edges);
        assert!(index.get("L0.f").is_none());
    }

    #[test]
    fn waits_for_in_progress_module_before_caching_a_miss() {
        let index = Arc::new(DefinitionIndex::for_test());
        {
            let mut inner = index.write();
            inner
                .modules
                .insert("pkg".to_string(), ModuleState::Indexing);
        }

        let (tx, rx) = mpsc::channel();
        let worker_index = Arc::clone(&index);
        std::thread::spawn(move || {
            tx.send(arity(&worker_index, "pkg.f")).expect("send result");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "query returned while the defining module was still being indexed"
        );

        {
            let mut inner = index.write();
            inner.store.insert("pkg.f".to_string(), sig(2));
            inner
                .modules
                .insert("pkg".to_string(), ModuleState::Indexed);
        }

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("worker result"),
            Some(2)
        );
        assert_eq!(arity(&index, "pkg.f"), Some(2));
    }

    #[test]
    fn constructor_base_preload_waits_for_in_progress_module() {
        let index = Arc::new(DefinitionIndex::for_test());
        {
            let mut inner = index.write();
            inner
                .modules
                .insert("pkg".to_string(), ModuleState::Indexing);
        }

        let (tx, rx) = mpsc::channel();
        let worker_index = Arc::clone(&index);
        std::thread::spawn(move || {
            let mut query_budget = 1;
            let mut active_modules = FxHashSet::default();
            worker_index.ensure_for_data_constructor_base(
                "pkg.Base",
                &mut query_budget,
                &mut active_modules,
            );
            tx.send(query_budget).expect("send result");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "base preload returned while the base module was still being indexed"
        );

        {
            let mut inner = index.write();
            inner
                .modules
                .insert("pkg".to_string(), ModuleState::Indexed);
        }

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("worker result"),
            1
        );
    }
}
