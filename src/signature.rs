//! Function signature model and "max positional arguments at call site" logic.
//!
//! Mirrors ``mypy_strict_kwargs.plugin._transform_signature`` behavior.

/// A parameter in a callable signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub name: Option<String>,
    pub kind: ParameterKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterKind {
    PositionalOnly,
    PositionalOrKeyword,
    VarPositional,
    KeywordOnly,
    VarKeyword,
}

/// Parsed callable signature (functions and methods).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub parameters: Vec<Parameter>,
}

impl Signature {
    /// Maximum number of positional arguments a call may pass (excluding ``self``).
    pub fn max_positional_at_call_site(&self, fullname: &str, ignored: bool) -> Option<usize> {
        if ignored {
            return None;
        }

        let first_star = self
            .parameters
            .iter()
            .position(|p| p.kind == ParameterKind::VarPositional);

        let skip_first = fullname.ends_with(".__call__")
            || fullname.ends_with(".__get__")
            || fullname.ends_with(".__set__")
            || fullname.ends_with(".__init__")
            || fullname.ends_with(".__new__");
        let skip_second = fullname.ends_with(".__get__") || fullname.ends_with(".__set__");

        let mut max = 0usize;
        for (index, param) in self.parameters.iter().enumerate() {
            if skip_first && index == 0 {
                continue;
            }
            if skip_second && index == 1 {
                max += 1;
                continue;
            }

            let allows_positional = match param.kind {
                ParameterKind::PositionalOnly => true,
                ParameterKind::VarPositional
                | ParameterKind::KeywordOnly
                | ParameterKind::VarKeyword => false,
                ParameterKind::PositionalOrKeyword => {
                    matches!(first_star, Some(star_index) if index <= star_index)
                }
            };

            if allows_positional {
                max += 1;
            }
            // `__call__` gets no first-user-argument exemption. The `@C()`
            // decorator-application form (`C()(func)` with `func` forced
            // positional) is never a checked call site, so it never needs one;
            // an *explicit* `c(func)` / `C()(1, 2)` can pass the argument by
            // keyword and must be flagged like any bound method (issue #28).
        }

        Some(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(params: &[(&str, ParameterKind)]) -> Signature {
        Signature {
            parameters: params
                .iter()
                .map(|(name, kind)| Parameter {
                    name: Some((*name).to_string()),
                    kind: *kind,
                })
                .collect(),
        }
    }

    #[test]
    fn plain_function_no_positionals() {
        let s = sig(&[("a", ParameterKind::PositionalOrKeyword)]);
        assert_eq!(s.max_positional_at_call_site("main.func", false), Some(0));
    }

    #[test]
    fn positional_only_allows_one() {
        let s = sig(&[
            ("a", ParameterKind::PositionalOnly),
            ("b", ParameterKind::PositionalOrKeyword),
        ]);
        assert_eq!(s.max_positional_at_call_site("main.func", false), Some(1));
    }

    #[test]
    fn before_var_positional() {
        let s = sig(&[
            ("a", ParameterKind::PositionalOrKeyword),
            ("args", ParameterKind::VarPositional),
        ]);
        assert_eq!(s.max_positional_at_call_site("main.func", false), Some(1));
    }

    #[test]
    fn dunder_call_strips_self_with_no_first_arg_exemption() {
        // `self` is bound by the receiver; the remaining params can be passed
        // by keyword at an explicit call site, so none count (issue #28).
        let s = sig(&[
            ("self", ParameterKind::PositionalOrKeyword),
            ("func", ParameterKind::PositionalOrKeyword),
            ("a", ParameterKind::PositionalOrKeyword),
        ]);
        assert_eq!(
            s.max_positional_at_call_site("main.C.__call__", false),
            Some(0)
        );
    }

    #[test]
    fn method_excludes_self_from_count() {
        let s = sig(&[
            ("self", ParameterKind::PositionalOrKeyword),
            ("a", ParameterKind::PositionalOrKeyword),
        ]);
        assert_eq!(
            s.max_positional_at_call_site("main.C.method", false),
            Some(0)
        );
    }

    #[test]
    fn ignored_callable_has_no_limit() {
        let s = sig(&[("a", ParameterKind::PositionalOrKeyword)]);
        assert_eq!(s.max_positional_at_call_site("main.func", true), None);
    }

    #[test]
    fn init_and_new_skip_self_only() {
        let s = sig(&[
            ("self", ParameterKind::PositionalOrKeyword),
            ("a", ParameterKind::PositionalOnly),
        ]);
        assert_eq!(
            s.max_positional_at_call_site("main.C.__init__", false),
            Some(1)
        );
        assert_eq!(
            s.max_positional_at_call_site("main.C.__new__", false),
            Some(1)
        );
    }

    #[test]
    fn descriptor_set_skips_self_and_counts_instance() {
        let s = sig(&[
            ("self", ParameterKind::PositionalOrKeyword),
            ("instance", ParameterKind::PositionalOrKeyword),
            ("value", ParameterKind::PositionalOrKeyword),
        ]);
        assert_eq!(
            s.max_positional_at_call_site("main.D.__set__", false),
            Some(1)
        );
    }

    #[test]
    fn positional_or_keyword_after_var_positional_is_keyword_only() {
        // ``def f(a, *args, b)``: ``b`` follows ``*args`` so it cannot be
        // passed positionally (exercises the `index <= star_index` guard's
        // false arm).
        let s = sig(&[
            ("a", ParameterKind::PositionalOrKeyword),
            ("args", ParameterKind::VarPositional),
            ("b", ParameterKind::PositionalOrKeyword),
        ]);
        assert_eq!(s.max_positional_at_call_site("main.f", false), Some(1));
    }

    #[test]
    fn descriptor_get_skips_self_and_counts_instance() {
        // ``__get__(self, instance, owner)``: ``self`` is skipped and the
        // second parameter (``instance``) is always allowed positionally.
        let s = sig(&[
            ("self", ParameterKind::PositionalOrKeyword),
            ("instance", ParameterKind::PositionalOrKeyword),
            ("owner", ParameterKind::PositionalOrKeyword),
        ]);
        assert_eq!(
            s.max_positional_at_call_site("main.D.__get__", false),
            Some(1)
        );
    }
}
