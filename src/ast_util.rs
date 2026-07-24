//! Convert Ruff AST nodes into our signature model.

use ruff_python_ast::{self as ast};
use ruff_python_ast::{ParameterWithDefault, Parameters};

use crate::signature::{Parameter, ParameterKind, Signature};

pub fn signature_from_parameters(parameters: &Parameters) -> Signature {
    let mut params = Vec::new();

    for arg in &parameters.posonlyargs {
        push_param(&mut params, arg, ParameterKind::PositionalOnly);
    }
    for arg in &parameters.args {
        push_param(&mut params, arg, ParameterKind::PositionalOrKeyword);
    }
    if let Some(vararg) = &parameters.vararg {
        params.push(Parameter {
            name: Some(vararg.name.to_string()),
            kind: ParameterKind::VarPositional,
        });
    }
    for arg in &parameters.kwonlyargs {
        push_param(&mut params, arg, ParameterKind::KeywordOnly);
    }
    if let Some(kwarg) = &parameters.kwarg {
        params.push(Parameter {
            name: Some(kwarg.name.to_string()),
            kind: ParameterKind::VarKeyword,
        });
    }

    Signature { parameters: params }
}

fn push_param(params: &mut Vec<Parameter>, arg: &ParameterWithDefault, kind: ParameterKind) {
    params.push(Parameter {
        name: Some(arg.parameter.name.to_string()),
        kind,
    });
}

/// Count non-starred positional arguments in a call.
pub fn positional_argument_count(arguments: &ast::Arguments) -> usize {
    arguments
        .args
        .iter()
        .filter(|expr| !expr.is_starred_expr())
        .count()
}

/// Byte offset of the start of every line, using Python's universal-newline
/// rule: `\n`, a lone `\r`, and `\r\n` each end a line (the Ruff parser the
/// AST offsets come from splits lines the same way). Recognizing only `\n`
/// would collapse a carriage-return-delimited file onto line 1 (issue #270).
pub fn line_starts(source: &str) -> Vec<usize> {
    let bytes = source.as_bytes();
    let mut starts = vec![0usize];
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                starts.push(index + 1);
                index += 1;
            }
            b'\r' => {
                // `\r\n` is a single break; a lone `\r` still starts a line.
                index += if bytes.get(index + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                starts.push(index);
            }
            _ => index += 1,
        }
    }
    starts
}

pub fn line_column_from_starts(
    source: &str,
    line_starts: &[usize],
    offset: ruff_text_size::TextSize,
) -> (usize, usize) {
    let offset = offset.to_usize();
    let line = line_starts.partition_point(|&start| start <= offset);
    let line_start = line_starts
        .get(line.saturating_sub(1))
        .copied()
        .unwrap_or(0);
    // Columns count source characters, not UTF-8 bytes, so a multibyte code
    // point before the offset does not inflate the column (issue #271).
    let column = source.get(line_start..offset).map_or_else(
        || offset.saturating_sub(line_start),
        |prefix| prefix.chars().count(),
    ) + 1;
    (line.max(1), column)
}

pub fn line_column(source: &str, offset: ruff_text_size::TextSize) -> (usize, usize) {
    line_column_from_starts(source, &line_starts(source), offset)
}

#[cfg(test)]
mod tests {
    use super::{line_column, line_column_from_starts, line_starts};
    use ruff_text_size::TextSize;

    #[test]
    fn line_starts_splits_lf_cr_and_crlf() {
        assert_eq!(line_starts("a\nb"), vec![0, 2]);
        // A lone carriage return still starts a new line (issue #270).
        assert_eq!(line_starts("a\rb"), vec![0, 2]);
        // `\r\n` is a single break, not two.
        assert_eq!(line_starts("a\r\nb"), vec![0, 3]);
        // Mixed terminators and a trailing break.
        assert_eq!(line_starts("a\rb\nc\r\n"), vec![0, 2, 4, 7]);
    }

    #[test]
    fn line_column_counts_characters_not_bytes() {
        // `é` is one character but two UTF-8 bytes; the following column must
        // count the character, not the byte (issue #271).
        let source = "é = f";
        let offset = TextSize::new(u32::try_from(source.find('f').unwrap()).unwrap());
        assert_eq!(line_column(source, offset), (1, 5));
    }

    #[test]
    fn line_column_on_carriage_return_line_reports_physical_line() {
        // Byte offset of the `f` on the third physical line.
        let source = "a\rb\rf";
        let offset = TextSize::new(u32::try_from(source.rfind('f').unwrap()).unwrap());
        assert_eq!(line_column(source, offset), (3, 1));
    }

    #[test]
    fn line_column_from_starts_out_of_range_offset_saturates() {
        // A byte offset past the end (or one that does not sit on a char
        // boundary of the sliced prefix) falls back to the byte delta.
        let starts = line_starts("abc");
        assert_eq!(
            line_column_from_starts("abc", &starts, TextSize::new(99)),
            (1, 100)
        );
    }
}
