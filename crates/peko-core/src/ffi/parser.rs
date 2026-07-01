//! The tokenizer and parser for `.peko.h` headers.
//!
//! The tokenizer emits only the tokens a marked declaration can contain
//! (markers, identifiers, `(`, `)`, `,`, `;`, `...`, `*`) and discards
//! whitespace, preprocessor lines, and ordinary comments. The parser walks the
//! tokens, parses a declaration after each marker, and recovers at the next
//! marker on error.

use super::{FfiError, FfiFunction, FfiModule, FfiParam, FfiType, FfiVariable, ParsedHeader};

/// Parse a `.peko.h` source into its FFI declarations and any errors.
pub fn parse_header(source: &str) -> ParsedHeader {
    let tokens = tokenize(source);
    Parser::new(tokens).parse()
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// A token a marked declaration can contain.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    MarkerFn,
    MarkerVar,
    Ident(String),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Ellipsis,
    Star,
}

/// A token with its source position.
#[derive(Debug, Clone)]
struct Token {
    tok: Tok,
    line: usize,
    column: usize,
}

/// Tokenize a `.peko.h`, discarding whitespace, preprocessor lines, and
/// ordinary comments.
fn tokenize(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;
    let mut column = 1;

    while index < chars.len() {
        let c = chars[index];
        match c {
            '\n' => {
                index += 1;
                line += 1;
                column = 1;
            }
            ' ' | '\t' | '\r' => {
                index += 1;
                column += 1;
            }
            // Preprocessor line: discard to the end of the line.
            '#' => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                    column += 1;
                }
            }
            // Line comment.
            '/' if chars.get(index + 1) == Some(&'/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                    column += 1;
                }
            }
            // Block comment.
            '/' if chars.get(index + 1) == Some(&'*') => {
                index += 2;
                column += 2;
                while index < chars.len() {
                    if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                        index += 2;
                        column += 2;
                        break;
                    }
                    if chars[index] == '\n' {
                        line += 1;
                        column = 1;
                    } else {
                        column += 1;
                    }
                    index += 1;
                }
            }
            '(' => push_simple(&mut tokens, Tok::LParen, line, &mut index, &mut column),
            ')' => push_simple(&mut tokens, Tok::RParen, line, &mut index, &mut column),
            ',' => push_simple(&mut tokens, Tok::Comma, line, &mut index, &mut column),
            ';' => push_simple(&mut tokens, Tok::Semicolon, line, &mut index, &mut column),
            '*' => push_simple(&mut tokens, Tok::Star, line, &mut index, &mut column),
            '.' if chars.get(index + 1) == Some(&'.') && chars.get(index + 2) == Some(&'.') => {
                tokens.push(Token {
                    tok: Tok::Ellipsis,
                    line,
                    column,
                });
                index += 3;
                column += 3;
            }
            c if c.is_alphabetic() || c == '_' => {
                let start_column = column;
                let mut ident = String::new();
                while index < chars.len()
                    && (chars[index].is_alphanumeric() || chars[index] == '_')
                {
                    ident.push(chars[index]);
                    index += 1;
                    column += 1;
                }
                // p_fn and p_var lead a declaration and tokenize as markers.
                let tok = match ident.as_str() {
                    "p_fn" => Tok::MarkerFn,
                    "p_var" => Tok::MarkerVar,
                    _ => Tok::Ident(ident),
                };
                tokens.push(Token {
                    tok,
                    line,
                    column: start_column,
                });
            }
            // Anything else (stray punctuation between declarations) is ignored.
            _ => {
                index += 1;
                column += 1;
            }
        }
    }

    tokens
}

