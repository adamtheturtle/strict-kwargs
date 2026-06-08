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

pub fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(
        source
            .char_indices()
            .filter_map(|(index, ch)| (ch == '\n').then_some(index + 1)),
    );
    starts
}

pub fn line_column_from_starts(
    line_starts: &[usize],
    offset: ruff_text_size::TextSize,
) -> (usize, usize) {
    let offset = offset.to_usize();
    let line = line_starts.partition_point(|&start| start <= offset);
    let line_start = line_starts
        .get(line.saturating_sub(1))
        .copied()
        .unwrap_or(0);
    let column = offset.saturating_sub(line_start) + 1;
    (line.max(1), column)
}

pub fn line_column(source: &str, offset: ruff_text_size::TextSize) -> (usize, usize) {
    line_column_from_starts(&line_starts(source), offset)
}
