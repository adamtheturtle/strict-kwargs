//! Index of callable definitions discovered in the project.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ruff_python_ast::{self as ast};
use ruff_python_ast::{Expr, Stmt};
use ruff_python_parser::parse_module;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast_util::signature_from_parameters;
use crate::error::CheckError;
use crate::resolve::ModuleResolver;
use crate::signature::Signature;

#[derive(Debug, Default)]
pub struct DefinitionIndex {
    /// Fully-qualified name (e.g. ``main.C.method``) -> one or more
    /// signatures. Multiple entries occur for ``@overload``-ed definitions
    /// (common in ``.pyi`` stubs) and plain redefinitions.
    pub signatures: FxHashMap<String, Vec<Signature>>,
}

impl DefinitionIndex {
    pub fn insert(&mut self, fullname: String, signature: Signature) {
        self.signatures.entry(fullname).or_default().push(signature);
    }

    pub fn get(&self, fullname: &str) -> Option<&[Signature]> {
        self.signatures.get(fullname).map(Vec::as_slice)
    }
}

pub fn module_name_for_path(project_root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(project_root)
        .unwrap_or(path)
        .with_extension("");
    let mut parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // ``pkg/__init__.py`` is the module ``pkg``, not ``pkg.__init__``.
    if parts.last().map(String::as_str) == Some("__init__") {
        parts.pop();
    }
    parts.join(".")
}

/// Whether ``path`` is a package initializer (``__init__.py``/``.pyi``).
pub fn is_package_init(path: &Path) -> bool {
    path.file_stem().is_some_and(|s| s == "__init__")
}

/// Safety cap on how many modules a single run will resolve & index, so a
/// pathological dependency graph cannot blow up time/memory.
const MODULE_BUDGET: usize = 4000;
/// Safety cap on re-export alias expansion passes (handles chained
/// re-exports; converges well before this for real code).
const MAX_EXPAND_ITERS: usize = 16;

/// Imports discovered in a module: submodules to resolve next, and re-export
/// edges ``(source_prefix, dest_prefix)`` for alias expansion.
#[derive(Default)]
struct Collected {
    modules: Vec<String>,
    reexports: Vec<(String, String)>,
}

pub fn build_index(
    project_root: &Path,
    python_files: &[PathBuf],
) -> Result<DefinitionIndex, CheckError> {
    let resolver = ModuleResolver::new(project_root);
    let mut index = DefinitionIndex::default();
    let mut indexed: FxHashSet<String> = FxHashSet::default();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut reexports: Vec<(String, String)> = Vec::new();

    // Builtins come from vendored typeshed ``stdlib/builtins.pyi``.
    if let Some(m) = resolver.resolve("builtins") {
        if let Ok(parsed) = parse_module(&m.source) {
            index_module(&mut index, "builtins", parsed.suite());
        }
    }
    indexed.insert("builtins".to_string());

    // First-party: the files being checked.
    for path in python_files {
        let source = std::fs::read_to_string(path)?;
        let parsed = parse_module(&source)?;
        let module_name = module_name_for_path(project_root, path);
        let mut found = Collected::default();
        collect(
            parsed.suite(),
            &module_name,
            is_package_init(path),
            &mut found,
        );
        index_module(&mut index, &module_name, parsed.suite());
        indexed.insert(module_name);
        enqueue(&mut queue, found.modules);
        reexports.extend(found.reexports);
    }

    // Resolve & index imported modules, recursively following re-exports,
    // mirroring ty's resolution order: first-party, stdlib, site-packages.
    let mut budget = MODULE_BUDGET;
    while let Some(dotted) = queue.pop_front() {
        if !indexed.insert(dotted.clone()) {
            continue;
        }
        if budget == 0 {
            continue;
        }
        budget -= 1;
        let Some(m) = resolver.resolve(&dotted) else {
            continue;
        };
        let Ok(parsed) = parse_module(&m.source) else {
            continue;
        };
        let mut found = Collected::default();
        collect(parsed.suite(), &dotted, m.is_package, &mut found);
        index_module(&mut index, &dotted, parsed.suite());
        enqueue(&mut queue, found.modules);
        reexports.extend(found.reexports);
    }

    expand_reexports(&mut index, &reexports);
    Ok(index)
}

fn enqueue(queue: &mut VecDeque<String>, modules: Vec<String>) {
    for m in modules {
        if !m.is_empty() {
            queue.push_back(m);
        }
    }
}

