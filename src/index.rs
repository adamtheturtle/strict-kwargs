//! Index of callable definitions discovered in the project.

use std::path::{Path, PathBuf};

use ruff_python_ast::Stmt;
use ruff_python_ast::{self as ast};
use ruff_python_parser::parse_module;
use rustc_hash::FxHashMap;

use crate::ast_util::signature_from_parameters;
use crate::error::CheckError;
use crate::signature::Signature;

#[derive(Debug, Default)]
pub struct DefinitionIndex {
    /// Fully-qualified name (e.g. ``main.C.method``) -> signature.
    pub signatures: FxHashMap<String, Signature>,
}

impl DefinitionIndex {
    pub fn insert(&mut self, fullname: String, signature: Signature) {
        self.signatures.insert(fullname, signature);
    }

    pub fn get(&self, fullname: &str) -> Option<&Signature> {
        self.signatures.get(fullname)
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
    let mut index = DefinitionIndex::default();
    for path in python_files {
        let source = std::fs::read_to_string(path)?;
        let parsed = parse_module(&source)?;
        let module = parsed.suite();
        let module_name = module_name_for_path(project_root, path);
        index_module(&mut index, &module_name, module);
    }
    Ok(index)
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
