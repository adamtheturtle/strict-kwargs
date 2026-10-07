use proptest::prelude::*;

use super::{Parameter, ParameterKind, Signature};

fn parameters(positional_only: usize, keyword_capable: usize, variadic: bool) -> Vec<Parameter> {
    let mut parameters = Vec::new();
    parameters.extend((0..positional_only).map(|index| Parameter {
        name: Some(format!("positional{index}")),
        kind: ParameterKind::PositionalOnly,
    }));
    parameters.extend((0..keyword_capable).map(|index| Parameter {
        name: Some(format!("keyword{index}")),
        kind: ParameterKind::PositionalOrKeyword,
    }));
    if variadic {
        parameters.push(Parameter {
            name: Some("args".to_owned()),
            kind: ParameterKind::VarPositional,
        });
    }
    parameters
}

proptest! {
    #[test]
    fn plain_signatures_count_only_the_arguments_that_require_positionals(
        positional_only in 0usize..24,
        keyword_capable in 0usize..24,
        variadic in any::<bool>(),
    ) {
        let signature = Signature {
            parameters: parameters(positional_only, keyword_capable, variadic),
        };
        let expected = positional_only + if variadic { keyword_capable } else { 0 };
        prop_assert_eq!(signature.max_positional_at_call_site("module.function", false), Some(expected));
        prop_assert_eq!(signature.max_positional_at_call_site("module.function", true), None);
        let mut with_keywords = signature;
        with_keywords.parameters.extend([
            Parameter { name: Some("option".to_owned()), kind: ParameterKind::KeywordOnly },
            Parameter { name: Some("kwargs".to_owned()), kind: ParameterKind::VarKeyword },
        ]);
        prop_assert_eq!(with_keywords.max_positional_at_call_site("module.function", false), Some(expected));
    }

    #[test]
    fn implicit_receivers_do_not_change_the_user_argument_limit(
        positional_only in 0usize..24,
        keyword_capable in 0usize..24,
        variadic in any::<bool>(),
        receiver_positional_only in any::<bool>(),
    ) {
        let mut parameters = parameters(positional_only, keyword_capable, variadic);
        parameters.insert(0, Parameter {
            name: Some("self".to_owned()),
            kind: if receiver_positional_only || positional_only > 0 {
                ParameterKind::PositionalOnly
            } else {
                ParameterKind::PositionalOrKeyword
            },
        });
        let signature = Signature { parameters };
        let expected = positional_only + if variadic { keyword_capable } else { 0 };
        for fullname in ["C.__call__", "C.__init__", "C.__new__"] {
            prop_assert_eq!(signature.max_positional_at_call_site(fullname, false), Some(expected));
        }
    }

    #[test]
    fn descriptors_allow_the_instance_but_cached_properties_require_its_keyword(
        keyword_capable in 0usize..24,
        variadic in any::<bool>(),
    ) {
        let mut parameters = parameters(0, keyword_capable, variadic);
        parameters.splice(0..0, [
            Parameter { name: Some("self".to_owned()), kind: ParameterKind::PositionalOrKeyword },
            Parameter { name: Some("instance".to_owned()), kind: ParameterKind::PositionalOrKeyword },
        ]);
        let signature = Signature { parameters };
        let rest = if variadic { keyword_capable } else { 0 };
        for fullname in ["C.__get__", "C.__set__"] {
            prop_assert_eq!(signature.max_positional_at_call_site(fullname, false), Some(1 + rest));
        }
        let cached_limit = if variadic { 1 + keyword_capable } else { 0 };
        prop_assert_eq!(signature.max_positional_at_call_site("functools.cached_property.__get__", false), Some(cached_limit));
        let bound = Signature { parameters: signature.parameters[1..].to_vec() };
        prop_assert_eq!(bound.max_positional_at_call_site("C.__get__", false), Some(cached_limit));
    }
}
