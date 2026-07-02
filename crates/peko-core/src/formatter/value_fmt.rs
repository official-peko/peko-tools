//! Formatting for value-literal AST nodes.
//!
//! These are the leaves of the tree: numbers, booleans, characters, `null`,
//! and strings (plain and interpolated). Literal content is reconstructed from
//! the AST and re-escaped, so the printed form parses back to an equal value.

use crate::asts::data_structures::StringChunkContent;
use crate::asts::values::{
    BooleanAST, CharAST, EncryptedStringAST, NullAST, NumberAST, StringAST,
};

use super::Format;
use super::context::FormatContext;

/// Escape a decoded string body for re-emission inside a literal delimited by
/// `delimiter`. Backslashes, the delimiter, and the common control characters
/// are escaped; everything else, including non-ASCII text, is emitted verbatim.
pub(crate) fn escape_body(text: &str, delimiter: char) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == delimiter => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

impl Format for BooleanAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(if self.value.value { "true" } else { "false" });
    }
}

impl Format for NumberAST {
    fn format(&self, ctx: &mut FormatContext) {
        if self.integer {
            ctx.write(&format!("{}", self.value.value as i64));
        } else {
            // A non-integer literal must keep a decimal point so it does not
            // reparse as an integer (a whole float like `1.0` prints as `1`).
            let mut rendered = format!("{}", self.value.value);
            if !rendered.contains(['.', 'e', 'E']) {
                rendered.push_str(".0");
            }
            ctx.write(&rendered);
        }
    }
}

impl Format for CharAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("'");
        ctx.write(&escape_body(&self.value.value.to_string(), '\''));
        ctx.write("'");
    }
}

impl Format for NullAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("null");
    }
}

impl Format for EncryptedStringAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("#\"");
        ctx.write(&escape_body(&self.encrypt.value, '"'));
        ctx.write("\"");
    }
}

impl Format for StringAST {
    fn format(&self, ctx: &mut FormatContext) {
        if self.interpolated {
            // Backtick template: literal text is escaped for a backtick body,
            // and each interpolation is re-wrapped in `${ ... }`.
            ctx.write("`");
            for chunk in &self.chunks {
                match &chunk.content {
                    StringChunkContent::Text(text) => {
                        ctx.write(&escape_body(text, '`'));
                    }
                    StringChunkContent::Interpolation(expressions) => {
                        ctx.write("${");
                        for (index, expression) in expressions.iter().enumerate() {
                            if index > 0 {
                                ctx.write("; ");
                            }
                            expression.format(ctx);
                        }
                        ctx.write("}");
                    }
                }
            }
            ctx.write("`");
        } else {
            // Plain double-quoted string: a single text chunk.
            ctx.write("\"");
            for chunk in &self.chunks {
                if let StringChunkContent::Text(text) = &chunk.content {
                    ctx.write(&escape_body(text, '"'));
                }
            }
            ctx.write("\"");
        }
    }
}
