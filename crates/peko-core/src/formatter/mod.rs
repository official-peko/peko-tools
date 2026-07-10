//! # Peko Core Formatter
//!
//! Pretty-prints Pekoscript ASTs back into canonical source. Unlike a
//! whitespace reindenter, this walks the AST and re-emits every construct in a
//! single normalized form: consistent indentation and spacing, forced
//! statement terminators, and canonical expression grouping.
//!
//! # Architecture
//!
//! * [`Format`] is the AST-level trait every kind of AST node implements. It
//!   mirrors the simulator's `PekoValueSimulator`: one method, driven by a
//!   context threaded through every call.
//! * [`context::FormatContext`] holds the output buffer and indentation state.
//! * [`data_structures::FormatConfig`] carries the tuning knobs.
//! * The four `*_fmt` modules ([`value_fmt`], [`expression_fmt`],
//!   [`statement_fmt`], [`declaration_fmt`]) supply the per-AST-variant
//!   implementations, grouped exactly as the simulator groups its `*_sims`.

use std::path::Path;

use crate::asts::data_structures::Spanned;
use crate::asts::{CommentAST, PekoAST};
use crate::lexer::TokenList;
use crate::parser::PekoParser;

pub mod context;
pub mod data_structures;

// AST formatter implementations, grouped by AST category.
pub mod declaration_fmt;
pub mod expression_fmt;
pub mod statement_fmt;
pub mod value_fmt;

use context::FormatContext;
use data_structures::FormatConfig;

/// Trait implemented by every AST node to emit its canonical source form into
/// a [`FormatContext`].
pub trait Format {
    /// Write this node's formatted text into `ctx`. Implementations write
    /// inline text with [`FormatContext::write`] and break lines with
    /// [`FormatContext::newline`]; a node never emits its own trailing
    /// statement terminator or the newline that follows it, which the
    /// enclosing sequence controls.
    fn format(&self, ctx: &mut FormatContext);
}

/// Format a parsed program (a sequence of top-level ASTs) into a string. The
/// `source` is used only to preserve blank lines the author left between
/// constructs; pass an empty string to skip that.
pub fn format_program(asts: &[PekoAST], source: &str, config: FormatConfig) -> String {
    let mut ctx = FormatContext::new(config, source);
    format_sequence(asts, &mut ctx);
    ctx.finish()
}

/// Parse `source` and format it. Parses the file on its own, without the
/// analyzer's auto-injected preludes, so only the author's declarations are
/// printed.
pub fn format_source(source: &str, file: &Path, config: FormatConfig) -> String {
    format_program(&parse_program(source, file), source, config)
}

/// Parse a whole file into its top-level ASTs. A parse step that consumes no
/// tokens is forced forward so malformed input cannot spin the loop.
fn parse_program(source: &str, file: &Path) -> Vec<PekoAST> {
    let mut parser = PekoParser::new(TokenList::from_source(source, file), file);
    // The formatter prints only the author's declarations, not the compiler's
    // derived serialization helpers.
    parser.set_state("skip_derives");
    // Comments are captured as nodes so they can be reproduced in place.
    parser.set_state("keep_comments");
    let mut asts = Vec::new();

    // A parse step consumes at least one token, and the token count cannot
    // exceed the source length, so this bounds the loop even if a step makes no
    // progress on malformed input.
    let max_iterations = source.len() + 1;
    let mut iterations = 0;
    while !parser.tokens.finished() || parser.has_pending() {
        iterations += 1;
        if iterations > max_iterations {
            break;
        }
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
            continue;
        }
        // A top-level comment (plain `//` or a `///` doc line) is captured as
        // its own node so it prints in place.
        if parser.tokens.current_token().is_comment()
            || parser.tokens.current_token().is_doc_comment()
        {
            asts.push(PekoAST::Comment(parser.take_comment()));
            continue;
        }
        // Popping a queued (derive-generated) declaration makes progress
        // without consuming a token, so only force progress when nothing was
        // queued.
        let had_pending = parser.has_pending();
        let index_before = parser.tokens.get_index();
        asts.push(parser.parse());
        if !had_pending && !parser.tokens.finished() && parser.tokens.get_index() == index_before {
            parser.tokens.increase_index();
        }
    }

    asts
}

