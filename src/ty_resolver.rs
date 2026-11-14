//! Resolve call targets through the bundled ty inference engine.

use std::path::Path;

mod engine;
pub use engine::TyResolver;
pub use ty_backend::{Definition as DefLocation, Session as TySession};

/// Parse a ty hover into a callable signature description.
pub struct HoverSignature {
    /// `def NAME(` / `bound method T.NAME(` — the callee's display name.
    pub name: String,
    /// Owning type for a bound method (`list[int]` etc.), if any.
    pub owner: Option<String>,
    /// The parameter-list text between the outermost parentheses.
    pub params: String,
}

/// Extract a signature from ty hover text. Handles `def name(params) -> ret`,
/// `class Name(params)` (a constructor; ty renders the `__init__`/`__new__`
/// parameters with `self` already omitted), and `bound method Owner.name(params)
/// -> ret`, including multi-line params, and stops at the `---` docstring
/// separator. Returns `None` for plain types (`<class 'A'>`, `list[int]`) —
/// the caller falls back to goto-def.
///
/// Parsing the `class Name(...)` shape directly matters because ty's
/// goto-definition for a re-exported stdlib class resolves into the runtime
/// `.py` shim and lands on the `from … import …` statement (not the class),
/// so the goto-def fallback silently drops the diagnostic in environments
/// where ty resolves against a runtime interpreter rather than its vendored
/// typeshed stubs (issue #195). The hover carries the constructor signature
/// consistently regardless of which the environment ty discovers.
pub fn parse_hover_signature(value: &str) -> Option<HoverSignature> {
    let head = value.split("\n---").next().unwrap_or(value);
    let head = head.trim();

    let (name, owner) = if let Some(rest) = head.strip_prefix("def ") {
        let name = rest.split('(').next()?.trim().to_string();
        (name, None)
    } else if let Some(rest) = head.strip_prefix("class ") {
        // A constructor: ty already omits `self`, so the parameter list maps
        // to the call site exactly like an unbound `def`.
        let name = rest.split('(').next()?.trim().to_string();
        (name, None)
    } else {
        let rest = head.strip_prefix("bound method ")?;
        let qualified = rest.split('(').next()?.trim();
        let (owner, name) = qualified.rsplit_once('.')?;
        (name.to_string(), Some(owner.to_string()))
    };
    if name.is_empty() {
        return None;
    }

    // Balanced extraction of the parameter list.
    let open = head.find('(')?;
    let mut depth = 0i32;
    let mut end = None;
    for (i, ch) in head[open..].char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let params = head[open + 1..end?].trim().to_string();
    Some(HoverSignature {
        name,
        owner,
        params,
    })
}

/// Split `s` on top-level `sep` (bracket/paren/brace depth 0 only).
pub fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

/// If `s` begins with `(` whose matching `)` is the final char, return the
/// inside; else `None`.
///
/// Part of the ty-wire parsing layer: in production these helpers are
/// reached only through the (excluded) `resolve_pending_with_ty` glue,
/// where real `ty` output never exercises their malformed-input branches.
/// Their behaviour — including those edge branches — is verified by the
/// `#[coverage(off)]` unit tests below, so they are excluded from the gate
/// for the same reason as the rest of the ty glue.
#[cfg_attr(coverage, coverage(off))]
fn unwrap_enclosing_parens(s: &str) -> Option<&str> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return if i == s.len() - 1 {
                        Some(&s[1..i])
                    } else {
                        None
                    };
                }
            }
            _ => {}
        }
    }
    None
}

/// Balanced leading `(...)` group of `s`, if it is immediately followed
/// (modulo spaces) by `->`. Returns the inside (the parameter-list text).
fn leading_callable_params(s: &str) -> Option<&str> {
    let s = s.trim();
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    let rest = s[i + 1..].trim_start();
                    return rest.starts_with("->").then(|| s[1..i].trim());
                }
            }
            _ => {}
        }
    }
    None
}

