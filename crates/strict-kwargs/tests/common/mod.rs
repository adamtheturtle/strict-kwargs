use std::path::PathBuf;

use strict_kwargs::{
    check_paths, fix_paths, fix_paths_with_opt_ins, Config, Diagnostic, FixOptIns,
};

pub const DEFAULT_PYPROJECT: &str = "[project]\nname = \"t\"\nversion = \"0\"\n";

pub struct TestProject {
    _temp: tempfile::TempDir,
    pub root: PathBuf,
    paths: Vec<PathBuf>,
}

#[allow(dead_code)]
impl TestProject {
    pub fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("strictkw")
            .tempdir()
            .expect("tempdir");
        let root = temp.path().to_path_buf();
        std::fs::write(root.join("pyproject.toml"), DEFAULT_PYPROJECT).expect("write pyproject");
        Self {
            _temp: temp,
            root,
            paths: Vec::new(),
        }
    }

    /// Write a project file and add it to the explicit paths set.
    pub fn file(mut self, path: &str, content: &str) -> Self {
        let file_path = self.write_file(path, content);
        self.paths.push(file_path);
        self
    }

    /// Write a project file that is discovered only through imports or a
    /// directory walk, not through the explicit paths set.
    pub fn dep(self, path: &str, content: &str) -> Self {
        self.write_file(path, content);
        self
    }

    pub fn main(self, content: &str) -> Self {
        self.file("main.py", content)
    }

    pub fn pyproject(self, content: &str) -> Self {
        self.write_file("pyproject.toml", content);
        self
    }

    pub fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    pub fn main_path(&self) -> PathBuf {
        self.path("main.py")
    }

    pub fn config(&self) -> Config {
        Config::load(&self.root).expect("valid config")
    }

    /// Diagnostics for `main.py`, formatted `main:<line>: <message>`.
    pub fn check(&self) -> Vec<String> {
        self.check_one(&self.main_path())
            .iter()
            .map(|d| format!("main:{}: {}", d.line, d.message()))
            .collect()
    }

    /// Diagnostics for `main.py`, formatted as just the diagnostic message.
    pub fn check_main(&self) -> Vec<String> {
        self.check_one(&self.main_path())
            .iter()
            .map(Diagnostic::message)
            .collect()
    }

    /// Diagnostics for the explicitly-added project files, formatted as
    /// `<filename>:<line>: <message>`.
    pub fn check_explicit(&self) -> Vec<String> {
        let diagnostics =
            check_paths(&self.root, &self.paths, &self.config(), None, None).expect("check");
        Self::format_by_filename(&diagnostics)
    }

    /// Diagnostics for the whole project directory, formatted as
    /// `<filename>:<line>: <message>`.
    pub fn check_dir(&self) -> Vec<String> {
        let diagnostics = check_paths(
            &self.root,
            std::slice::from_ref(&self.root),
            &self.config(),
            None,
            None,
        )
        .expect("check");
        Self::format_by_filename(&diagnostics)
    }

    /// Run the fixer over `main.py` and return the rewritten source, or the
    /// original source when nothing was fixed.
    pub fn fixed_main(&self) -> String {
        self.fixed_main_with_opt_ins(FixOptIns::default())
    }

    pub fn fixed_main_with_opt_ins(&self, fix_opt_ins: FixOptIns) -> String {
        let main = self.main_path();
        let outcome = fix_paths_with_opt_ins(
            &self.root,
            std::slice::from_ref(&main),
            &self.config(),
            None,
            fix_opt_ins,
        )
        .expect("fix");
        outcome
            .files
            .into_iter()
            .find(|f| f.path == main)
            .map_or_else(
                || std::fs::read_to_string(&main).expect("read"),
                |f| f.fixed,
            )
    }

    /// Run the fixer over `main.py`, returning the raw result so tests can
    /// assert on fail-safe errors and declined-fix details.
    pub fn fix_main_result(&self) -> Result<strict_kwargs::FixOutcome, strict_kwargs::CheckError> {
        let main = self.main_path();
        fix_paths(
            &self.root,
            std::slice::from_ref(&main),
            &self.config(),
            None,
        )
    }

    fn write_file(&self, path: &str, content: &str) -> PathBuf {
        let file_path = self.root.join(path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&file_path, content).expect("write file");
        file_path
    }

    fn check_one(&self, path: &PathBuf) -> Vec<Diagnostic> {
        check_paths(
            &self.root,
            std::slice::from_ref(path),
            &self.config(),
            None,
            None,
        )
        .expect("check")
    }

    fn format_by_filename(diagnostics: &[Diagnostic]) -> Vec<String> {
        diagnostics
            .iter()
            .map(|d| {
                format!(
                    "{}:{}: {}",
                    d.path.file_name().unwrap().to_string_lossy(),
                    d.line,
                    d.message()
                )
            })
            .collect()
    }
}
