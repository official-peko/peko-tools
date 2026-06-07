//! Shared data structures used across all Pekoscript AST node types.
//!
//! This module provides infrastructure that every AST file consumes:
//! source-position tracking ([`PositionData`], [`PositionedValue`], the
//! [`Spanned`] trait), visibility modifiers ([`VisibilityData`]),
//! documentation metadata ([`DocInfo`]), string-literal chunks
//! ([`StringChunk`]), expression operators, conditional bodies, and
//! class-method descriptors.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::OnceLock;

use derive_new::new;
use indexmap::IndexMap;

use crate::lexer;
use crate::types::{self, PekoType};

use super::{PekoAST, expressions::FunctionCallAST};

// ----- Spanned trait -------------------------------------------------------

/// Anything that has a source span.
///
/// Implemented by [`PekoAST`] (which dispatches to its inner variant) and by
/// each concrete AST node type. Callers can use this trait directly when they
/// only care about positions, or call through [`PekoAST`]'s inherent
/// `get_start` / `get_end` methods (which forward to the trait).
pub trait Spanned {
    /// Returns a reference to the inclusive start position of this node's
    /// source span.
    fn get_start(&self) -> &PositionData;

    /// Returns a reference to the inclusive end position of this node's
    /// source span.
    fn get_end(&self) -> &PositionData;
}

/// Stable storage for the position returned by [`Spanned`]
/// impls on nodes that don't correspond to a real source location
/// (currently just [`super::PlaceholderAST`]).
pub(crate) fn placeholder_position() -> &'static PositionData {
    static PLACEHOLDER: OnceLock<PositionData> = OnceLock::new();
    PLACEHOLDER.get_or_init(PositionData::default)
}

// ----- Module / import metadata --------------------------------------------

/// A list of symbols unpacked from a module via an `import` statement.
#[derive(Clone, new)]
pub struct ModuleUnpacks {
    pub module_name: PositionedValue<String>,
    pub unpacked_items: Vec<UnpackItem>,
}

/// One item in an `import` statement's unpack list.
///
/// Pekoscript imports can pull in a single symbol, recursively unpack symbols
/// from a nested module, or glob-import everything (`*`).
#[derive(Clone, new)]
pub enum UnpackItem {
    /// Recursive unpack of a nested module's symbols.
    ModuleSymbols(ModuleUnpacks),
    /// A single named symbol.
    Symbol(PositionedValue<String>),
    /// Glob import (`*`).
    All,
}

// ----- Source positions -----------------------------------------------------

/// A single character position within a source file.
///
/// Carries both a line/column pair (for human-readable diagnostics) and a
/// flat byte index into the source (for cheap span comparisons).
#[derive(Clone, Debug, new)]
pub struct PositionData {
    pub column: usize,
    pub index: usize,
    pub line: usize,
    pub file: PathBuf,
}

impl Default for PositionData {
    /// Constructs a position used for ASTs that don't correspond to
    /// any concrete source location (synthetic nodes, placeholders, etc).
    ///
    /// `line` and `column` are both `1` so the position reads as "the start
    /// of line 1" rather than the impossible "line 0"; `index` is `0` and
    /// `file` is empty.
    fn default() -> Self {
        Self {
            column: 1,
            index: 0,
            line: 1,
            file: PathBuf::new(),
        }
    }
}

impl PositionData {
    /// Returns `true` if `other` refers to the same byte offset in the same
    /// file as this position.
    #[must_use]
    pub fn equals(&self, other: PositionData) -> bool {
        self.index == other.index && self.file == other.file
    }

    /// Returns `true` if this position comes strictly before `other` in source
    /// order (by flat byte index).
    #[must_use]
    pub fn positioned_before(&self, other: PositionData) -> bool {
        self.index < other.index
    }

    /// Returns `true` if this position comes at or before `other` in source
    /// order (by flat byte index).
    #[must_use]
    pub fn positioned_before_inclusive(&self, other: PositionData) -> bool {
        self.index <= other.index
    }
}