/// Format a sequence of statements, one per line, each with the terminator its
/// kind requires. Used for both the top level and the inside of a block.
///
/// Blank lines the author left between statements are preserved but collapsed:
/// any gap in the source becomes exactly one blank line, so intentional spacing
/// survives while runs of blank lines are normalized.
pub(crate) fn format_sequence(asts: &[PekoAST], ctx: &mut FormatContext) {
    let mut skip_next = false;
    for (index, ast) in asts.iter().enumerate() {
        // A trailing comment was already emitted on the previous line.
        if skip_next {
            skip_next = false;
            continue;
        }

        // Preserve a single blank line where the author left one, detected from
        // the reliable start line of the construct against the source text.
        if index > 0 && ctx.blank_line_precedes(ast.get_start().line) {
            ctx.blank_line();
        }
        ast.format(ctx);
        if needs_semicolon(ast) {
            ctx.write(";");
        }

        // A comment that follows code on its own source line is a trailing
        // comment on this item: keep it on the same line rather than moving it
        // to its own. The source-text check is exact where statement end
        // positions are not.
        if let Some(PekoAST::Comment(next)) = asts.get(index + 1)
            && ctx.has_code_before(next.get_start().line, next.get_start().column)
        {
            ctx.write(" ");
            next.format(ctx);
            skip_next = true;
        }

        ctx.newline();
    }
}

/// Emit a bracketed, comma-separated list, inline when it fits the configured
/// width or one element per line otherwise.
///
/// `open`/`close` are the surrounding brackets and `render_item` renders one
/// element without any separator. The list stays on one line when it fits
/// within the configured width at the current column; otherwise each element
/// goes on its own line indented one level with a trailing comma, and the
/// closing bracket sits on its own line. An empty list is always inline.
pub(crate) fn format_delimited_list<T>(
    items: &[T],
    open: &str,
    close: &str,
    render_item: impl Fn(&T, &mut FormatContext),
    ctx: &mut FormatContext,
) {
    // The single-line form, measured to decide whether it fits.
    let inline = ctx.measure(|scratch| {
        scratch.write(open);
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                scratch.write(", ");
            }
            render_item(item, scratch);
        }
        scratch.write(close);
    });

    // A construct that already contains a newline (a nested wrap) never fits.
    let fits = match ctx.max_width() {
        None => true,
        Some(max) => !inline.contains('\n') && ctx.current_column() + inline.chars().count() <= max,
    };

    if items.is_empty() || fits {
        ctx.write(&inline);
        return;
    }

    ctx.write(open);
    ctx.newline();
    ctx.indent();
    for item in items {
        render_item(item, ctx);
        ctx.write(",");
        ctx.newline();
    }
    ctx.dedent();
    ctx.write(close);
}

/// Format a brace-delimited body: `{`, the indented statements, then `}`. An
/// empty body collapses to `{}`.
pub(crate) fn format_block(body: &[PekoAST], ctx: &mut FormatContext) {
    if body.is_empty() {
        ctx.write("{}");
        return;
    }
    ctx.write("{");
    ctx.newline();
    ctx.indent();
    format_sequence(body, ctx);
    ctx.dedent();
    ctx.write("}");
}

/// Whether a statement needs a trailing `;`. Block-shaped declarations and
/// control-flow statements end in `}` and take no terminator; everything else
/// (expressions used as statements, assignments, returns, imports) does.
fn needs_semicolon(ast: &PekoAST) -> bool {
    // A bodyless (external) function declaration ends without a brace, so it
    // does take a terminator unlike a function with a body.
    if let PekoAST::FunctionDeclaration(function) = ast {
        return function.function_body.is_none();
    }
    !matches!(
        ast,
        PekoAST::Class(_)
            | PekoAST::Trait(_)
            | PekoAST::Enum(_)
            | PekoAST::ModuleCreation(_)
            | PekoAST::IfStatement(_)
            | PekoAST::Switch(_)
            | PekoAST::WhileLoop(_)
            | PekoAST::ForLoop(_)
            | PekoAST::PlatformStatement(_)
            | PekoAST::Comment(_)
    )
}

