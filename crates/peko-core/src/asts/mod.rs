//! # Peko Core ASTs
//!
//! Structural representation of every kind of Pekoscript AST node, plus the
//! [`PekoAST`] sum type that wraps them.
//!
//! AST nodes are split across four submodules by syntactic role:
//!
//! * [`values`] -- literals (numbers, strings, characters, booleans, ...)
//! * [`expressions`] -- anything that produces a value at runtime
//! * [`statements`] -- anything that performs an action but doesn't produce
//!   a value
//! * [`declarations`] -- anything that introduces a new named entity
//!
//! Cross-cutting infrastructure (source positions, visibility, documentation
//! metadata, and the [`data_structures::Spanned`] trait) lives in
//! [`data_structures`].

#[cfg(test)]
mod tests;

pub mod data_structures;
pub mod declarations;
pub mod expressions;
pub mod statements;
pub mod values;

use derive_new::new;

pub use data_structures::Spanned;
use data_structures::{PositionData, placeholder_position};

/// A placeholder AST used by the parser during error recovery/expression parsing
/// (as the operator in an op stack) and by the simulator as a default value.
///
/// The `value` field carries a free-form string whose meaning depends on the
/// context where the placeholder was inserted. The parser leans on this
/// during binary- and unary-expression parsing (as previously mentioned).
#[derive(Clone, new)]
pub struct PlaceholderAST {
    pub value: String,
}

/// A source comment (`// ...`), captured as a first-class node so a formatter
/// can reproduce it. The parser only emits these when comment capture is
/// requested; ordinary compilation still skips comments, so the simulator and
/// code generator never receive one.
///
/// The `text` is the whole comment line including the leading `//` marker, with
/// surrounding indentation and the trailing newline stripped.
#[derive(Clone, new)]
pub struct CommentAST {
    pub text: String,
    pub start: PositionData,
    pub end: PositionData,
}

impl Spanned for CommentAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// The standard-library prelude injected into every non-foundational module,
/// mirroring the type-providing imports the main source file receives. `core`
/// and `collections` are unpacked so their types (`string`, `bool`, `number`,
/// `Array`, and the rest) resolve without qualification, the same way they do
/// in the main file. The foundational modules that bootstrap the standard
/// library (`core`, `collections`, `runtime`) are excluded to avoid an import
/// cycle; each declares its own imports explicitly.
///
/// `module_name` is the imported module's bare name (the last path segment).
/// Only unpacked imports are injected because an unpacked import never
/// conflicts with a module that also imports the same file explicitly.
pub fn module_prelude_imports(module_name: &str) -> Vec<PekoAST> {
    const FOUNDATIONAL: [&str; 3] = ["core", "collections", "runtime"];
    if FOUNDATIONAL.contains(&module_name) {
        return Vec::new();
    }

    fn path_segments(path: &str) -> Vec<data_structures::PositionedValue<String>> {
        path.split("::")
            .map(|segment| data_structures::PositionedValue::create_no_position(segment.to_owned()))
            .collect()
    }

    fn unpack_import(path: &str) -> PekoAST {
        PekoAST::ImportStatement(statements::ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            path_segments(path),
            None,
            vec![data_structures::UnpackItem::All],
            None,
            false,
        ))
    }

    fn aliased_import(path: &str, alias: &str) -> PekoAST {
        PekoAST::ImportStatement(statements::ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            path_segments(path),
            Some(data_structures::PositionedValue::create_no_position(
                alias.to_owned(),
            )),
            Vec::new(),
            None,
            false,
        ))
    }

    let mut imports = vec![
        unpack_import("std::core"),
        unpack_import("std::collections"),
    ];

    // bundle:: is compile-time project metadata, available in every module the
    // same way it is in the main file. The bundle module itself is excluded so
    // it does not import itself. Unlike core/collections it is aliased (used
    // as `bundle::name`), so no module should also import it explicitly.
    if module_name != "bundle" {
        imports.push(aliased_import("std::bundle", "bundle"));
    }

    imports
}

