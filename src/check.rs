//! Check Python sources for positional calls that should use keywords.

use std::path::{Path, PathBuf};

use ruff_python_ast::visitor::{walk_expr, walk_stmt, Visitor};
use ruff_python_ast::Expr;
use ruff_python_ast::{self as ast};
use ruff_python_ast::{Stmt, StmtFunctionDef, StmtClassDef};
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
  path.extension().is_some_and(|ext| ext == "py" || ext == "pyi")
}

fn is_ignored_path(path: &Path) -> bool {
  path.components().any(|component| {
    let name = component.as_os_str().to_string_lossy();
    name.starts_with('.') || name == "venv" || name == "__pycache__"
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
  names: FxHashMap<String, String>,
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
    self
      .current_scope()
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

  fn record_instance(&mut self, local_name: &str, class_fullname: String) {
    self
      .current_scope()
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
    let Some(signature) = self.index.get(&callee_fullname) else {
      return;
    };
    if self.config.debug {
      eprintln!("DEBUG: strict_kwargs: {callee_fullname}");
    }
    let ignored = self.config.is_ignored(&callee_fullname);
    let positional_count = positional_argument_count(&call.arguments);
    if !call_exceeds_positional_limit(signature, &callee_fullname, ignored, positional_count) {
      return;
    }
    let max_positional = signature
      .max_positional_at_call_site(&callee_fullname, ignored)
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

  fn resolve_callee(&self, func: &Expr) -> Option<String> {
    match func {
      Expr::Name(name) => {
        let local = name.id.as_str();
        if let Some(resolved) = self.resolve_local(local) {
          let dunder_call = format!("{resolved}.__call__");
          if self.index.get(&dunder_call).is_some() {
            return Some(dunder_call);
          }
          return Some(resolved);
        }
        Some(format!("{}.{}", self.module_name, local))
      }
      Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
        let attr_name = attr.id.as_str();
        if let Expr::Name(base) = &**value {
          let base_name = base.id.as_str();
          if let Some(class_fullname) = self.resolve_local(base_name) {
            return Some(format!("{class_fullname}.{attr_name}"));
          }
          return Some(format!("{}.{}.{}", self.module_name, base_name, attr_name));
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
              body: method_body,
              ..
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
  if parent.contains('.') {
    let class = parent.rsplit('.').next().unwrap_or(parent);
    format!("\"{method}\" of \"{class}\"")
  } else {
    format!("\"{method}\"")
  }
}
