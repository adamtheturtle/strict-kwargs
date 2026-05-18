//! Robustness bounds for pathological input (issue #54).
//!
//! Ruff's recursive-descent parser, our AST [`Visitor`](ruff_python_ast::visitor::Visitor)
//! walk, and even dropping the resulting tree all recurse once per level of
//! expression nesting. The vendored `rustpython-ruff_python_parser` fork (unlike
//! upstream Ruff) enforces *no* parser recursion limit, so a deeply nested file
//! — machine-generated code, a giant data literal, or hostile input such as
//! `f(f(f(…f(1)…)))` — overflowed the stack and aborted the whole process with
//! `SIGABRT` (exit 134). A single such file in a directory or pre-commit run
//! took everything down.
//!
//! Two independent guards make that a bounded, graceful failure instead:
//!
//! 1. [`parse_module_guarded`] refuses to hand the recursive parser a file
//!    nested deeper than [`MAX_NESTING_DEPTH`], returning
//!    [`CheckError::TooDeeplyNested`] (exit 2) instead of crashing. It is
//!    two-stage: short files are admitted immediately, and otherwise a cheap
//!    byte count ([`open_bracket_bytes`]) is a sound upper bound on real
//!    nesting, so all but pathological files are admitted without tokenizing;
//!    only a file that count cannot clear pays the exact, *non-recursive*
//!    lexer scan ([`max_nesting_depth`], safe at any depth).
//! 2. [`run_with_large_stack`] runs the whole analysis on a thread with a
//!    large, explicit stack, so the depth that is *legitimately* handled is
//!    high and identical across build profiles and platforms — in particular
//!    it does not depend on the host's default stack (musl's is orders of
//!    magnitude smaller than glibc's), which would otherwise make the bound
//!    non-deterministic.

use rayon::ThreadPoolBuilder;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::ModModule;
use ruff_python_parser::{lexer::lex, parse_module, Mode, Parsed};

use crate::error::CheckError;

/// Maximum bracket-nesting depth a source file may reach before it is rejected
/// rather than parsed.
///
/// `1000` matches `CPython`'s own default recursion limit
/// (`sys.getrecursionlimit()`) and the depth the issue #54 report itself
/// considers fine — no human-written code, and essentially no sane
/// machine-generated code, nests parentheses/brackets/braces this deeply;
/// anything that does is almost certainly generated or hostile. The analysis
/// runs on a [`STACK_SIZE`]-byte stack, where 1000 levels of
/// parser + walk + drop recursion use a small fraction of the stack with a
/// wide margin on every supported platform, so this is a deterministic bound
/// rather than one that depends on the host stack or build profile.
pub const MAX_NESTING_DEPTH: usize = 1000;

/// Stack size for the analysis thread (see [`run_with_large_stack`]).
///
/// 256 MiB is reserved address space, not resident memory, so the cost is
/// negligible; it raises the parser/walk/drop recursion ceiling far above
/// [`MAX_NESTING_DEPTH`] regardless of the platform's (often tiny) default
/// thread/main stack.
pub const STACK_SIZE: usize = 256 * 1024 * 1024;

/// Number of `(`, `[` and `{` *bytes* in `source`.
///
/// A sound, cheap upper bound on the real maximum expression nesting depth:
/// every nesting level opens one of these brackets, so the real depth can
/// never exceed how many such bytes the file contains. Brackets inside string
/// or comment content only *inflate* this count — they never reduce real
/// nesting — so using it to *skip* the precise (lexer-based) scan is always
/// safe. A plain byte scan (these are ASCII, so multi-byte UTF-8 sequences
/// cannot collide) is far cheaper than tokenizing, which matters because this
/// runs on every checked file (issue #54 performance follow-up).
fn open_bracket_bytes(source: &str) -> usize {
    source
        .as_bytes()
        .iter()
        .filter(|&&byte| matches!(byte, b'(' | b'[' | b'{'))
        .count()
}

