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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use ruff_python_parser::parse_module;

    fn parse_error() -> ParseError {
        // `def f(:` is an unrecoverable syntax error.
        parse_module("def f(:\n").expect_err("expected a parse error")
    }

    #[test]
    fn io_error_converts_and_displays() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let error = CheckError::from(io);
        assert_eq!(error.to_string(), "missing");
        // The `Debug` derive identifies the variant without a `matches!`
        // (whose synthesized non-matching arm would be uncoverable).
        // Also exercises the `std::error::Error` impl.
        let _: &dyn std::error::Error = &error;
        assert!(format!("{error:?}").starts_with("Io("));
    }

    #[test]
    fn parse_error_converts_and_displays() {
        let error = CheckError::from(parse_error());
        assert!(!error.to_string().is_empty());
        assert!(format!("{error:?}").starts_with("Parse("));
    }
}
