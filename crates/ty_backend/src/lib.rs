//! An in-process bridge to the pinned ty inference engine.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_python_ast as ast;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_text_size::{Ranged, TextSize};
use ty_project::metadata::options::{EnvironmentOptions, Options};
use ty_project::metadata::value::RelativePathBuf;
use ty_project::{CheckMode, ProjectDatabase, ProjectMetadata, SemanticDb};
use ty_python_semantic::types::ide_support::{
    call_type_simplified_by_overloads, constructor_signature,
};
use ty_python_semantic::types::{KnownInstanceType, Type};
use ty_python_semantic::{HasType, SemanticModel};

/// Include the dependency pin in cache keys automatically when it changes.
pub const ENGINE_ID: &str = concat!("embedded ty\n", include_str!("../Cargo.toml"));

mod overlay;
use overlay::OverlaySystem;

static PROCESS_WILL_EXIT: AtomicBool = AtomicBool::new(false);

/// Let the one-shot CLI leave cached inference storage to process teardown.
/// Library callers retain ordinary reclamation after each invocation.
pub fn prepare_for_process_exit() {
    PROCESS_WILL_EXIT.store(true, Ordering::Relaxed);
}

/// A single check/fix invocation owns one lazily initialized database.
/// Sharing is confined to this invocation: simultaneous runs must not share
/// frozen filesystem inputs or source buffers.
#[derive(Clone)]
pub struct Session(Arc<SessionInner>);

struct SessionInner {
    root: PathBuf,
    python: Option<PathBuf>,
    database: Mutex<Option<ProjectDatabase>>,
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        if PROCESS_WILL_EXIT.load(Ordering::Relaxed) {
            if let Some(db) = self
                .database
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                // Match ty's CLI. Only Salsa storage is retained; remaining
                // fields still drop so environment workers shut down normally.
                db.forget_storage_on_exit();
            }
        }
    }
}

impl Session {
    pub fn new(root: &Path, python: Option<&Path>) -> Self {
        Self(Arc::new(SessionInner {
            root: root.to_path_buf(),
            python: python.map(Path::to_path_buf),
            database: Mutex::new(None),
        }))
    }

    pub fn engine(&self) -> Result<Engine, String> {
        let mut database = self.0.database.lock().map_err(|e| e.to_string())?;
        if database.is_none() {
            let mut db = self.create_database()?;
            db.freeze();
            *database = Some(db);
        }
        let db = database
            .as_ref()
            .ok_or("database initialization failed")?
            .clone();
        Ok(Engine {
            db,
            session: self.clone(),
            mutable: false,
        })
    }

    fn create_database(&self) -> Result<ProjectDatabase, String> {
        let root = SystemPathBuf::from_path_buf(
            std::path::absolute(&self.0.root).map_err(|e| e.to_string())?,
        )
        .map_err(|_| "non-UTF8 project path")?;
        let system = OverlaySystem::new(&root);
        let mut metadata = ProjectMetadata::discover(&root, &system)
            .unwrap_or_else(|_| ProjectMetadata::new("strict-kwargs", root.clone()));
        if let Some(python) = &self.0.python {
            let python = SystemPathBuf::from_path_buf(
                std::path::absolute(python).map_err(|e| e.to_string())?,
            )
            .map_err(|_| "non-UTF8 Python path")?;
            metadata.set_override_options(Options {
                environment: Some(EnvironmentOptions {
                    python: Some(RelativePathBuf::cli(python)),
                    ..EnvironmentOptions::default()
                }),
                ..Options::default()
            });
        }
        // Match the language server's handling of invalid environment settings.
        let _ = metadata.apply_configuration_files(&system);
        let mut db = ProjectDatabase::fallible(metadata.clone(), system.clone())
            .unwrap_or_else(|_| ProjectDatabase::use_defaults(metadata, system));
        // Signature queries still infer types for every required scope. An
        // empty, frozen open-file set skips diagnostic validation and editor
        // completion metadata that this checker never consumes. Apply the
        // same policy to private databases for decoded or changed buffers.
        db.set_check_mode(CheckMode::OpenFiles);
        db.freeze_open_files();
        Ok(db)
    }
}

/// A definition in the source's native UTF-8 byte coordinates.
#[derive(Clone)]
pub struct Definition {
    pub path: PathBuf,
    pub offset: u32,
}

/// One worker's database handle. Ordinary files share cached inference;
/// changed or decoded source switches this worker to a mutable database.
pub struct Engine {
    db: ProjectDatabase,
    session: Session,
    mutable: bool,
}

