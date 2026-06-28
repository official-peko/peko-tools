//! # Peko Core Parser
//!
//! Recursive-descent parser that turns a [`TokenList`](crate::lexer::TokenList)
//! into a [`PekoAST`].
//!
//! The parser is designed for editor and IDE use: rather than bailing on the
//! first error, it **collects diagnostics into a [`DiagnosticList`]
//! ([`PekoParser::get_diagnostics`])** and keeps going wherever it reasonably
//! can. Synthetic [`PlaceholderAST`] nodes stand in for malformed sub-trees so
//! that surrounding syntax can continue to parse cleanly.
//!
//! ## Usage
//!
//! ```no_run
//! use peko_core::{lexer::TokenList, parser::PekoParser};
//!
//! let source = "fn main() { return 0 }";
//! let tokens = TokenList::from_source(source, "main.peko");
//! let mut parser = PekoParser::new(tokens, "main.peko");
//!
//! while !parser.tokens.finished() {
//!     let _ast = parser.parse();
//! }
//!
//! for diag in parser.get_diagnostics() {
//!     eprintln!("{diag}");
//! }
//! ```
//!
//! ## Entry points
//!
//! * [`PekoParser::parse`]: top-level entry point; parses one statement,
//!   declaration, or expression including any leading visibility modifiers
//!   and doc-comments.
//! * [`PekoParser::secondary_parse`](#) (private): internal helper that
//!
//! parses simpler ASTs without operator-precedence handling.
//! * [`PekoParser::parse_expression`]: operator-precedence expression
//!   parser (modified shunting-yard).

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::asts::data_structures::*;
use crate::asts::{
    PekoAST, PlaceholderAST, declarations::*, expressions::*, statements::*, values::*,
};
use crate::diagnostics;
use crate::lexer;
use crate::types;

/// Set of HTML / SVG tag names that the parser recognizes as PekoX tag
/// elements rather than ordinary `<` expressions.
///
/// The list mirrors the HTML element tag-name registry. Order is irrelevant
/// (lookup is a linear scan via [`is_pekox_tag`], which is fine at this
/// size); keeping the names sorted-ish reads better at the call site.
const PEKOX_TAG_NAMES: &[&str] = &[
    "abbr",
    "acronym",
    "address",
    "a",
    "applet",
    "area",
    "article",
    "aside",
    "audio",
    "base",
    "basefont",
    "bdi",
    "bdo",
    "bgsound",
    "big",
    "blockquote",
    "body",
    "b",
    "br",
    "button",
    "caption",
    "canvas",
    "center",
    "cite",
    "code",
    "colgroup",
    "col",
    "data",
    "datalist",
    "dd",
    "dfn",
    "del",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "font",
    "footer",
    "form",
    "frame",
    "frameset",
    "head",
    "header",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "ins",
    "isindex",
    "i",
    "kbd",
    "keygen",
    "label",
    "legend",
    "li",
    "main",
    "mark",
    "marquee",
    "menuitem",
    "meta",
    "meter",
    "nav",
    "nobr",
    "noembed",
    "noscript",
    "object",
    "optgroup",
    "option",
    "output",
    "p",
    "param",
    "em",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "samp",
    "script",
    "section",
    "small",
    "source",
    "spacer",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "summary",
    "svg",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "time",
    "title",
    "tr",
    "track",
    "tt",
    "u",
    "var",
    "video",
    "wbr",
    "ul",
    "xmp",
];

/// Returns `true` if `tag` is one of the recognized PekoX HTML tag names.
fn is_pekox_tag(tag: &str) -> bool {
    PEKOX_TAG_NAMES.contains(&tag)
}

/// Incremental parser over a [`TokenList`](lexer::TokenList).
///
/// The parser is stateful: it owns the token cursor (`tokens`), a diagnostic
/// list it appends to as it discovers problems, the source file path, and a
/// small stack of context tags used by inner methods to communicate
/// parser-mode information (e.g. "we're inside a function-argument list, so
/// don't eat the closing paren as part of an expression").
pub struct PekoParser {
    /// Token cursor. The parser advances this through the input.
    pub tokens: lexer::TokenList,

    /// Context-tag stack. See `set_state`/`remove_state`. Maintained as a
    /// `Vec<String>` rather than a `HashSet` because nested parsing scopes
    /// can push/pop the same tag multiple times.
    state: Vec<String>,

    /// Collected diagnostics. The parser never returns errors via `Result`;
    /// every problem goes here.
    pub diagnostics: diagnostics::DiagnosticList,

    /// Path of the source file being parsed. Used to tag every emitted
    /// diagnostic and position.
    pub file: PathBuf,
}

type FunctionArgumentsInfo = (
    Vec<types::PekoType>,
    Vec<(Option<PositionedValue<String>>, PekoAST)>,
    PositionData,
);
type FunctionHeaderInfo = (
    IndexMap<PositionedValue<String>, DeclarationArgumentData>,
    Option<types::PekoType>,
    Option<types::PekoType>,
    PositionedValue<String>,
);

impl PekoParser {
    /// Constructs a new parser over `tokens` from the file at `file`.
    ///
    /// The cursor starts at the first token; diagnostics list starts empty.
    #[must_use]
    pub fn new(tokens: lexer::TokenList, file: impl AsRef<Path>) -> PekoParser {
        PekoParser {
            tokens,
            state: Vec::new(),
            diagnostics: diagnostics::DiagnosticList::new(),
            file: file.as_ref().to_path_buf(),
        }
    }

    /// Returns all diagnostics collected during parsing.
    #[must_use]
    pub fn get_diagnostics(&self) -> &diagnostics::DiagnosticList {
        &self.diagnostics
    }

    // ----- Error-reporting helpers -----------------------------------------

