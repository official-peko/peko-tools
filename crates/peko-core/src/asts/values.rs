//! Value-literal AST nodes.
//!
//! Each type here represents a single literal value in Pekoscript source --
//! a number, a boolean, a character, a `null`, a string, or an encrypted
//! string. These are leaves in the AST: they carry their own positions and
//! payloads but contain no child nodes.

use derive_new::new;

use super::data_structures::{PositionData, PositionedValue, Spanned, StringChunk};

/// A boolean literal: `true` or `false`.
#[derive(Clone, new)]
pub struct BooleanAST {
    pub value: PositionedValue<bool>,
}

impl Spanned for BooleanAST {
    fn get_start(&self) -> &PositionData {
        &self.value.start
    }

    fn get_end(&self) -> &PositionData {
        &self.value.end
    }
}

/// A numeric literal.
///
/// Pekoscript uses a single AST node for all numeric literals (integers,
/// floats, doubles). The `integer` flag records whether the literal had a
/// fractional component in source; the actual value is stored as `f32`
/// regardless, with the simulator and code generator deciding the final type
/// based on the surrounding context.
#[derive(Clone, new)]
pub struct NumberAST {
    pub value: PositionedValue<f64>,
    pub integer: bool,
}

impl Spanned for NumberAST {
    fn get_start(&self) -> &PositionData {
        &self.value.start
    }

    fn get_end(&self) -> &PositionData {
        &self.value.end
    }
}

/// A character literal: `'a'`.
#[derive(Clone, new)]
pub struct CharAST {
    pub value: PositionedValue<char>,
}

impl Spanned for CharAST {
    fn get_start(&self) -> &PositionData {
        &self.value.start
    }

    fn get_end(&self) -> &PositionData {
        &self.value.end
    }
}

/// An encrypted string literal.
///
/// The `encrypt` field holds the raw source form of the encrypted string
/// (actual encryption/decryption happens downstream of the parser in codegen).
#[derive(Clone, new)]
pub struct EncryptedStringAST {
    pub encrypt: PositionedValue<String>,
}

impl Spanned for EncryptedStringAST {
    fn get_start(&self) -> &PositionData {
        &self.encrypt.start
    }

    fn get_end(&self) -> &PositionData {
        &self.encrypt.end
    }
}

/// The `null` literal.
///
/// `NullAST` carries only its source span, there is no value payload because
/// `null` is itself the value.
#[derive(Clone, new)]
pub struct NullAST {
    pub start: PositionData,
    pub end: PositionData,
}

impl Spanned for NullAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A string literal, either plain (`"value"`) or interpolated
/// (`` `value ${expr}` ``).
///
/// The string body is broken into a sequence of [`StringChunk`]s that
/// alternate between literal text and interpolated expressions. A plain
/// string has exactly one text chunk; an interpolated string has any mix.
#[derive(Clone, new)]
pub struct StringAST {
    pub start: PositionData,
    pub end: PositionData,
    pub interpolated: bool,
    pub chunks: Vec<StringChunk>,
}

impl Spanned for StringAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}
