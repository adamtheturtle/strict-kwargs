//! Check Python sources for positional calls that should use keywords.

use std::path::{Path, PathBuf};

use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::Expr;
use ruff_python_ast::{self as ast};
use ruff_python_ast::{Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;

use ruff_text_size::TextSize;

use crate::ast_util::{line_column, positional_argument_count, signature_from_parameters};
use crate::config::Config;
use crate::diagnostic::Diagnostic;
use crate::error::CheckError;
use crate::index::{
    build_index, is_package_init, module_name_for_path, relative_base, DefinitionIndex,
};
use crate::signature::Signature;
use crate::ty_resolver::{
    byte_offset_to_lsp, location_from_value, lsp_to_byte_offset, parse_callable_type_overloads,
    parse_hover_signature, same_path, ty_binary_present, TyResolver,
};

pub fn check_paths(
    project_root: &Path,
    paths: &[PathBuf],
    config: &Config,
) -> Result<Vec<Diagnostic>, CheckError> {
    let python_files = collect_python_files(paths);
    let index = build_index(project_root, &python_files)?;
    // ty-grade resolution (inheritance/MRO, return types, annotated params,
    // overloads) for calls the built-in resolver cannot resolve. Optional:
    // absence of `ty` just disables the fallback.
    let mut ty = TyResolver::start(project_root);
    if ty.is_none() && ty_binary_present() {
        eprintln!(
            "strict-kwargs: `ty` found but its language server could not be \
             started; continuing without the type-inference fallback"
        );
    }
    let mut ty_file_cache: FxHashMap<PathBuf, Option<String>> = FxHashMap::default();
    let mut diagnostics = Vec::new();
    for path in &python_files {
        let source = std::fs::read_to_string(path)?;
        let parsed = parse_module(&source)?;
        let module_name = module_name_for_path(project_root, path);
        let mut checker = CallChecker::new(
            path.clone(),
            module_name,
            is_package_init(path),
            &source,
            &index,
            config,
            &mut diagnostics,
        );
        for stmt in parsed.suite() {
            checker.visit_stmt(stmt);
        }
        let pending = std::mem::take(&mut checker.ty_pending);
        if let Some(ty) = ty.as_mut() {
            resolve_pending_with_ty(
                ty,
                path,
                &source,
                &pending,
                &mut ty_file_cache,
                &mut diagnostics,
            );
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
    /// Whether the file is a package initializer (`__init__.py`), which is
    /// the anchor for its own relative imports.
    is_package: bool,
    source: &'a str,
    index: &'a DefinitionIndex,
    config: &'a Config,
    diagnostics: &'a mut Vec<Diagnostic>,
    scopes: Vec<Scope>,
    /// Calls the built-in resolver couldn't resolve, deferred for a single
    /// pipelined batch of ty queries per file.
    ty_pending: Vec<PendingTy>,
}

/// A call awaiting ty resolution: byte offsets into the file's source.
struct PendingTy {
    /// Start of the callee identifier (where we hover / goto-definition).
    callee_offset: usize,
    /// Start of the whole call expression (for the diagnostic position).
    call_start: usize,
    positional_count: usize,
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
        is_package: bool,
        source: &'a str,
        index: &'a DefinitionIndex,
        config: &'a Config,
        diagnostics: &'a mut Vec<Diagnostic>,
    ) -> Self {
        Self {
            path,
            module_name,
            is_package,
            source,
            index,
            config,
            diagnostics,
            scopes: vec![Scope::default()],
            ty_pending: Vec::new(),
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

    /// Resolve ``from <level dots><module> import ...`` to its base dotted
    /// path, using the shared resolver so package (`__init__`) anchoring
    /// matches the indexer.
    fn resolve_import_base(&self, level: u32, module: Option<&str>) -> Option<String> {
        relative_base(&self.module_name, self.is_package, level, module)
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
        let resolved = self.resolve_callee(&call.func);
        let indexed = resolved
            .as_deref()
            .filter(|name| self.index.get(name).is_some())
            .map(str::to_string);
        let Some(callee_fullname) = indexed else {
            // Built-in resolver couldn't resolve: defer to a pipelined ty
            // query (handled once per file after the walk).
            self.record_ty_pending(call);
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

    /// Defer a call the built-in resolver missed to a pipelined ty query.
    fn record_ty_pending(&mut self, call: &ast::ExprCall) {
        // Position at the callee identifier: the attribute for ``x.m()``,
        // otherwise the name itself.
        let callee_offset = match &*call.func {
            Expr::Attribute(attr) => attr.attr.range().start(),
            Expr::Name(name) => name.range().start(),
            _ => return,
        };
        self.ty_pending.push(PendingTy {
            callee_offset: callee_offset.to_usize(),
            call_start: call.start().to_usize(),
            positional_count: positional_argument_count(&call.arguments),
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
                    // Local bindings (incl. a locally redefined class) take
                    // precedence over a stale ``import`` module binding.
                    let candidate = if let Some(local) = self.resolve_local(base_name) {
                        format!("{local}.{attr_name}")
                    } else if let Some(module_path) = self.resolve_module(base_name) {
                        // ``import a.b as m`` / ``import lib`` then ``m.f()``.
                        format!("{module_path}.{attr_name}")
                    } else {
                        format!("{}.{}.{}", self.module_name, base_name, attr_name)
                    };
                    // Resolve through constructors so e.g. ``lib.MyClass(1)``
                    // finds ``lib.MyClass.__init__``.
                    return Some(self.callable_fullname(&candidate).unwrap_or(candidate));
                }
                // Deeper chains: ``import os.path`` then ``os.path.join()``.
                if let Some(chain) = Self::dotted_path(value) {
                    let (head, rest) = chain
                        .split_once('.')
                        .map_or((chain.as_str(), None), |(h, r)| (h, Some(r)));
                    if let Some(module_path) = self.resolve_module(head) {
                        let candidate = match rest {
                            Some(rest) => format!("{module_path}.{rest}.{attr_name}"),
                            None => format!("{module_path}.{attr_name}"),
                        };
                        return Some(self.callable_fullname(&candidate).unwrap_or(candidate));
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

/// Whether byte `offset` falls within an identifier's range.
fn ident_hit(ident: &ast::Identifier, offset: usize) -> bool {
    let range = ident.range();
    offset >= range.start().to_usize() && offset < range.end().to_usize()
}

type FnEntry<'a> = (Option<String>, &'a StmtFunctionDef);

/// Collect every function (with its immediate enclosing class name) and class
/// defined in `stmts`, recursing through classes and control-flow blocks
/// (typeshed gates defs behind `if sys.version_info`).
fn collect_defs<'a>(
    stmts: &'a [Stmt],
    class: Option<&str>,
    funcs: &mut Vec<FnEntry<'a>>,
    classes: &mut Vec<&'a StmtClassDef>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(f) => {
                funcs.push((class.map(str::to_string), f));
                collect_defs(&f.body, None, funcs, classes);
            }
            Stmt::ClassDef(c) => {
                classes.push(c);
                collect_defs(&c.body, Some(c.name.as_str()), funcs, classes);
            }
            Stmt::If(ast::StmtIf {
                body,
                elif_else_clauses,
                ..
            }) => {
                collect_defs(body, class, funcs, classes);
                for clause in elif_else_clauses {
                    collect_defs(&clause.body, class, funcs, classes);
                }
            }
            Stmt::Try(ast::StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                ..
            }) => {
                collect_defs(body, class, funcs, classes);
                for handler in handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_defs(&h.body, class, funcs, classes);
                }
                collect_defs(orelse, class, funcs, classes);
                collect_defs(finalbody, class, funcs, classes);
            }
            Stmt::With(ast::StmtWith { body, .. })
            | Stmt::For(ast::StmtFor { body, .. })
            | Stmt::While(ast::StmtWhile { body, .. }) => {
                collect_defs(body, class, funcs, classes);
            }
            _ => {}
        }
    }
}

/// Given the byte offset ty resolved a callee to, find the function (or class
/// constructor) defined there and return its synthetic fullname plus all
/// overload signatures (most-permissive overload wins downstream).
fn resolve_def_at(stmts: &[Stmt], offset: usize) -> Option<(String, Vec<Signature>)> {
    let mut funcs: Vec<FnEntry> = Vec::new();
    let mut classes: Vec<&StmtClassDef> = Vec::new();
    collect_defs(stmts, None, &mut funcs, &mut classes);

    if let Some((class, target)) = funcs.iter().find(|(_, f)| ident_hit(&f.name, offset)) {
        let name = target.name.as_str();
        let overloads: Vec<Signature> = funcs
            .iter()
            .filter(|(c, f)| c.as_deref() == class.as_deref() && f.name.as_str() == name)
            .map(|(_, f)| signature_from_parameters(&f.parameters))
            .collect();
        let fullname = match class {
            Some(c) if name == "__init__" || name == "__new__" => {
                format!("ty.{c}.__init__")
            }
            Some(c) => format!("ty.{c}.{name}"),
            None => format!("ty.{name}"),
        };
        return Some((fullname, overloads));
    }

    // ty pointed at a class itself: a constructor call.
    if let Some(class) = classes.iter().find(|c| ident_hit(&c.name, offset)) {
        for ctor in ["__init__", "__new__"] {
            let sigs: Vec<Signature> = class
                .body
                .iter()
                .filter_map(|s| match s {
                    Stmt::FunctionDef(f) if f.name.as_str() == ctor => {
                        Some(signature_from_parameters(&f.parameters))
                    }
                    _ => None,
                })
                .collect();
            if !sigs.is_empty() {
                return Some((format!("ty.{}.__init__", class.name.as_str()), sigs));
            }
        }
    }
    None
}

/// The identifier starting at byte `offset` in `source` (the callee name, for
/// the diagnostic display when hover gave an unnamed callable type).
fn identifier_at(source: &str, offset: usize) -> Option<String> {
    let rest = source.get(offset..)?;
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_string())
}

/// Parse a ty-reported parameter list (`a: int, b: int = ..., /`) into a
/// signature by reusing the real parser. `None` if it doesn't parse.
fn signature_from_param_text(params: &str) -> Option<Signature> {
    let src = format!("def __sk__({params}): ...\n");
    let parsed = parse_module(&src).ok()?;
    parsed.suite().iter().find_map(|stmt| match stmt {
        Stmt::FunctionDef(f) => Some(signature_from_parameters(&f.parameters)),
        _ => None,
    })
}

fn emit_if_violation(
    fullname: &str,
    signatures: &[Signature],
    positional_count: usize,
    source: &str,
    call_start: usize,
    path: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if signatures.is_empty()
        || signatures
            .iter()
            .any(|s| !call_exceeds_positional_limit(s, fullname, false, positional_count))
    {
        return;
    }
    let max_positional = signatures
        .iter()
        .filter_map(|s| s.max_positional_at_call_site(fullname, false))
        .max()
        .unwrap_or(0);
    let (line, column) = line_column(source, TextSize::new(call_start as u32));
    diagnostics.push(Diagnostic {
        path: path.to_path_buf(),
        line,
        column,
        callee: format_callee_display(fullname),
        positional_count,
        max_positional,
    });
}

fn hover_text(value: &serde_json::Value) -> Option<String> {
    let contents = value.get("contents")?;
    if let Some(s) = contents.as_str() {
        return Some(s.to_string());
    }
    contents
        .get("value")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Resolve, in one pipelined batch per file, the calls the built-in resolver
/// missed: hover (precise, overload- and inheritance-resolved, stdlib too),
/// then goto-definition for the rest (constructors). Fails closed.
fn resolve_pending_with_ty(
    ty: &mut TyResolver,
    path: &Path,
    source: &str,
    pending: &[PendingTy],
    file_cache: &mut FxHashMap<PathBuf, Option<String>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if pending.is_empty() || ty.ensure_open(path, source).is_none() {
        return;
    }

    // Phase A: pipeline all hover requests, then collect.
    let hover_ids: Vec<Option<i64>> = pending
        .iter()
        .map(|p| {
            let (line, ch) = byte_offset_to_lsp(source, p.callee_offset);
            ty.ask("textDocument/hover", path, line, ch)
        })
        .collect();

    let mut needs_def: Vec<usize> = Vec::new();
    for (i, p) in pending.iter().enumerate() {
        let raw = hover_ids[i]
            .and_then(|id| ty.take(id))
            .as_ref()
            .and_then(hover_text);
        let Some(raw) = raw else {
            needs_def.push(i);
            continue;
        };

        // `def …`/`bound method …` display: a single, named signature.
        if let Some(sig) = parse_hover_signature(&raw) {
            let Some(signature) = signature_from_param_text(&sig.params) else {
                continue;
            };
            let fullname = match &sig.owner {
                Some(owner) => {
                    let owner = owner.split('[').next().unwrap_or(owner);
                    let owner = owner.rsplit('.').next().unwrap_or(owner);
                    format!("ty.{owner}.{}", sig.name)
                }
                None => format!("ty.{}", sig.name),
            };
            emit_if_violation(
                &fullname,
                &[signature],
                p.positional_count,
                source,
                p.call_start,
                path,
                diagnostics,
            );
            continue;
        }

        // Callable-*type* display, incl. `Overload[…]`: ty already excluded
        // `self` and kept typeshed positional-only `/` markers. Use it
        // directly rather than falling through to goto-definition, which on
        // an inferred stdlib receiver lands on runtime `.py` source whose
        // signatures drop `/` and yield false positives (issue #14).
        let overloads: Vec<Signature> = parse_callable_type_overloads(&raw)
            .iter()
            .filter_map(|params| signature_from_param_text(params))
            .collect();
        if overloads.is_empty() {
            needs_def.push(i);
            continue;
        }
        let name = identifier_at(source, p.callee_offset).unwrap_or_default();
        emit_if_violation(
            &format!("ty.{name}"),
            &overloads,
            p.positional_count,
            source,
            p.call_start,
            path,
            diagnostics,
        );
    }

    // Phase B: pipeline goto-definition for hover misses (constructors).
    let def_ids: Vec<(usize, Option<i64>)> = needs_def
        .iter()
        .map(|&i| {
            let (line, ch) = byte_offset_to_lsp(source, pending[i].callee_offset);
            (i, ty.ask("textDocument/definition", path, line, ch))
        })
        .collect();
    for (i, id) in def_ids {
        let Some(loc) = id
            .and_then(|id| ty.take(id))
            .as_ref()
            .and_then(location_from_value)
        else {
            continue;
        };
        let target = if same_path(&loc.path, path) {
            Some(source.to_string())
        } else {
            file_cache
                .entry(loc.path.clone())
                .or_insert_with(|| std::fs::read_to_string(&loc.path).ok())
                .clone()
        };
        let Some(target) = target else { continue };
        let Ok(parsed) = parse_module(&target) else {
            continue;
        };
        let Some(off) = lsp_to_byte_offset(&target, loc.line, loc.character) else {
            continue;
        };
        if let Some((fullname, sigs)) = resolve_def_at(parsed.suite(), off) {
            emit_if_violation(
                &fullname,
                &sigs,
                pending[i].positional_count,
                source,
                pending[i].call_start,
                path,
                diagnostics,
            );
        }
    }
}