    /// Records an error diagnostic at the current token with the given
    /// message. Used by [`Self::expect_token_type`],
    /// [`Self::expect_token_value`], and [`Self::report_diagnostic`].
    fn report_at_current(&mut self, message: String) {
        let token = self.tokens.current_token();
        self.diagnostics
            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                token.get_start().clone(),
                token.get_end().clone(),
                message,
                diagnostics::DiagnosticType::Error,
                self.file.clone(),
            ));
    }

    /// Checks that the current token's type matches `token_type`. On
    /// mismatch, records an error diagnostic mentioning `errorfor` as the
    /// context.
    ///
    /// Returns `true` if the type matched (caller may proceed to consume
    /// the token), `false` otherwise (caller decides whether to consume or
    /// leave the cursor in place).
    pub fn expect_token_type(&mut self, token_type: &lexer::TokenType, errorfor: &str) -> bool {
        if std::mem::discriminant(self.tokens.current_token().get_type())
            == std::mem::discriminant(token_type)
        {
            return true;
        }

        let message = format!(
            "expected {} token for {errorfor}, found {} token `{}`",
            token_type.get_name(),
            self.tokens.current_token().get_type().get_name(),
            self.tokens.current_token().get_value(),
        );
        self.report_at_current(message);
        false
    }

    /// Checks that the current token's literal value equals `token_value`.
    /// On mismatch, records an error diagnostic mentioning `errorfor` as
    /// the context.
    pub fn expect_token_value(&mut self, token_value: &str, errorfor: &str) -> bool {
        if self.tokens.current_token().equals(token_value) {
            return true;
        }

        let message = format!(
            "expected `{token_value}` for {errorfor}, found `{}`",
            self.tokens.current_token().get_value()
        );
        self.report_at_current(message);
        false
    }

    /// Records an arbitrary error diagnostic at the current token.
    pub fn report_diagnostic(&mut self, error: &str) {
        self.report_at_current(error.to_owned());
    }

    // ----- Position / state helpers ----------------------------------------

    /// Returns the current cursor position, with `file` overwritten to this
    /// parser's file path (since the token may have been lexed against a
    /// placeholder path).
    #[must_use]
    pub fn get_current_position(&self) -> PositionData {
        if self.tokens.length() == 0 {
            return PositionData {
                file: self.file.clone(),
                ..PositionData::default()
            };
        }
        let mut pos = self.tokens.current_token().get_start().clone();
        pos.file = self.file.clone();
        pos
    }

    /// Pushes a context tag onto the state stack.
    fn set_state(&mut self, state: &str) {
        self.state.push(state.to_owned());
    }

    /// Removes the first occurrence of a context tag from the state stack.
    /// No-op if the tag isn't present.
    fn remove_state(&mut self, state: &str) {
        if let Some(idx) = self.state.iter().position(|s| s == state) {
            self.state.remove(idx);
        }
    }

    /// Returns `true` if the state stack currently contains `state`.
    fn has_state(&self, state: &str) -> bool {
        self.state.iter().any(|s| s == state)
    }

    // ----- General parsing shorthands --------------------------------------

    /// Skips forward from the current comment token until the comment line
    /// ends. No-op if the current token isn't a comment.
    pub fn skip_comment(&mut self) {
        if !self.tokens.current_token().is_comment() {
            return;
        }

        // Skip until the last token of the comment line.
        while !self.tokens.finished() && !self.tokens.current_token().has_newline() {
            self.tokens.increase_index();
        }
        // Skip the last token of the comment.
        self.tokens.increase_index();
    }

    /// Parses module-level doc info (`//!` lines).
    ///
    /// Used by the simulator when importing modules to surface their
    /// top-of-file documentation.
    pub fn parse_module_doc_info(&mut self) -> DocInfo {
        let mut documentation = DocInfo::default();

        while !self.tokens.finished() && self.tokens.current_token().equals("//!") {
            // Consume any contiguous run of empty `//!` lines as blank lines.
            while self.tokens.current_token().equals("//!")
                && self.tokens.current_token().has_trailing_newline()
            {
                self.tokens.increase_index();
                documentation.description.push('\n');
            }

            if !self.tokens.current_token().equals("//!") {
                break;
            }

            self.tokens.increase_index(); // eat the '//!'

            // Append every token up to the end of the current source line.
            while !self.tokens.finished() && !self.tokens.current_token().has_trailing_newline() {
                documentation
                    .description
                    .push_str(&self.tokens.current_token().get_value_with_whitespace(false));
                self.tokens.increase_index();
            }

            // Append the trailing token (the one that has the newline).
            documentation
                .description
                .push_str(&self.tokens.current_token().get_value_with_whitespace(false));
            self.tokens.increase_index();
        }

        documentation.description = documentation.description.trim().to_owned();
        documentation
    }

    /// Parses a doc-info block: `///` description lines followed by any
    /// number of `@param name desc` and `@example { ... }` directives.
    pub fn parse_doc_info(&mut self) -> DocInfo {
        let mut documentation = DocInfo::default();

        // First pass: description lines (until the first `@param` / `@example`).
        while !self.tokens.finished() && self.tokens.current_token().equals("///") {
            // Consume blank `///` lines.
            while self.tokens.current_token().equals("///")
                && self.tokens.current_token().has_trailing_newline()
            {
                self.tokens.increase_index();
                documentation.description.push('\n');
            }

            if !self.tokens.current_token().equals("///") {
                break;
            }

            self.tokens.increase_index(); // eat the '///'

            // Stop description parsing once we see `@param` / `@example`.
            if self.tokens.current_token().equals("@")
                && (self.tokens.get_token_forward(1).equals("param")
                    || self.tokens.get_token_forward(1).equals("example"))
            {
                break;
            }

            // Append every token up to the end of the current source line.
            while !self.tokens.finished() && !self.tokens.current_token().has_trailing_newline() {
                documentation
                    .description
                    .push_str(&self.tokens.current_token().get_value_with_whitespace(false));
                self.tokens.increase_index();
            }

            // Trailing token of the line.
            documentation
                .description
                .push_str(&self.tokens.current_token().get_value_with_whitespace(false));
            self.tokens.increase_index();
        }

        documentation.description = documentation.description.trim().to_owned();

        // Second pass: `@directive` lines.
        while !self.tokens.finished() && self.tokens.current_token().equals("@") {
            self.tokens.increase_index();
            let detail_type = self.tokens.current_token().get_value().clone();
            self.tokens.increase_index();

            // `@example { ... }` carries an embedded code block.
            if detail_type == "example" {
                if self.tokens.current_token().get_value() != "{" {
                    return documentation;
                }
                self.tokens.increase_index();

                // Read the example source from the enclosed `{ ... }`. Every
                // documentation line inside the block begins with a `///`
                // marker, which delimits the line and is stripped. Brace
                // depth tracks nested `{` and `}` so inner braces stay in the
                // text, and the block ends at the matching `}` at depth zero.
                // Each line is read whole, so inline `//` comments and other
                // line content stay attached to their line.
                let mut closing_braces = 0;
                let mut source_code = String::new();

                while !(self.tokens.finished()
                    || self.tokens.current_token().equals("}") && closing_braces == 0)
                {
                    // Content with no leading marker, for example a one-line
                    // `@example { ... }`, is appended as a single line.
                    if !self.tokens.current_token().equals("///") {
                        if self.tokens.current_token().equals("{") {
                            closing_braces += 1;
                        } else if self.tokens.current_token().equals("}") {
                            closing_braces -= 1;
                        }
                        source_code.push_str(
                            &self.tokens.current_token().get_value_with_whitespace(false),
                        );
                        self.tokens.increase_index();
                        continue;
                    }

                    let blank_line = self.tokens.current_token().has_trailing_newline();

                    // Carry indentation that follows the conventional single
                    // space after the marker.
                    let mut line = String::new();
                    if !blank_line {
                        for ws in self
                            .tokens
                            .current_token()
                            .following_whitespace
                            .iter()
                            .skip(1)
                        {
                            line.push(ws.get_value());
                        }
                    }

                    self.tokens.increase_index(); // eat the '///'

                    // Read the rest of the line up to the next marker, the
                    // block close at depth zero, or the end of input.
                    while !(self.tokens.finished()
                        || self.tokens.current_token().equals("///")
                        || self.tokens.current_token().equals("}") && closing_braces == 0)
                    {
                        if self.tokens.current_token().equals("{") {
                            closing_braces += 1;
                        } else if self.tokens.current_token().equals("}") {
                            closing_braces -= 1;
                        }
                        line.push_str(
                            &self.tokens.current_token().get_value_with_whitespace(false),
                        );
                        self.tokens.increase_index();
                    }

                    source_code.push_str(line.trim_end());
                    source_code.push('\n');
                }

                if !self.tokens.current_token().equals("}") {
                    return documentation;
                }
                self.tokens.increase_index(); // eat the closing '}'

                documentation.examples.push(source_code.trim().to_owned());

                // Consume blank `///` lines between examples.
                while self.tokens.current_token().equals("///")
                    && self.tokens.current_token().has_trailing_newline()
                {
                    self.tokens.increase_index();
                }

                if !self.tokens.current_token().equals("///") {
                    break;
                }
                self.tokens.increase_index(); // eat the '///'
                continue;
            }

            // Parameter / attribute name (e.g. `@param name description`).
            let item_name = self.tokens.current_token().get_value().clone();
            self.tokens.increase_index();

            let mut item_details = String::new();
            while !self.tokens.finished()
                && !(self.tokens.current_token().equals("@")
                    && (self.tokens.get_token_forward(1).equals("param")
                        || self.tokens.get_token_forward(1).equals("example")))
            {
                let added_empty_lines = self.tokens.current_token().equals("///")
                    && self.tokens.current_token().has_trailing_newline();

                while self.tokens.current_token().equals("///")
                    && self.tokens.current_token().has_trailing_newline()
                {
                    self.tokens.increase_index();
                    item_details.push('\n');
                }

                if added_empty_lines {
                    if !self.tokens.current_token().equals("///") {
                        break;
                    }
                    self.tokens.increase_index(); // eat the '///'

                    // A `@param` or `@example` directive after the blank line
                    // ends this item. The marker is already consumed, so the
                    // outer loop sees the `@` and parses the next directive.
                    if self.tokens.current_token().equals("@")
                        && (self.tokens.get_token_forward(1).equals("param")
                            || self.tokens.get_token_forward(1).equals("example"))
                    {
                        break;
                    }
                }

                while !self.tokens.finished() && !self.tokens.current_token().has_trailing_newline()
                {
                    item_details
                        .push_str(&self.tokens.current_token().get_value_with_whitespace(false));
                    self.tokens.increase_index();
                }

                // Trailing token of the line.
                item_details
                    .push_str(&self.tokens.current_token().get_value_with_whitespace(false));
                self.tokens.increase_index();

                if self.tokens.finished() || !self.tokens.current_token().equals("///") {
                    break;
                }
            }

            documentation
                .parameter_docs
                .insert(item_name, item_details.trim().to_owned());
        }

        documentation
    }

    // ----- Visibility ------------------------------------------------------

    /// Parses a `[modifier modifier ...]` visibility block.
    ///
    /// Returns a [`VisibilityData`] with the corresponding flags set. The
    /// leading `[` and trailing `]` are consumed.
    pub fn parse_visibility(&mut self) -> VisibilityData {
        if self.tokens.current_token().equals("[") {
            self.tokens.increase_index();
        }

        let mut private = false;
        let mut public = false;
        let mut constant = false;
        let mut external = false;
        let mut notrack = false;
        let mut variadic = false;
        let mut blockexit = false;
        let mut hidden = false;
        let mut state = false;
        let mut mutates = false;
        let mut gcsafe = false;

        while !self.tokens.finished() && !self.tokens.current_token().equals("]") {
            match self.tokens.current_token().get_type() {
                lexer::TokenType::Private => private = true,
                lexer::TokenType::External => external = true,
                lexer::TokenType::Constant => constant = true,
                // `public` overrides `private` (last-modifier-wins) and is
                // also recorded so it can suppress the unused warning.
                lexer::TokenType::Public => {
                    private = false;
                    public = true;
                }
                lexer::TokenType::Notrack => notrack = true,
                lexer::TokenType::Variadic => variadic = true,
                lexer::TokenType::Blockexit => blockexit = true,
                lexer::TokenType::Hide => hidden = true,
                lexer::TokenType::State => state = true,
                lexer::TokenType::Mutates => mutates = true,
                lexer::TokenType::GCSafe => gcsafe = true,
                _ => {}
            }
            self.tokens.increase_index();
        }

        if self.tokens.current_token().equals("]") {
            self.tokens.increase_index();
        }

        VisibilityData::new(
            private, public, constant, external, notrack, variadic, blockexit, hidden, state,
            mutates, gcsafe, false,
        )
    }

    // ----- Character / string parsing -------------------------------------

    /// Resolves the current token as a single character, handling escape
    /// sequences.
    ///
    /// Standard escapes (`\n`, `\t`, `\r`, `\\`, `\"`, `\'`, `` \` ``) and
    /// two-digit hex escapes (`\xNN` for any `NN` in `00..=ff`) are decoded.
    /// On a malformed escape, records a diagnostic and returns a single
    /// space as a placeholder so parsing can continue.
    fn parse_token_as_char(&mut self) -> String {
        // Non-escape: return the token verbatim.
        if self.tokens.current_token().get_value() != "\\" {
            return self.tokens.current_token().get_value().clone();
        }

        self.tokens.increase_index();
        if self.tokens.finished() {
            return String::from(" ");
        }

        let token_value = self.tokens.current_token().get_value().clone();
        let token_chars: Vec<char> = token_value.chars().collect();

        // Single-character escapes consume only the first character of the
        // token. Any remaining characters in the same token are part of the
        // string and follow the escape.
        let single_char_remainder = &token_value[token_chars[0].len_utf8()..];

        match token_chars[0] {
            'n' => format!("\n{single_char_remainder}"),
            't' => format!("\t{single_char_remainder}"),
            'r' => format!("\r{single_char_remainder}"),
            '\\' => format!("\\{single_char_remainder}"),
            '"' => format!("\"{single_char_remainder}"),
            '\'' => format!("'{single_char_remainder}"),
            '`' => format!("`{single_char_remainder}"),

            // `\xNN`, two-digit hex byte. The token must be exactly `xNN`;
            // any extra characters after the hex digits are appended to the
            // decoded byte.
            'x' => {
                if token_chars.len() < 3 {
                    self.report_diagnostic("invalid `\\x` escape in string literal. The `\\x` escape must be followed by exactly two hex digits, e.g. `\\x1F`");
                    return String::from(" ");
                }
                let hex = &token_value[1..3];
                let remainder = &token_value[3..];
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => format!("{}{remainder}", byte as char),
                    Err(_) => {
                        self.report_diagnostic("invalid hex digits in `\\x` escape. The two characters after `\\x` must be hex digits (`0`-`9`, `a`-`f`, `A`-`F`)");
                        String::from(" ")
                    }
                }
            }

            _ => {
                self.report_diagnostic("invalid escape sequence in string literal. Valid escapes are `\\n`, `\\t`, `\\r`, `\\\\`, `\\\"`, `\\'`, `` \\` ``, and `\\xHH` for hex codes");
                String::from(" ")
            }
        }
    }

    /// Rewrites every `%` in interpolation text as `%~`.
    ///
    /// Interpolated string text and XML inner text run their text chunks
    /// through this so a literal percent sign reaches the final
    /// representation as `%~`.
    fn escape_interpolation_percents(text: &str) -> String {
        text.replace('%', "%~")
    }

    /// Parses a string literal's contents up to one of the given delimiters.
    ///
    /// Used by both the plain string parser (delimiter `"`) and the
    /// interpolated string parser (delimiters `` ` `` and `${`).
    pub fn parse_string_literal(&mut self, delimiters: Vec<String>) -> String {
        let mut final_string = String::new();

        // A delimiter matches if every char of it equals the corresponding
        // forward-lookup token's value.
        let matches_any_delim = |this: &Self| {
            delimiters.iter().any(|delim| {
                delim
                    .chars()
                    .enumerate()
                    .all(|(idx, ch)| this.tokens.get_token_forward(idx).equals(&ch.to_string()))
            })
        };

        while !self.tokens.finished() && !matches_any_delim(self) {
            for ws in &self.tokens.current_token().preceeding_whitespace {
                final_string.push(ws.get_value());
            }

            final_string.push_str(&self.parse_token_as_char());

            for ws in &self.tokens.current_token().following_whitespace {
                final_string.push(ws.get_value());
            }

            self.tokens.increase_index();
        }

        final_string
    }

    /// Parses a `{ ... }` block.
    ///
    /// `blockfor` is used in error messages to identify the kind of block
    /// being parsed (e.g. `"function body"`, `"if statement body"`).
    pub fn parse_block(&mut self, blockfor: &str) -> PositionedValue<Vec<PekoAST>> {
        let block_start = self.get_current_position();
        if self.expect_token_value("{", blockfor) {
            self.tokens.increase_index();
        }

        let mut block_body = Vec::new();

        while !self.tokens.finished() && !self.tokens.current_token().equals("}") {
            // Eat comments and stray semicolons before each statement.
            loop {
                if self.tokens.finished() {
                    break;
                }
                match self.tokens.current_token().get_type() {
                    lexer::TokenType::Comment => self.skip_comment(),
                    _ => {
                        if self.tokens.current_token().get_value() == ";" {
                            self.tokens.increase_index();
                        } else {
                            break;
                        }
                    }
                }
            }

            // Re-check for finish / closing brace after the eat-loop.
            if self.tokens.finished() {
                break;
            }
            if self.tokens.current_token().get_value() == "}" {
                break;
            }

            let index_before = self.tokens.get_index();
            block_body.push(self.parse());
            // If parse() consumed nothing the cursor is stuck. Force one token
            // of progress so the loop always terminates.
            if !self.tokens.finished() && self.tokens.get_index() == index_before {
                self.tokens.increase_index();
            }

            // Eat trailing semicolons and comments.
            while !self.tokens.finished()
                && (self.tokens.current_token().equals(";")
                    || self.tokens.current_token().is_comment())
            {
                if self.tokens.current_token().is_comment() {
                    self.skip_comment();
                } else {
                    self.tokens.increase_index();
                }
            }
        }

        let block_end = self.tokens.current_token().get_end().clone();

        if self.expect_token_value("}", blockfor) {
            self.tokens.increase_index();
        }

        PositionedValue::new(block_body, block_start, block_end)
    }

    /// Parses a function-call argument list: optional `<generic, ...>` followed
    /// by `(arg, arg, ...)`.
    ///
    /// Returns `(generic types, arguments, closing-paren end position)`. Each
    /// argument is paired with an optional keyword name (`Some` if the
    /// argument was passed as `name = value`, `None` otherwise).
    pub fn parse_arguments(&mut self) -> FunctionArgumentsInfo {
        // Optional `<T, U, ...>` generic argument list.
        let mut function_generics = Vec::new();
        if self.tokens.current_token().equals("<") {
            self.tokens.increase_index();

            while !self.tokens.finished() && !self.tokens.current_token().equals(">") {
                let index_before = self.tokens.get_index();
                function_generics.push(types::PekoType::from_tokens(self));
                // If from_tokens consumed nothing the cursor is stuck. Force
                // one token of progress so the loop always terminates.
                if !self.tokens.finished()
                    && !self.tokens.current_token().equals(">")
                    && self.tokens.get_index() == index_before
                {
                    self.tokens.increase_index();
                }
                if self.tokens.current_token().equals(",") {
                    self.tokens.increase_index();
                }
            }

            if self.expect_token_value(">", "argument generics") {
                self.tokens.increase_index();
            }
        }

        let mut arguments = Vec::new();

        if self.expect_token_value("(", "argument list") {
            self.tokens.increase_index();
        }

        // While inside an argument list, expression parsing must not treat
        // the closing `)` as part of an expression (count parens instead).
        self.set_state("count_parens");

        while !self.tokens.finished() && !self.tokens.current_token().equals(")") {
            // Keyword argument: `name = value` (only when the next token is `=`).
            let keyword = match self.tokens.current_token().get_type() {
                lexer::TokenType::Identifier if self.tokens.get_token_forward(1).equals("=") => {
                    let kw = PositionedValue::from_token(self.tokens.current_token());
                    self.tokens.increase_index();
                    if self.expect_token_value("=", "keyword argument") {
                        self.tokens.increase_index();
                    }
                    Some(kw)
                }
                _ => None,
            };

            let index_before = self.tokens.get_index();
            arguments.push((keyword, self.parse()));
            // If parse() consumed nothing the cursor is stuck. Force one token
            // of progress so the loop always terminates.
            if !self.tokens.finished()
                && !self.tokens.current_token().equals(")")
                && self.tokens.get_index() == index_before
            {
                self.tokens.increase_index();
            }

            if self.tokens.current_token().equals(",") {
                self.tokens.increase_index();
            }
        }

        self.remove_state("count_parens");

        let ending_position = self.tokens.current_token().get_end().clone();
        if self.expect_token_value(")", "argument list") {
            self.tokens.increase_index();
        }

        (function_generics, arguments, ending_position)
    }

    /// Parses an object instantiation: `new Class(args)` or
    /// `new Class<T1, T2>(args)`. The cursor starts on `new`.
    fn parse_new_expression(&mut self) -> PekoAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index();

        let class_name = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "class name");
        self.tokens.increase_index();

        let (object_generics, arguments, ending_position) = self.parse_arguments();

        PekoAST::ObjectConstruction(ObjectConstructionAST::new(
            starting_position,
            ending_position,
            class_name,
            object_generics,
            arguments,
        ))
    }

    /// Parses a forced cast: `danger_cast<T>(value)`. The cursor starts on
    /// `danger_cast`.
    fn parse_danger_cast(&mut self) -> PekoAST {
        self.tokens.increase_index();

        if self.expect_token_value("<", "danger_cast type parameter") {
            self.tokens.increase_index();
        }
        let cast_to = types::PekoType::from_tokens(self);
        if self.expect_token_value(">", "danger_cast type parameter") {
            self.tokens.increase_index();
        }

        if self.expect_token_value("(", "danger_cast value") {
            self.tokens.increase_index();
        }
        let value = self.parse();
        if self.expect_token_value(")", "danger_cast value") {
            self.tokens.increase_index();
        }

        PekoAST::Cast(CastAST::new(Box::new(value), cast_to, CastKind::Forced))
    }

    /// Parses an FFI constant builtin: `constant<T>(value)`. The cursor starts
    /// on `constant`.
    fn parse_constant_builtin(&mut self) -> PekoAST {
        self.tokens.increase_index();

        if self.expect_token_value("<", "constant type parameter") {
            self.tokens.increase_index();
        }
        let constant_type = types::PekoType::from_tokens(self);
        if self.expect_token_value(">", "constant type parameter") {
            self.tokens.increase_index();
        }

        if self.expect_token_value("(", "constant value") {
            self.tokens.increase_index();
        }
        let value = self.parse();
        if self.expect_token_value(")", "constant value") {
            self.tokens.increase_index();
        }

        PekoAST::Cast(CastAST::new(Box::new(value), constant_type, CastKind::Constant))
    }

    /// Parses a function-header argument list: `(arg: type, ..., Args<T> => name) => ret`.
    ///
    /// `parse_varargs` controls whether `Args<T> => name` variadic syntax is
    /// permitted; if `false`, encountering one records a diagnostic but the
    /// header still parses for recovery.
    ///
    /// Returns `(arguments, return_type, varargs_type, varargs_name)`.
    pub fn parse_function_header(&mut self, parse_varargs: bool) -> FunctionHeaderInfo {
        let mut varargs_type = None;
        let mut varargs_name = String::new();

        if self.expect_token_value("(", "parameter list") {
            self.tokens.increase_index();
        }

        let mut function_arguments: IndexMap<PositionedValue<String>, DeclarationArgumentData> =
            IndexMap::new();

        let mut varargs_start: Option<PositionData> = None;
        let mut varargs_end: Option<PositionData> = None;

        while !self.tokens.finished() && !self.tokens.current_token().equals(")") {
            // Variadic argument: `Args<T> => name`.
            if self.tokens.current_token().equals("Args") {
                varargs_start = Some(self.get_current_position());

                if !parse_varargs {
                    self.report_diagnostic("variadic arguments are not allowed in this header. Only function and method declarations support `<T> => name` variadics");
                }

                self.tokens.increase_index();

                if self.expect_token_value("<", "variable args type") {
                    self.tokens.increase_index();
                }

                varargs_type = Some(types::PekoType::from_tokens(self));

                if self.expect_token_value(">", "variable args type") {
                    self.tokens.increase_index();
                }

                if self.expect_token_value("=>", "variadic separator") {
                    self.tokens.increase_index();
                }

                varargs_name = self.tokens.current_token().get_value().clone();
                self.expect_token_type(&lexer::TokenType::Identifier, "parameter name");
                self.tokens.increase_index();

                varargs_end = Some(self.get_current_position());
                continue;
            }

            // Normal argument: `name: type [= default]`. Constness is carried
            // by the type (`name: const type`), not an argument modifier.
            let visibility = VisibilityData::open_visibility();

            let argument_start = self.get_current_position();

            let argument_name = PositionedValue::from_token(self.tokens.current_token());
            self.expect_token_type(&lexer::TokenType::Identifier, "parameter name");
            self.tokens.increase_index();

            let mut argument_type = types::PekoType::error_type();
            if self.expect_token_value(":", "parameter type") {
                self.tokens.increase_index();
                argument_type = types::PekoType::from_tokens(self);
            }

            let default_value = if self.tokens.current_token().equals("=") {
                self.tokens.increase_index();
                Some(self.parse())
            } else {
                None
            };

            function_arguments.insert(
                argument_name,
                DeclarationArgumentData::new(
                    argument_start,
                    self.get_current_position(),
                    argument_type,
                    default_value,
                    visibility,
                ),
            );

            if self.tokens.current_token().equals(",") {
                self.tokens.increase_index();
            }
        }

        if self.expect_token_value(")", "parameter list") {
            self.tokens.increase_index();
        }

        // Optional `=> ReturnType`.
        let return_type = match self.tokens.current_token().get_type() {
            lexer::TokenType::Returns => {
                self.tokens.increase_index();
                Some(types::PekoType::from_tokens(self))
            }
            _ => None,
        };

        let varargs_name = match (varargs_start, varargs_end) {
            (Some(s), Some(e)) => PositionedValue::new(varargs_name, s, e),
            _ => PositionedValue::create_no_position(varargs_name),
        };

        (function_arguments, return_type, varargs_type, varargs_name)
    }

    // ----- AST-specific parsing --------------------------------------------
    //
    // All AST-specific parsing functions assume the current token is the
    // proper starter for that AST (e.g. `parse_function_declaration` is
    // called with the cursor on `fn`). They are private-by-convention and
    // are only called from this file. Throughout the comments below, "eat"
    // means "advance the cursor past."

    /// Parses a boolean literal (`true` or `false`).
    fn parse_boolean(&mut self) -> BooleanAST {
        let starting_position = self.get_current_position();

        let value = self.tokens.current_token().get_value() == "true";

        let mut ending_position = self.tokens.current_token().get_end().clone();
        ending_position.file = self.file.clone();

        self.tokens.increase_index();

        BooleanAST::new(PositionedValue::new(
            value,
            starting_position,
            ending_position,
        ))
    }

    /// Parses an encrypted string literal: `#"..."`.
    fn parse_encrypted_string(&mut self) -> EncryptedStringAST {
        let start = self.get_current_position();
        self.tokens.increase_index(); // eat '#'
        self.tokens.increase_index(); // eat '"'

        let mut string = String::new();

        while !self.tokens.finished()
            && self.tokens.current_token().get_type() != &lexer::TokenType::DoubleString
        {
            for ws in &self.tokens.current_token().preceeding_whitespace {
                string.push(ws.get_value());
            }

            // Raw-string forms of the standard escapes get converted to
            // their byte equivalents; other tokens are appended verbatim.
            match self.tokens.current_token().get_value().as_str() {
                r"\n" => string.push('\n'),
                r"\r" => string.push('\r'),
                r"\t" => string.push('\t'),
                "\\" => string.push('\\'),
                other => string.push_str(other),
            }

            for ws in &self.tokens.current_token().following_whitespace {
                string.push(ws.get_value());
            }

            self.tokens.increase_index();
        }

        if self.expect_token_type(&lexer::TokenType::DoubleString, "closing string delimiter") {
            self.tokens.increase_index();
        }

        EncryptedStringAST::new(PositionedValue::new(
            string,
            start,
            self.get_current_position(),
        ))
    }

    /// Parses a string literal, plain (`"…"`) or interpolated (`` `...${expr}...` ``).
    fn parse_string(&mut self) -> StringAST {
        let starting_position = self.get_current_position();

        let interpolated = matches!(
            self.tokens.current_token().get_type(),
            lexer::TokenType::InterpolatedString
        );

        // Preserve whitespace immediately after the opening quote so it
        // appears in the first text chunk.
        let mut preceeding_whitespace = String::new();
        for ws in &self.tokens.current_token().following_whitespace {
            preceeding_whitespace.push(ws.get_value());
        }

        self.tokens.increase_index(); // eat the opening quote

        let mut string_contents = Vec::new();

        if interpolated {
            let mut last_string = preceeding_whitespace;
            let mut last_start = self.get_current_position();
            let mut last_end: PositionData;
            let mut last_block_start: PositionData;
            let mut last_block_end: PositionData;

            let mut following_interpolation_whitespace = String::new();

            while !self.tokens.finished()
                && self.tokens.current_token().get_type() != &lexer::TokenType::InterpolatedString
            {
                // Interpolation site: `${ ... }`.
                if self.tokens.current_token().equals("$")
                    && self.tokens.get_token_forward(1).equals("{")
                {
                    // Whitespace captured after a previous closing `}`
                    // belongs to the text between the two interpolations.
                    if !following_interpolation_whitespace.is_empty() {
                        last_string.push_str(&following_interpolation_whitespace);
                        following_interpolation_whitespace.clear();
                    }

                    last_end = self.get_current_position();
                    string_contents.push(StringChunk::new_text(
                        last_start.clone(),
                        last_end.clone(),
                        Self::escape_interpolation_percents(&last_string),
                    ));
                    last_string = String::new();
                    self.tokens.increase_index();

                    let interpolated_block = self.parse_block("string interpolation");

                    last_block_start = interpolated_block.start;
                    last_block_end = interpolated_block.end;

                    // Capture whitespace after the closing `}` so it
                    // belongs to the next text chunk rather than being lost.
                    following_interpolation_whitespace.clear();
                    for ws in &self.tokens.get_token_backward(1).following_whitespace {
                        following_interpolation_whitespace.push(ws.get_value());
                    }

                    string_contents.push(StringChunk::new_interpolation(
                        last_block_start,
                        last_block_end,
                        interpolated_block.value,
                    ));

                    last_start = self.get_current_position();
                } else {
                    if !following_interpolation_whitespace.is_empty() {
                        last_string.push_str(&following_interpolation_whitespace);
                        following_interpolation_whitespace.clear();
                    }

                    last_string.push_str(
                        &self.parse_string_literal(vec!["`".to_owned(), "${".to_owned()]),
                    );
                }
            }

            if !following_interpolation_whitespace.is_empty() {
                last_string.push_str(&following_interpolation_whitespace);
            }

            if !last_string.is_empty() {
                last_end = self.get_current_position();
                string_contents.push(StringChunk::new_text(
                    last_start,
                    last_end,
                    Self::escape_interpolation_percents(&last_string),
                ));
            }
        } else {
            let mut string = preceeding_whitespace;
            let start_position = self.get_current_position();

            string.push_str(&self.parse_string_literal(vec!["\"".to_owned()]));

            let mut ending_position = self.tokens.current_token().get_end().clone();
            ending_position.file = self.file.clone();

            string_contents.push(StringChunk::new_text(
                start_position,
                ending_position,
                string,
            ));
        }

        let ending_position = self.get_current_position();

        let closes_correctly = (interpolated
            && self.tokens.current_token().get_type() == &lexer::TokenType::InterpolatedString)
            || self.tokens.current_token().get_type() == &lexer::TokenType::DoubleString;

        if closes_correctly {
            self.tokens.increase_index();
        } else {
            self.report_diagnostic(
                "unterminated string literal. Expected a closing `\"` to end this string",
            );
        }

        StringAST::new(
            starting_position,
            ending_position,
            interpolated,
            string_contents,
        )
    }

    /// Parses a character literal: `'x'`.
    ///
    /// Special-cased for whitespace characters, which the lexer attaches to
    /// the surrounding quote tokens rather than emitting as separate
    /// character tokens.
    pub fn parse_char(&mut self) -> CharAST {
        let starting_position = self.get_current_position();

        // Whitespace literal: the character is hiding in the opening
        // quote's following-whitespace run.
        if !self.tokens.current_token().following_whitespace.is_empty() {
            let value = self.tokens.current_token().following_whitespace[0].get_value();
            self.tokens.increase_index();

            let ending_position = self.get_current_position();
            if self.expect_token_type(&lexer::TokenType::Character, "character") {
                self.tokens.increase_index();
            }

            return CharAST::new(PositionedValue::new(
                value,
                starting_position,
                ending_position,
            ));
        }

        self.tokens.increase_index(); // eat opening '

        let character_value = self.parse_token_as_char();
        self.tokens.increase_index();

        if character_value.len() > 1 {
            self.report_diagnostic("character literal must contain exactly one character. Use a string literal (double quotes) for multi-character text");
        }

        let ending_position = self.get_current_position();

        if self.expect_token_type(&lexer::TokenType::Character, "character") {
            self.tokens.increase_index();
        }

        // Take the first char, falling back to space if the escape result
        // was empty (shouldn't happen but defensive).
        let ch = character_value.chars().next().unwrap_or(' ');

        CharAST::new(PositionedValue::new(ch, starting_position, ending_position))
    }

    /// Parses a numeric literal. Doesn't handle leading sign (that's a
    /// unary-expression concern).
    pub fn parse_number(&mut self) -> NumberAST {
        let starting_position = self.tokens.current_token().get_start().clone();
        let number_value = self
            .tokens
            .current_token()
            .get_value()
            .parse::<f64>()
            .unwrap();

        let is_float = self.tokens.current_token().get_value().contains('.');

        let ending_position = self.tokens.current_token().get_end().clone();
        self.tokens.increase_index();

        NumberAST::new(
            PositionedValue::new(number_value, starting_position, ending_position),
            !is_float,
        )
    }

    /// Parses the `null` literal.
    pub fn parse_null(&mut self) -> NullAST {
        let starting_position = self.tokens.current_token().get_start().clone();
        let ending_position = self.tokens.current_token().get_end().clone();
        self.tokens.increase_index();
        NullAST::new(starting_position, ending_position)
    }

    /// Parses a variable declaration: `name := value` or `name: Type = value`.
    pub fn parse_variable_declaration(&mut self) -> NewVariableAST {
        let starting_position = self.get_current_position();

        // The `let` keyword introduces the declaration.
        if matches!(self.tokens.current_token().get_type(), lexer::TokenType::Let) {
            self.tokens.increase_index();
        }

        let variable_name = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "variable name");
        self.tokens.increase_index();

        // `let x: T = v` declares with an explicit type; `let x = v` infers the
        // type from the initializer.
        let error: bool;
        let variable_type = if self.tokens.current_token().equals(":") {
            error = false;
            self.tokens.increase_index();

            let var_type = types::PekoType::from_tokens(self);

            if self.expect_token_value("=", "variable value") {
                self.tokens.increase_index();
            }

            Some(var_type)
        } else if self.tokens.current_token().equals("=") {
            error = false;
            self.tokens.increase_index();
            None
        } else {
            error = true;
            self.report_diagnostic("expected `:` for a typed declaration like `let x: int = 0`, or `=` for an inferred declaration like `let x = 0`");
            None
        };

        let variable_value = if error {
            PekoAST::Null(NullAST::new(
                self.get_current_position(),
                self.get_current_position(),
            ))
        } else {
            self.parse()
        };

        NewVariableAST::new(
            starting_position,
            self.get_current_position(),
            VisibilityData::open_visibility(),
            None,
            false,
            variable_name,
            variable_type,
            Box::new(variable_value),
        )
    }

    /// Parses a function declaration: `fn name<G>(args) => Ret { body }` or
    /// `fn name<G>(args) => Ret;` for an external function.
    pub fn parse_function_declaration(&mut self) -> FunctionDeclarationAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index();

        let function_name = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "function name");
        self.tokens.increase_index();

        // Optional generic parameter list.
        let mut function_generics: Vec<PositionedValue<String>> = Vec::new();
        if self.tokens.current_token().equals("<") {
            self.tokens.increase_index();

            while !self.tokens.finished()
                && !self.tokens.current_token().equals(">")
                && !self.tokens.current_token().equals("(")
            {
                function_generics.push(PositionedValue::from_token(self.tokens.current_token()));
                self.expect_token_type(&lexer::TokenType::Identifier, "generic parameter name");
                self.tokens.increase_index();

                if self.tokens.current_token().equals(",") {
                    self.tokens.increase_index();
                }
            }

            if self.tokens.current_token().equals(">") {
                self.tokens.increase_index();
            } else {
                self.report_diagnostic("expected `>` to close the generic parameter list");
            }
        }

        let (function_arguments, return_type, varargs_type, varargs_name) =
            self.parse_function_header(true);

        let mut end_position = self.get_current_position();

        // `;` body marker = external declaration; otherwise parse a block.
        let function_body = if self.tokens.current_token().equals(";") {
            None
        } else {
            let block = self.parse_block("function body");
            end_position = block.end.clone();
            Some(block)
        };

        FunctionDeclarationAST::new(
            starting_position,
            end_position,
            VisibilityData::open_visibility(),
            None,
            function_name,
            function_generics,
            function_arguments,
            return_type,
            function_body,
            varargs_type,
            varargs_name,
            0,
        )
    }

    /// Parses a closure declaration: `closure[captures](args) -> Ret { body }`.
    pub fn parse_closure_declaration(&mut self) -> ClosureAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat 'closure'

        let mut captures = Vec::new();

        if self.tokens.current_token().equals("[") {
            self.tokens.increase_index();

            while !self.tokens.finished() && !self.tokens.current_token().equals("]") {
                captures.push(PositionedValue::from_token(self.tokens.current_token()));
                self.expect_token_type(
                    &lexer::TokenType::Identifier,
                    "closure capture variable name",
                );
                self.tokens.increase_index();

                if self.tokens.current_token().equals(",") {
                    self.tokens.increase_index();
                }
            }

            if self.expect_token_value("]", "closure capture list") {
                self.tokens.increase_index();
            }
        }

        let (function_arguments, return_type, _, _) = self.parse_function_header(false);
        let function_body = self.parse_block("closure body");

        ClosureAST::new(
            starting_position,
            function_body.end.clone(),
            function_arguments,
            captures,
            return_type,
            function_body,
        )
    }

    /// Parses a class declaration: `class Name<G> from Parent { attrs; methods }`.
    pub fn parse_class_declaration(&mut self) -> ClassAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat 'class'

        let class_name = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "class name");
        self.tokens.increase_index();

        // Optional generic parameter list.
        let mut generics: Vec<PositionedValue<String>> = Vec::new();
        if self.tokens.current_token().equals("<") {
            self.tokens.increase_index();

            while !self.tokens.finished()
                && !self.tokens.current_token().equals(">")
                && !self.tokens.current_token().equals("{")
            {
                generics.push(PositionedValue::from_token(self.tokens.current_token()));
                self.expect_token_type(&lexer::TokenType::Identifier, "generic parameter name");
                self.tokens.increase_index();

                if self.tokens.current_token().equals(",") {
                    self.tokens.increase_index();
                }
            }

            if self.tokens.current_token().equals(">") {
                self.tokens.increase_index();
            } else {
                self.report_diagnostic("expected `>` to close the generic parameter list");
            }
        }

        // Optional `from Parent1, Parent2, ...` clause.
        let mut derives_from = Vec::new();

        if matches!(
            self.tokens.current_token().get_type(),
            lexer::TokenType::From
        ) {
            self.tokens.increase_index();

            while !self.tokens.finished() && !self.tokens.current_token().equals("{") {
                let index_before = self.tokens.get_index();
                derives_from.push(types::PekoType::from_tokens(self));
                // If from_tokens consumed nothing the cursor is stuck. Force
                // one token of progress so the loop always terminates.
                if !self.tokens.finished()
                    && !self.tokens.current_token().equals("{")
                    && self.tokens.get_index() == index_before
                {
                    self.tokens.increase_index();
                }

                if self.tokens.current_token().equals(",") {
                    self.tokens.increase_index();
                }
            }
        }

        if self.expect_token_value("{", "class body") {
            self.tokens.increase_index();
        }

        let mut attributes: IndexMap<PositionedValue<String>, ClassAttributeData> = IndexMap::new();
        let mut methods: Vec<ClassMethod> = Vec::new();

        let mut method_index = 0;

        while !self.tokens.finished() && !self.tokens.current_token().equals("}") {
            // Each member can be preceded by visibility and doc-info.
            let mut visibility: Option<VisibilityData> = None;
            let mut docinfo: Option<DocInfo> = None;

            while (self.tokens.current_token().equals("[")
                && !self.tokens.get_token_forward(1).equals("operator")
                && visibility.is_none())
                || (self.tokens.current_token().equals("///") && docinfo.is_none())
            {
                if self.tokens.current_token().equals("[") {
                    visibility = Some(self.parse_visibility());
                } else {
                    docinfo = Some(self.parse_doc_info());
                }
            }

            let visibility = visibility.unwrap_or_else(VisibilityData::open_visibility);

            match self.tokens.current_token().get_type() {
                // Attribute declaration: `name: Type`.
                lexer::TokenType::Identifier => {
                    let attribute_name = PositionedValue::from_token(self.tokens.current_token());
                    self.tokens.increase_index();

                    let attribute_type = if self.expect_token_value(":", "class attribute type") {
                        self.tokens.increase_index();
                        types::PekoType::from_tokens(self)
                    } else {
                        types::PekoType::error_type()
                    };

                    attributes.insert(
                        attribute_name,
                        ClassAttributeData::new(visibility, docinfo, Box::new(attribute_type)),
                    );
                }

                // Constructor declaration.
                lexer::TokenType::Constructor => {
                    let name = PositionedValue::from_token(self.tokens.current_token());
                    let start_position = self.get_current_position();
                    self.tokens.increase_index();

                    if self.expect_token_value("(", "constructor parameters") {
                        self.tokens.increase_index();
                    }

                    let mut constructor_arguments: IndexMap<
                        PositionedValue<String>,
                        DeclarationArgumentData,
                    > = IndexMap::new();

                    let mut varargs_type = None;
                    let mut varargs_name = PositionedValue::create_no_position(String::new());

                    while !self.tokens.finished() && !self.tokens.current_token().equals(")") {
                        if self.tokens.current_token().equals("Args") {
                            // Variadic argument.
                            self.tokens.increase_index();

                            if self.tokens.current_token().equals("<") {
                                self.tokens.increase_index();
                            } else {
                                self.report_diagnostic("expected `<` to begin the variadic argument type in a constructor's `Args<T> => name` declaration");
                            }

                            varargs_type = Some(types::PekoType::from_tokens(self));

                            if self.tokens.current_token().equals(">") {
                                self.tokens.increase_index();
                            } else {
                                self.report_diagnostic("expected `>` to close the variadic argument type in a constructor's `Args<T> => name` declaration");
                            }

                            if self.tokens.current_token().equals("=>") {
                                self.tokens.increase_index();
                            } else {
                                self.report_diagnostic("expected `=>` between the variadic argument type and its name in a constructor's `Args<T> => name` declaration");
                            }

                            varargs_name = PositionedValue::from_token(self.tokens.current_token());
                            self.expect_token_type(&lexer::TokenType::Identifier, "parameter name");
                            self.tokens.increase_index();
                        } else {
                            // Normal argument. Constness is carried by the type
                            // (`name: const type`), not an argument modifier.
                            let visibility = VisibilityData::open_visibility();

                            let argument_definition_start = self.get_current_position();
                            let argument_name =
                                PositionedValue::from_token(self.tokens.current_token());
                            self.expect_token_type(
                                &lexer::TokenType::Identifier,
                                "constructor parameter name",
                            );
                            self.tokens.increase_index();

                            let argument_type =
                                if self.expect_token_value(":", "constructor argument type") {
                                    self.tokens.increase_index();
                                    types::PekoType::from_tokens(self)
                                } else {
                                    types::PekoType::error_type()
                                };

                            let default_value = if self.tokens.current_token().equals("=") {
                                self.tokens.increase_index();
                                Some(self.parse())
                            } else {
                                None
                            };

                            constructor_arguments.insert(
                                argument_name,
                                DeclarationArgumentData::new(
                                    argument_definition_start,
                                    self.get_current_position(),
                                    argument_type,
                                    default_value,
                                    visibility,
                                ),
                            );

                            if self.tokens.current_token().equals(",") {
                                self.tokens.increase_index();
                            }
                        }
                    }

                    if self.expect_token_value(")", "constructor parameters") {
                        self.tokens.increase_index();
                    }

                    // Optional `=> super(...)` call.
                    let super_call = match self.tokens.current_token().get_type() {
                        lexer::TokenType::Returns => {
                            self.tokens.increase_index();

                            let function_call = self.parse_identifier();
                            match function_call {
                                PekoAST::FunctionCall(ast) => Some(ast),
                                _ => {
                                    self.report_diagnostic(
                                        "expected a function call after `super`. Use `super(arg1, arg2, ...)` to call the parent class constructor",
                                    );
                                    None
                                }
                            }
                        }
                        _ => None,
                    };

                    let constructor_body = self.parse_block("class constructor body");

                    methods.push(ClassMethod::Constructor(
                        ClassMethodInfo::new(
                            start_position,
                            constructor_body.end.clone(),
                            visibility,
                            docinfo,
                            constructor_arguments,
                            constructor_body,
                            varargs_type,
                            varargs_name,
                            name,
                        ),
                        super_call,
                    ));

                    method_index += 1;
                }

                // Regular method.
                lexer::TokenType::FunctionDeclarator => {
                    let mut method = self.parse_function_declaration();
                    method.class_order = method_index;
                    methods.push(ClassMethod::Method(
                        ClassMethodInfo::new(
                            method.start.clone(),
                            method.end.clone(),
                            visibility,
                            docinfo,
                            method.arguments,
                            method
                                .function_body
                                .unwrap_or_else(|| PositionedValue::create_no_position(Vec::new())),
                            method.varargs_type,
                            method.varargs_name,
                            method.function_name,
                        ),
                        method.return_type,
                    ));

                    method_index += 1;
                }

                lexer::TokenType::Comment => self.skip_comment(),

                // Operator overload: `[operator <op>](args) { body }`.
                _ => {
                    let start_position = self.get_current_position();

                    if !self.tokens.get_token_forward(1).equals("operator") {
                        self.tokens.increase_index();
                        continue;
                    }

                    self.tokens.increase_index(); // eat '['
                    self.tokens.increase_index(); // eat 'operator'

                    let mut operator_name = String::from("[operator ");

                    // Capture the overloaded operator name, tracking
                    // bracket nesting so operators that themselves contain
                    // `[`/`]` parse correctly.
                    let mut closing_braces = 1;
                    while !self.tokens.finished() && closing_braces > 0 {
                        if self.tokens.current_token().equals("[") {
                            closing_braces += 1;
                        } else if self.tokens.current_token().equals("]") {
                            closing_braces -= 1;
                        }

                        if closing_braces > 0 {
                            operator_name.push_str(self.tokens.current_token().get_value());
                        }

                        self.tokens.increase_index();
                    }

                    operator_name.push(']');

                    let operator_name = PositionedValue::new(
                        operator_name,
                        start_position.clone(),
                        self.get_current_position(),
                    );

                    let (function_arguments, return_type, varargs_type, varargs_name) =
                        self.parse_function_header(true);
                    let function_body = self.parse_block("class operator body");

                    methods.push(ClassMethod::Method(
                        ClassMethodInfo::new(
                            start_position,
                            function_body.end.clone(),
                            visibility,
                            docinfo,
                            function_arguments,
                            function_body,
                            varargs_type,
                            varargs_name,
                            operator_name,
                        ),
                        return_type,
                    ));

                    method_index += 1;
                }
            }
        }

        if self.expect_token_value("}", "class body") {
            self.tokens.increase_index();
        }

        ClassAST::new(
            starting_position,
            self.get_current_position(),
            VisibilityData::open_visibility(),
            None,
            class_name,
            derives_from,
            attributes,
            methods,
            generics,
        )
    }

    /// Parses an enum declaration: `enum Name { Variant1, Variant2, ... }`.
    pub fn parse_enum_declaration(&mut self) -> EnumDeclarationAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat 'enum'

        let enum_name = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "enum name");
        self.tokens.increase_index();

        if self.expect_token_value("{", "enum body") {
            self.tokens.increase_index();
        }

        let mut variants: Vec<PositionedValue<String>> = Vec::new();

        while !self.tokens.finished() && !self.tokens.current_token().equals("}") {
            // Eat comments and stray separators before each variant.
            while !self.tokens.finished()
                && (self.tokens.current_token().is_comment()
                    || self.tokens.current_token().equals(","))
            {
                if self.tokens.current_token().is_comment() {
                    self.skip_comment();
                } else {
                    self.tokens.increase_index();
                }
            }

            if self.tokens.finished() || self.tokens.current_token().equals("}") {
                break;
            }

            let index_before = self.tokens.get_index();
            variants.push(PositionedValue::from_token(self.tokens.current_token()));
            self.expect_token_type(&lexer::TokenType::Identifier, "enum variant name");
            self.tokens.increase_index();

            // Force progress so a stray token can never spin the loop.
            if !self.tokens.finished() && self.tokens.get_index() == index_before {
                self.tokens.increase_index();
            }
        }

        if self.expect_token_value("}", "enum body") {
            self.tokens.increase_index();
        }

        EnumDeclarationAST::new(
            starting_position,
            self.get_current_position(),
            VisibilityData::open_visibility(),
            None,
            enum_name,
            variants,
        )
    }

    /// Parses a `switch` over an enum: `switch subject { Enum::Variant => { ...
    /// }, _ => { ... } }`.
    pub fn parse_switch_statement(&mut self) -> SwitchStatementAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat 'switch'

        let subject = self.parse_expression();

        if self.expect_token_value("{", "switch body") {
            self.tokens.increase_index();
        }

        let mut arms: Vec<SwitchArm> = Vec::new();

        while !self.tokens.finished() && !self.tokens.current_token().equals("}") {
            // Eat comments and stray separators between arms.
            while !self.tokens.finished()
                && (self.tokens.current_token().is_comment()
                    || self.tokens.current_token().equals(","))
            {
                if self.tokens.current_token().is_comment() {
                    self.skip_comment();
                } else {
                    self.tokens.increase_index();
                }
            }

            if self.tokens.finished() || self.tokens.current_token().equals("}") {
                break;
            }

            let arm_start = self.get_current_position();
            let index_before = self.tokens.get_index();

            // `_` is the default arm; anything else is an `Enum::Variant`
            // pattern parsed as an expression.
            let pattern = if self.tokens.current_token().equals("_") {
                self.tokens.increase_index();
                None
            } else {
                Some(Box::new(self.parse_expression()))
            };

            if self.expect_token_value("=>", "switch arm") {
                self.tokens.increase_index();
            }

            let body = self.parse_block("switch arm body");

            arms.push(SwitchArm::new(
                arm_start,
                self.get_current_position(),
                pattern,
                body,
            ));

            // Force progress so a malformed arm can never spin the loop.
            if !self.tokens.finished() && self.tokens.get_index() == index_before {
                self.tokens.increase_index();
            }
        }

        if self.expect_token_value("}", "switch body") {
            self.tokens.increase_index();
        }

        SwitchStatementAST::new(
            starting_position,
            self.get_current_position(),
            Box::new(subject),
            arms,
        )
    }

    /// Parses an identifier-led expression: variable reference, object
    /// access, array access, function call, variable assignment (plain or
    /// compound), or optional unwrap. Returns the resulting AST.
    pub fn parse_identifier(&mut self) -> PekoAST {
        let starting_position = self.get_current_position();

        let identifier = PositionedValue::from_token(self.tokens.current_token());
        self.tokens.increase_index();

        let mut identifier_reference =
            PekoAST::VariableReference(VariableReferenceAST::new(identifier));

        // Walk any chain of access / call / assignment suffixes.
        loop {
            match self.tokens.current_token().get_type() {
                // Generic function call: `<T, U>(args)`.
                lexer::TokenType::BooleanOperator => {
                    if !self.tokens.current_token().equals("<") {
                        break;
                    }

                    // Look ahead to confirm the `<` opens a generic-args list
                    // (and isn't a less-than comparison).
                    let mut index = 1;
                    let mut is_type =
                        types::PekoType::test_next_tokens_for_type_with_index(self, &mut index);

                    while !self.tokens.finished()
                        && is_type
                        && self.tokens.get_token_forward(index).equals(",")
                    {
                        self.tokens.index_increase_index(&mut index);
                        is_type =
                            types::PekoType::test_next_tokens_for_type_with_index(self, &mut index);
                    }

                    if !is_type || !self.tokens.get_token_forward(index).equals(">") {
                        break;
                    }

                    let was_skip_accessors = self.has_state("skip_accessors");
                    self.remove_state("skip_accessors");

                    let (function_generics, arguments, ending_position) = self.parse_arguments();

                    if was_skip_accessors {
                        self.set_state("skip_accessors");
                    }

                    identifier_reference = PekoAST::FunctionCall(FunctionCallAST::new(
                        identifier_reference.get_start().clone(),
                        ending_position,
                        Box::new(identifier_reference),
                        function_generics,
                        arguments,
                    ));
                }

                // `.member` access.
                lexer::TokenType::ObjectAccessor => {
                    if self.has_state("skip_accessors") {
                        break;
                    }

                    self.tokens.increase_index();
                    self.set_state("skip_accessors");

                    let access = self.secondary_parse();

                    identifier_reference = PekoAST::ObjectAccess(ObjectAccessAST::new(
                        Box::new(identifier_reference),
                        Box::new(access),
                    ));

                    self.remove_state("skip_accessors");
                }

                // `[index]` access.
                lexer::TokenType::LBracket => {
                    if self.has_state("skip_accessors") {
                        break;
                    }

                    self.tokens.increase_index();

                    let accessor = self.parse();

                    if self.expect_token_value("]", "array access") {
                        self.tokens.increase_index();
                    }

                    identifier_reference = PekoAST::ArrayAccess(ArrayAccessAST::new(
                        identifier_reference.get_start().clone(),
                        self.get_current_position(),
                        Box::new(identifier_reference),
                        Box::new(accessor),
                    ));
                }

                // `(args)` function-call suffix.
                lexer::TokenType::LParen => {
                    let was_skip_accessors = self.has_state("skip_accessors");
                    self.remove_state("skip_accessors");

                    let (function_generics, arguments, ending_position) = self.parse_arguments();

                    if was_skip_accessors {
                        self.set_state("skip_accessors");
                    }

                    identifier_reference = PekoAST::FunctionCall(FunctionCallAST::new(
                        identifier_reference.get_start().clone(),
                        ending_position,
                        Box::new(identifier_reference),
                        function_generics,
                        arguments,
                    ));
                }

                // Compound assignment: `+=`, `-=`, etc.
                lexer::TokenType::AssignmentWithOperator => {
                    let was_skip_accessors = self.has_state("skip_accessors");
                    self.remove_state("skip_accessors");

                    // The operator's first char identifies the arithmetic op.
                    let assignment_operator = self
                        .tokens
                        .current_token()
                        .get_value()
                        .chars()
                        .next()
                        .map(|c| c.to_string())
                        .unwrap_or_default();
                    self.tokens.increase_index();

                    let value_assigned = self.parse();

                    if was_skip_accessors {
                        self.set_state("skip_accessors");
                    }

                    identifier_reference =
                        PekoAST::VariableReassignment(VariableReassignmentAST::new(
                            Box::new(identifier_reference),
                            Box::new(value_assigned),
                            Some(assignment_operator),
                        ));
                    break;
                }

                // Plain assignment: `=`.
                lexer::TokenType::Equals => {
                    let was_skip_accessors = self.has_state("skip_accessors");
                    self.remove_state("skip_accessors");

                    self.tokens.increase_index();

                    let value = self.parse();

                    identifier_reference =
                        PekoAST::VariableReassignment(VariableReassignmentAST::new(
                            Box::new(identifier_reference),
                            Box::new(value),
                            None,
                        ));

                    if was_skip_accessors {
                        self.set_state("skip_accessors");
                    }
                    break;
                }

                // Optional unwrap: `?`.
                lexer::TokenType::QuestionMark => {
                    if self.has_state("skip_accessors") {
                        break;
                    }

                    let ending_position = self.tokens.current_token().get_end().clone();
                    self.tokens.increase_index();

                    identifier_reference = PekoAST::Unwrap(UnwrapAST::new(
                        starting_position.clone(),
                        ending_position,
                        Box::new(identifier_reference),
                    ));
                }

                _ => break,
            }
        }

        identifier_reference
    }

    /// Parses a module-qualified access: `module::nested::symbol`.
    ///
    /// The accessor is the identifier inside the foreign module, plus an
    /// optional generic-args list and an optional immediate call `(args)`,
    /// and an optional assignment. Any further chain (`.method(...)`,
    /// `[i]`, suffix calls on the call's return value) belongs to the
    /// caller's module and is parsed by the surrounding expression loop
    /// as a post-value suffix.
    pub fn parse_module_access(&mut self) -> ModuleAccessAST {
        let starting_position = self.get_current_position();
        let mut modules = Vec::new();

        let mut current_ending = self.tokens.current_token().get_end().clone();
        while !self.tokens.finished() && self.tokens.get_token_forward(1).equals("::") {
            modules.push(PositionedValue::from_token(self.tokens.current_token()));
            self.expect_token_type(&lexer::TokenType::Identifier, "module name");
            self.tokens.increase_index();

            current_ending = self.tokens.current_token().get_end().clone();
            self.tokens.increase_index();
        }

        // skip_accessors caps the accessor at one identifier and an
        // optional generic-args call or immediate call. Post-value
        // suffixes such as `.method(...)`, `[i]`, and `?` are left for
        // the surrounding expression loop to chain onto the
        // ModuleAccess value, so they execute in the caller's module.
        let was_skip_accessors = self.has_state("skip_accessors");
        self.set_state("skip_accessors");

        let accessor = self.secondary_parse();

        if !was_skip_accessors {
            self.remove_state("skip_accessors");
        }

        ModuleAccessAST::new(
            starting_position,
            current_ending,
            modules,
            Box::new(accessor),
        )
    }

    /// Parses an `if` / `else if` / `else` chain.
    pub fn parse_if_statement(&mut self) -> IfStatementAST {
        let starting_position = self.get_current_position();

        let mut conditional_bodies: Vec<ConditionBody> = Vec::new();
        let mut else_body: Option<PositionedValue<Vec<PekoAST>>> = None;

        // Tracks whether the previous iteration consumed an `else` keyword
        // so we know whether the next `if` is `else if` or starts a new
        // statement. Initially true so the very first `if` parses.
        let mut previous_was_else = true;

        loop {
            match self.tokens.current_token().get_type() {
                lexer::TokenType::If => {
                    if !previous_was_else {
                        break;
                    }
                    previous_was_else = false;

                    self.tokens.increase_index();
                    let condition = self.parse();
                    let body = self.parse_block("if statement body");

                    conditional_bodies.push(ConditionBody::new(Box::new(condition), body));
                }

                lexer::TokenType::Else => {
                    previous_was_else = true;
                    self.tokens.increase_index();
                }

                lexer::TokenType::LBrace => {
                    else_body = Some(self.parse_block("else body"));
                }

                _ => break,
            }
        }

        IfStatementAST::new(
            starting_position,
            self.get_current_position(),
            conditional_bodies,
            else_body,
        )
    }

    /// Parses a `while` loop.
    pub fn parse_while_loop(&mut self) -> WhileLoopAST {
        let starting_position = self.get_current_position();

        self.tokens.increase_index();
        let condition = self.parse();
        let body = self.parse_block("while loop body");

        WhileLoopAST::new(
            starting_position,
            body.end.clone(),
            ConditionBody::new(Box::new(condition), body),
        )
    }

    /// Parses a `for` loop: `for item in iterable { body }`.
    pub fn parse_for_loop(&mut self) -> ForLoopAST {
        let starting_position = self.get_current_position();

        self.tokens.increase_index();

        let iterator_identifier = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "for loop item id");
        self.tokens.increase_index();

        if self.expect_token_type(&lexer::TokenType::In, "for loop") {
            self.tokens.increase_index();
        }

        let iterator = self.parse();
        let loop_body = self.parse_block("for loop body");

        ForLoopAST::new(
            starting_position,
            loop_body.end.clone(),
            iterator_identifier,
            Box::new(iterator),
            loop_body,
        )
    }

    /// Parses a `break` statement.
    pub fn parse_break(&mut self) -> BreakAST {
        let starting_position = self.get_current_position();
        let ending_position = self.tokens.current_token().get_end().clone();
        self.tokens.increase_index();
        BreakAST::new(starting_position, ending_position)
    }

    /// Parses a `continue` statement.
    pub fn parse_continue(&mut self) -> ContinueAST {
        let starting_position = self.get_current_position();
        let ending_position = self.tokens.current_token().get_end().clone();
        self.tokens.increase_index();
        ContinueAST::new(starting_position, ending_position)
    }

    /// Parses a `return [value]` statement.
    pub fn parse_return(&mut self) -> ReturnAST {
        let starting_position = self.get_current_position();
        let mut ending_position = self.tokens.current_token().get_end().clone();
        self.tokens.increase_index();

        let return_value = if self.tokens.current_token().equals(";") {
            None
        } else {
            let return_value = self.parse();
            ending_position = return_value.get_end().clone();
            Some(Box::new(return_value))
        };

        ReturnAST::new(starting_position, ending_position, return_value)
    }

    /// Parses a module declaration: `module name { ... }`.
    pub fn parse_module_creation(&mut self) -> ModuleCreationAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat 'module'

        let module_name = PositionedValue::from_token(self.tokens.current_token());
        self.expect_token_type(&lexer::TokenType::Identifier, "module name");
        self.tokens.increase_index();

        let module_body = self.parse_block("module body");

        ModuleCreationAST::new(
            starting_position,
            module_body.end.clone(),
            VisibilityData::open_visibility(),
            None,
            module_name,
            module_body,
        )
    }

    /// Parses a `{ ... }` symbol unpack list from an `import` statement.
    pub fn parse_unpack_list(&mut self) -> Vec<UnpackItem> {
        let mut symbols_to_unwrap = Vec::new();

        if !self.tokens.current_token().equals("{") {
            return symbols_to_unwrap;
        }
        self.tokens.increase_index();

        // Glob unpack: `{ * }`.
        if self.tokens.current_token().equals("*") {
            self.tokens.increase_index();
            if self.expect_token_value("}", "import statement") {
                self.tokens.increase_index();
            }
            return vec![UnpackItem::All];
        }

        while !self.tokens.finished() && !self.tokens.current_token().equals("}") {
            let current_identifier = PositionedValue::from_token(self.tokens.current_token());
            self.tokens.increase_index();

            if self.tokens.current_token().equals("::") {
                self.tokens.increase_index();
                symbols_to_unwrap.push(UnpackItem::ModuleSymbols(ModuleUnpacks::new(
                    current_identifier,
                    self.parse_unpack_list(),
                )));
            } else {
                symbols_to_unwrap.push(UnpackItem::Symbol(current_identifier));
            }

            if self.tokens.current_token().equals(",") {
                self.tokens.increase_index();
            }
        }

        if self.expect_token_value("}", "import statement") {
            self.tokens.increase_index();
        }

        symbols_to_unwrap
    }

    /// Parses an `import` statement, including any unpack list, version pin,
    /// and `as` alias.
    pub fn parse_import(&mut self) -> ImportStatementAST {
        let starting_position = self.get_current_position();

        self.tokens.increase_index();

        // Optional `{ ... } from` unpack prefix.
        let symbols_to_unwrap = if self.tokens.current_token().equals("{") {
            let unwrap = self.parse_unpack_list();

            if self.expect_token_value("from", "import statement") {
                self.tokens.increase_index();
            }

            unwrap
        } else {
            Vec::new()
        };

        // Module path: each `::`-separated identifier becomes one segment.
        let mut module_path = Vec::new();

        let first_segment = PositionedValue::from_token(self.tokens.current_token());
        let mut ending_position = self.tokens.current_token().get_end().clone();
        module_path.push(first_segment);

        self.expect_token_type(&lexer::TokenType::Identifier, "imported module name");
        self.tokens.increase_index();

        while !self.tokens.finished() && self.tokens.current_token().equals("::") {
            self.tokens.increase_index();

            let segment = PositionedValue::from_token(self.tokens.current_token());
            ending_position = self.tokens.current_token().get_end().clone();
            module_path.push(segment);

            self.expect_token_type(&lexer::TokenType::Identifier, "imported module name");
            self.tokens.increase_index();
        }

        // Optional `@"version"`.
        let module_version = if self.tokens.current_token().equals("@") {
            self.tokens.increase_index();

            self.expect_token_value("\"", "module version");
            let version_string = self.parse_string();

            ending_position = version_string.end.clone();
            let mut version =
                PositionedValue::create_no_position(version_string.chunks[0].get_text());
            version.start = version_string.start;
            version.end = version_string.end;
            Some(version)
        } else {
            None
        };

        // Optional `as alias`.
        let import_as = match self.tokens.current_token().get_type() {
            lexer::TokenType::As => {
                self.tokens.increase_index();
                self.expect_token_type(&lexer::TokenType::Identifier, "module alias");
                let import_as = Some(PositionedValue::from_token(self.tokens.current_token()));
                ending_position = self.tokens.current_token().get_end().clone();
                self.tokens.increase_index();
                import_as
            }
            _ => None,
        };

        ImportStatementAST::new(
            starting_position,
            ending_position,
            module_path,
            import_as,
            symbols_to_unwrap,
            module_version,
        )
    }

    /// Parses a `link object as type` statement.
    pub fn parse_link(&mut self) -> LinkStatementAST {
        let starting_position = self.get_current_position();

        self.tokens.increase_index();
        let module_name_start = self.get_current_position();

        let mut module_name = String::new();
        module_name.push_str(self.tokens.current_token().get_value());
        module_name.push('/');
        self.expect_token_type(&lexer::TokenType::Identifier, "linker file");
        self.tokens.increase_index();

        while !self.tokens.finished() && self.tokens.current_token().equals("::") {
            self.tokens.increase_index();
            module_name.push_str(self.tokens.current_token().get_value());
            module_name.push('/');
            self.expect_token_type(&lexer::TokenType::Identifier, "linker file");
            self.tokens.increase_index();
        }

        module_name.pop();
        let module_name_end = self.get_current_position();

        let mut ending_position = self.tokens.current_token().get_end().clone();

        let link_as = match self.tokens.current_token().get_type() {
            lexer::TokenType::As => {
                self.tokens.increase_index();
                self.expect_token_type(&lexer::TokenType::Identifier, "linker file type");
                let link_as = PositionedValue::from_token(self.tokens.current_token());
                ending_position = self.tokens.current_token().get_end().clone();
                self.tokens.increase_index();
                link_as
            }
            _ => PositionedValue::create_no_position(String::new()),
        };

        LinkStatementAST::new(
            starting_position,
            ending_position,
            PositionedValue::new(module_name, module_name_start, module_name_end),
            link_as,
        )
    }

    /// Parses a `style stylesheet::path` statement.
    pub fn parse_style(&mut self) -> StyleStatementAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat 'style'

        let stylesheet_start = self.get_current_position();

        let mut stylesheet_name = String::new();
        stylesheet_name.push_str(self.tokens.current_token().get_value());
        stylesheet_name.push('/');

        self.expect_token_type(&lexer::TokenType::Identifier, "stylesheet file");
        let mut ending_position = self.tokens.current_token().get_end().clone();
        self.tokens.increase_index();

        while !self.tokens.finished() && self.tokens.current_token().equals("::") {
            self.tokens.increase_index();
            stylesheet_name.push_str(self.tokens.current_token().get_value());
            stylesheet_name.push('/');
            self.expect_token_type(&lexer::TokenType::Identifier, "stylesheet file");
            ending_position = self.tokens.current_token().get_end().clone();
            self.tokens.increase_index();
        }

        stylesheet_name.pop();

        StyleStatementAST::new(
            starting_position,
            ending_position,
            PositionedValue::new(
                stylesheet_name,
                stylesheet_start,
                self.get_current_position(),
            ),
        )
    }

    /// Parses a `platform <id> | <id> { ... }` or `arch <id> | <id> { ... }`
    /// conditional-compilation block.
    pub fn parse_platform(&mut self) -> PlatformStatementAST {
        let starting_position = self.get_current_position();

        let is_platform = self.tokens.current_token().equals("platform");
        self.tokens.increase_index();

        let mut platforms = Vec::new();
        platforms.push(PositionedValue::from_token(self.tokens.current_token()));
        self.tokens.increase_index();

        while !self.tokens.finished() && self.tokens.current_token().equals("|") {
            self.tokens.increase_index();
            platforms.push(PositionedValue::from_token(self.tokens.current_token()));
            self.tokens.increase_index();
        }

        let body = self.parse_block("platform body");

        PlatformStatementAST::new(
            starting_position,
            body.end.clone(),
            !is_platform,
            platforms,
            body,
        )
    }

    /// Parses an array literal: `#[a, b, ...]`.
    pub fn parse_array(&mut self) -> ArrayAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat '#'
        self.tokens.increase_index(); // eat '['

        let mut values = Vec::new();
        while !self.tokens.finished() && !self.tokens.current_token().equals("]") {
            let index_before = self.tokens.get_index();
            values.push(self.parse());
            // If parse() consumed nothing the cursor is stuck. Force one token
            // of progress so the loop always terminates.
            if !self.tokens.finished()
                && !self.tokens.current_token().equals("]")
                && self.tokens.get_index() == index_before
            {
                self.tokens.increase_index();
            }
            if self.tokens.current_token().equals(",") {
                self.tokens.increase_index();
            }
        }

        let ending_position = self.tokens.current_token().get_end().clone();
        if !self.tokens.current_token().equals("]") {
            self.report_diagnostic("expected `]` to close this array literal");
        } else {
            self.tokens.increase_index();
        }

        ArrayAST::new(starting_position, ending_position, values)
    }

    /// Parses a map literal: `#{key: value, ...}`.
    pub fn parse_map(&mut self) -> MapAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index(); // eat '#'
        self.tokens.increase_index(); // eat '{'

        let mut key_values = Vec::new();
        while !self.tokens.finished() && !self.tokens.current_token().equals("}") {
            let key = self.secondary_parse();

            if !self.tokens.current_token().equals(":") {
                self.report_diagnostic(
                    "expected `:` between map key and value, e.g. `{key: value}`",
                );
            } else {
                self.tokens.increase_index();
            }

            key_values.push((key, self.secondary_parse()));

            if !self.tokens.current_token().equals(",") && !self.tokens.current_token().equals("}")
            {
                self.report_diagnostic(
                    "expected `,` to separate map entries, or `}` to close the map literal",
                );
            } else if !self.tokens.current_token().equals("}") {
                self.tokens.increase_index();
            }
        }

        let ending_position = self.tokens.current_token().get_end().clone();
        if !self.tokens.current_token().equals("}") {
            self.report_diagnostic("expected `}` to close this map literal");
        } else {
            self.tokens.increase_index();
        }

        MapAST::new(starting_position, ending_position, key_values)
    }

    /// Parses a PekoX (XML-style) tag: `<tag attr=value>children</tag>` or
    /// self-closing `<tag />`.
    pub fn parse_pekox(&mut self) -> PekoXTagAST {
        let starting_position = self.get_current_position();
        self.tokens.increase_index();

        let element_tag = if self.expect_token_type(&lexer::TokenType::Identifier, "pekox tag") {
            self.tokens.current_token().get_value().clone()
        } else {
            String::new()
        };
        self.tokens.increase_index();

        let mut element_attributes = HashMap::new();
        let mut element_events = HashMap::new();

        let attributes_start = self.get_current_position();

        // Attributes and events, terminated by `>` or `/>`.
        while !(self.tokens.finished()
            || self.tokens.current_token().equals(">")
            || self.tokens.current_token().equals("/")
                && self.tokens.get_token_forward(1).equals(">"))
        {
            let mut errd = false;

            let mut attribute_key =
                if self.expect_token_type(&lexer::TokenType::Identifier, "pekox attribute key") {
                    self.tokens.current_token().get_value().clone()
                } else {
                    errd = true;
                    String::new()
                };

            // `class` is reserved; the framework convention is `className`.
            if attribute_key == "className" {
                attribute_key = String::from("class");
            }

            self.tokens.increase_index();

            if self.expect_token_value("=", "pekox attribute") {
                self.tokens.increase_index();
            } else {
                errd = true;
            }

            // Event handlers (`onclick`, `onhover`, `oninput`) carry block
            // bodies rather than expression values.
            if attribute_key.starts_with("on") {
                let event_body = self.parse_block("event body");
                if !errd {
                    element_events.insert(attribute_key, event_body);
                }
            } else {
                let attribute_value = self.secondary_parse();
                if !errd {
                    element_attributes.insert(attribute_key, attribute_value);
                }
            }
        }

        let attributes_end = self.get_current_position();

        // Self-closing form: `/>`.
        if self.tokens.current_token().equals("/") {
            self.tokens.increase_index();
            let ending_position = self.tokens.current_token().get_end().clone();
            self.tokens.increase_index();

            return PekoXTagAST::new(
                starting_position,
                ending_position,
                attributes_start,
                attributes_end,
                element_tag,
                element_attributes,
                element_events,
                Vec::new(),
                Vec::new(),
            );
        }

        // Closing `>` of the opening tag.
        if self.tokens.current_token().equals(">") {
            self.tokens.increase_index();
        }

        let mut children = Vec::new();

        // Parse children until the `</tag>` closing form.
        while !self.tokens.finished()
            && (!self.tokens.current_token().equals("<")
                || !self.tokens.get_token_forward(1).equals("/"))
        {
            if self.tokens.current_token().equals("<") {
                let index_before = self.tokens.get_index();
                children.push(self.parse());
                // If parse() consumed nothing (bare non-tag '<') force past it
                // so the loop always terminates.
                if !self.tokens.finished() && self.tokens.get_index() == index_before {
                    self.tokens.increase_index();
                }
            } else if self.tokens.current_token().equals("{") {
                // `{expr}` inline child insertion.
                self.tokens.increase_index();
                children.push(self.parse());

                let child_start = self.get_current_position();
                let closed = self.tokens.current_token().equals("}");
                if closed {
                    self.tokens.increase_index();
                }

                // Whitespace following the closing `}` is inner text that
                // sits between this child and whatever comes next.
                let mut trailing_whitespace = String::new();
                if closed {
                    for ws in &self.tokens.get_token_backward(1).following_whitespace {
                        trailing_whitespace.push(ws.get_value());
                    }
                }

                if !trailing_whitespace.is_empty() {
                    let trailing_end = self.get_current_position();
                    let text_chunk = StringChunk::new_text(
                        child_start.clone(),
                        trailing_end.clone(),
                        Self::escape_interpolation_percents(&trailing_whitespace),
                    );
                    children.push(PekoAST::PekoXTag(PekoXTagAST::new(
                        child_start,
                        trailing_end,
                        PositionData::default(),
                        PositionData::default(),
                        String::new(),
                        HashMap::new(),
                        HashMap::new(),
                        Vec::new(),
                        vec![text_chunk],
                    )));
                }
            } else {
                // Inner text: string-like, with `${expr}` interpolation.
                let mut inner_text = Vec::new();
                let mut last_start = PositionData::default();
                let mut last_end: PositionData;
                let mut last_block_start: PositionData;
                let mut last_block_end: PositionData;
                let mut last_string = String::new();

                while !self.tokens.finished()
                    && !self.tokens.current_token().equals("<")
                    && !self.tokens.current_token().equals("{")
                {
                    if self.tokens.current_token().equals("$")
                        && self.tokens.get_token_forward(1).equals("{")
                    {
                        for ws in &self.tokens.current_token().preceeding_whitespace {
                            last_string.push(ws.get_value());
                        }

                        last_end = self.get_current_position();
                        if !last_string.is_empty() {
                            inner_text.push(StringChunk::new_text(
                                last_start,
                                last_end,
                                Self::escape_interpolation_percents(&std::mem::take(
                                    &mut last_string,
                                )),
                            ));
                        }

                        self.tokens.increase_index();
                        last_start = self.get_current_position();

                        let interpolated = self.parse_block("string interpolation");
                        last_block_start = interpolated.start;
                        last_block_end = interpolated.end;

                        inner_text.push(StringChunk::new_interpolation(
                            last_block_start,
                            last_block_end,
                            interpolated.value,
                        ));

                        for ws in &self.tokens.get_token_backward(1).following_whitespace {
                            last_string.push(ws.get_value());
                        }
                    } else {
                        last_start = self.get_current_position();

                        match self.tokens.current_token().get_value().as_str() {
                            r"\n" => last_string.push('\n'),
                            r"\r" => last_string.push('\r'),
                            r"\t" => last_string.push('\t'),
                            "\\" => last_string.push('\\'),
                            _ => last_string.push_str(
                                &self.tokens.current_token().get_value_with_whitespace(false),
                            ),
                        }

                        self.tokens.increase_index();
                    }
                }

                if !last_string.is_empty() {
                    last_end = self.get_current_position();
                    inner_text.push(StringChunk::new_text(
                        last_start.clone(),
                        last_end,
                        Self::escape_interpolation_percents(&last_string),
                    ));
                }

                // Wrap inner text in an empty-tag PekoX child so it sits
                // alongside other children in `children`.
                if !inner_text.is_empty() {
                    children.push(PekoAST::PekoXTag(PekoXTagAST::new(
                        last_start,
                        self.get_current_position(),
                        PositionData::default(),
                        PositionData::default(),
                        String::new(),
                        HashMap::new(),
                        HashMap::new(),
                        Vec::new(),
                        inner_text,
                    )));
                }
            }
        }

        // Closing tag: `</tag>`.
        if self.tokens.current_token().equals("<") {
            self.tokens.increase_index();
            if self.tokens.current_token().equals("/") {
                self.tokens.increase_index();
            }
        }

        if self.expect_token_type(&lexer::TokenType::Identifier, "pekox tag") {
            if !self.tokens.current_token().equals(element_tag.as_str()) {
                self.report_diagnostic("closing tag name does not match the opening tag. Every `<X>` must be closed by a matching `</X>` (or use a self-closing `<X />` form)");
            }
            self.tokens.increase_index();
        }

        let ending_position = self.tokens.current_token().get_end().clone();
        if self.tokens.current_token().equals(">") {
            self.tokens.increase_index();
        }

        PekoXTagAST::new(
            starting_position,
            ending_position,
            attributes_start,
            attributes_end,
            element_tag,
            element_attributes,
            element_events,
            children,
            Vec::new(),
        )
    }

    /// Returns the binding precedence of `op` for expression parsing.
    ///
    /// Higher numbers bind tighter. Parentheses are 0 (lowest) so they
    /// terminate expression flushing without being consumed.
    fn get_operator_precedence(&self, op: &str, operator_type: ExpressionOperatorType) -> i32 {
        if op == "(" || op == ")" {
            return 0;
        }

        if operator_type == ExpressionOperatorType::Unary {
            return 8;
        }

        match op {
            ".." => 1,
            "&&" | "||" => 2,
            "==" | ">=" | ">" | "<=" | "<" | "!=" => 3,
            "+" | "-" => 4,
            "*" | "/" | "%" => 5,
            "^" => 6,
            "&" | "!" => 7,
            _ => -1,
        }
    }

    /// Parses an expression using a modified shunting-yard algorithm.
    ///
    /// Mixes unary and binary operators; flushes the operator stack on
    /// closing parens and at end-of-expression to build a binary AST tree.
    pub fn parse_expression(&mut self) -> PekoAST {
        let mut operator_stack: Vec<ExpressionOperator> = Vec::new();
        let mut operand_stack: Vec<PekoAST> = Vec::new();
        let mut state = ExpressionOperatorType::Unary;

        // First-token shortcut: if it's not an operator or paren, parse a
        // single value and short-circuit if no operator follows.
        match self.tokens.current_token().get_type() {
            lexer::TokenType::Operator
            | lexer::TokenType::BooleanOperator
            | lexer::TokenType::RangeOp
            | lexer::TokenType::LParen => {}
            _ => {
                let mut first_val = self.secondary_parse();

                // Optional type cast: `value as Type`.
                if matches!(self.tokens.current_token().get_type(), lexer::TokenType::As) {
                    self.tokens.increase_index();
                    let cast_to = types::PekoType::from_tokens(self);
                    first_val =
                        PekoAST::Cast(CastAST::new(Box::new(first_val), cast_to, CastKind::Checked));
                }

                match self.tokens.current_token().get_type() {
                    lexer::TokenType::Operator
                    | lexer::TokenType::BooleanOperator
                    | lexer::TokenType::RangeOp
                    | lexer::TokenType::ObjectAccessor
                    | lexer::TokenType::LBracket
                    | lexer::TokenType::QuestionMark => {}
                    _ => return first_val,
                }

                state = ExpressionOperatorType::Binary;
                operand_stack.push(first_val);
            }
        }

        // Tracks balanced parens inside the expression. When called from
        // inside an argument list (`count_parens` is set), we don't pre-bias
        // the count because the outer `(` was already consumed.
        let count_parentheses = self.has_state("count_parens");
        let mut parentheses_to_close = if count_parentheses { 0 } else { 1 };
        let mut previous_was_value = false;

        loop {
            if self.tokens.finished() {
                break;
            }

            match self.tokens.current_token().get_type() {
                // Operand.
                lexer::TokenType::Identifier
                | lexer::TokenType::Character
                | lexer::TokenType::DoubleString
                | lexer::TokenType::InterpolatedString
                | lexer::TokenType::Null
                | lexer::TokenType::Number
                | lexer::TokenType::False
                | lexer::TokenType::True => {
                    // Two operands in a row terminate the expression -- the
                    // second one belongs to whatever follows.
                    if previous_was_value {
                        break;
                    }
                    previous_was_value = true;

                    operand_stack.push(self.secondary_parse());
                    state = ExpressionOperatorType::Binary;
                }

                // Mid-expression type cast: `(a + b) as int`.
                lexer::TokenType::As => {
                    self.tokens.increase_index();

                    let convert_type = types::PekoType::from_tokens(self);

                    if let Some(convert_value) = operand_stack.pop() {
                        operand_stack.push(PekoAST::Cast(CastAST::new(
                            Box::new(convert_value),
                            convert_type,
                            CastKind::Checked,
                        )));
                    } else {
                        self.report_diagnostic("`as` requires a value to cast. Use `value as Type` to cast a value to another type");
                    }
                }

                // Operator (unary or binary).
                lexer::TokenType::Operator
                | lexer::TokenType::BooleanOperator
                | lexer::TokenType::RangeOp => {
                    previous_was_value = false;

                    match state {
                        ExpressionOperatorType::Unary => {
                            operator_stack.push(ExpressionOperator::new(
                                PositionedValue::from_token(self.tokens.current_token()),
                                ExpressionOperatorType::Unary,
                            ));
                        }

                        ExpressionOperatorType::Binary => {
                            // While the incoming operator has lower-or-equal
                            // precedence than the top of the operator stack,
                            // flush the top operator into the operand tree.
                            while !operand_stack.is_empty()
                                && !operator_stack.is_empty()
                                && self.get_operator_precedence(
                                    self.tokens.current_token().get_value(),
                                    ExpressionOperatorType::Binary,
                                ) <= self.get_operator_precedence(
                                    &operator_stack.last().unwrap().operator.value,
                                    operator_stack.last().unwrap().operator_type,
                                )
                            {
                                let operator = operator_stack.pop().unwrap();

                                if operator.operator_type == ExpressionOperatorType::Unary
                                    && !operand_stack.is_empty()
                                {
                                    let operand = operand_stack.pop().unwrap();
                                    operand_stack.push(PekoAST::UnaryExpression(
                                        UnaryExpressionAST::new(
                                            Box::new(operand),
                                            operator.operator.value,
                                        ),
                                    ));
                                } else if operand_stack.len() > 1 {
                                    let rhs = operand_stack.pop().unwrap();
                                    let lhs = operand_stack.pop().unwrap();
                                    operand_stack.push(PekoAST::BinaryExpression(
                                        BinaryExpressionAST::new(
                                            Box::new(lhs),
                                            Box::new(rhs),
                                            operator.operator.value,
                                        ),
                                    ));
                                } else {
                                    self.report_diagnostic(
                                        format!(
                                            "operator `{}` has no operand. Operators must appear between expressions (binary) or before an expression (unary)",
                                            operator.operator.value
                                        )
                                        .as_str(),
                                    );
                                }
                            }

                            operator_stack.push(ExpressionOperator::new(
                                PositionedValue::from_token(self.tokens.current_token()),
                                ExpressionOperatorType::Binary,
                            ));
                            state = ExpressionOperatorType::Unary;
                        }
                    }
                    self.tokens.increase_index();
                }

                // Open paren.
                lexer::TokenType::LParen => {
                    previous_was_value = false;

                    if count_parentheses {
                        parentheses_to_close += 1;
                    }

                    operator_stack.push(ExpressionOperator::new(
                        PositionedValue::from_token(self.tokens.current_token()),
                        state,
                    ));
                    self.tokens.increase_index();
                }

                // Close paren (flush until matching open paren).
                lexer::TokenType::RParen => {
                    previous_was_value = false;

                    if parentheses_to_close == 0 && count_parentheses {
                        break;
                    }

                    if count_parentheses {
                        parentheses_to_close -= 1;
                    }

                    while !operator_stack.is_empty()
                        && operator_stack.last().unwrap().operator.value != "("
                    {
                        let operator = operator_stack.pop().unwrap();

                        if operator.operator_type == ExpressionOperatorType::Unary
                            && !operand_stack.is_empty()
                        {
                            let operand = operand_stack.pop().unwrap();
                            operand_stack.push(PekoAST::UnaryExpression(UnaryExpressionAST::new(
                                Box::new(operand),
                                operator.operator.value,
                            )));
                        } else if operand_stack.len() > 1 {
                            let rhs = operand_stack.pop().unwrap();
                            let lhs = operand_stack.pop().unwrap();
                            operand_stack.push(PekoAST::BinaryExpression(
                                BinaryExpressionAST::new(
                                    Box::new(lhs),
                                    Box::new(rhs),
                                    operator.operator.value,
                                ),
                            ));
                        } else {
                            self.report_diagnostic(
                                format!(
                                    "operator `{}` has no operand. Operators must appear between expressions (binary) or before an expression (unary)",
                                    operator.operator.value
                                )
                                .as_str(),
                            );
                        }
                    }

                    // Pop the matching open paren and recover its state.
                    let parens_state = if operator_stack.is_empty()
                        || operator_stack.last().unwrap().operator.value != "("
                    {
                        self.report_diagnostic("mismatched parentheses in this expression. Each `(` must be matched by a `)`");
                        ExpressionOperatorType::Binary
                    } else {
                        operator_stack.pop().unwrap().operator_type
                    };

                    // If the just-closed parens were preceded by a unary
                    // operator (e.g. `-(a+b)`), apply it now.
                    if parens_state == ExpressionOperatorType::Unary
                        && operator_stack
                            .last()
                            .is_some_and(|op| op.operator_type == ExpressionOperatorType::Unary)
                        && !operand_stack.is_empty()
                    {
                        let operator = operator_stack.pop().unwrap();
                        let operand = operand_stack.pop().unwrap();
                        operand_stack.push(PekoAST::UnaryExpression(UnaryExpressionAST::new(
                            Box::new(operand),
                            operator.operator.value,
                        )));
                    }

                    state = ExpressionOperatorType::Binary;
                    self.tokens.increase_index();
                }

                // `.member` suffix on the most recent operand. The chain
                // here covers cases that secondary_parse cannot reach,
                // such as `(expr).member` and `(expr) as Type.member`.
                lexer::TokenType::ObjectAccessor
                    if state == ExpressionOperatorType::Binary && !operand_stack.is_empty() =>
                {
                    self.tokens.increase_index();

                    self.set_state("skip_accessors");
                    let access = self.secondary_parse();
                    self.remove_state("skip_accessors");

                    let base = operand_stack.pop().unwrap();
                    operand_stack.push(PekoAST::ObjectAccess(ObjectAccessAST::new(
                        Box::new(base),
                        Box::new(access),
                    )));
                    previous_was_value = true;
                }

                // `[index]` suffix on the most recent operand.
                lexer::TokenType::LBracket
                    if state == ExpressionOperatorType::Binary && !operand_stack.is_empty() =>
                {
                    self.tokens.increase_index();

                    let accessor = self.parse();

                    if self.expect_token_value("]", "array access") {
                        self.tokens.increase_index();
                    }

                    let base = operand_stack.pop().unwrap();
                    let start = base.get_start().clone();
                    operand_stack.push(PekoAST::ArrayAccess(ArrayAccessAST::new(
                        start,
                        self.get_current_position(),
                        Box::new(base),
                        Box::new(accessor),
                    )));
                    previous_was_value = true;
                }

                // `?` unwrap suffix on the most recent operand.
                lexer::TokenType::QuestionMark
                    if state == ExpressionOperatorType::Binary && !operand_stack.is_empty() =>
                {
                    let ending_position = self.tokens.current_token().get_end().clone();
                    self.tokens.increase_index();

                    let base = operand_stack.pop().unwrap();
                    let start = base.get_start().clone();
                    operand_stack.push(PekoAST::Unwrap(UnwrapAST::new(
                        start,
                        ending_position,
                        Box::new(base),
                    )));
                    previous_was_value = true;
                }

                _ => break,
            }
        }

        // Drain the operator stack into the operand tree.
        while let Some(operator) = operator_stack.pop() {
            if operator.operator.value == "(" {
                self.report_diagnostic(
                    "mismatched parentheses in this expression. Each `(` must be matched by a `)`",
                );
            } else if operator.operator_type == ExpressionOperatorType::Unary
                && !operand_stack.is_empty()
            {
                let operand = operand_stack.pop().unwrap();
                operand_stack.push(PekoAST::UnaryExpression(UnaryExpressionAST::new(
                    Box::new(operand),
                    operator.operator.value,
                )));
            } else if operand_stack.len() > 1 {
                let rhs = operand_stack.pop().unwrap();
                let lhs = operand_stack.pop().unwrap();
                operand_stack.push(PekoAST::BinaryExpression(BinaryExpressionAST::new(
                    Box::new(lhs),
                    Box::new(rhs),
                    operator.operator.value,
                )));
            } else {
                self.report_diagnostic(
                    format!(
                        "operator `{}` has no operand. Operators must appear between expressions (binary) or before an expression (unary)",
                        operator.operator.value
                    )
                    .as_str(),
                );
            }
        }

        // The remaining operand (if any) is the parsed expression.
        operand_stack
            .pop()
            .unwrap_or_else(|| PekoAST::Placeholder(PlaceholderAST::new(String::new())))
    }

    /// Top-level entry point: parses one statement, declaration, or
    /// expression from the current cursor position.
    pub fn parse(&mut self) -> PekoAST {
        // Eat any leading semicolons and comments.
        while !self.tokens.finished()
            && (self.tokens.current_token().equals(";") || self.tokens.current_token().is_comment())
        {
            if self.tokens.current_token().equals(";") {
                self.tokens.increase_index();
            } else {
                self.skip_comment();
            }
        }

        // Consume any leading visibility block and / or doc-info comment.
        let mut visibility: Option<VisibilityData> = None;
        let mut docinfo: Option<DocInfo> = None;

        while (self.tokens.current_token().equals("[") && visibility.is_none())
            || (self.tokens.current_token().equals("///") && docinfo.is_none())
        {
            if self.tokens.current_token().equals("[") {
                visibility = Some(self.parse_visibility());
            } else {
                docinfo = Some(self.parse_doc_info());
            }
        }

        match self.tokens.current_token().get_type() {
            lexer::TokenType::Class => {
                let mut class_declaration = self.parse_class_declaration();
                class_declaration.docinfo = docinfo;
                if let Some(v) = visibility {
                    class_declaration.visibility = v;
                }
                PekoAST::Class(class_declaration)
            }

            lexer::TokenType::Enum => {
                let mut enum_declaration = self.parse_enum_declaration();
                enum_declaration.docinfo = docinfo;
                if let Some(v) = visibility {
                    enum_declaration.visibility = v;
                }
                PekoAST::Enum(enum_declaration)
            }

            lexer::TokenType::FunctionDeclarator => {
                let mut function_declaration = self.parse_function_declaration();
                function_declaration.docinfo = docinfo;
                if let Some(v) = visibility {
                    function_declaration.visibility = v;
                }
                PekoAST::FunctionDeclaration(function_declaration)
            }

            lexer::TokenType::Let => {
                let mut variable_declaration = self.parse_variable_declaration();
                variable_declaration.docinfo = docinfo;
                if let Some(v) = visibility {
                    variable_declaration.visibility = v;
                }
                PekoAST::NewVariable(variable_declaration)
            }

            lexer::TokenType::Module => {
                let mut module_declaration = self.parse_module_creation();
                module_declaration.docinfo = docinfo;
                if let Some(v) = visibility {
                    module_declaration.visibility = v;
                }
                PekoAST::ModuleCreation(module_declaration)
            }

            lexer::TokenType::Import => PekoAST::ImportStatement(self.parse_import()),
            lexer::TokenType::Link => PekoAST::LinkStatement(self.parse_link()),
            lexer::TokenType::Style => PekoAST::StyleStatement(self.parse_style()),

            lexer::TokenType::Arch | lexer::TokenType::Platform => {
                PekoAST::PlatformStatement(self.parse_platform())
            }

            lexer::TokenType::If => PekoAST::IfStatement(self.parse_if_statement()),
            lexer::TokenType::Switch => PekoAST::Switch(self.parse_switch_statement()),
            lexer::TokenType::While => PekoAST::WhileLoop(self.parse_while_loop()),
            lexer::TokenType::For => PekoAST::ForLoop(self.parse_for_loop()),
            lexer::TokenType::Break => PekoAST::Break(self.parse_break()),
            lexer::TokenType::Continue => PekoAST::Continue(self.parse_continue()),
            lexer::TokenType::Return => PekoAST::Return(self.parse_return()),

            // Identifier-led statements are expressions. Variable declarations
            // are introduced by `let`, handled above.
            lexer::TokenType::Identifier | lexer::TokenType::Super => self.parse_expression(),

            // Anything else: PekoX tag if it starts with a known HTML tag,
            // otherwise an expression.
            _ => {
                if self.tokens.current_token().equals("<")
                    && matches!(
                        self.tokens.get_token_forward(1).get_type(),
                        lexer::TokenType::Identifier
                    )
                    && is_pekox_tag(self.tokens.get_token_forward(1).get_value())
                {
                    return PekoAST::PekoXTag(self.parse_pekox());
                }

                self.parse_expression()
            }
        }
    }

    /// Inner helper: parses simpler ASTs (literals, closures, identifier
    /// expressions, array / map / xml literals, encrypted strings) without
    /// invoking operator-precedence handling.
    fn secondary_parse(&mut self) -> PekoAST {
        while !self.tokens.finished() && self.tokens.current_token().is_comment() {
            self.skip_comment();
        }

        match self.tokens.current_token().get_type() {
            lexer::TokenType::True | lexer::TokenType::False => {
                PekoAST::Boolean(self.parse_boolean())
            }
            lexer::TokenType::DoubleString | lexer::TokenType::InterpolatedString => {
                PekoAST::String(self.parse_string())
            }
            lexer::TokenType::Character => PekoAST::Char(self.parse_char()),
            lexer::TokenType::Number => PekoAST::Number(self.parse_number()),
            lexer::TokenType::Null => PekoAST::Null(self.parse_null()),
            lexer::TokenType::Closure => PekoAST::Closure(self.parse_closure_declaration()),
            lexer::TokenType::New => self.parse_new_expression(),
            lexer::TokenType::DangerCast => self.parse_danger_cast(),
            lexer::TokenType::Constant => self.parse_constant_builtin(),
            lexer::TokenType::If => PekoAST::IfStatement(self.parse_if_statement()),

            lexer::TokenType::Identifier => {
                // Generic-call lookahead: if a `<...>` parses as types and
                // closes properly, treat the following call as generic.
                if self.tokens.get_token_forward(1).equals("<") {
                    let mut index = 2;
                    let mut is_type =
                        types::PekoType::test_next_tokens_for_type_with_index(self, &mut index);

                    while !self.tokens.finished()
                        && is_type
                        && self.tokens.get_token_forward(index).equals(",")
                    {
                        self.tokens.index_increase_index(&mut index);
                        is_type =
                            types::PekoType::test_next_tokens_for_type_with_index(self, &mut index);
                    }

                    if is_type && self.tokens.get_token_forward(index).equals(">") {
                        return self.parse_identifier();
                    }
                }

                match self.tokens.get_token_forward(1).get_type() {
                    lexer::TokenType::ModuleAccessor => {
                        PekoAST::ModuleAccess(self.parse_module_access())
                    }
                    _ => self.parse_identifier(),
                }
            }

            _ => {
                // `#`-prefixed literals: array, map, encrypted string.
                if self.tokens.current_token().equals("#") {
                    if self.tokens.get_token_forward(1).equals("[") {
                        return PekoAST::Array(self.parse_array());
                    } else if self.tokens.get_token_forward(1).equals("{") {
                        return PekoAST::Map(self.parse_map());
                    } else if self.tokens.get_token_forward(1).get_value() == "\"" {
                        return PekoAST::EncryptedString(self.parse_encrypted_string());
                    }
                }

                // PekoX tag literal.
                if self.tokens.current_token().equals("<")
                    && matches!(
                        self.tokens.get_token_forward(1).get_type(),
                        lexer::TokenType::Identifier
                    )
                    && is_pekox_tag(self.tokens.get_token_forward(1).get_value())
                {
                    return PekoAST::PekoXTag(self.parse_pekox());
                }

                // Nothing recognized, so emit a diagnostic and advance.
                self.report_diagnostic(
                    format!(
                        "unexpected token `{}`. This token is not valid in this context",
                        self.tokens.current_token().get_value(),
                    )
                    .as_str(),
                );
                let final_ast = PekoAST::Null(NullAST::new(
                    self.get_current_position(),
                    self.get_current_position(),
                ));
                self.tokens.increase_index();
                final_ast
            }
        }
    }
}
