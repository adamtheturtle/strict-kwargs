//! Check Python sources for positional calls that should use keywords.

use std::path::{Path, PathBuf};

use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::Expr;
use ruff_python_ast::{self as ast};
use ruff_python_ast::{Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;

use crate::ast_util::{line_column, positional_argument_count};
use crate::config::Config;
use crate::diagnostic::Diagnostic;
use crate::index::{build_index, module_name_for_path, DefinitionIndex};
use crate::signature::Signature;

use crate::error::CheckError;

pub fn check_paths(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
) -> Result<Vec<Diagnostic>, CheckError> {
    let python_files = collect_python_files(paths);
    let index = build_index(project_root, &python_files)?;
    let mut diagnostics = Vec::new();
    for path in &python_files {
        let source = std::fs::read_to_string(path)?;
        let parsed = parse_module(&source)?;
        let module_name = module_name_for_path(project_root, path);
        let mut checker = CallChecker::new(
            path.clone(),
            module_name,
            &source,
            &index,
            config,
            &mut diagnostics,
        );
        for stmt in parsed.suite() {
            checker.visit_stmt(stmt);
        }
    }
    diagnostics.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.column.cmp(&right.column))
    });
    Ok(diagnostics)
}

fn collect_python_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() && is_python_file(path) {
            files.push(path.clone());
        } else if path.is_dir() {
            for entry in walkdir::WalkDir::new(path)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_file())
            {
                let entry_path = entry.path().to_path_buf();
                if is_python_file(&entry_path) && !is_ignored_path(&entry_path) {
                    files.push(entry_path);
                }
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

fn is_python_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext == "py" || ext == "pyi")
}

fn is_ignored_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => {
            let name = name.to_string_lossy();
            name.starts_with('.') || name == "venv" || name == "__pycache__"
        }
        _ => false,
    })
}

struct CallChecker<'a> {
    path: PathBuf,
    module_name: String,
    source: &'a str,
    index: &'a DefinitionIndex,
    config: &'a Config,
    diagnostics: &'a mut Vec<Diagnostic>,
    scopes: Vec<Scope>,
}

#[derive(Debug, Default, Clone)]
struct Scope {
    /// Local name -> fully-qualified callable/class name.
    names: FxHashMap<String, String>,
    /// Local name -> fully-qualified *module* path (from ``import``).
    modules: FxHashMap<String, String>,
}

impl<'a> CallChecker<'a> {
    fn new(
        path: PathBuf,
        module_name: String,
        source: &'a str,
        index: &'a DefinitionIndex,
        config: &'a Config,
        diagnostics: &'a mut Vec<Diagnostic>,
    ) -> Self {
        Self {
            path,
            module_name,
            source,
            index,
            config,
            diagnostics,
            scopes: vec![Scope::default()],
        }
    }

    fn current_scope(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("scope stack non-empty")
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, local_name: &str, fullname: String) {
        self.current_scope()
            .names
            .insert(local_name.to_string(), fullname);
    }