/// A value wrapped with the source span it was parsed from.
///
/// `PositionedValue<T>` is the standard way to thread source positions
/// through the AST without polluting every value-bearing field with separate
/// `start` / `end` neighbors. Equality and hashing delegate to the inner
/// value -- positions are *carried* but not *compared*, so two identical
/// identifiers at different source positions are considered equal
/// (done for ability to store in structures like `HashMap`).
#[derive(Clone, Debug, new)]
pub struct PositionedValue<T> {
    pub value: T,
    pub start: PositionData,
    pub end: PositionData,
}

impl<T: Hash> Hash for PositionedValue<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<T: PartialEq> PartialEq for PositionedValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq> Eq for PositionedValue<T> {}

impl PositionedValue<String> {
    /// Constructs a `PositionedValue<String>` from a lexer token, copying its
    /// value and span.
    #[must_use]
    pub fn from_token(token: &lexer::Token) -> Self {
        Self {
            value: token.get_value().clone(),
            start: token.get_start().clone(),
            end: token.get_end().clone(),
        }
    }
}

impl<T> PositionedValue<T> {
    /// Wraps a value with a default position, for AST nodes that don't
    /// correspond to a concrete source span (synthetic, placeholder, etc).
    #[must_use]
    pub fn create_no_position(value: T) -> Self {
        Self {
            value,
            start: PositionData::default(),
            end: PositionData::default(),
        }
    }

    /// Returns `true` if `position` falls within this value's span and the
    /// two positions refer to the same source file.
    ///
    /// File paths are canonicalized before comparison so that logically-equal
    /// paths (e.g. with `./` components or symlinks) compare equal. If
    /// canonicalization fails on either side -- typically because the file
    /// no longer exists or refers to in-memory source -- the raw paths are
    /// compared instead as a best-effort fallback.
    #[must_use]
    pub fn holds_position(&self, position: PositionData) -> bool {
        let self_file = self
            .start
            .file
            .canonicalize()
            .unwrap_or_else(|_| self.start.file.clone());
        let other_file = position
            .file
            .canonicalize()
            .unwrap_or_else(|_| position.file.clone());

        self_file == other_file
            && self.start.positioned_before_inclusive(position.clone())
            && position.positioned_before_inclusive(self.end.clone())
    }
}

// ----- Visibility ----------------------------------------------------------

/// Visibility and behavior modifiers on a declaration.
///
/// Each flag corresponds to a square-bracketed modifier in source
/// (`[private]`, `[constant]`, etc). The struct stores every modifier as an
/// independent `bool` rather than as a single enum because many modifiers
/// can apply simultaneously (e.g. `[external constant]`).
#[derive(Clone, Debug, new)]
pub struct VisibilityData {
    /// `[private]` -- only allows local access.
    pub private: bool,
    /// `[constant]` -- disallows modification of the variable.
    pub constant: bool,
    /// `[external]` -- linkage signifier; suppresses name mangling.
    pub external: bool,
    /// `[notrack]` -- for runtime-exit handling; suppresses position tracking
    /// when printing error messages from this function.
    pub notrack: bool,
    /// `[variadic]` -- C-style variadic function; intended for linking against
    /// foreign variadic functions.
    pub variadic: bool,
    /// `[blockexit]` -- signals that a function will exit the current block,
    /// suppressing missing-return errors at the call site.
    pub blockexit: bool,
    /// `[hidden]` -- hides the symbol from code-suggestion tooling.
    pub hidden: bool,
    /// `[state]` -- class-attribute only; calls the parent class when modified.
    pub state: bool,
    /// `[mutates]` -- class-method only; signifies the method modifies the
    /// class's attributes.
    pub mutates: bool,

    /// `[gcsafe]` -- for garbage collection
    pub gc_safepoint: bool,
}

