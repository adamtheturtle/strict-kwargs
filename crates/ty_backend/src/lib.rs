//! An in-process bridge to the pinned ty inference engine.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::source::{line_index, source_text};
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_source_file::{OneIndexed, PositionEncoding, SourceLocation};
use ruff_text_size::TextSize;
use ty_project::metadata::options::{EnvironmentOptions, Options};
use ty_project::metadata::value::RelativePathBuf;
use ty_project::{CheckMode, ProjectDatabase, ProjectMetadata, SemanticDb};

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

    pub fn offset(&self, path: &Path, line: u32, character: u32) -> Option<u32> {
        let file = system_path_to_file(&self.db, SystemPath::from_std_path(path)?).ok()?;
        let source = source_text(&self.db, file);
        let index = line_index(&self.db, file);
        Some(
            index
                .offset(
                    SourceLocation {
                        line: OneIndexed::from_zero_indexed(line as usize),
                        character_offset: OneIndexed::from_zero_indexed(character as usize),
                    },
                    &source,
                    PositionEncoding::Utf16,
                )
                .to_u32(),
        )
    }

    pub fn hover(&self, path: &Path, offset: u32) -> Option<String> {
        let file = system_path_to_file(&self.db, SystemPath::from_std_path(path)?).ok()?;
        let hover = ty_ide::hover(&self.db, self.db.program_file(file), TextSize::from(offset))?;
        Some(
            hover
                .value
                .display(&self.db, ty_ide::MarkupKind::PlainText)
                .to_string(),
        )
    }

    pub fn definitions(&self, path: &Path, offset: u32) -> Option<Vec<(PathBuf, u32, u32)>> {
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
                    let source = source_text(&self.db, file);
                    let location = line_index(&self.db, file).source_location(
                        target.focus_range().start(),
                        &source,
                        PositionEncoding::Utf16,
                    );
                    Some((
                        path,
                        u32::try_from(location.line.to_zero_indexed()).ok()?,
                        u32::try_from(location.character_offset.to_zero_indexed()).ok()?,
                    ))
                })
                .collect(),
        )
    }
}
