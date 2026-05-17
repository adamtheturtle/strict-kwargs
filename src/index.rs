//! Index of callable definitions discovered in the project.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ruff_python_ast::Stmt;
use ruff_python_ast::{self as ast};
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
    for _ in 0..MAX_EXPAND_ITERS {
        let mut additions: Vec<(String, Vec<Signature>)> = Vec::new();
        for (src, dst) in edges {
            if src == dst || src.is_empty() || dst.is_empty() {
                continue;
            }
            let src_dot = format!("{src}.");
            for (key, sigs) in &index.signatures {
                let suffix = if key == src {
                    ""
                } else if let Some(rest) = key.strip_prefix(&src_dot) {
                    rest
                } else {
                    continue;
                };
                let new_key = if suffix.is_empty() {
                    dst.clone()
                } else {
                    format!("{dst}.{suffix}")
                };
                if !index.signatures.contains_key(&new_key) {
                    additions.push((new_key, sigs.clone()));
                }
            }
        }
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
    collect_scoped(stmts, module_name, is_package, true, out);
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
                        out.reexports
                            .push((qualified, format!("{module_name}.{exported}")));
                    }
                }
            }
            // Imports here bind in the function/class namespace, never the
            // module's, so descend with ``module_scope = false``.
            Stmt::FunctionDef(ast::StmtFunctionDef { body, .. })
            | Stmt::ClassDef(ast::StmtClassDef { body, .. }) => {
                collect_scoped(body, module_name, is_package, false, out);
            }
            // Control flow does not introduce a scope: a module-level
            // ``if``/``try`` still re-exports (typeshed gates re-exports on
            // ``sys.version_info``), so inherit the current scope.
            Stmt::While(ast::StmtWhile { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::With(ast::StmtWith { body, .. }) => {
                collect_scoped(body, module_name, is_package, module_scope, out);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_scoped(body, module_name, is_package, module_scope, out);
                for clause in elif_else_clauses {
                    collect_scoped(&clause.body, module_name, is_package, module_scope, out);
                }
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                collect_scoped(body, module_name, is_package, module_scope, out);
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_scoped(&handler.body, module_name, is_package, module_scope, out);
                }
                collect_scoped(orelse, module_name, is_package, module_scope, out);
                collect_scoped(finalbody, module_name, is_package, module_scope, out);
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
