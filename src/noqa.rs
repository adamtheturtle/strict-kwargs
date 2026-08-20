//! Ruff-style `# noqa` suppression of `KW001` diagnostics (issue #185).
//!
//! A line-level `# noqa` comment suppresses diagnostics reported on that line.
//! Two forms are honored, matching Ruff:
//!
//! - Bare `# noqa` suppresses every diagnostic on the line.
//! - `# noqa: KW001` (optionally with more comma/whitespace-separated codes)
//!   suppresses only the listed codes; a directive that names other codes
//!   leaves `KW001` reported.
//!
//! The directive must live on the line the diagnostic points at — the first
//! line of the offending call — which is where the `path:line:col` output
//! tells the user to look.

use ruff_python_ast::token::Tokens;
use ruff_text_size::{Ranged, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast_util::line_column_from_starts;

/// What a parsed `# noqa` directive suppresses on its line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Directive {
    /// Bare `# noqa`: every code on the line.
    All,
    /// `# noqa: KW001, ...`: only the listed codes.
    Codes(Vec<String>),
}

/// One line's `# noqa` directive and where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LineDirective {
    directive: Directive,
    /// Byte offset of the `#` that introduces the directive, used to report
    /// an unused directive at the comment rather than at the code before it.
    offset: usize,
}

/// Per-file map of line number to its `# noqa` directive.
#[derive(Debug, Default)]
pub struct NoqaDirectives {
    /// 1-based line number to directive. Empty when the file has no `# noqa`
    /// comments — the common case, which short-circuits every query.
    by_line: FxHashMap<usize, LineDirective>,
    /// Byte offset of the start of each line, used to map a call offset to its
    /// line without rescanning the source per call. Only populated when at
    /// least one directive exists.
    line_starts: Vec<usize>,
}

impl NoqaDirectives {
    /// Scan `source`'s comment tokens for `# noqa` directives.
    ///
    /// Only real comment tokens are inspected, so a `# noqa` appearing inside a
    /// string literal is never mistaken for a directive.
    pub fn from_source(source: &str, tokens: &Tokens) -> Self {
        let mut directives: Vec<(usize, Directive)> = Vec::new();
        for token in tokens {
            if !token.kind().is_comment() {
                continue;
            }
            let comment_start = token.range().start().to_usize();
            if let Some((hash, directive)) = parse_directive(&source[token.range()]) {
                directives.push((comment_start + hash, directive));
            }
        }
        if directives.is_empty() {
            return Self::default();
        }
        let line_starts = line_starts(source);
        let mut by_line = FxHashMap::default();
        for (offset, directive) in directives {
            by_line.insert(
                line_for_offset(&line_starts, offset),
                LineDirective { directive, offset },
            );
        }
        Self {
            by_line,
            line_starts,
        }
    }

    /// Whether a `code` diagnostic reported at `offset` (the start of the
    /// offending call) is suppressed by a `# noqa` on that line.
    pub fn suppresses(&self, offset: usize, code: &str) -> bool {
        if self.by_line.is_empty() {
            return false;
        }
        let line = line_for_offset(&self.line_starts, offset);
        match self.by_line.get(&line).map(|entry| &entry.directive) {
            Some(Directive::All) => true,
            Some(Directive::Codes(codes)) => codes.iter().any(|candidate| candidate == code),
            None => false,
        }
    }

    /// The 1-based lines whose directive suppresses `code`.
    pub fn suppressed_lines(&self, code: &str) -> FxHashSet<usize> {
        self.by_line
            .iter()
            .filter(|(_, entry)| match &entry.directive {
                Directive::All => true,
                Directive::Codes(codes) => codes.iter().any(|candidate| candidate == code),
            })
            .map(|(line, _)| *line)
            .collect()
    }

    /// Reportable `(line, column)` positions of the directives that name
    /// `code` explicitly, sorted by line.
    ///
    /// Blanket `# noqa` directives are deliberately excluded: this tool sees
    /// only its own rule, so a bare directive on a line with no `KW001`
    /// violation may still be suppressing another tool's finding. Only a
    /// directive that names `code` itself can be proven redundant here.
    pub fn explicit_code_directive_positions(
        &self,
        source: &str,
        code: &str,
    ) -> Vec<(usize, usize)> {
        let mut positions: Vec<(usize, usize)> = self
            .by_line
            .values()
            .filter(|entry| match &entry.directive {
                Directive::All => false,
                Directive::Codes(codes) => codes.iter().any(|candidate| candidate == code),
            })
            .map(|entry| {
                let offset = u32::try_from(entry.offset).unwrap_or(u32::MAX);
                line_column_from_starts(source, &self.line_starts, TextSize::new(offset))
            })
            .collect();
        positions.sort_unstable();
        positions
    }
}

