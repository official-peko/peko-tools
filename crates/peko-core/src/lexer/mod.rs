//! # Peko Core Lexer
//!
//! Converts Pekoscript source text into a stream of [`Token`]s.
//!
//! The lexer is permissive and never fails: any character it doesn't
//! recognize is emitted as a [`TokenType::Unknown`] token. Whitespace is
//! attached to surrounding tokens rather than emitted as separate tokens,
//! so the parser can ignore it but downstream consumers (e.g. formatters)
//! can still reconstruct the original source via
//! [`Token::get_value_with_whitespace`].
//!
//! # Example
//!
//! ```
//! use peko_core::lexer::{TokenList, TokenType};
//!
//! let tokens = TokenList::from_source("fn main", "<inline>");
//! assert_eq!(tokens.length(), 2);
//! assert_eq!(tokens.get_token_forward(0).get_type(), &TokenType::FunctionDeclarator);
//! assert_eq!(tokens.get_token_forward(1).get_type(), &TokenType::Identifier);
//! ```

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::asts::data_structures::PositionData;

/// Discriminant for each kind of token the lexer can produce.
///
/// Variants are grouped in source order by category: constant-value literals,
/// visibility modifiers, operators, keywords, built-in types, and details
/// (comments, doc comments, unknown characters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenType {
    // Constant values
    DoubleString,       // "
    InterpolatedString, // `
    ByteCode,           // \[byte]
    Character,          // '
    True,               // true
    False,              // false
    Number,             // 0-9, 0.1-0.9, etc.
    Null,               // null

    // Visibility modifiers
    Const,     // const
    Let,       // let
    State,     // state
    External,  // external
    Private,   // private
    Constant,  // constant
    Public,    // public
    Notrack,   // notrack
    Blockexit, // blockexit
    Hide,      // hide
    Variadic,  // variadic
    Mutates,   // mutates
    GCSafe,    // gcsafe

    Identifier,

    // Operators
    Equals,                 // =
    Walrus,                 // :=
    Returns,                // =>
    LBracket,               // [
    RBracket,               // ]
    LParen,                 // (
    RParen,                 // )
    LBrace,                 // {
    RBrace,                 // }
    ObjectAccessor,         // .
    QuestionMark,           // ?
    RangeOp,                // ..
    Operator,               // +, -, *, /, %, ^
    BooleanOperator,        // ==, !=, >, <, >=, <=
    ModuleAccessor,         // ::
    Colon,                  // :
    AssignmentWithOperator, // +=, -=, /=, *=, %=, ^=
    AtSymbol,               // @

    // Keywords
    FunctionDeclarator, // fn
    Args,               // Args
    Closure,            // closure
    New,                // new
    DangerCast,         // danger_cast
    Return,             // return
    Class,              // class
    Trait,              // trait
    Impl,               // impl
    Enum,               // enum
    Switch,             // switch
    From,               // from
    Constructor,        // constructor
    Super,              // super
    Static,             // static
    Serial,             // serial
    If,                 // if
    Else,               // else
    While,              // while
    For,                // for
    Break,              // break
    Continue,           // continue
    In,                 // in
    Module,             // module
    Import,             // import
    Export,             // export
    As,                 // as
    Link,               // link
    Platform,           // platform
    Arch,               // arch
    Style,              // style

    // The only V2 FFI type spelled with its own keyword. The integer and
    // float FFI types (`i1`, `i8`, `i16`, `i32`, `i64`, `i128`, `f16`, `f32`,
    // `f64`) and `pointer<T>` lex as identifiers, as do the boxed value types
    // (`number`, `string`, `bool`, `char`) defined in std::core.
    OpaqueType,

    // Details
    Comment,     // //
    DocComment,  // ///
    Description, // @description

    Unknown, // any other character not recognized
}

