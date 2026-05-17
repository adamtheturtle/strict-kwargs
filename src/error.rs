//! Error types for ``strict-kwargs``.

use ruff_python_parser::ParseError;

/// Fatal error while reading or parsing a checked file.
#[derive(Debug)]
pub enum CheckError {
    /// Filesystem error while reading a source file.
    Io(std::io::Error),
    /// The Python source could not be parsed.
    Parse(ParseError),
}

impl From<std::io::Error> for CheckError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ParseError> for CheckError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Parse(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for CheckError {}