/// Parse the directive carried by a single comment, with the offset of the
/// `#` that introduces it within `comment`, or `None` if it is not a `# noqa`
/// comment.
///
/// The directive may follow other comment text introduced by a later `#`
/// (e.g. `# explanation # noqa: KW001`), matching Ruff, so every `#`-led run is
/// tried in turn.
fn parse_directive(comment: &str) -> Option<(usize, Directive)> {
    comment.match_indices('#').find_map(|(hash, _)| {
        parse_directive_after_hash(&comment[hash + 1..]).map(|directive| (hash, directive))
    })
}

/// Parse a `noqa` directive from the text immediately following a `#`.
fn parse_directive_after_hash(after_hash: &str) -> Option<Directive> {
    let body = after_hash.trim_start();
    let rest = strip_ascii_ci_prefix(body, "noqa")?;
    match rest.chars().next() {
        // `# noqa` at end of comment: blanket directive.
        None => Some(Directive::All),
        // `# noqa: KW001`.
        Some(':') => Some(codes_directive(&rest[1..])),
        // `# noqa <anything>` / `# noqa : KW001`: a space ends the `noqa`
        // keyword, so anything after it is either a blanket explanation or a
        // colon-led code list.
        Some(separator) if separator.is_whitespace() => match rest.trim_start().strip_prefix(':') {
            Some(after_colon) => Some(codes_directive(after_colon)),
            None => Some(Directive::All),
        },
        // `# noqab` / `# noqa_foo`: not a directive.
        Some(_) => None,
    }
}

/// Build a directive from the text after the `noqa:` colon. A colon with no
/// parseable codes behaves like a bare blanket directive, matching Ruff.
fn codes_directive(after_colon: &str) -> Directive {
    let codes = parse_codes(after_colon);
    if codes.is_empty() {
        Directive::All
    } else {
        Directive::Codes(codes)
    }
}

/// Collect rule codes from a comma/whitespace-separated list, stopping at the
/// first token that is not a rule code (e.g. a trailing explanation), matching
/// Ruff's code-list parsing.
fn parse_codes(text: &str) -> Vec<String> {
    let mut codes = Vec::new();
    for segment in text.split(|c: char| c == ',' || c.is_whitespace()) {
        if segment.is_empty() {
            continue;
        }
        if is_rule_code(segment) {
            codes.push(segment.to_string());
        } else {
            break;
        }
    }
    codes
}