impl TokenType {
    /// Returns a short human-readable label for this token type.
    ///
    /// Keywords and punctuation are quoted (e.g. `'fn'`, `'=>'`); open
    /// categories such as identifiers and numbers are not (e.g. `identifier`,
    /// `number`). Used by the parser to format "expected X, got Y" messages.
    ///
    /// # Examples
    ///
    /// ```
    /// use peko_core::lexer::TokenType;
    /// assert_eq!(TokenType::FunctionDeclarator.get_name(), "'fn'");
    /// assert_eq!(TokenType::Identifier.get_name(), "identifier");
    /// ```
    #[must_use]
    pub fn get_name(&self) -> String {
        match self {
            // Open categories -- not quoted, since the user sees a class of
            // value rather than a specific keyword.
            Self::DoubleString | Self::InterpolatedString => "string",
            Self::Character => "character",
            Self::ByteCode => "byte",
            Self::Number => "number",
            Self::Identifier => "identifier",
            Self::Operator => "operator",
            Self::BooleanOperator => "boolean operator",
            Self::AssignmentWithOperator => "assignment with operator",
            Self::Unknown => "unknown",

            // Boolean & null literals.
            Self::True => "'true'",
            Self::False => "'false'",
            Self::Null => "'null'",

            // Visibility modifiers.
            Self::Const => "'const'",
            Self::Let => "'let'",
            Self::State => "'state'",
            Self::External => "'external'",
            Self::Private => "'private'",
            Self::Constant => "'constant'",
            Self::Public => "'public'",
            Self::Notrack => "'notrack'",
            Self::Blockexit => "'blockexit'",
            Self::Hide => "'hide'",
            Self::Variadic => "'variadic'",
            Self::Mutates => "'mutates'",
            Self::GCSafe => "'gcsafe'",

            // Punctuation.
            Self::Equals => "'='",
            Self::Walrus => "':='",
            Self::Returns => "'=>'",
            Self::LBracket => "'['",
            Self::RBracket => "']'",
            Self::LParen => "'('",
            Self::RParen => "')'",
            Self::LBrace => "'{'",
            Self::RBrace => "'}'",
            Self::ObjectAccessor => "'.'",
            Self::QuestionMark => "'?'",
            Self::RangeOp => "..",
            Self::ModuleAccessor => "'::'",
            Self::Colon => "':'",
            Self::AtSymbol => "'@'",

            // Control-flow and declarator keywords.
            Self::FunctionDeclarator => "'fn'",
            Self::Args => "'Args'",
            Self::Closure => "'closure'",
            Self::New => "'new'",
            Self::DangerCast => "'danger_cast'",
            Self::Return => "'return'",
            Self::Class => "'class'",
            Self::Trait => "'trait'",
            Self::Impl => "'impl'",
            Self::Enum => "'enum'",
            Self::Switch => "'switch'",
            Self::From => "'from'",
            Self::Constructor => "'constructor'",
            Self::Super => "'super'",
            Self::Static => "'static'",
            Self::Serial => "'serial'",
            Self::If => "'if'",
            Self::Else => "'else'",
            Self::While => "'while'",
            Self::For => "'for'",
            Self::Break => "'break'",
            Self::Continue => "'continue'",
            Self::In => "'in'",
            Self::Module => "'module'",
            Self::Import => "'import'",
            Self::Export => "'export'",
            Self::As => "'as'",
            Self::Link => "'link'",
            Self::Platform => "'platform'",
            Self::Arch => "'arch'",
            Self::Style => "'style'",

            // Built-in types.
            Self::OpaqueType => "'opaque'",

            // Comments.
            Self::Comment => "'//'",
            Self::DocComment => "'///'",
            Self::Description => "'@description'",
        }
        .to_owned()
    }
}

/// A single piece of whitespace attached to a token.
///
/// Whitespace runs are preserved (rather than discarded) so that downstream
/// tools (i.e. formatters, source-reconstructing utilitiesm, etc.) can recover the
/// original layout of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whitespace {
    Space,
    Tab,
    Newline,
    CarriageReturn,
}

impl Whitespace {
    /// Returns the literal character this whitespace represents.
    #[must_use]
    pub fn get_value(&self) -> char {
        match self {
            Self::Space => ' ',
            Self::Tab => '\t',
            Self::Newline => '\n',
            Self::CarriageReturn => '\r',
        }
    }

