//! # Peko Core ASTs
//!
//! Structural representation of every kind of Pekoscript AST node, plus the
//! [`PekoAST`] sum type that wraps them.
//!
//! AST nodes are split across four submodules by syntactic role:
//!
//! * [`values`] -- literals (numbers, strings, characters, booleans, …)
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
    FunctionDeclaration(declarations::FunctionDeclarationAST),
    Closure(declarations::ClosureAST),
    Class(declarations::ClassAST),
    Trait(declarations::TraitDeclarationAST),
    Enum(declarations::EnumDeclarationAST),
    ModuleCreation(declarations::ModuleCreationAST),

    // Placeholder
    Placeholder(PlaceholderAST),
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
            Self::FunctionDeclaration(ast) => ast.get_start(),
            Self::Closure(ast) => ast.get_start(),
            Self::Class(ast) => ast.get_start(),
            Self::Trait(ast) => ast.get_start(),
            Self::Enum(ast) => ast.get_start(),
            Self::ModuleCreation(ast) => ast.get_start(),

            // Placeholder (synthetic, no real span).
            Self::Placeholder(_) => placeholder_position(),
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
            Self::FunctionDeclaration(ast) => ast.get_end(),
            Self::Closure(ast) => ast.get_end(),
            Self::Class(ast) => ast.get_end(),
            Self::Trait(ast) => ast.get_end(),
            Self::Enum(ast) => ast.get_end(),
            Self::ModuleCreation(ast) => ast.get_end(),

            // Placeholder (synthetic, no real span).
            Self::Placeholder(_) => placeholder_position(),
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