/// Like [`leading_callable_params`], but for an entry whose parameter list is
/// preceded by a callable's display name — `bound method Owner.name(p) -> r`
/// or `def name(p) -> r`. ty renders a single *resolved* signature this way
/// (rather than as an `Overload[…]` callable type) once it narrows an inferred
/// receiver: as of ty 0.0.42 `sys.stdout.write` hovers as
/// `(bound method TextIO.write(s: str, /) -> int) | Any`, where 0.0.40 used
/// `(Overload[(s: Buffer, /) -> int, (s: str, /) -> int]) | Any`. The `(p)`
/// group still carries the positional-only `/`, so strip the name and read it
/// directly, avoiding the goto-definition fallback that drops `/` (issue #14).
///
/// Ty-wire parsing layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (unit-tested, production-reached only via the
/// excluded ty glue).
#[cfg_attr(coverage, coverage(off))]
fn named_callable_params(s: &str) -> Option<&str> {
    let rest = s
        .trim()
        .strip_prefix("bound method ")
        .or_else(|| s.trim().strip_prefix("def "))?;
    // The (possibly dotted) name runs up to the first `(`, with no spaces —
    // anything else is not a `name(params)` head.
    let open = rest.find('(')?;
    let name = rest[..open].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    leading_callable_params(&rest[open..])
}

/// Parse ty hover text that is a *callable type* (not a bare `def`/`bound
/// method` display): `(p) -> r`, `(Overload[(p1) -> r1, (p2) -> r2]) | Any`,
/// or a single resolved signature rendered as `(bound method Owner.name(p) ->
/// r) | Any` / `(def name(p) -> r) | Any`, optionally wrapped in a top-level
/// union. Returns one parameter-list string per overload — `self` is already
/// excluded, as ty renders bound-method types without it. Crucially this
/// preserves typeshed positional-only `/` markers, which the goto-definition
/// fallback loses when it lands on runtime stdlib `.py` source (see issue #14).
///
/// Ty-wire parsing layer; excluded for the reason given on
/// [`unwrap_enclosing_parens`] (unit-tested, production-reached only via the
/// excluded ty glue).
#[cfg_attr(coverage, coverage(off))]
pub fn parse_callable_type_overloads(value: &str) -> Vec<String> {
    let head = value.split("\n---").next().unwrap_or(value).trim();

    // Pick the callable arm of a top-level union (drop `Any`, `None`, …).
    let Some(callable) = split_top_level(head, '|')
        .into_iter()
        .map(str::trim)
        .find(|s| s.starts_with("Overload[") || (s.starts_with('(') && s.contains("->")))
    else {
        return Vec::new();
    };

    // `(Overload[…])` / `(… ) -> …` may be wrapped in one enclosing paren.
    let callable = match unwrap_enclosing_parens(callable) {
        Some(inner) if leading_callable_params(callable).is_none() => inner,
        _ => callable,
    };

    let entries: Vec<&str> = if let Some(inner) = callable
        .strip_prefix("Overload[")
        .and_then(|s| s.strip_suffix(']'))
    {
        split_top_level(inner, ',')
    } else {
        vec![callable]
    };

    entries
        .into_iter()
        .filter_map(|e| {
            leading_callable_params(e)
                .or_else(|| named_callable_params(e))
                .map(str::to_string)
        })
        .collect()
}