    /// Returns the escaped form of this whitespace.
    ///
    /// Spaces are returned literally; tabs, newlines, and carriage returns
    /// are returned as `\t`, `\n`, and `\r`. Used by
    /// [`Token::get_value_with_whitespace`] when emitting source for
    /// inclusion in error messages or string literals.
    #[must_use]
    pub fn to_string_backslashes(&self) -> &'static str {
        match self {
            Self::Space => " ",
            Self::Tab => "\\t",
            Self::Newline => "\\n",
            Self::CarriageReturn => "\\r",
        }
    }
}

/// A single lexed token: type, source value, source span, and surrounding
/// whitespace.
#[derive(Clone, Debug)]
pub struct Token {
    token_type: TokenType,
    token_value: String,

    // Inclusive source span.
    start: PositionData,
    end: PositionData,

    /// Whitespace run immediately preceding this token in the source.
    pub preceeding_whitespace: Vec<Whitespace>,
    /// Whitespace run immediately following this token in the source.
    pub following_whitespace: Vec<Whitespace>,
}

impl Token {
    /// Constructs a new token with empty surrounding whitespace.
    ///
    /// Whitespace runs are filled in by the lexer after the token itself is
    /// built.
    #[must_use]
    pub fn new(
        token_type: TokenType,
        token_value: String,
        start: PositionData,
        end: PositionData,
    ) -> Self {
        Self {
            token_type,
            token_value,
            start,
            end,
            preceeding_whitespace: Vec::new(),
            following_whitespace: Vec::new(),
        }
    }

    /// Returns the inclusive end position of this token.
    #[must_use]
    pub fn get_end(&self) -> &PositionData {
        &self.end
    }

    /// Returns the inclusive start position of this token.
    #[must_use]
    pub fn get_start(&self) -> &PositionData {
        &self.start
    }

    /// Returns `true` if this token is a line comment (`//` or `//!`).
    ///
    /// Doc comments (`///`) are deliberately *not* treated as comments here:
    /// they carry semantic content the parser consumes.
    #[must_use]
    pub fn is_comment(&self) -> bool {
        matches!(self.token_type, TokenType::Comment)
    }

    /// Returns `true` if this token is a `///` doc-comment marker. Distinct from
    /// [`Self::is_comment`], which covers only plain `//` comments.
    #[must_use]
    pub fn is_doc_comment(&self) -> bool {
        matches!(self.token_type, TokenType::DocComment)
    }

    /// Returns `true` if either the preceding or following whitespace of this
    /// token contains a newline.
    ///
    /// Used by the parser to detect statement boundaries in newline-sensitive
    /// contexts.
    #[must_use]
    pub fn has_newline(&self) -> bool {
        self.preceeding_whitespace
            .iter()
            .chain(self.following_whitespace.iter())
            .any(|w| matches!(w, Whitespace::Newline))
    }

    /// Returns `true` if the *following* whitespace of this token contains a
    /// newline.
    ///
    /// Unlike [`Token::has_newline`], which inspects both adjacent runs, this
    /// only considers whitespace after the token. Used by doc-comment
    /// parsing where the relevant question is "does this line of source end
    /// after this token?"
    #[must_use]
    pub fn has_trailing_newline(&self) -> bool {
        self.following_whitespace
            .iter()
            .any(|w| matches!(w, Whitespace::Newline))
    }

    /// Returns `true` if this token's value equals `value`.
    #[must_use]
    pub fn equals(&self, value: &str) -> bool {
        self.token_value == value
    }

    /// Returns this token's type.
    #[must_use]
    pub fn get_type(&self) -> &TokenType {
        &self.token_type
    }

    /// Returns this token's raw source value, excluding surrounding whitespace.
    #[must_use]
    pub fn get_value(&self) -> &String {
        &self.token_value
    }

    /// Reconstructs this token's source text including its preceding and
    /// following whitespace runs.
    ///
    /// If `cancel_backslashes` is `true`, all whitespace runs are rendered in
    /// their escaped forms (e.g. `\n` rather than a literal newline) and any
    /// literal backslash in the token value is doubled (`\\`). This is the
    /// form needed when embedding the token in a string literal or quoted
    /// error message.
    #[must_use]
    pub fn get_value_with_whitespace(&self, cancel_backslashes: bool) -> String {
        let mut out = String::new();

        for ws in &self.preceeding_whitespace {
            if cancel_backslashes {
                out.push_str(ws.to_string_backslashes());
            } else {
                out.push(ws.get_value());
            }
        }

        for ch in self.token_value.chars() {
            if ch == '\\' && cancel_backslashes {
                out.push_str("\\\\");
            } else {
                out.push(ch);
            }
        }

        for ws in &self.following_whitespace {
            if cancel_backslashes {
                out.push_str(ws.to_string_backslashes());
            } else {
                out.push(ws.get_value());
            }
        }

        out
    }
}

