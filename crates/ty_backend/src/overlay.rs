use std::any::Any;
use std::collections::HashMap;

use ruff_db::file_revision::FileRevision;
use ruff_db::files::FilePath;
use ruff_db::system::walk_directory::WalkDirectoryBuilder;
use ruff_db::system::{
    CommandExecutor, DirectoryEntry, FileType, Metadata, OsSystem, Result, System, SystemPath,
    SystemPathBuf, SystemVirtualPath, WhichResult, WritableSystem,
};
use ruff_notebook::{Notebook, NotebookError};

/// Preserve the UTF-8 source decoded by strict-kwargs, including non-UTF-8
/// Python files and buffers that differ from their on-disk contents.
#[derive(Clone, Debug)]
pub(crate) struct OverlaySystem {
    native: OsSystem,
    buffers: HashMap<SystemPathBuf, (String, u128)>,
}

impl OverlaySystem {
    pub(crate) fn new(root: &SystemPath) -> Self {
        Self {
            native: OsSystem::new(root),
            buffers: HashMap::new(),
        }
    }

    pub(crate) fn set_source(&mut self, path: SystemPathBuf, source: String) {
        let revision = self
            .buffers
            .get(&path)
            .map_or(1, |(_, revision)| revision + 1);
        self.buffers.insert(path, (source, revision));
    }
}

impl System for OverlaySystem {
    fn path_metadata(&self, path: &SystemPath) -> Result<Metadata> {
        if let Some((_, revision)) = self.buffers.get(path) {
            Ok(Metadata::new(
                FileRevision::new(*revision),
                None,
                FileType::File,
            ))
        } else {
            self.native.path_metadata(path)
        }
    }

    fn canonicalize_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        self.native.canonicalize_path(path)
    }

    fn is_same_file(&self, first: &SystemPath, second: &SystemPath) -> Result<bool> {
        self.native.is_same_file(first, second)
    }

    fn is_file_open(&self, path: &FilePath) -> bool {
        path.as_system_path()
            .is_some_and(|path| self.buffers.contains_key(path))
    }

    fn read_to_string(&self, path: &SystemPath) -> Result<String> {
        self.buffers.get(path).map_or_else(
            || self.native.read_to_string(path),
            |(source, _)| Ok(source.clone()),
        )
    }

    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        self.native.read_to_notebook(path)
    }

    fn read_virtual_path_to_string(&self, path: &SystemVirtualPath) -> Result<String> {
        self.native.read_virtual_path_to_string(path)
    }

    fn read_virtual_path_to_notebook(
        &self,
        path: &SystemVirtualPath,
    ) -> std::result::Result<Notebook, NotebookError> {
        self.native.read_virtual_path_to_notebook(path)
    }

    fn which(&self, name: &str) -> WhichResult {
        self.native.which(name)
    }
    fn current_directory(&self) -> &SystemPath {
        self.native.current_directory()
    }
    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        self.native.user_config_directory()
    }
    fn cache_dir(&self) -> Option<SystemPathBuf> {
        self.native.cache_dir()
    }
    fn read_directory<'a>(
        &'a self,
        path: &SystemPath,
    ) -> Result<Box<dyn Iterator<Item = Result<DirectoryEntry>> + 'a>> {
        self.native.read_directory(path)
    }
    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        self.native.walk_directory(path)
    }
    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        self.native.as_writable()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn env_var(&self, name: &str) -> std::result::Result<String, std::env::VarError> {
        self.native.env_var(name)
    }
    fn command_executor(&self) -> Option<&dyn CommandExecutor> {
        self.native.command_executor()
    }
    fn dyn_clone(&self) -> Box<dyn System> {
        Box::new(self.clone())
    }
}