/// Active-flag table for [`VisibilityData`] formatting.
///
/// Order matters: this is the order modifiers appear in the rendered
/// `[a b c]` form. Defined as a constant to keep [`VisibilityData::flag_names`]
/// and the [`Display`] impl in lockstep.
const VISIBILITY_FLAG_ORDER: &[(fn(&VisibilityData) -> bool, &str)] = &[
    (|v| v.private, "private"),
    (|v| v.constant, "constant"),
    (|v| v.external, "external"),
    (|v| v.notrack, "notrack"),
    (|v| v.variadic, "variadic"),
    (|v| v.blockexit, "blockexit"),
    (|v| v.hidden, "hidden"),
    (|v| v.state, "state"),
    (|v| v.mutates, "mutator"),
    (|v| v.gc_safepoint, "gcsafe"),
];

impl fmt::Display for VisibilityData {
    /// Renders visibility as `[a b c]`, where `a`, `b`, `c` are the names of
    /// the active flags in declaration order. An all-default visibility
    /// renders as `[]`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let active: Vec<&str> = VISIBILITY_FLAG_ORDER
            .iter()
            .filter_map(|(is_set, name)| is_set(self).then_some(*name))
            .collect();
        write!(f, "[{}]", active.join(" "))
    }
}

impl VisibilityData {
    /// Visibility with only `[constant]` set.
    #[must_use]
    pub fn constant() -> Self {
        Self {
            constant: true,
            ..Self::open_visibility()
        }
    }

    /// Visibility with no modifiers set.
    #[must_use]
    pub fn open_visibility() -> Self {
        Self {
            private: false,
            constant: false,
            external: false,
            notrack: false,
            variadic: false,
            blockexit: false,
            hidden: false,
            state: false,
            mutates: false,
            gc_safepoint: false,
        }
    }
}

// ----- Documentation -------------------------------------------------------

/// Documentation extracted from `///` doc-comments attached to a declaration.
#[derive(new, Clone, Default, Debug)]
pub struct DocInfo {
    pub description: String,
    pub parameter_docs: HashMap<String, String>,
    pub examples: Vec<String>,
}

// ----- String literal chunks -----------------------------------------------

/// Content of one piece of a string literal.
///
/// A `StringChunk` is either literal source text or an interpolation site
/// containing one or more AST nodes whose values are spliced into the
/// surrounding string at runtime.
#[derive(Clone)]
pub enum StringChunkContent {
    /// Literal text that appears verbatim in the resulting string.
    Text(String),
    /// One or more ASTs whose evaluated values are spliced into the string.
    Interpolation(Vec<PekoAST>),
}

/// One piece of a string literal, with its source span.
///
/// Pekoscript's interpolated strings (backtick-delimited) are parsed into a
/// sequence of `StringChunk`s alternating between literal text and
/// interpolation sites.
#[derive(Clone)]
pub struct StringChunk {
    pub start: PositionData,
    pub end: PositionData,
    pub content: StringChunkContent,
}

impl StringChunk {
    /// Constructs a chunk of literal text.
    #[must_use]
    pub fn new_text(start: PositionData, end: PositionData, text: String) -> Self {
        Self {
            start,
            end,
            content: StringChunkContent::Text(text),
        }
    }

    /// Constructs an interpolation chunk.
    #[must_use]
    pub fn new_interpolation(
        start: PositionData,
        end: PositionData,
        interpolation: Vec<PekoAST>,
    ) -> Self {
        Self {
            start,
            end,
            content: StringChunkContent::Interpolation(interpolation),
        }
    }

    /// Returns `true` if this chunk is literal text.
    #[must_use]
    pub fn is_text(&self) -> bool {
        matches!(self.content, StringChunkContent::Text(_))
    }

    /// Returns the literal text of this chunk.
    ///
    /// # Panics
    ///
    /// Panics if this chunk is an interpolation. Use [`StringChunk::is_text`]
    /// or match on [`StringChunk::content`] directly to check first.
    #[must_use]
    pub fn get_text(&self) -> String {
        match &self.content {
            StringChunkContent::Text(t) => t.clone(),
            StringChunkContent::Interpolation(_) => {
                panic!("StringChunk::get_text called on interpolation chunk")
            }
        }
    }