/// A cursored list of tokens with bidirectional indexing.
///
/// `TokenList` is the parser's primary input. It owns a `Vec<Token>` and a
/// current-position cursor, with methods to peek forward and backward, advance,
/// retreat, and check for end-of-input.
///
/// The original source string is retained on the list so that consumers which
/// need to reference it (e.g. for richer diagnostics) can do so via
/// [`TokenList::get_source`].
#[derive(Clone, Debug)]
pub struct TokenList {
    tokens: Vec<Token>,
    token_index: usize,
    source: String,
}

impl TokenList {
    /// Constructs a `TokenList` from a pre-lexed token vector and the source
    /// it was lexed from.
    ///
    /// The cursor begins at index 0. Use [`TokenList::from_source`] to lex
    /// source text directly into a `TokenList`.
    #[must_use]
    pub fn new(tokens: Vec<Token>, source: String) -> Self {
        Self {
            tokens,
            token_index: 0,
            source,
        }
    }

    /// Returns the number of tokens in this list.
    #[must_use]
    pub fn length(&self) -> usize {
        self.tokens.len()
    }

    /// Returns the source string this list was lexed from.
    #[must_use]
    pub fn get_source(&self) -> &str {
        &self.source
    }

    /// Lexes a source string into a `TokenList`, tagging each token's
    /// positions with the given file path.
    ///
    /// The lexer never fails: unrecognized characters are emitted as
    /// [`TokenType::Unknown`] tokens. Whitespace is attached to surrounding
    /// tokens via [`Token::preceeding_whitespace`] /
    /// [`Token::following_whitespace`] rather than emitted as its own tokens.
    ///
    /// # Examples
    ///
    /// ```
    /// use peko_core::lexer::{TokenList, TokenType};
    ///
    /// let tokens = TokenList::from_source("if true", "<inline>");
    /// assert_eq!(tokens.length(), 2);
    /// assert_eq!(tokens.get_token_forward(0).get_type(), &TokenType::If);
    /// assert_eq!(tokens.get_token_forward(1).get_type(), &TokenType::True);
    /// ```
    pub fn from_source(source: &str, file_path: impl AsRef<Path>) -> Self {
        let file_path: PathBuf = file_path.as_ref().to_path_buf();
        let mut tokens: Vec<Token> = Vec::new();

        if source.is_empty() {
            return Self::new(tokens, source.to_owned());
        }

        // Indexing state.
        let mut index = 0;
        let mut column = 0; // resets on every newline
        let mut line = 1; // increments on every newline

        let chars: Vec<char> = source.chars().collect();

        while index < chars.len() {
            let mut preceeding_whitespace: Vec<Whitespace> = Vec::new();
            let mut following_whitespace: Vec<Whitespace> = Vec::new();

            // Eat any whitespace preceding the next token.
            while index < chars.len() {
                let ws = match chars[index] {
                    ' ' => Whitespace::Space,
                    '\n' => {
                        line += 1;
                        column = 0;
                        Whitespace::Newline
                    }
                    '\t' => Whitespace::Tab,
                    '\r' => Whitespace::CarriageReturn,
                    _ => break,
                };
                preceeding_whitespace.push(ws);

                // Column advances on inline whitespace; newlines reset it
                // (handled in the match arm above).
                if matches!(chars[index], ' ' | '\t') {
                    column += 1;
                }
                index += 1;
            }

            // If preceding whitespace consumed everything, we're done.
            if index >= chars.len() {
                break;
            }

            // Lex one token. Most arms are uniform single-character punctuation;
            // identifier, number, and the multi-character operators are more
            // involved and inlined below.
            let mut token = match chars[index] {
                // Strings/chars -- the lexer only emits the *delimiter*; the parser
                // handles the inner contents.
                '"' => Token::new(
                    TokenType::DoubleString,
                    "\"".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '`' => Token::new(
                    TokenType::InterpolatedString,
                    "`".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '\'' => Token::new(
                    TokenType::Character,
                    "'".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '\\' => Token::new(
                    TokenType::Character,
                    "\\".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),

                // Identifiers and keywords.
                // Identifiers may *start* with '$' but '$' cannot appear elsewhere.
                'a'..='z' | 'A'..='Z' | '_' | '$' => {
                    let start = PositionData::new(column, index, line, file_path.clone());

                    let mut identifier = String::new();
                    identifier.push(chars[index]);
                    index += 1;
                    column += 1;

                    while index < chars.len() {
                        match chars[index] {
                            'a'..='z' | 'A'..='Z' | '_' | '0'..='9' => {
                                identifier.push(chars[index])
                            }
                            _ => break,
                        }
                        index += 1;
                        column += 1;
                    }

                    let end = PositionData::new(column, index, line, file_path.clone());

                    // Step back one position so the outer `index += 1`
                    // at the end of the main loop advances us correctly.
                    index -= 1;
                    column -= 1;

                    // Match against the keyword table; fall through to Identifier
                    // for anything not in the table.
                    let ty = match identifier.as_str() {
                        "hide" => TokenType::Hide,
                        "Args" => TokenType::Args,
                        "true" => TokenType::True,
                        "false" => TokenType::False,
                        "null" => TokenType::Null,
                        "const" => TokenType::Const,
                        "let" => TokenType::Let,
                        "state" => TokenType::State,
                        "mutates" => TokenType::Mutates,
                        "constant" => TokenType::Constant,
                        "fn" => TokenType::FunctionDeclarator,
                        "closure" => TokenType::Closure,
                        "new" => TokenType::New,
                        "danger_cast" => TokenType::DangerCast,
                        "return" => TokenType::Return,
                        "class" => TokenType::Class,
                        "static" => TokenType::Static,
                        "serial" => TokenType::Serial,
                        "trait" => TokenType::Trait,
                        "impl" => TokenType::Impl,
                        "enum" => TokenType::Enum,
                        "switch" => TokenType::Switch,
                        "from" => TokenType::From,
                        "operator" => TokenType::Operator,
                        "external" => TokenType::External,
                        "private" => TokenType::Private,
                        "public" => TokenType::Public,
                        "notrack" => TokenType::Notrack,
                        "variadic" => TokenType::Variadic,
                        "blockexit" => TokenType::Blockexit,
                        "constructor" => TokenType::Constructor,
                        "super" => TokenType::Super,
                        "if" => TokenType::If,
                        "else" => TokenType::Else,
                        "while" => TokenType::While,
                        "for" => TokenType::For,
                        "break" => TokenType::Break,
                        "continue" => TokenType::Continue,
                        "in" => TokenType::In,
                        "module" => TokenType::Module,
                        "import" => TokenType::Import,
                        "export" => TokenType::Export,
                        "as" => TokenType::As,
                        "link" => TokenType::Link,
                        "platform" => TokenType::Platform,
                        "arch" => TokenType::Arch,
                        "style" => TokenType::Style,
                        "opaque" => TokenType::OpaqueType,
                        "gcsafe" => TokenType::GCSafe,
                        _ => TokenType::Identifier,
                    };
                    Token::new(ty, identifier, start, end)
                }

                // Numbers.
                // Digits with optional underscores (skipped) and an optional
                // single decimal point. A `..` is treated as the range operator
                // rather than two decimals, so we peek for it.
                '0'..='9' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    let mut number = String::new();
                    let mut found_decimal = false;

                    while index < chars.len() {
                        match chars[index] {
                            '0'..='9' | '.' | '_' => {
                                if chars[index] == '.' {
                                    // Lookahead for the range operator `..`
                                    // so we don't swallow it into the number.
                                    if index + 1 < chars.len() && chars[index + 1] == '.' {
                                        break;
                                    }
                                    if found_decimal {
                                        break;
                                    }
                                    found_decimal = true;
                                }

                                // Underscores are visual separators only.
                                if chars[index] != '_' {
                                    number.push(chars[index]);
                                }
                            }
                            _ => break,
                        }
                        index += 1;
                        column += 1;
                    }

                    index -= 1;
                    column -= 1;
                    Token::new(
                        TokenType::Number,
                        number,
                        start,
                        PositionData::new(column + 1, index + 1, line, file_path.clone()),
                    )
                }

                // `:` / `::` / `:=`
                ':' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || (chars[index] != ':' && chars[index] != '=') {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::Colon,
                            ":".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else if chars[index] == ':' {
                        Token::new(
                            TokenType::ModuleAccessor,
                            "::".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::Walrus,
                            ":=".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `=` / `==` / `=>`
                '=' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || (chars[index] != '>' && chars[index] != '=') {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::Equals,
                            "=".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else if chars[index] == '>' {
                        Token::new(
                            TokenType::Returns,
                            "=>".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::BooleanOperator,
                            "==".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `>` / `>=`
                '>' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '=' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::BooleanOperator,
                            ">".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::BooleanOperator,
                            ">=".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `<` / `<=`
                '<' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '=' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::BooleanOperator,
                            "<".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::BooleanOperator,
                            "<=".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `!` / `!=`
                '!' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '=' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::BooleanOperator,
                            "!".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::BooleanOperator,
                            "!=".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `&` / `&&` -- single `&` is a reference operator.
                '&' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '&' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::Operator,
                            "&".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::BooleanOperator,
                            "&&".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `/` / `//` / `///` / `//!` / `/=`
                '/' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || (chars[index] != '/' && chars[index] != '=') {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::Operator,
                            "/".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else if chars[index] == '/' {
                        // `///` doc comment or `//!` inner doc comment vs plain `//`.
                        //
                        // `////` (four or more slashes) lexes as repeated `//`
                        // tokens, not `///` + `/`. So we only treat the
                        // three-slash sequence as DocComment when the fourth
                        // character is not another `/`.
                        if index + 1 < chars.len()
                            && chars[index + 1] == '/'
                            && !(index + 2 < chars.len() && chars[index + 2] == '/')
                        {
                            index += 1;
                            column += 1;
                            Token::new(
                                TokenType::DocComment,
                                "///".to_owned(),
                                start,
                                PositionData::new(column, index, line, file_path.clone()),
                            )
                        } else if index + 1 < chars.len() && chars[index + 1] == '!' {
                            index += 1;
                            column += 1;
                            Token::new(
                                TokenType::Comment,
                                "//!".to_owned(),
                                start,
                                PositionData::new(column, index, line, file_path.clone()),
                            )
                        } else {
                            Token::new(
                                TokenType::Comment,
                                "//".to_owned(),
                                start,
                                PositionData::new(column + 1, index + 1, line, file_path.clone()),
                            )
                        }
                    } else {
                        Token::new(
                            TokenType::AssignmentWithOperator,
                            "/=".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `@`
                '@' => Token::new(
                    TokenType::AtSymbol,
                    "@".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),

                // `||` / single `|` (emitted as Unknown since Pekoscript has no bitwise OR).
                '|' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '|' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::Unknown,
                            "|".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::BooleanOperator,
                            "||".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `op` or `op=`.
                '+' | '-' | '*' | '%' | '^' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    let operator: String = chars[index].to_string();
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '=' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::Operator,
                            operator,
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        let mut compound = operator;
                        compound.push('=');
                        Token::new(
                            TokenType::AssignmentWithOperator,
                            compound,
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // `.` / `..`
                '.' => {
                    let start = PositionData::new(column, index, line, file_path.clone());
                    index += 1;
                    column += 1;

                    if index >= chars.len() || chars[index] != '.' {
                        index -= 1;
                        column -= 1;
                        Token::new(
                            TokenType::ObjectAccessor,
                            ".".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    } else {
                        Token::new(
                            TokenType::RangeOp,
                            "..".to_owned(),
                            start,
                            PositionData::new(column + 1, index + 1, line, file_path.clone()),
                        )
                    }
                }

                // Single-character punctuation.
                '[' => Token::new(
                    TokenType::LBracket,
                    "[".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                ']' => Token::new(
                    TokenType::RBracket,
                    "]".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '(' => Token::new(
                    TokenType::LParen,
                    "(".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                ')' => Token::new(
                    TokenType::RParen,
                    ")".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '{' => Token::new(
                    TokenType::LBrace,
                    "{".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '}' => Token::new(
                    TokenType::RBrace,
                    "}".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
                '?' => Token::new(
                    TokenType::QuestionMark,
                    "?".to_owned(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),

                // Anything else (emit as Unknown for the parser to surface).
                other => Token::new(
                    TokenType::Unknown,
                    other.to_string(),
                    PositionData::new(column, index, line, file_path.clone()),
                    PositionData::new(column + 1, index + 1, line, file_path.clone()),
                ),
            };
            index += 1;
            column += 1;

            // Eat any whitespace following this token.
            while index < chars.len() {
                let ws = match chars[index] {
                    ' ' => Whitespace::Space,
                    '\n' => {
                        line += 1;
                        column = 0;
                        Whitespace::Newline
                    }
                    '\t' => Whitespace::Tab,
                    '\r' => Whitespace::CarriageReturn,
                    _ => break,
                };
                following_whitespace.push(ws);

                if matches!(chars[index], ' ' | '\t') {
                    column += 1;
                }
                index += 1;
            }

            token.preceeding_whitespace.extend(preceeding_whitespace);
            token.following_whitespace.extend(following_whitespace);

            tokens.push(token);
        }

        Self::new(tokens, source.to_owned())
    }

    /// Returns the token `idx` positions forward from the cursor.
    ///
    /// If `idx` would exceed the end of the list, returns the last token.
    /// This is the parser's primary lookahead mechanism; the saturating
    /// behavior simplifies callers that would otherwise need to bounds-check
    /// before every peek.
    ///
    /// # Panics
    ///
    /// Panics if the list is empty. Callers should ensure
    /// `self.length() > 0` (e.g. via [`TokenList::finished`]) before peeking.
    #[must_use]
    pub fn get_token_forward(&self, idx: usize) -> &Token {
        if self.token_index + idx >= self.tokens.len() {
            return &self.tokens[self.tokens.len() - 1];
        }
        &self.tokens[self.token_index + idx]
    }

    /// Returns the token `idx` positions backward from the cursor.
    ///
    /// If the cursor is at index 0, returns the first token regardless of
    /// `idx`.
    ///
    /// # Panics
    ///
    /// Panics if the list is empty, or if `idx > token_index` (i.e. peeking
    /// backward past the start). Callers should bounds-check via
    /// [`TokenList::get_index`] before peeking backward.
    #[must_use]
    pub fn get_token_backward(&self, idx: usize) -> &Token {
        if self.token_index == 0 {
            return &self.tokens[0];
        }
        &self.tokens[self.token_index - idx]
    }

    /// Returns the token at the cursor's current position.
    ///
    /// If the cursor is past the end of the list, returns the last token.
    ///
    /// # Panics
    ///
    /// Panics if the list is empty. The parser ensures the cursor is over a
    /// valid token before each call.
    #[must_use]
    pub fn current_token(&self) -> &Token {
        if self.token_index >= self.tokens.len() {
            return self.tokens.last().expect("TokenList must not be empty");
        }
        &self.tokens[self.token_index]
    }

    /// Returns the cursor's current index.
    #[must_use]
    pub fn get_index(&self) -> usize {
        self.token_index
    }

    /// Returns `true` if the cursor is at or past the end of the list.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.token_index >= self.tokens.len()
    }

    /// Advances the cursor by one. No-op once the cursor is at end-of-list.
    pub fn increase_index(&mut self) {
        if self.finished() {
            return;
        }
        self.token_index += 1;
    }

    /// Retreats the cursor by one. No-op when the cursor is at index 0.
    pub fn decrease_index(&mut self) {
        if self.token_index == 0 {
            return;
        }
        self.token_index -= 1;
    }

    /// Advances a caller-supplied lookahead index, capped at the end of the
    /// list relative to the cursor's current position.
    ///
    /// Used by the parser when doing multi-token lookahead without committing
    /// to advancing the main cursor: the caller passes its own `&mut usize`
    /// offset and this method bumps it while respecting the list bounds.
    pub fn index_increase_index(&mut self, index: &mut usize) {
        if *index + self.token_index >= self.tokens.len() {
            return;
        }
        *index += 1;
    }
}