/// Push a single-character token and advance.
fn push_simple(tokens: &mut Vec<Token>, tok: Tok, line: usize, index: &mut usize, column: &mut usize) {
    tokens.push(Token {
        tok,
        line,
        column: *column,
    });
    *index += 1;
    *column += 1;
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Parser {
        Parser {
            tokens,
            position: 0,
        }
    }

    fn parse(mut self) -> ParsedHeader {
        let mut module = FfiModule::default();
        let mut errors = Vec::new();

        while let Some(token) = self.tokens.get(self.position) {
            match token.tok {
                Tok::MarkerFn => {
                    self.position += 1;
                    match self.parse_function() {
                        Ok(function) => module.functions.push(function),
                        Err(error) => {
                            errors.push(error);
                            self.recover();
                        }
                    }
                }
                Tok::MarkerVar => {
                    self.position += 1;
                    match self.parse_variable() {
                        Ok(variable) => module.variables.push(variable),
                        Err(error) => {
                            errors.push(error);
                            self.recover();
                        }
                    }
                }
                // Anything not a marker is not an FFI declaration.
                _ => self.position += 1,
            }
        }

        ParsedHeader { module, errors }
    }

    fn parse_function(&mut self) -> Result<FfiFunction, FfiError> {
        // Optional `p_gcsafe` attribute, placed before the return type.
        let gc_safe = self.try_consume_ident("p_gcsafe");

        let return_type = self.parse_type()?;
        let name = self.expect_ident("a function name")?;
        self.expect(&Tok::LParen, "(")?;

        let mut params = Vec::new();
        let mut variadic = false;

        if !self.check(&Tok::RParen) {
            // `(void)` means no parameters.
            if self.is_void_param_list() {
                self.position += 1;
            } else {
                loop {
                    if self.check(&Tok::Ellipsis) {
                        self.position += 1;
                        variadic = true;
                        break;
                    }
                    let ty = self.parse_type()?;
                    let param_name = self.expect_ident("a parameter name")?;
                    params.push(FfiParam {
                        name: param_name,
                        ty,
                    });
                    if self.check(&Tok::Comma) {
                        self.position += 1;
                        continue;
                    }
                    break;
                }
            }
        }

        self.expect(&Tok::RParen, ")")?;
        self.expect(&Tok::Semicolon, ";")?;

        Ok(FfiFunction {
            name,
            return_type,
            params,
            variadic,
            gc_safe,
        })
    }

    fn parse_variable(&mut self) -> Result<FfiVariable, FfiError> {
        let ty = self.parse_type()?;
        let name = self.expect_ident("a variable name")?;
        self.expect(&Tok::Semicolon, ";")?;
        Ok(FfiVariable { name, ty })
    }

    /// Parse one type: a bare alias, `void`, or `p_gc(T)`.
    fn parse_type(&mut self) -> Result<FfiType, FfiError> {
        let name = self.expect_ident("a type")?;

        if name == "p_gc" {
            self.expect(&Tok::LParen, "(")?;
            let inner = self.expect_ident("a type")?;
            self.expect(&Tok::RParen, ")")?;
            return Ok(FfiType {
                peko: format!("pointer<{}>", map_pointer_target(&inner)),
            });
        }

        if self.check(&Tok::Star) {
            return Err(self.error("raw pointers are not allowed; use p_gc(T), p_gc_opaque, or p_opaque"));
        }

        match map_bare_type(&name) {
            Some(peko) => Ok(FfiType { peko }),
            None => Err(self.error(&format!("unknown FFI type `{name}`"))),
        }
    }

    /// `true` if the next two tokens are `void )`, the C empty-parameter form.
    fn is_void_param_list(&self) -> bool {
        matches!(self.tokens.get(self.position), Some(Token { tok: Tok::Ident(name), .. }) if name == "void")
            && matches!(self.tokens.get(self.position + 1), Some(Token { tok: Tok::RParen, .. }))
    }

    fn check(&self, tok: &Tok) -> bool {
        matches!(self.tokens.get(self.position), Some(token) if &token.tok == tok)
    }

    /// Consume the next token if it is the named identifier, reporting whether
    /// it was consumed.
    fn try_consume_ident(&mut self, name: &str) -> bool {
        if matches!(self.tokens.get(self.position), Some(Token { tok: Tok::Ident(ident), .. }) if ident == name)
        {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok, label: &str) -> Result<(), FfiError> {
        if self.check(tok) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{label}`")))
        }
    }

    fn expect_ident(&mut self, label: &str) -> Result<String, FfiError> {
        match self.tokens.get(self.position) {
            Some(Token {
                tok: Tok::Ident(name),
                ..
            }) => {
                let name = name.clone();
                self.position += 1;
                Ok(name)
            }
            _ => Err(self.error(&format!("expected {label}"))),
        }
    }

    /// Build an error at the current token, or at the end of input.
    fn error(&self, message: &str) -> FfiError {
        let (line, column) = self
            .tokens
            .get(self.position)
            .map(|token| (token.line, token.column))
            .unwrap_or((0, 0));
        FfiError {
            line,
            column,
            message: message.to_owned(),
        }
    }

    /// Skip to the start of the next declaration after an error.
    ///
    /// Stops before the next marker, or after the next `;`.
    fn recover(&mut self) {
        while let Some(token) = self.tokens.get(self.position) {
            match token.tok {
                Tok::MarkerFn | Tok::MarkerVar => return,
                Tok::Semicolon => {
                    self.position += 1;
                    return;
                }
                _ => self.position += 1,
            }
        }
    }
}

/// Map a bare type token to its Peko FFI type.
fn map_bare_type(name: &str) -> Option<String> {
    let peko = match name {
        // A C `char` is a raw 8-bit scalar; the `char` value type wraps an i8.
        "p_ch" => "i8",
        "p_i1" => "i1",
        "p_i8" => "i8",
        "p_i16" => "i16",
        "p_i32" => "i32",
        "p_i64" => "i64",
        "p_i128" => "i128",
        "p_f16" => "f16",
        "p_f32" => "f32",
        "p_f64" => "f64",
        // A C boolean is a raw 1-bit scalar; the `bool` value type wraps an i1.
        "p_bool" => "i1",
        "p_cstr" => "cstr",
        "p_opaque" => "opaque",
        "p_gc_opaque" => "pointer<void>",
        "void" => "void",
        _ => return None,
    };
    Some(peko.to_owned())
}

/// Map the target type inside `p_gc(...)`.
///
/// A known alias maps to its Peko type. Any other identifier is a user type and
/// passes through unchanged.
fn map_pointer_target(name: &str) -> String {
    map_bare_type(name).unwrap_or_else(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::parse_header;

    #[test]
    fn parses_marked_functions_and_variables() {
        let source = "\
#include <peko.h>

// an ordinary comment is ignored
p_i32 not_exported(p_i32 x);

p_fn p_gc_opaque mem_alloc(p_i32 bytes);
p_fn void mem_free(p_opaque ptr);
p_fn p_i32 printf(p_cstr fmt, ...);
p_fn p_gc(p_i32) box_int(p_i32 value);
p_var p_i64 count;
";
        let parsed = parse_header(source);
        assert!(parsed.errors.is_empty(), "errors: {:?}", parsed.errors);

        let functions = &parsed.module.functions;
        assert_eq!(functions.len(), 4);

        assert_eq!(functions[0].name, "mem_alloc");
        assert_eq!(functions[0].return_type.peko, "pointer<void>");
        assert_eq!(functions[0].params.len(), 1);
        assert_eq!(functions[0].params[0].name, "bytes");
        assert_eq!(functions[0].params[0].ty.peko, "i32");
        assert!(!functions[0].variadic);

        assert_eq!(functions[1].return_type.peko, "void");
        assert_eq!(functions[1].params[0].ty.peko, "opaque");

        assert_eq!(functions[2].name, "printf");
        assert!(functions[2].variadic);
        assert_eq!(functions[2].params.len(), 1);

        assert_eq!(functions[3].return_type.peko, "pointer<i32>");

        assert_eq!(parsed.module.variables.len(), 1);
        assert_eq!(parsed.module.variables[0].name, "count");
        assert_eq!(parsed.module.variables[0].ty.peko, "i64");
    }

    #[test]
    fn void_parameter_list_means_no_parameters() {
        let parsed = parse_header("p_fn void boot(void);");
        assert!(parsed.errors.is_empty());
        assert_eq!(parsed.module.functions[0].params.len(), 0);
        assert!(!parsed.module.functions[0].variadic);
    }

    #[test]
    fn reports_an_error_and_recovers_at_the_next_marker() {
        let source = "\
p_fn p_bogus broken(p_i32 x);
p_var p_i32 good;
";
        let parsed = parse_header(source);
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("p_bogus"));
        assert_eq!(parsed.module.variables.len(), 1);
        assert_eq!(parsed.module.variables[0].name, "good");
    }

    #[test]
    fn rejects_raw_pointers() {
        let parsed = parse_header("p_var p_i32 * ptr;");
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.contains("raw pointers"));
    }

    #[test]
    fn ignores_linkage_wrappers() {
        let source = "\
#include <peko.h>

PEKO_BEGIN
extern \"C\" {
p_fn p_i32 add(p_i32 a, p_i32 b);
}
PEKO_END
";
        let parsed = parse_header(source);
        assert!(parsed.errors.is_empty(), "errors: {:?}", parsed.errors);
        assert_eq!(parsed.module.functions.len(), 1);
        assert_eq!(parsed.module.functions[0].name, "add");
        assert_eq!(parsed.module.functions[0].params.len(), 2);
    }
}