    fn resolve_local(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(fullname) = scope.names.get(name) {
                return Some(fullname.clone());
            }
        }
        None
    }

    fn define_module(&mut self, local_name: &str, module_path: String) {
        self.current_scope()
            .modules
            .insert(local_name.to_string(), module_path);
    }

    fn resolve_module(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(path) = scope.modules.get(name) {
                return Some(path.clone());
            }
        }
        None
    }

    /// Package containing the current module, for relative imports.
    /// ``pkg.sub.mod`` -> ``pkg.sub``; a top-level module -> ``""``.
    fn current_package(&self) -> &str {
        self.module_name
            .rsplit_once('.')
            .map_or("", |(parent, _)| parent)
    }

    /// Resolve ``from <level dots><module> import ...`` to the base dotted
    /// path that names are imported from.
    fn resolve_import_base(&self, level: u32, module: Option<&str>) -> Option<String> {
        if level == 0 {
            return module.map(str::to_string);
        }
        // ``level`` leading dots: 1 == current package, 2 == its parent, ...
        let mut package: Vec<&str> = if self.current_package().is_empty() {
            Vec::new()
        } else {
            self.current_package().split('.').collect()
        };
        for _ in 1..level {
            package.pop()?;
        }
        let mut base = package.join(".");
        if let Some(module) = module {
            if !base.is_empty() {
                base.push('.');
            }
            base.push_str(module);
        }
        Some(base)
    }

    fn record_import(&mut self, stmt: &Stmt) {
        match stmt {
            // ``import a.b.c`` / ``import a.b as c``
            Stmt::Import(ast::StmtImport { names, .. }) => {
                for alias in names {
                    let dotted = alias.name.as_str();
                    if let Some(asname) = &alias.asname {
                        // ``import a.b as c`` binds ``c`` -> ``a.b``.
                        self.define_module(asname.as_str(), dotted.to_string());
                    } else {
                        // ``import a.b`` binds the top-level ``a``; attribute
                        // access uses the full dotted path.
                        let top = dotted.split('.').next().unwrap_or(dotted);
                        self.define_module(top, top.to_string());
                    }
                }
            }
            // ``from a.b import c [as d]`` / ``from . import x``
            Stmt::ImportFrom(ast::StmtImportFrom {
                module,
                names,
                level,
                ..
            }) => {
                let Some(base) =
                    self.resolve_import_base(*level, module.as_ref().map(ast::Identifier::as_str))
                else {
                    return;
                };
                for alias in names {
                    let imported = alias.name.as_str();
                    if imported == "*" {
                        continue;
                    }
                    let local = alias
                        .asname
                        .as_ref()
                        .map_or(imported, ast::Identifier::as_str);
                    let fullname = if base.is_empty() {
                        imported.to_string()
                    } else {
                        format!("{base}.{imported}")
                    };
                    // The imported name may be a submodule or a callable; bind
                    // both interpretations so attribute and direct calls work.
                    self.define(local, fullname.clone());
                    self.define_module(local, fullname);
                }
            }
            _ => {}
        }
    }

    /// Flatten an attribute/name chain (``a.b.c``) into a dotted string.
    fn dotted_path(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Name(name) => Some(name.id.to_string()),
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                let base = Self::dotted_path(value)?;
                Some(format!("{base}.{attr}"))
            }
            _ => None,
        }
    }

    fn record_instance(&mut self, local_name: &str, class_fullname: String) {
        self.current_scope()
            .names
            .insert(local_name.to_string(), class_fullname);
    }

    fn class_from_constructor(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Call(ast::ExprCall { func, .. }) => match &**func {
                Expr::Name(name) => self.resolve_local(name.id.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    fn check_call(&mut self, call: &ast::ExprCall) {
        let Some(callee_fullname) = self.resolve_callee(&call.func) else {
            return;
        };
        let Some(signatures) = self.index.get(&callee_fullname) else {
            return;
        };
        if self.config.debug {
            eprintln!("DEBUG: strict_kwargs: {callee_fullname}");
        }
        // A constructor call resolves to ``Class.__init__``/``__new__``; also
        // honor an ``ignore_names`` entry for the class itself (``builtins.str``).
        let ignored = self.config.is_ignored(&callee_fullname)
            || callee_fullname
                .strip_suffix(".__init__")
                .or_else(|| callee_fullname.strip_suffix(".__new__"))
                .is_some_and(|class| self.config.is_ignored(class));
        let positional_count = positional_argument_count(&call.arguments);
        // Overload-safe: only flag when the call exceeds the positional limit
        // of *every* candidate signature (the most permissive overload wins),
        // so ``.pyi`` stub overloads never produce false positives.
        if signatures.iter().any(|signature| {
            !call_exceeds_positional_limit(signature, &callee_fullname, ignored, positional_count)
        }) {
            return;
        }
        let max_positional = signatures
            .iter()
            .filter_map(|signature| {
                signature.max_positional_at_call_site(&callee_fullname, ignored)
            })
            .max()
            .unwrap_or(0);
        let (line, column) = line_column(self.source, call.start());
        self.diagnostics.push(Diagnostic {
            path: self.path.clone(),
            line,
            column,
            callee: format_callee_display(&callee_fullname),
            positional_count,
            max_positional,
        });
    }

    /// Map a base name to the signature-bearing fullname to check: the name
    /// itself (a function), else its constructor (``__init__``/``__new__``
    /// for a class). Returns `None` when nothing is indexed.
    fn callable_fullname(&self, base: &str) -> Option<String> {
        if self.index.get(base).is_some() {
            return Some(base.to_string());
        }
        for ctor in ["__init__", "__new__"] {
            let candidate = format!("{base}.{ctor}");
            if self.index.get(&candidate).is_some() {
                return Some(candidate);
            }
        }
        None
    }

    fn resolve_callee(&self, func: &Expr) -> Option<String> {
        match func {
            Expr::Name(name) => {
                let local = name.id.as_str();
                if let Some(resolved) = self.resolve_local(local) {
                    let dunder_call = format!("{resolved}.__call__");
                    if self.index.get(&dunder_call).is_some() {
                        return Some(dunder_call);
                    }
                    // Class name -> its constructor, if indexed.
                    return Some(self.callable_fullname(&resolved).unwrap_or(resolved));
                }
                // Not a local binding: try this module, then builtins.
                let module_candidate = format!("{}.{}", self.module_name, local);
                if let Some(found) = self.callable_fullname(&module_candidate) {
                    return Some(found);
                }
                if let Some(found) = self.callable_fullname(&format!("builtins.{local}")) {
                    return Some(found);
                }
                Some(module_candidate)
            }
            Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
                let attr_name = attr.id.as_str();
                if let Expr::Name(base) = &**value {
                    let base_name = base.id.as_str();
                    // ``import a.b as m`` / ``import os.path`` then ``m.f()``.
                    if let Some(module_path) = self.resolve_module(base_name) {
                        return Some(format!("{module_path}.{attr_name}"));
                    }
                    if let Some(class_fullname) = self.resolve_local(base_name) {
                        return Some(format!("{class_fullname}.{attr_name}"));
                    }
                    return Some(format!("{}.{}.{}", self.module_name, base_name, attr_name));
                }
                // Deeper chains: ``import os.path`` then ``os.path.join()``.
                if let Some(chain) = Self::dotted_path(value) {
                    let (head, rest) = chain
                        .split_once('.')
                        .map_or((chain.as_str(), None), |(h, r)| (h, Some(r)));
                    if let Some(module_path) = self.resolve_module(head) {
                        return Some(match rest {
                            Some(rest) => format!("{module_path}.{rest}.{attr_name}"),
                            None => format!("{module_path}.{attr_name}"),
                        });
                    }
                }
                None
            }
            Expr::Call(constructor) => {
                if let Expr::Name(class_name) = &*constructor.func {
                    if let Some(class_fullname) = self.resolve_local(class_name.id.as_str()) {
                        let dunder_call = format!("{class_fullname}.__call__");
                        if self.index.get(&dunder_call).is_some() {
                            return Some(dunder_call);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }
}

impl<'a> Visitor<'a> for CallChecker<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(StmtFunctionDef { name, body, .. }) => {
                self.define(name, format!("{}.{}", self.module_name, name));
                self.push_scope();
                for inner in body {
                    walk_stmt(self, inner);
                }
                self.pop_scope();
            }
            Stmt::ClassDef(StmtClassDef { name, body, .. }) => {
                let class_fullname = format!("{}.{}", self.module_name, name);
                self.define(name, class_fullname.clone());
                self.push_scope();
                for inner in body {
                    match inner {
                        Stmt::FunctionDef(StmtFunctionDef {
                            body: method_body, ..
                        }) => {
                            self.push_scope();
                            for method_stmt in method_body {
                                walk_stmt(self, method_stmt);
                            }
                            self.pop_scope();
                        }
                        _ => walk_stmt(self, inner),
                    }
                }
                self.pop_scope();
            }
            Stmt::Assign(ast::StmtAssign { targets, value, .. }) => {
                if let Some(class_fullname) = self.class_from_constructor(value) {
                    for target in targets {
                        if let Expr::Name(name) = target {
                            self.record_instance(name.id.as_str(), class_fullname.clone());
                        }
                    }
                }
                walk_stmt(self, stmt);
            }
            Stmt::AnnAssign(ast::StmtAnnAssign {
                target,
                value: Some(value),
                ..
            }) => {
                if let Some(class_fullname) = self.class_from_constructor(value) {
                    if let Expr::Name(name) = &**target {
                        self.record_instance(name.id.as_str(), class_fullname);
                    }
                }
                walk_stmt(self, stmt);
            }
            Stmt::Import(_) | Stmt::ImportFrom(_) => {
                self.record_import(stmt);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Call(call) = expr {
            self.check_call(call);
        }
        walk_expr(self, expr);
    }
}

fn call_exceeds_positional_limit(
    signature: &Signature,
    fullname: &str,
    ignored: bool,
    positional_count: usize,
) -> bool {
    if ignored {
        return false;
    }
    let Some(max_positional) = signature.max_positional_at_call_site(fullname, false) else {
        return false;
    };
    let has_var_positional = signature
        .parameters
        .iter()
        .any(|p| p.kind == crate::signature::ParameterKind::VarPositional);
    if has_var_positional && positional_count > max_positional {
        return false;
    }
    positional_count > max_positional
}

/// Format like mypy: ``"func"`` or ``"method" of "C"``.
fn format_callee_display(fullname: &str) -> String {
    let Some((parent, method)) = fullname.rsplit_once('.') else {
        return format!("\"{fullname}\"");
    };
    if method == "__init__" || method == "__new__" {
        // Constructor: report the class name (``"str"``), as mypy does.
        let class = parent.rsplit('.').next().unwrap_or(parent);
        return format!("\"{class}\"");
    }
    if parent.contains('.') {
        let class = parent.rsplit('.').next().unwrap_or(parent);
        format!("\"{method}\" of \"{class}\"")
    } else {
        format!("\"{method}\"")
    }
}
