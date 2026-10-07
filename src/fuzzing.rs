//! Fuzz-only checks of signatures, encoding, and Unicode-aware insertions.

use std::collections::BTreeMap;

use crate::fix::{apply_insertions, Insertion};
use crate::signature::{Parameter, ParameterKind, Signature};
use crate::source::{decode_python_source, encode_python_source, Source};

/// Keyword-only parameters cannot change the positional argument limit.
///
/// # Panics
///
/// Panics if the generated input violates the asserted invariant.
pub fn signatures(kinds: &[u8], fullname: &str) {
    let mut signature = Signature {
        parameters: kinds
            .iter()
            .take(32)
            .enumerate()
            .map(|(index, kind)| Parameter {
                name: Some(if index == 0 {
                    "self".to_owned()
                } else {
                    format!("argument{index}")
                }),
                kind: match kind % 5 {
                    0 => ParameterKind::PositionalOnly,
                    1 => ParameterKind::PositionalOrKeyword,
                    2 => ParameterKind::VarPositional,
                    3 => ParameterKind::KeywordOnly,
                    _ => ParameterKind::VarKeyword,
                },
            })
            .collect(),
    };
    if fullname.ends_with(".__get__") || fullname.ends_with(".__set__") {
        // Descriptor protocol signatures have a receiver and an instance slot.
        while signature.parameters.len() < 2 {
            signature.parameters.push(Parameter {
                name: Some(
                    if signature.parameters.is_empty() {
                        "self"
                    } else {
                        "instance"
                    }
                    .to_owned(),
                ),
                kind: ParameterKind::PositionalOrKeyword,
            });
        }
    }
    assert_eq!(signature.max_positional_at_call_site(fullname, true), None);
    let limit = signature.max_positional_at_call_site(fullname, false);
    assert!(limit.is_some_and(|limit| limit <= signature.parameters.len()));
    signature.parameters.push(Parameter {
        name: Some("option".to_owned()),
        kind: ParameterKind::KeywordOnly,
    });
    assert_eq!(
        signature.max_positional_at_call_site(fullname, false),
        limit
    );
}

/// Re-encoding an unchanged decodable file must retain all its original bytes.
///
/// # Panics
///
/// Panics if the generated input violates the asserted invariant.
pub fn source_encoding(bytes: &[u8]) {
    if let Source::Decoded(source) = decode_python_source(bytes) {
        assert_eq!(encode_python_source(bytes, &source), Ok(bytes.to_vec()));
    }
}

/// Insertions at valid byte boundaries must preserve the surrounding text.
///
/// # Panics
///
/// Panics if the generated input violates the asserted invariant.
pub fn source_insertions(source: &str, insertions: &[(u16, String)]) {
    let boundaries: Vec<_> = source
        .char_indices()
        .map(|(index, _)| index)
        .chain([source.len()])
        .collect();
    let insertions: BTreeMap<_, _> = insertions
        .iter()
        .take(32)
        .map(|(offset, text)| {
            (
                boundaries[usize::from(*offset) % boundaries.len()],
                text.clone(),
            )
        })
        .collect();
    let mut expected = String::new();
    for (offset, character) in source.char_indices() {
        if let Some(text) = insertions.get(&offset) {
            expected.push_str(text);
        }
        expected.push(character);
    }
    if let Some(text) = insertions.get(&source.len()) {
        expected.push_str(text);
    }
    let edits: Vec<_> = insertions
        .into_iter()
        .map(|(at, text)| Insertion { at, text })
        .collect();
    assert_eq!(apply_insertions(source, &edits), expected);
}