/// Maximum depth of nested `()`, `[]` and `{}` in `source`.
///
/// Driven by the lexer's `next_token`, which is an iterative state machine —
/// it never recurses on nesting (nested f-strings included), so this is safe
/// to run on any input, however pathological, before the recursive parser is
/// involved. Brackets inside string/comment content are part of a single
/// string/comment token and are correctly not counted; f-string interpolation
/// braces are real expression nesting and are.
fn max_nesting_depth(source: &str) -> usize {
    let mut lexer = lex(source, Mode::Module);
    let mut depth: usize = 0;
    let mut max: usize = 0;
    loop {
        match lexer.next_token() {
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => {
                depth += 1;
                if depth > max {
                    max = depth;
                }
            }
            TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => {
                depth = depth.saturating_sub(1);
            }
            TokenKind::EndOfFile => return max,
            _ => {}
        }
    }
}

/// Parse a module, refusing input nested deeper than [`MAX_NESTING_DEPTH`].
///
/// Two-stage so the common case stays cheap: a source shorter than the limit
/// cannot nest deeper than the limit, and [`open_bracket_bytes`] is a sound
/// upper bound on real nesting for longer sources. Files cleared by either
/// test skip the precise (full-tokenization) [`max_nesting_depth`] scan
/// entirely. Only a file with more than [`MAX_NESTING_DEPTH`] bracket bytes —
/// pathological, or a huge string of brackets — pays the exact scan, which
/// alone decides rejection (so a shallow file with many bracket bytes inside
/// string literals is *not* falsely rejected).
///
/// # Errors
///
/// [`CheckError::TooDeeplyNested`] if the source exceeds the nesting bound (so
/// the recursive parser is never reached), otherwise [`CheckError::Parse`] on
/// a syntax error.
pub fn parse_module_guarded(source: &str) -> Result<Parsed<ModModule>, CheckError> {
    if source.len() > MAX_NESTING_DEPTH && open_bracket_bytes(source) > MAX_NESTING_DEPTH {
        let depth = max_nesting_depth(source);
        if depth > MAX_NESTING_DEPTH {
            return Err(CheckError::TooDeeplyNested {
                depth,
                limit: MAX_NESTING_DEPTH,
            });
        }
    }
    Ok(parse_module(source)?)
}