/// The structural representation of any Pekoscript AST.
///
/// Each variant wraps a concrete AST type defined in one of the four
/// submodules. The variants are grouped in declaration order: values,
/// expressions, statements, declarations, with [`PekoAST::Placeholder`]
/// last.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum PekoAST {
    // Value ASTs
    Char(values::CharAST),
    Number(values::NumberAST),
    Boolean(values::BooleanAST),
    String(values::StringAST),
    EncryptedString(values::EncryptedStringAST),
    Null(values::NullAST),

    // Expression ASTs
    Array(expressions::ArrayAST),
    Map(expressions::MapAST),
    VariableReference(expressions::VariableReferenceAST),
    FunctionCall(expressions::FunctionCallAST),
    ObjectConstruction(expressions::ObjectConstructionAST),
    ObjectAccess(expressions::ObjectAccessAST),
    ArrayAccess(expressions::ArrayAccessAST),
    BinaryExpression(expressions::BinaryExpressionAST),
    UnaryExpression(expressions::UnaryExpressionAST),
    ModuleAccess(expressions::ModuleAccessAST),
    Unwrap(expressions::UnwrapAST),
    Cast(expressions::CastAST),
    PekoXTag(expressions::PekoXTagAST),
    Range(expressions::RangeAST),

    // Statement ASTs
    VariableReassignment(statements::VariableReassignmentAST),
    Return(statements::ReturnAST),
    IfStatement(statements::IfStatementAST),
    Switch(statements::SwitchStatementAST),
    WhileLoop(statements::WhileLoopAST),
    ForLoop(statements::ForLoopAST),
    Break(statements::BreakAST),
    Continue(statements::ContinueAST),
    ImportStatement(statements::ImportStatementAST),
    LinkStatement(statements::LinkStatementAST),
    StyleStatement(statements::StyleStatementAST),
    PlatformStatement(statements::PlatformStatementAST),

    // Declaration ASTs
    NewVariable(declarations::NewVariableAST),
    Destructure(declarations::DestructureAST),
    FunctionDeclaration(declarations::FunctionDeclarationAST),
    Closure(declarations::ClosureAST),
    Class(declarations::ClassAST),
    Trait(declarations::TraitDeclarationAST),
    Enum(declarations::EnumDeclarationAST),
    ModuleCreation(declarations::ModuleCreationAST),

    // Placeholder
    Placeholder(PlaceholderAST),

    // Comment (only present when comment capture is requested, e.g. formatting)
    Comment(CommentAST),
}

impl Spanned for PekoAST {
    /// Returns a reference to the start position of this AST's source span by
    /// dispatching to the inner variant's [`Spanned`] impl.
    fn get_start(&self) -> &PositionData {
        match self {
            // Values
            Self::Char(ast) => ast.get_start(),
            Self::Number(ast) => ast.get_start(),
            Self::Boolean(ast) => ast.get_start(),
            Self::String(ast) => ast.get_start(),
            Self::EncryptedString(ast) => ast.get_start(),
            Self::Null(ast) => ast.get_start(),

            // Expressions
            Self::Array(ast) => ast.get_start(),
            Self::Map(ast) => ast.get_start(),
            Self::VariableReference(ast) => ast.get_start(),
            Self::FunctionCall(ast) => ast.get_start(),
            Self::ObjectConstruction(ast) => ast.get_start(),
            Self::ObjectAccess(ast) => ast.get_start(),
            Self::ArrayAccess(ast) => ast.get_start(),
            Self::BinaryExpression(ast) => ast.get_start(),
            Self::UnaryExpression(ast) => ast.get_start(),
            Self::ModuleAccess(ast) => ast.get_start(),
            Self::Unwrap(ast) => ast.get_start(),
            Self::Cast(ast) => ast.get_start(),
            Self::PekoXTag(ast) => ast.get_start(),
            Self::Range(ast) => ast.get_start(),

            // Statements
            Self::VariableReassignment(ast) => ast.get_start(),
            Self::Return(ast) => ast.get_start(),
            Self::IfStatement(ast) => ast.get_start(),
            Self::Switch(ast) => ast.get_start(),
            Self::WhileLoop(ast) => ast.get_start(),
            Self::ForLoop(ast) => ast.get_start(),
            Self::Break(ast) => ast.get_start(),
            Self::Continue(ast) => ast.get_start(),
            Self::ImportStatement(ast) => ast.get_start(),
            Self::LinkStatement(ast) => ast.get_start(),
            Self::StyleStatement(ast) => ast.get_start(),
            Self::PlatformStatement(ast) => ast.get_start(),

            // Declarations
            Self::NewVariable(ast) => ast.get_start(),
            Self::Destructure(ast) => ast.get_start(),
            Self::FunctionDeclaration(ast) => ast.get_start(),
            Self::Closure(ast) => ast.get_start(),
            Self::Class(ast) => ast.get_start(),
            Self::Trait(ast) => ast.get_start(),
            Self::Enum(ast) => ast.get_start(),
            Self::ModuleCreation(ast) => ast.get_start(),

            // Placeholder (synthetic, no real span).
            Self::Placeholder(_) => placeholder_position(),

            // Comment carries a real span.
            Self::Comment(ast) => ast.get_start(),
        }
    }