/// Whether two paths denote the same file, allowing native or forward-slash
/// separators when comparing indexed and inference paths on Windows.
pub fn same_path(a: &Path, b: &Path) -> bool {
    a == b || a.to_string_lossy().replace('\\', "/") == b.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn callable_type_overloads_parses_overload_union() {
        // The exact ty hover for `sys.stdout.write` (issue #14): the `/`
        // positional-only markers must survive so the call is not flagged.
        assert_eq!(
            parse_callable_type_overloads(
                "(Overload[(s: Buffer, /) -> int, (s: str, /) -> int]) | Any"
            ),
            vec!["s: Buffer, /".to_string(), "s: str, /".to_string()],
        );
    }

    #[test]
    fn callable_type_overloads_parses_wrapped_bound_method() {
        // ty 0.0.42 renders `sys.stdout.write` as a single resolved bound
        // method wrapped in `| Any` (0.0.40 used `Overload[…]`). The
        // positional-only `/` must still be recovered from the hover so the
        // call is not flagged — otherwise we fall through to goto-definition,
        // which drops `/` on the inferred stdlib receiver (issue #14 redux).
        assert_eq!(
            parse_callable_type_overloads("(bound method TextIO.write(s: str, /) -> int) | Any"),
            vec!["s: str, /".to_string()],
        );
        // The same wrapping around a free-function `def` display.
        assert_eq!(
            parse_callable_type_overloads("(def f(a: int, b: str) -> None) | Any"),
            vec!["a: int, b: str".to_string()],
        );
        // A `def`/`bound method` head with an empty name is not a signature
        // and yields nothing (no crash).
        assert_eq!(
            parse_callable_type_overloads("(def () -> int) | Any").len(),
            0
        );
    }

    #[test]
    fn callable_type_overloads_single_and_bare_overload() {
        assert_eq!(
            parse_callable_type_overloads("(x: int) -> str"),
            vec!["x: int".to_string()],
        );
        assert_eq!(
            parse_callable_type_overloads("Overload[(a: int, /) -> int, (a: str, /) -> str]"),
            vec!["a: int, /".to_string(), "a: str, /".to_string()],
        );
        // Union in the return type, not the params.
        assert_eq!(
            parse_callable_type_overloads("(x: int) -> int | None"),
            vec!["x: int".to_string()],
        );
    }

    #[test]
    fn callable_type_overloads_keeps_callable_typed_param_intact() {
        // A callable-typed parameter must not be mistaken for a second
        // overload — only the leading `(...) ->` group is the signature.
        assert_eq!(
            parse_callable_type_overloads("(cb: (int) -> str, /) -> None"),
            vec!["cb: (int) -> str, /".to_string()],
        );
    }

    #[test]
    fn callable_type_overloads_rejects_non_callables() {
        assert_eq!(parse_callable_type_overloads("<class 'C'>").len(), 0);
        assert_eq!(
            parse_callable_type_overloads("<method-wrapper 'startswith' of string 'abc'>").len(),
            0
        );
        assert_eq!(parse_callable_type_overloads("list[int]").len(), 0);
    }

    #[test]
    fn same_path_tolerates_separator_mismatch() {
        // Indexed and inference paths may use different separators on Windows.
        assert!(same_path(
            Path::new("C:/Users/a/x.py"),
            Path::new(r"C:\Users\a\x.py"),
        ));
        assert!(same_path(
            Path::new("/home/u/a.py"),
            Path::new("/home/u/a.py")
        ));
        assert!(!same_path(
            Path::new("/home/u/a.py"),
            Path::new("/home/u/b.py")
        ));
    }

    // ----- Pure parsing helpers -------------------------------

    #[test]
    fn parse_hover_signature_def_and_bound_method() {
        let def = parse_hover_signature("def f(a: int, b: str) -> None").unwrap();
        assert_eq!(def.name, "f");
        assert!(def.owner.is_none());
        assert_eq!(def.params, "a: int, b: str");

        let bound = parse_hover_signature("bound method list[int].append(x: int) -> None").unwrap();
        assert_eq!(bound.name, "append");
        assert_eq!(bound.owner.as_deref(), Some("list[int]"));
        assert_eq!(bound.params, "x: int");

        // Docstring separator is stripped before parsing.
        let doc = parse_hover_signature("def g(a: int) -> int\n---\nDocs here").unwrap();
        assert_eq!(doc.params, "a: int");

        // Nested brackets in the parameter list keep `depth != 0` until the
        // outermost `)` (exercises the balance loop's non-terminating arm).
        let nested = parse_hover_signature("def h(a: dict[str, list[int]]) -> None").unwrap();
        assert_eq!(nested.params, "a: dict[str, list[int]]");
    }

    #[test]
    fn parse_hover_signature_class_constructor() {
        // ty renders a constructor hover as `class Name(params)` with `self`
        // omitted; it parses like an unbound `def` (issue #195).
        let single = parse_hover_signature("class FileReader(loader: FileLoader)").unwrap();
        assert_eq!(single.name, "FileReader");
        assert!(single.owner.is_none());
        assert_eq!(single.params, "loader: FileLoader");

        // Multi-line constructor hover with a docstring tail, as ty emits for
        // `BytesGenerator`.
        let multiline = parse_hover_signature(
            "class BytesGenerator(\n    \
             outfp: SupportsWrite[bytes],\n    \
             *,\n    \
             policy: Policy[_MessageT]\n\
             )\n---\nCreate the generator.",
        )
        .unwrap();
        assert_eq!(multiline.name, "BytesGenerator");
        assert!(multiline.owner.is_none());
        assert!(multiline.params.contains("outfp: SupportsWrite[bytes]"));
        assert!(multiline.params.contains("policy: Policy[_MessageT]"));

        // A bare class type display (no constructor parentheses) is not a
        // signature and still falls back to goto-def.
        assert!(parse_hover_signature("<class 'C'>").is_none());
    }

    #[test]
    fn parse_hover_signature_rejects_non_signatures() {
        // Neither `def ` nor `bound method `.
        assert!(parse_hover_signature("<class 'C'>").is_none());
        // `bound method ` with no `.` to split owner/name.
        assert!(parse_hover_signature("bound method noselfdot(a) -> None").is_none());
        // Empty callee name.
        assert!(parse_hover_signature("def (a) -> None").is_none());
    }

    #[test]
    fn split_top_level_respects_bracket_depth() {
        assert_eq!(split_top_level("a, b, c", ','), vec!["a", " b", " c"]);
        // Commas inside brackets are not split points.
        assert_eq!(
            split_top_level("a, f(x, y), b[1, 2]", ','),
            vec!["a", " f(x, y)", " b[1, 2]"]
        );
    }

    #[test]
    fn unwrap_enclosing_parens_cases() {
        assert_eq!(unwrap_enclosing_parens("(abc)"), Some("abc"));
        // Does not start with `(`.
        assert_eq!(unwrap_enclosing_parens("abc"), None);
        // First group closes before the end => not enclosing.
        assert_eq!(unwrap_enclosing_parens("(a)b"), None);
        // Never balances.
        assert_eq!(unwrap_enclosing_parens("(a"), None);
    }

    #[test]
    fn leading_callable_params_cases() {
        assert_eq!(leading_callable_params("(x: int) -> str"), Some("x: int"));
        // Not starting with `(`.
        assert_eq!(leading_callable_params("x: int"), None);
        // Balanced group not followed by `->`.
        assert_eq!(leading_callable_params("(x: int) : str"), None);
        // Never balances.
        assert_eq!(leading_callable_params("(x: int"), None);
    }

    #[test]
    fn parse_callable_type_overloads_edges() {
        // Enclosing parens whose inside is itself a direct `(...) -> ...`
        // keeps the wrapper (the guard's false arm).
        assert_eq!(
            parse_callable_type_overloads("((x: int) -> str)"),
            vec!["x: int".to_string()]
        );
        // `Overload[` without a closing `]` is treated as a single entry.
        assert_eq!(parse_callable_type_overloads("Overload[(a) -> b").len(), 0);
        // No callable arm in the union at all.
        assert_eq!(parse_callable_type_overloads("None | int").len(), 0);
    }
}