/// Run `f` on a dedicated thread with a [`STACK_SIZE`]-byte stack and return
/// its result.
///
/// All of the analysis's unbounded-by-input recursion (the parser, the AST
/// walk, and dropping the AST) happens inside `f`; running it here decouples
/// the tolerated nesting depth from the caller's stack, which varies by
/// platform and build profile.
///
/// Excluded from the coverage gate: the happy path is exercised by every
/// integration test that calls [`check_paths`](crate::check_paths) /
/// [`fix_paths`](crate::fix_paths), but the thread-spawn-failure arm is
/// environment-only and the panic-propagation arm is reachable only by a
/// panicking `f` (a bug, not normal control flow) — consistent with the glue
/// code already excluded in `lib.rs`.
///
/// # Errors
///
/// Whatever `f` returns, or [`CheckError::Io`] if the analysis thread cannot
/// be spawned.
#[cfg_attr(coverage, coverage(off))]
pub fn run_with_large_stack<T, F>(f: F) -> Result<T, CheckError>
where
    F: FnOnce() -> Result<T, CheckError> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, f)
            .map_err(CheckError::Io)?;
        match handle.join() {
            Ok(result) => result,
            // Propagate a panic in `f` as a panic on the caller's thread,
            // exactly as if `f` had run inline (no behaviour change).
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// Run `f` inside a `rayon` pool whose every worker has a [`STACK_SIZE`]
/// stack, and return its result.
///
/// The whole-project built-in pass runs in parallel (issue #46), so the deep
/// stack issue #54 needs must cover the *worker* threads too, not only the
/// main analysis thread [`run_with_large_stack`] provides — rayon's default
/// worker stack is far smaller and a legitimately-accepted ~[`MAX_NESTING_DEPTH`]
/// file would overflow it. The pool is scoped to this call (mirroring
/// `run_with_large_stack`'s per-call thread); `f` typically drives a
/// `par_iter` whose closures then execute on these deep-stacked workers.
///
/// Excluded from the coverage gate for the same reason as
/// [`run_with_large_stack`]: every integration test that runs a check/fix
/// exercises the happy path, but the pool-construction-failure arm is
/// environment-only (thread creation) and not deterministically reachable.
///
/// # Errors
///
/// Whatever `f` returns, or [`CheckError::Io`] if the worker pool cannot be
/// built.
#[cfg_attr(coverage, coverage(off))]
pub fn with_large_stack_pool<T, F>(f: F) -> Result<T, CheckError>
where
    F: FnOnce() -> Result<T, CheckError> + Send,
    T: Send,
{
    let pool = ThreadPoolBuilder::new()
        .stack_size(STACK_SIZE)
        .build()
        .map_err(|e| CheckError::Io(std::io::Error::other(e.to_string())))?;
    pool.install(f)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn counts_mixed_bracket_nesting() {
        // `[` `(` `{` all nest; the max across the file is reported.
        assert_eq!(max_nesting_depth("x = [1, (2, {3: 4})]\n"), 3);
        assert_eq!(max_nesting_depth("f(g(h(1)))\n"), 3);
        assert_eq!(max_nesting_depth("a = 1\nb = 2\n"), 0);
        // Re-opening a bracket after closing does *not* raise the running
        // max: the second `(` reaches depth 1 while max is already 2.
        assert_eq!(max_nesting_depth("f((1), (2))\n"), 2);
    }

    #[test]
    fn brackets_in_strings_and_comments_are_not_counted() {
        assert_eq!(max_nesting_depth("s = '((((' # ))))\n"), 0);
        assert_eq!(max_nesting_depth("s = \"\"\"[[[[\"\"\"\n"), 0);
    }

    #[test]
    fn deep_input_is_rejected_without_parsing() {
        let deep = format!("{}1{}\n", "f(".repeat(5000), ")".repeat(5000));
        let error = parse_module_guarded(&deep).expect_err("must reject");
        match error {
            CheckError::TooDeeplyNested { depth, limit } => {
                assert_eq!(depth, 5000);
                assert_eq!(limit, MAX_NESTING_DEPTH);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn depth_just_under_the_limit_is_accepted_by_the_guard() {
        // The *guard* admits it (depth <= limit); the recursive parse itself
        // is exercised on the large analysis stack by the end-to-end CLI
        // test, not here — a unit-test thread's small stack cannot parse
        // anywhere near `MAX_NESTING_DEPTH` deep.
        let src = format!(
            "{}1{}\n",
            "f(".repeat(MAX_NESTING_DEPTH),
            ")".repeat(MAX_NESTING_DEPTH)
        );
        assert_eq!(max_nesting_depth(&src), MAX_NESTING_DEPTH);
    }

    #[test]
    fn shallow_source_parses_through_the_guard() {
        // Few bracket bytes: the cheap pre-filter rules it in and the precise
        // scan is skipped entirely.
        assert!(parse_module_guarded("def f(a):\n    return a\n").is_ok());
    }

    #[test]
    fn many_bracket_bytes_in_a_string_are_not_falsely_rejected() {
        // > `MAX_NESTING_DEPTH` `(` *bytes* so the cheap pre-filter cannot
        // rule the file out, but they are all inside a string literal: real
        // nesting is 0, so the precise scan must accept it (the byte count is
        // only ever used to *skip* the scan, never to reject).
        let src = format!("s = \"{}\"\n", "(".repeat(MAX_NESTING_DEPTH + 50));
        assert!(open_bracket_bytes(&src) > MAX_NESTING_DEPTH);
        assert_eq!(max_nesting_depth(&src), 0);
        assert!(parse_module_guarded(&src).is_ok());
    }

    #[test]
    fn syntax_error_still_surfaces_as_parse_error() {
        let error = parse_module_guarded("def f(:\n").expect_err("syntax error");
        assert!(matches!(error, CheckError::Parse(_)));
    }
}