/// Whether `segment` looks like a rule code: a letter-led, all-alphanumeric
/// token containing at least one digit (e.g. `KW001`).
fn is_rule_code(segment: &str) -> bool {
    segment
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && segment.chars().any(|c| c.is_ascii_digit())
        && segment.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Strip a case-insensitive ASCII `prefix` from `text`, returning the rest.
fn strip_ascii_ci_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// Byte offset of the start of every line in `source` (line 1 starts at 0),
/// using the same universal-newline rule as the rest of the tool so a `# noqa`
/// on a `\r`-delimited line maps to the correct physical line (issue #270).
use crate::ast_util::line_starts;

/// 1-based line containing `offset`, matching [`crate::ast_util::line_column`].
fn line_for_offset(line_starts: &[usize], offset: usize) -> usize {
    line_starts.partition_point(|&start| start <= offset)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use ruff_python_parser::parse_module;

    use super::*;

    fn directives(source: &str) -> NoqaDirectives {
        let parsed = parse_module(source).expect("valid module");
        NoqaDirectives::from_source(source, parsed.tokens())
    }

    #[test]
    fn no_comments_suppresses_nothing() {
        let source = "f(1, 2, 3)\n";
        let noqa = directives(source);
        assert!(!noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn bare_noqa_suppresses_kw001() {
        let source = "f(1, 2, 3)  # noqa\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn coded_noqa_suppresses_named_code_only() {
        let source = "f(1, 2, 3)  # noqa: KW001\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
        assert!(!noqa.suppresses(0, "E501"));
    }

    #[test]
    fn coded_noqa_for_other_code_does_not_suppress() {
        let source = "f(1, 2, 3)  # noqa: E501\n";
        let noqa = directives(source);
        assert!(!noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn multiple_codes_are_each_honored() {
        let source = "f(1, 2, 3)  # noqa: E501, KW001\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
        assert!(noqa.suppresses(0, "E501"));
        assert!(!noqa.suppresses(0, "F401"));
    }

    #[test]
    fn directive_only_applies_to_its_own_line() {
        let source = "f(1, 2, 3)\ng(1, 2, 3)  # noqa: KW001\n";
        let noqa = directives(source);
        // Offset 0 is line 1 (no directive); the call on line 2 starts after
        // the first newline.
        let line_two_start = source.find('g').expect("call on line 2");
        assert!(!noqa.suppresses(0, "KW001"));
        assert!(noqa.suppresses(line_two_start, "KW001"));
    }

    #[test]
    fn noqa_colon_without_codes_is_blanket() {
        let source = "f(1, 2, 3)  # noqa:\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn noqa_with_trailing_explanation_is_blanket() {
        let source = "f(1, 2, 3)  # noqa keep positional for clarity\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn noqa_space_before_colon_parses_codes() {
        let source = "f(1, 2, 3)  # noqa : KW001\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
        assert!(!noqa.suppresses(0, "E501"));
    }

    #[test]
    fn coded_noqa_stops_at_non_code_explanation() {
        let source = "f(1, 2, 3)  # noqa: KW001 keep this positional\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn noqa_in_string_literal_is_not_a_directive() {
        let source = "x = \"# noqa: KW001\"\nf(1, 2, 3)\n";
        let noqa = directives(source);
        let call_offset = source.find('f').expect("call");
        assert!(!noqa.suppresses(call_offset, "KW001"));
    }

    #[test]
    fn noqa_after_other_comment_text_is_a_directive() {
        let source = "f(1, 2, 3)  # explanation # noqa: KW001\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn double_hash_noqa_is_a_directive() {
        let source = "f(1, 2, 3)  ## noqa: KW001\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn noqa_is_case_insensitive() {
        let source = "f(1, 2, 3)  # NoQA: KW001\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn noqa_prefix_of_longer_word_is_not_a_directive() {
        let source = "f(1, 2, 3)  # noqable thoughts\n";
        let noqa = directives(source);
        assert!(!noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn unrelated_comment_is_not_a_directive() {
        let source = "f(1, 2, 3)  # just a note\n";
        let noqa = directives(source);
        assert!(!noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn empty_comment_is_not_a_directive() {
        let source = "f(1, 2, 3)  #\n";
        let noqa = directives(source);
        assert!(!noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn malformed_codes_fall_back_to_blanket() {
        // A leading separator yields an empty segment (skipped), then a
        // non-code token stops parsing, leaving no codes — a blanket directive.
        let source = "f(1, 2, 3)  # noqa: , because\n";
        let noqa = directives(source);
        assert!(noqa.suppresses(0, "KW001"));
    }

    #[test]
    fn explicit_code_directive_positions_lists_only_coded_directives() {
        let source = concat!(
            "f(1, 2, 3)  # noqa: KW001\n",
            "g(1, 2, 3)  # noqa\n",
            "h(1, 2, 3)  # noqa: E501\n",
            "i(1, 2, 3)\n",
            "j(1, 2, 3)  # explanation # noqa: E501, KW001\n",
        );
        let noqa = directives(source);
        // Columns point at the `#` that introduces the directive itself, not
        // at the start of the comment: on line 5 that is the second `#`.
        assert_eq!(
            noqa.explicit_code_directive_positions(source, "KW001"),
            vec![(1, 13), (5, 27)]
        );
    }

    #[test]
    fn explicit_code_directive_positions_are_empty_without_comments() {
        let source = "f(1, 2, 3)\n";
        assert_eq!(
            directives(source)
                .explicit_code_directive_positions(source, "KW001")
                .len(),
            0
        );
    }

    #[test]
    fn suppressed_lines_covers_blanket_and_coded_directives() {
        let source = concat!(
            "f(1, 2, 3)\n",
            "g(1, 2, 3)  # noqa: KW001\n",
            "h(1, 2, 3)  # noqa\n",
            "i(1, 2, 3)  # noqa: E501\n",
        );
        let noqa = directives(source);
        let mut lines: Vec<usize> = noqa.suppressed_lines("KW001").into_iter().collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![2, 3]);
    }

    #[test]
    fn digit_led_or_punctuated_tokens_are_not_codes() {
        assert!(!is_rule_code("001KW"));
        assert!(!is_rule_code("KW-1"));
        assert!(!is_rule_code("ABCDEF"));
        assert!(is_rule_code("KW001"));
    }
}