/// Copy signatures across re-export edges so ``pkg.name`` resolves when
/// ``pkg/__init__`` does ``from .impl import name`` (and ``import *``).
/// Real definitions always win; aliases never overwrite them. Iterated to a
/// fixpoint to follow chained re-exports.
fn expand_reexports(index: &mut DefinitionIndex, edges: &[(String, String)]) {
    // Drop no-op edges once so they cost nothing on every iteration.
    let edges: Vec<(&str, &str)> = edges
        .iter()
        .filter(|(src, dst)| src != dst && !src.is_empty() && !dst.is_empty())
        .map(|(src, dst)| (src.as_str(), dst.as_str()))
        .collect();
    if edges.is_empty() {
        return;
    }
    for _ in 0..MAX_EXPAND_ITERS {
        // The keys touched by an edge are exactly `src` and everything under
        // `src.`. A sorted snapshot lets each edge binary-search to its
        // contiguous `src.` block instead of scanning the whole (growing)
        // index, which is what made this super-quadratic on large
        // import closures (issue #31). The set is unchanged within an
        // iteration: additions are deferred, exactly as before.
        let mut keys: Vec<&str> = index.signatures.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut additions: Vec<(String, Vec<Signature>)> = Vec::new();
        for &(src, dst) in &edges {
            // `src` itself re-exported as `dst` (the old `key == src` case).
            if let Some(sigs) = index.signatures.get(src) {
                if !index.signatures.contains_key(dst) {
                    additions.push((dst.to_string(), sigs.clone()));
                }
            }
            // Everything under `src.` re-exported as `dst.<suffix>`. Keys
            // with this prefix are contiguous in the sorted snapshot.
            let src_dot = format!("{src}.");
            let start = keys.partition_point(|key| *key < src_dot.as_str());
            for &key in &keys[start..] {
                let Some(suffix) = key.strip_prefix(&src_dot) else {
                    break;
                };
                if suffix.is_empty() {
                    continue;
                }
                let new_key = format!("{dst}.{suffix}");
                if index.signatures.contains_key(&new_key) {
                    continue;
                }
                if let Some(sigs) = index.signatures.get(key) {
                    additions.push((new_key, sigs.clone()));
                }
            }
        }
        drop(keys);
        if additions.is_empty() {
            break;
        }
        for (key, sigs) in additions {
            index.signatures.entry(key).or_insert(sigs);
        }
    }
}