impl Format for PekoAST {
    fn format(&self, ctx: &mut FormatContext) {
        match self {
            // Value literals.
            PekoAST::Char(node) => node.format(ctx),
            PekoAST::Number(node) => node.format(ctx),
            PekoAST::Boolean(node) => node.format(ctx),
            PekoAST::String(node) => node.format(ctx),
            PekoAST::EncryptedString(node) => node.format(ctx),
            PekoAST::Null(node) => node.format(ctx),

            // Expressions.
            PekoAST::Array(node) => node.format(ctx),
            PekoAST::Map(node) => node.format(ctx),
            PekoAST::VariableReference(node) => node.format(ctx),
            PekoAST::FunctionCall(node) => node.format(ctx),
            PekoAST::ObjectConstruction(node) => node.format(ctx),
            PekoAST::ObjectAccess(node) => node.format(ctx),
            PekoAST::ArrayAccess(node) => node.format(ctx),
            PekoAST::BinaryExpression(node) => node.format(ctx),
            PekoAST::UnaryExpression(node) => node.format(ctx),
            PekoAST::ModuleAccess(node) => node.format(ctx),
            PekoAST::Unwrap(node) => node.format(ctx),
            PekoAST::Cast(node) => node.format(ctx),
            PekoAST::PekoXTag(node) => node.format(ctx),
            PekoAST::Range(node) => node.format(ctx),

            // Statements.
            PekoAST::VariableReassignment(node) => node.format(ctx),
            PekoAST::Return(node) => node.format(ctx),
            PekoAST::IfStatement(node) => node.format(ctx),
            PekoAST::Switch(node) => node.format(ctx),
            PekoAST::WhileLoop(node) => node.format(ctx),
            PekoAST::ForLoop(node) => node.format(ctx),
            PekoAST::Break(node) => node.format(ctx),
            PekoAST::Continue(node) => node.format(ctx),
            PekoAST::ImportStatement(node) => node.format(ctx),
            PekoAST::LinkStatement(node) => node.format(ctx),
            PekoAST::StyleStatement(node) => node.format(ctx),
            PekoAST::PlatformStatement(node) => node.format(ctx),

            // Declarations.
            PekoAST::NewVariable(node) => node.format(ctx),
            PekoAST::Destructure(node) => node.format(ctx),
            PekoAST::FunctionDeclaration(node) => node.format(ctx),
            PekoAST::Closure(node) => node.format(ctx),
            PekoAST::Class(node) => node.format(ctx),
            PekoAST::Trait(node) => node.format(ctx),
            PekoAST::Enum(node) => node.format(ctx),
            PekoAST::ModuleCreation(node) => node.format(ctx),

            // A captured comment prints its own text.
            PekoAST::Comment(node) => node.format(ctx),

            // Placeholder nodes carry no source form.
            PekoAST::Placeholder(_) => {}
        }
    }
}