    /// Returns the interpolation ASTs of this chunk.
    ///
    /// # Panics
    ///
    /// Panics if this chunk is literal text. Use [`StringChunk::is_text`] or
    /// match on [`StringChunk::content`] directly to check first.
    #[must_use]
    pub fn get_interpolation(&self) -> Vec<PekoAST> {
        match &self.content {
            StringChunkContent::Interpolation(asts) => asts.clone(),
            StringChunkContent::Text(_) => {
                panic!("StringChunk::get_interpolation called on text chunk")
            }
        }
    }
}

// ----- Operators -----------------------------------------------------------

/// Arity classification for an [`ExpressionOperator`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpressionOperatorType {
    /// A one-operand operator (e.g. `!x`, `-x`).
    Unary,
    /// A two-operand operator (e.g. `a + b`, `a == b`).
    Binary,
}

/// An operator token alongside its arity.
#[derive(Clone, new)]
pub struct ExpressionOperator {
    pub operator: PositionedValue<String>,
    pub operator_type: ExpressionOperatorType,
}

// ----- Control-flow bodies -------------------------------------------------

/// A conditional body: a guard expression and the block executed when the
/// guard holds.
///
/// Used as the building block of `if` (one or more `ConditionBody` arms plus
/// an optional `else` block) and `while` (exactly one `ConditionBody`).
#[derive(Clone, new)]
pub struct ConditionBody {
    pub condition: Box<PekoAST>,
    pub body: PositionedValue<Vec<PekoAST>>,
}

// ----- Class methods -------------------------------------------------------

/// Common information attached to every class method, regardless of whether
/// it's a constructor or a regular method.
#[derive(Clone, new)]
pub struct ClassMethodInfo {
    pub start: PositionData,
    pub end: PositionData,
    pub visibility: VisibilityData,
    pub docinfo: Option<DocInfo>,
    pub arguments: IndexMap<PositionedValue<String>, DeclarationArgumentData>,
    pub body: PositionedValue<Vec<PekoAST>>,
    pub varargs_type: Option<types::PekoType>,
    pub varargs_name: PositionedValue<String>,
    pub name: PositionedValue<String>,
}

/// One method declared on a class.
///
/// Constructors and regular methods share most of their structure (captured
/// by [`ClassMethodInfo`]) but differ in their tails: a constructor may call
/// `super(...)`, while a regular method may declare a return type.
#[derive(Clone)]
pub enum ClassMethod {
    /// A `constructor` declaration. The optional payload is the call to the
    /// parent class's constructor, if any.
    Constructor(ClassMethodInfo, Option<FunctionCallAST>),
    /// A regular method declaration. The optional payload is the declared
    /// return type; absence means `void`.
    Method(ClassMethodInfo, Option<types::PekoType>),
}

impl ClassMethod {
    /// Returns the shared method information, regardless of variant.
    #[must_use]
    pub fn get_info(&self) -> &ClassMethodInfo {
        match self {
            Self::Constructor(info, _) | Self::Method(info, _) => info,
        }
    }

    /// Returns the method's declared return type, or `void` if none was
    /// declared (or if this is a constructor -- constructors always return
    /// `void`).
    #[must_use]
    pub fn get_return_type(&self) -> PekoType {
        match self {
            Self::Constructor(_, _) => PekoType::simple_type("void"),
            Self::Method(_, return_type) => return_type
                .clone()
                .unwrap_or_else(|| PekoType::simple_type("void")),
        }
    }
}

// ----- Class attributes ----------------------------------------------------

/// Type and visibility information for a class attribute.
#[derive(Clone, new)]
pub struct ClassAttributeData {
    pub visibility: VisibilityData,
    pub docinfo: Option<DocInfo>,
    pub attribute_type: Box<types::PekoType>,
}

// ----- Function arguments --------------------------------------------------

/// Type, visibility, and optional default value for an argument in any
/// function-like declaration (function, closure, method, constructor).
#[derive(Clone, new)]
pub struct DeclarationArgumentData {
    pub start: PositionData,
    pub end: PositionData,
    pub argument_type: types::PekoType,
    pub default_value: Option<PekoAST>,
    pub visibility: VisibilityData,
}