/// Walk ``stmts`` collecting submodules to resolve and re-export edges,
/// resolving relative imports against ``module_name``/``is_package``.
fn collect(stmts: &[Stmt], module_name: &str, is_package: bool, out: &mut Collected) {
    let mut bindings: FxHashMap<String, String> = FxHashMap::default();
    collect_scoped(stmts, module_name, is_package, true, &mut bindings, out);
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

/// `module_scope` is true only at true module level. Imports nested inside a
/// function or class body bind in that local/class namespace, *not* the
/// module's, so they must not create module-level re-export edges (which
/// would make ``module.name`` a false alias). Submodules are still queued for
/// resolution everywhere — indexing an extra module is harmless and lets
/// function-local calls be checked.
fn collect_scoped(
    stmts: &[Stmt],
    module_name: &str,
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
                    out.modules.push(dotted.to_string());
                    let parts: Vec<&str> = dotted.split('.').collect();
                    for end in 1..parts.len() {
                        out.modules.push(parts[..end].join("."));
                    }
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
                if !base.is_empty() {
                    out.modules.push(base.clone());
                }
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
                    // ``name`` may itself be a submodule.
                    out.modules.push(qualified.clone());
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
                    out.reexports
                        .push((src, format!("{module_name}.{}", name.id)));
                }
            }
            // Imports here bind in the function/class namespace, never the
            // module's, so descend with ``module_scope = false``.
            Stmt::FunctionDef(ast::StmtFunctionDef { body, .. })
            | Stmt::ClassDef(ast::StmtClassDef { body, .. }) => {
                collect_scoped(body, module_name, is_package, false, bindings, out);
            }
            // Control flow does not introduce a scope: a module-level
            // ``if``/``try`` still re-exports (typeshed gates re-exports on
            // ``sys.version_info``), so inherit the current scope.
            Stmt::While(ast::StmtWhile { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::With(ast::StmtWith { body, .. }) => {
                collect_scoped(body, module_name, is_package, module_scope, bindings, out);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_scoped(body, module_name, is_package, module_scope, bindings, out);
                for clause in elif_else_clauses {
                    collect_scoped(
                        &clause.body,
                        module_name,
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
                collect_scoped(body, module_name, is_package, module_scope, bindings, out);
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_scoped(
                        &handler.body,
                        module_name,
                        is_package,
                        module_scope,
                        bindings,
                        out,
                    );
                }
                collect_scoped(orelse, module_name, is_package, module_scope, bindings, out);
                collect_scoped(
                    finalbody,
                    module_name,
                    is_package,
                    module_scope,
                    bindings,
                    out,
                );
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

fn index_module(index: &mut DefinitionIndex, module_name: &str, stmts: &[Stmt]) {
    for stmt in stmts {
        index_stmt(index, module_name, stmt);
    }
}

fn index_stmt(index: &mut DefinitionIndex, module_name: &str, stmt: &Stmt) {
    match stmt {
        Stmt::FunctionDef(ast::StmtFunctionDef {
            name,
            parameters,
            body,
            ..
        }) => {
            let fullname = format!("{module_name}.{name}");
            index.insert(fullname, signature_from_parameters(parameters));
            index_module(index, module_name, body);
        }
        Stmt::ClassDef(ast::StmtClassDef { name, body, .. }) => {
            let class_name = format!("{module_name}.{name}");
            index_class_body(index, &class_name, body);
        }
        Stmt::If(ast::StmtIf {
            body,
            elif_else_clauses,
            ..
        }) => {
            index_module(index, module_name, body);
            for clause in elif_else_clauses {
                index_module(index, module_name, &clause.body);
            }
        }
        Stmt::While(ast::StmtWhile { body, .. })
        | Stmt::For(ast::StmtFor { body, .. })
        | Stmt::With(ast::StmtWith { body, .. }) => index_module(index, module_name, body),
        Stmt::Try(ast::StmtTry {
            body,
            handlers,
            orelse,
            finalbody,
            ..
        }) => {
            index_module(index, module_name, body);
            for handler in handlers {
                let ast::ExceptHandler::ExceptHandler(handler) = handler;
                index_module(index, module_name, &handler.body);
            }
            index_module(index, module_name, orelse);
            index_module(index, module_name, finalbody);
        }
        Stmt::Match(ast::StmtMatch { cases, .. }) => {
            for case in cases {
                index_module(index, module_name, &case.body);
            }
        }
        _ => {}
    }
}

fn index_class_body(index: &mut DefinitionIndex, class_name: &str, body: &[Stmt]) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(ast::StmtFunctionDef {
                name,
                parameters,
                body,
                ..
            }) => {
                let fullname = format!("{class_name}.{name}");
                index.insert(fullname, signature_from_parameters(parameters));
                index_module(index, class_name, body);
            }
            Stmt::ClassDef(ast::StmtClassDef {
                name: inner, body, ..
            }) => {
                index_class_body(index, &format!("{class_name}.{inner}"), body);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                index_class_body(index, class_name, body);
                for clause in elif_else_clauses {
                    index_class_body(index, class_name, &clause.body);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_reexports, DefinitionIndex};
    use crate::signature::{Parameter, ParameterKind, Signature};

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
        let mut index = DefinitionIndex::default();
        for &(name, arity) in pairs {
            index.insert(name.to_string(), sig(arity));
        }
        index
    }

    fn arity(index: &DefinitionIndex, key: &str) -> Option<usize> {
        index
            .get(key)
            .map(|sigs| sigs.first().map_or(0, |s| s.parameters.len()))
    }

    #[test]
    fn copies_exact_name_and_everything_under_the_prefix() {
        let mut index = index_of(&[("numpy", 1), ("numpy.array", 2), ("numpy.linalg.norm", 3)]);
        expand_reexports(&mut index, &[("numpy".into(), "np".into())]);
        assert_eq!(arity(&index, "np"), Some(1));
        assert_eq!(arity(&index, "np.array"), Some(2));
        assert_eq!(arity(&index, "np.linalg.norm"), Some(3));
    }

    #[test]
    fn prefix_match_respects_the_dotted_boundary() {
        // The sorted-range scan must not treat `numpy_core` / `numpyfoo` as
        // being under the `numpy.` prefix (they sort adjacent to `numpy.`).
        let mut index = index_of(&[
            ("numpy.array", 2),
            ("numpy_core", 9),
            ("numpyfoo.bar", 9),
            ("numpz.x", 9),
        ]);
        expand_reexports(&mut index, &[("numpy".into(), "np".into())]);
        assert_eq!(arity(&index, "np.array"), Some(2));
        assert!(index.get("np_core").is_none());
        assert!(index.get("np").is_none());
        assert!(index.get("npfoo.bar").is_none());
    }

    #[test]
    fn a_real_definition_is_never_overwritten_by_an_alias() {
        let mut index = index_of(&[("impl.f", 2), ("pkg.f", 5)]);
        expand_reexports(&mut index, &[("impl".into(), "pkg".into())]);
        // `pkg.f` already had a real definition; the alias must not clobber it.
        assert_eq!(arity(&index, "pkg.f"), Some(5));
    }

    #[test]
    fn chained_reexports_resolve_to_a_fixpoint() {
        let mut index = index_of(&[("a.f", 1)]);
        expand_reexports(
            &mut index,
            &[("a".into(), "b".into()), ("b".into(), "c".into())],
        );
        assert_eq!(arity(&index, "b.f"), Some(1));
        assert_eq!(arity(&index, "c.f"), Some(1));
    }

    #[test]
    fn noop_edges_are_ignored() {
        let mut index = index_of(&[("a.f", 1)]);
        expand_reexports(
            &mut index,
            &[
                ("a".into(), "a".into()),
                (String::new(), "b".into()),
                ("c".into(), String::new()),
            ],
        );
        assert_eq!(index.signatures.len(), 1);
    }
}