impl Engine {
    pub fn open_source(&mut self, path: &Path, text: &str) -> Option<()> {
        let path = SystemPath::from_std_path(path)?;
        let file = system_path_to_file(&self.db, path).ok()?;
        let current = source_text(&self.db, file);
        if current.as_str() == text && current.read_error().is_none() {
            return Some(());
        }
        // Salsa mutations wait for every other snapshot to be dropped. Fork
        // before changing a buffer so parallel workers cannot deadlock or see
        // another worker's unsaved source. Earlier buffers matched disk.
        if !self.mutable {
            self.db = self.session.create_database().ok()?;
            self.mutable = true;
        }
        self.db
            .system_mut()
            .as_any_mut()
            .downcast_mut::<OverlaySystem>()?
            .set_source(path.to_path_buf(), text.to_owned());
        File::sync_path(&mut self.db, path);
        Some(())
    }

    pub fn hover(&self, path: &Path, offset: u32) -> Option<String> {
        let file = system_path_to_file(&self.db, SystemPath::from_std_path(path)?).ok()?;
        if let Some(signature) = self.call_hover(file, TextSize::from(offset)) {
            return Some(signature);
        }
        let hover = ty_ide::hover(&self.db, self.db.program_file(file), TextSize::from(offset))?;
        Some(
            hover
                .value
                .display(&self.db, ty_ide::MarkupKind::PlainText)
                .to_string(),
        )
    }

    // Call sites need only the signature, not IDE docstrings and their
    // additional definition/argument-resolution work. Other targets retain
    // the ordinary hover implementation.
    fn call_hover(&self, file: File, offset: TextSize) -> Option<String> {
        let program_file = self.db.program_file(file);
        let parsed = parsed_module(&self.db, program_file.python_file(&self.db)).load(&self.db);
        let tokens = parsed.tokens();
        let token = tokens
            .at_offset(offset)
            .find(|token| token.kind() == TokenKind::Identifier)
            .or_else(|| {
                // Several zero-width dedents can precede an identifier at the
                // same offset. at_offset exposes only the first boundary.
                if !tokens
                    .at_offset(offset)
                    .any(|token| token.range().is_empty() && token.start() == offset)
                {
                    return None;
                }
                tokens
                    .after(offset)
                    .iter()
                    .take_while(|token| token.start() == offset)
                    .find(|token| token.kind() == TokenKind::Identifier)
                    .copied()
            })?;
        let node = covering_node(parsed.syntax().into(), token.range())
            .find_first(|node| node.is_identifier() || node.is_expression())
            .ok()?;
        let call = node.ancestors().find_map(|node| {
            let call = node.expr_call()?;
            match call.func.as_ref() {
                ast::Expr::Name(name) if name.range() == token.range() => Some(call),
                ast::Expr::Attribute(attr) if attr.attr.range() == token.range() => Some(call),
                _ => None,
            }
        })?;
        let model = SemanticModel::new(&self.db, program_file);
        if let Some(signature) = constructor_signature(&model, call)
            .or_else(|| call_type_simplified_by_overloads(&model, call))
        {
            return Some(signature);
        }
        let ty = call.func.inferred_type(&model)?;
        let env = model.program_environment();
        if matches!(
            ty,
            Type::TypeVar(_) | Type::KnownInstance(KnownInstanceType::TypeVar(_))
        ) || ty.display_alias_declaration(&self.db, &env).is_some()
        {
            return None;
        }
        Some(ty.display(&self.db, &env).multiline().to_string())
    }

    pub fn definitions(&self, path: &Path, offset: u32) -> Option<Vec<Definition>> {
        let file = system_path_to_file(&self.db, SystemPath::from_std_path(path)?).ok()?;
        let definitions =
            ty_ide::goto_definition(&self.db, self.db.program_file(file), TextSize::from(offset))?;
        Some(
            definitions
                .into_iter()
                .filter_map(|target| {
                    let file = target.file();
                    let path = match file.path(&self.db) {
                        FilePath::System(path) => path.as_std_path().to_path_buf(),
                        FilePath::Vendored(path) => ty_ide::cached_vendored_path(&self.db, path)?
                            .as_std_path()
                            .to_path_buf(),
                        FilePath::SystemVirtual(_) => return None,
                    };
                    Some(Definition {
                        path,
                        offset: target.focus_range().start().to_u32(),
                    })
                })
                .collect(),
        )
    }
}