    /// Returns a reference to the end position of this AST's source span by
    /// dispatching to the inner variant's [`Spanned`] impl.
    fn get_end(&self) -> &PositionData {
        match self {
            // Values
            Self::Char(ast) => ast.get_end(),
            Self::Number(ast) => ast.get_end(),
            Self::Boolean(ast) => ast.get_end(),
            Self::String(ast) => ast.get_end(),
            Self::EncryptedString(ast) => ast.get_end(),
            Self::Null(ast) => ast.get_end(),

            // Expressions
            Self::Array(ast) => ast.get_end(),
            Self::Map(ast) => ast.get_end(),
            Self::VariableReference(ast) => ast.get_end(),
            Self::FunctionCall(ast) => ast.get_end(),
            Self::ObjectConstruction(ast) => ast.get_end(),
            Self::ObjectAccess(ast) => ast.get_end(),
            Self::ArrayAccess(ast) => ast.get_end(),
            Self::BinaryExpression(ast) => ast.get_end(),
            Self::UnaryExpression(ast) => ast.get_end(),
            Self::ModuleAccess(ast) => ast.get_end(),
            Self::Unwrap(ast) => ast.get_end(),
            Self::Cast(ast) => ast.get_end(),
            Self::PekoXTag(ast) => ast.get_end(),
            Self::Range(ast) => ast.get_end(),

            // Statements
            Self::VariableReassignment(ast) => ast.get_end(),
            Self::Return(ast) => ast.get_end(),
            Self::IfStatement(ast) => ast.get_end(),
            Self::Switch(ast) => ast.get_end(),
            Self::WhileLoop(ast) => ast.get_end(),
            Self::ForLoop(ast) => ast.get_end(),
            Self::Break(ast) => ast.get_end(),
            Self::Continue(ast) => ast.get_end(),
            Self::ImportStatement(ast) => ast.get_end(),
            Self::LinkStatement(ast) => ast.get_end(),
            Self::StyleStatement(ast) => ast.get_end(),
            Self::PlatformStatement(ast) => ast.get_end(),

            // Declarations
            Self::NewVariable(ast) => ast.get_end(),
            Self::Destructure(ast) => ast.get_end(),
            Self::FunctionDeclaration(ast) => ast.get_end(),
            Self::Closure(ast) => ast.get_end(),
            Self::Class(ast) => ast.get_end(),
            Self::Trait(ast) => ast.get_end(),
            Self::Enum(ast) => ast.get_end(),
            Self::ModuleCreation(ast) => ast.get_end(),

            // Placeholder (synthetic, no real span).
            Self::Placeholder(_) => placeholder_position(),

            // Comment carries a real span.
            Self::Comment(ast) => ast.get_end(),
        }
    }
}

impl PekoAST {
    /// Returns a reference to the start position of this AST's source span.
    ///
    /// This is an ergonomic forwarder to [`Spanned::get_start`] so that
    /// callers don't need to bring the [`Spanned`] trait into scope.
    #[must_use]
    pub fn get_start(&self) -> &PositionData {
        <Self as Spanned>::get_start(self)
    }

    /// Returns a reference to the end position of this AST's source span.
    ///
    /// This is an ergonomic forwarder to [`Spanned::get_end`] so that
    /// callers don't need to bring the [`Spanned`] trait into scope.
    #[must_use]
    pub fn get_end(&self) -> &PositionData {
        <Self as Spanned>::get_end(self)
    }
}
