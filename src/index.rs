//! Index of callable definitions discovered in the project.

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
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join(".")
}

pub fn build_index(
    project_root: &Path,
    python_files: &[PathBuf],
) -> Result<DefinitionIndex, CheckError> {
    let resolver = ModuleResolver::new(project_root);
    let mut index = DefinitionIndex::default();
    let mut indexed: FxHashSet<String> = FxHashSet::default();
    let mut imports: Vec<String> = Vec::new();

    // Builtins come from vendored typeshed ``stdlib/builtins.pyi``.
    if let Some(src) = resolver.resolve("builtins") {
        if let Ok(parsed) = parse_module(&src) {
            index_module(&mut index, "builtins", parsed.suite());
        }
    }
    indexed.insert("builtins".to_string());

    // First-party: the files being checked.
    for path in python_files {
        let source = std::fs::read_to_string(path)?;
        let parsed = parse_module(&source)?;
        let module_name = module_name_for_path(project_root, path);
        collect_imports(parsed.suite(), &module_name, &mut imports);
        index_module(&mut index, &module_name, parsed.suite());
        indexed.insert(module_name);
    }

    // Lazily resolve & index modules imported by the checked files (one level),
    // mirroring ty's resolution order: first-party, stdlib, site-packages.
    for dotted in imports {
        if !indexed.insert(dotted.clone()) {
            continue;
        }
        if let Some(src) = resolver.resolve(&dotted) {
            if let Ok(parsed) = parse_module(&src) {
                index_module(&mut index, &dotted, parsed.suite());
            }
        }
    }

    Ok(index)
}

/// Collect dotted module names imported by ``stmts`` (recursively), resolving
/// relative imports against ``current_module``. Records the imported module
/// and useful parents so attribute access (`a.b.c`) resolves.
fn collect_imports(stmts: &[Stmt], current_module: &str, out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Import(ast::StmtImport { names, .. }) => {
                for alias in names {
                    let dotted = alias.name.as_str();
                    out.push(dotted.to_string());
                    // Parents, for ``import a.b.c`` then ``a.b.c.f()``.
                    let parts: Vec<&str> = dotted.split('.').collect();
                    for end in 1..parts.len() {
                        out.push(parts[..end].join("."));
                    }
                }
            }
            Stmt::ImportFrom(ast::StmtImportFrom {
                module,
                names,
                level,
                ..
            }) => {
                let Some(base) = resolve_relative(
                    current_module,
                    *level,
                    module.as_ref().map(ast::Identifier::as_str),
                ) else {
                    continue;
                };
                if !base.is_empty() {
                    out.push(base.clone());
                }
                // ``from a import b`` where ``b`` is itself a submodule.
                for alias in names {
                    let name = alias.name.as_str();
                    if name != "*" {
                        let sub = if base.is_empty() {
                            name.to_string()
                        } else {
                            format!("{base}.{name}")
                        };
                        out.push(sub);
                    }
                }
            }
            Stmt::FunctionDef(ast::StmtFunctionDef { body, .. })
            | Stmt::ClassDef(ast::StmtClassDef { body, .. })
            | Stmt::While(ast::StmtWhile { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::With(ast::StmtWith { body, .. }) => {
                collect_imports(body, current_module, out);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_imports(body, current_module, out);
                for clause in elif_else_clauses {
                    collect_imports(&clause.body, current_module, out);
                }
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                collect_imports(body, current_module, out);
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    collect_imports(&handler.body, current_module, out);
                }
                collect_imports(orelse, current_module, out);
                collect_imports(finalbody, current_module, out);
            }
            _ => {}
        }
    }
}

/// Resolve ``from <level dots><module> import ...`` to its base dotted path,
/// relative to ``current_module`` for ``level > 0``.
fn resolve_relative(current_module: &str, level: u32, module: Option<&str>) -> Option<String> {
    if level == 0 {
        return module.map(str::to_string);
    }
    let package = current_module.rsplit_once('.').map_or("", |(p, _)| p);
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
        Stmt::While(ast::StmtWhile { body, .. }) => index_module(index, module_name, body),
        Stmt::For(ast::StmtFor { body, .. }) => index_module(index, module_name, body),
        Stmt::With(ast::StmtWith { body, .. }) => index_module(index, module_name, body),
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