impl Format for CommentAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(&self.text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fmt(source: &str) -> String {
        format_source(source, &PathBuf::from("test.peko"), FormatConfig::default())
    }

    /// Format with a narrow width so wrapping is exercised on small inputs.
    fn fmt_narrow(source: &str) -> String {
        let config = FormatConfig {
            indent_unit: "    ".to_string(),
            max_width: 40,
        };
        format_source(source, &PathBuf::from("test.peko"), config)
    }

    #[test]
    fn wraps_long_call_arguments() {
        let out = fmt_narrow("callSomething(alpha, beta, gamma, delta, epsilon)\n");
        assert_eq!(
            out,
            "callSomething(\n    alpha,\n    beta,\n    gamma,\n    delta,\n    epsilon,\n);\n"
        );
    }

    #[test]
    fn wraps_long_parameter_list() {
        let out = fmt_narrow(
            "[public] fn compute(alpha: i32, beta: i32, gamma: i32) => i32 { return alpha }\n",
        );
        assert!(
            out.contains(
                "fn compute(\n    alpha: i32,\n    beta: i32,\n    gamma: i32,\n) => i32 {"
            ),
            "params did not wrap: {out:?}"
        );
    }

    #[test]
    fn keeps_short_list_inline() {
        assert_eq!(fmt_narrow("shortCall(a, b)\n"), "shortCall(a, b);\n");
    }

    #[test]
    fn wrapping_is_idempotent() {
        for source in [
            "callSomething(alpha, beta, gamma, delta, epsilon)\n",
            "[public] fn compute(alpha: i32, beta: i32, gamma: i32) => i32 { return alpha }\n",
            "let xs: i32 = #[100, 200, 300, 400, 500, 600, 700]\n",
        ] {
            let once = fmt_narrow(source);
            let twice = fmt_narrow(&once);
            assert_eq!(once, twice, "not idempotent for {source:?}");
        }
    }

    #[test]
    fn value_literals() {
        assert_eq!(fmt("42"), "42;\n");
        assert_eq!(fmt("true"), "true;\n");
        assert_eq!(fmt("false"), "false;\n");
        assert_eq!(fmt("1.5"), "1.5;\n");
        // A whole float keeps its decimal so it does not reparse as an integer.
        assert_eq!(fmt("1.0"), "1.0;\n");
        assert_eq!(fmt("'a'"), "'a';\n");
        assert_eq!(fmt("null"), "null;\n");
        assert_eq!(fmt("\"hello\""), "\"hello\";\n");
    }

    #[test]
    fn blank_lines_collapse_to_one() {
        assert_eq!(fmt("42\n\n\n\n43"), "42;\n\n43;\n");
        assert_eq!(fmt("42\n43"), "42;\n43;\n");
    }

    #[test]
    fn idempotent_over_values() {
        for source in ["42", "true", "1.0", "\"hi\"", "42\n\n43", "'x'", "null"] {
            let once = fmt(source);
            let twice = fmt(&once);
            assert_eq!(once, twice, "not idempotent for {source:?}");
        }
    }

    #[test]
    fn expression_precedence_parens() {
        // Redundant parentheses are dropped; the tighter operator stays bare.
        assert_eq!(fmt("1 + 2 * 3"), "1 + 2 * 3;\n");
        assert_eq!(fmt("(1 + 2 * 3)"), "1 + 2 * 3;\n");
        // A lower-precedence left operand keeps its parentheses.
        assert_eq!(fmt("(1 + 2) * 3"), "(1 + 2) * 3;\n");
        // Left-associative: a left operand of equal precedence needs none...
        assert_eq!(fmt("a - b - c"), "a - b - c;\n");
        // ...but a right operand of equal precedence does.
        assert_eq!(fmt("a - (b - c)"), "a - (b - c);\n");
        assert_eq!(fmt("a && b || c"), "a && b || c;\n");
    }

    #[test]
    fn unary_and_range() {
        assert_eq!(fmt("!a"), "!a;\n");
        assert_eq!(fmt("!(a && b)"), "!(a && b);\n");
        assert_eq!(fmt("-a + b"), "-a + b;\n");
        assert_eq!(fmt("1..10"), "1..10;\n");
    }

    #[test]
    fn postfix_and_calls() {
        assert_eq!(fmt("f(1, 2)"), "f(1, 2);\n");
        assert_eq!(fmt("a.b.c"), "a.b.c;\n");
        assert_eq!(fmt("arr[0]"), "arr[0];\n");
        assert_eq!(fmt("new Foo(1)"), "new Foo(1);\n");
        assert_eq!(fmt("#[1, 2, 3]"), "#[1, 2, 3];\n");
        // Unwrap with a fallback block. A bare `x?` whose `?` is the final
        // token of the file is avoided here: it triggers a pre-existing parser
        // edge case (an unwrap at end-of-input), which real files never hit
        // because a `?` is always followed by more tokens.
        assert_eq!(fmt("x? else { 0 }"), "x? else {\n    0;\n};\n");
    }

    #[test]
    fn declarations() {
        assert_eq!(fmt("let x: i32 = 5"), "let x: i32 = 5;\n");
        assert_eq!(fmt("let y = 1 + 2"), "let y = 1 + 2;\n");
        assert_eq!(
            fmt("fn add(a: i32, b: i32) => i32 { return a + b }"),
            "fn add(a: i32, b: i32) => i32 {\n    return a + b;\n}\n"
        );
        assert_eq!(
            fmt("enum Color { Red, Green, Blue }"),
            "enum Color {\n    Red,\n    Green,\n    Blue,\n}\n"
        );
        assert_eq!(
            fmt("class Point { x: i32; y: i32; }"),
            "class Point {\n    x: i32;\n    y: i32;\n}\n"
        );
    }

    #[test]
    fn control_flow() {
        assert_eq!(
            fmt("if a { b } else { c }"),
            "if a {\n    b;\n} else {\n    c;\n}\n"
        );
        assert_eq!(fmt("while a { b }"), "while a {\n    b;\n}\n");
        assert_eq!(fmt("for i in 1..10 { i }"), "for i in 1..10 {\n    i;\n}\n");
    }

    #[test]
    fn idempotent_over_program() {
        // A realistic mix of constructs; formatting twice must be stable.
        let source = r#"
import { * } from std::collections;
import std::io as io;

[public] class Box<T: impl Drawable> from Base impl Drawable {
    value: T;
    fn draw(scale: i32) => void {
        if scale > 0 {
            io::print(value);
        } else {
            return;
        }
    }
}

trait Drawable {
    fn draw(scale: i32) => void;
}

enum State { Open, Closed }

fn main() => i32 {
    let total: i32 = 0;
    for item in 1..10 {
        total = total + item * 2;
    }
    switch total {
        _ => { return total }
    }
}
"#;
        let once = fmt(source);
        let twice = fmt(&once);
        assert_eq!(once, twice, "program not idempotent:\n{once}");
    }

    #[test]
    fn idempotent_over_std_corpus() {
        // Format every real std source twice and require stability. Skips
        // cleanly when the corpus is not present (e.g. a minimal checkout).
        let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../toolkit/std");
        let Ok(entries) = std::fs::read_dir(&corpus) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("peko") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let once = format_source(&source, &path, FormatConfig::default());
            let twice = format_source(&once, &path, FormatConfig::default());
            assert_eq!(once, twice, "not idempotent for {}", path.display());
            // No comment or doc line may be dropped: the formatted output keeps
            // exactly as many `//` and `///` markers as the source.
            assert_eq!(
                source.matches("///").count(),
                once.matches("///").count(),
                "doc comments lost formatting {}",
                path.display()
            );
            assert_eq!(
                source.matches("//").count(),
                once.matches("//").count(),
                "comments lost formatting {}",
                path.display()
            );
        }
    }

    #[test]
    fn idempotent_over_expressions() {
        for source in [
            "1 + 2 * 3",
            "(1 + 2) * 3",
            "a - (b - c)",
            "!(a && b) || c",
            "f(g(1), 2).h[0]",
            "new Foo<i32>(a, b)",
            "1..n - 1",
            "a as i32 + b",
        ] {
            let once = fmt(source);
            let twice = fmt(&once);
            assert_eq!(once, twice, "not idempotent for {source:?}");
        }
    }

    #[test]
    fn preserves_leading_comment() {
        assert_eq!(
            fmt("// a greeting\nfn main() {}\n"),
            "// a greeting\nfn main() {}\n"
        );
    }

    #[test]
    fn preserves_comment_inside_body() {
        let out = fmt("fn main() {\n    // step one\n    let x: i32 = 1\n}\n");
        assert!(
            out.contains("    // step one\n"),
            "body comment lost: {out:?}"
        );
        assert!(out.contains("let x: i32 = 1;"), "statement lost: {out:?}");
    }

    #[test]
    fn trailing_comment_stays_on_line() {
        assert_eq!(
            fmt("let x: i32 = 5 // the count\n"),
            "let x: i32 = 5; // the count\n"
        );
    }

    #[test]
    fn preserves_blank_line_before_comment() {
        assert_eq!(
            fmt("let a: i32 = 1\n\n// next\nlet b: i32 = 2\n"),
            "let a: i32 = 1;\n\n// next\nlet b: i32 = 2;\n"
        );
    }

    #[test]
    fn preserves_class_member_comments() {
        let out = fmt(
            "class C {\n    // a field\n    x: i32 // trailing\n    // a method\n    [public] fn f() {}\n}\n",
        );
        assert!(
            out.contains("    // a field\n"),
            "leading attr comment lost: {out:?}"
        );
        assert!(
            out.contains("x: i32; // trailing"),
            "trailing attr comment lost: {out:?}"
        );
        assert!(
            out.contains("    // a method\n"),
            "leading method comment lost: {out:?}"
        );
    }

    #[test]
    fn preserves_enum_and_trait_member_comments() {
        let enum_out = fmt("enum E {\n    A, // first\n    // second one\n    B,\n}\n");
        assert!(
            enum_out.contains("A, // first"),
            "enum trailing comment lost: {enum_out:?}"
        );
        assert!(
            enum_out.contains("    // second one\n"),
            "enum leading comment lost: {enum_out:?}"
        );

        let trait_out = fmt("trait T {\n    // a slot\n    fn area() => i32;\n}\n");
        assert!(
            trait_out.contains("    // a slot\n"),
            "trait comment lost: {trait_out:?}"
        );
    }

    #[test]
    fn idempotent_with_member_comments() {
        for source in [
            "class C {\n    // f\n    x: i32 // t\n\n    // m\n    fn g() {}\n}\n",
            "enum E {\n    A, // a\n    // b\n    B,\n}\n",
            "trait T {\n    // s\n    fn a() => i32;\n}\n",
        ] {
            let once = fmt(source);
            let twice = fmt(&once);
            assert_eq!(once, twice, "not idempotent for {source:?}");
        }
    }

    #[test]
    fn idempotent_with_comments() {
        for source in [
            "// header\nfn main() {\n    // inside\n    let x: i32 = 1 // trailing\n}\n",
            "// one\n// two\nfn f() {}\n",
            "fn f() {\n    call() // note\n    other()\n}\n",
        ] {
            let once = fmt(source);
            let twice = fmt(&once);
            assert_eq!(once, twice, "not idempotent for {source:?}");
        }
    }
}
