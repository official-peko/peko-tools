//! # Peko LLVM Codegen
//!
//! `peko_llvm::codegen` implements LLVM IR code generation for every
//! `PekoAST` and object output for the resulting module.
//!
//! ## Layout
//!
//! - [`context`]: the `PekoCodegenContext` struct, its constructor, and
//!   the `ExecutionContextAlgorithms` impl that core's algorithms drive.
//! - [`builders`]: layered traits that decompose the LLVM-building
//!   surface of `PekoCodegenContext` into navigable pieces; see
//!   [`builders`] module docs for the layer table.
//! - [`data_structures`]: the concrete `Codegen*` types that satisfy
//!   the `Execution*` trait bounds from core.
//! - [`symbol`]: name mangling for Pekoscript symbols.
//! - [`declaration_gen`], [`expression_gen`], [`statement_gen`],
//!   [`value_gen`]: the `PekoValueBuilder` impls that translate each
//!   AST node family to IR.

pub mod builders;
pub mod context;
pub mod data_structures;
pub mod declaration_gen;
pub mod expression_gen;
pub mod statement_gen;
pub mod symbol;
pub mod value_gen;

use std::ffi::CString;

use peko_core::asts::PekoAST;

use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::{context::PekoCodegenContext, data_structures::CodegenValue};

/// Build an owned `CString` from anything string-like.
///
/// Bind the result to a local before taking `.as_ptr()` so the storage
/// outlives the FFI call. Calling `cstr("x").as_ptr()` in one expression
/// is a dangling-pointer bug (the `CString` is dropped at the end of
/// the expression and the pointer is invalidated).
///
/// Panics if `s` contains an interior NUL byte.
pub(crate) fn cstr(s: impl AsRef<str>) -> CString {
    CString::new(s.as_ref()).expect("string contains interior NUL byte")
}

/// Translate a [`PekoAST`] into a [`CodegenValue`] by emitting IR through
/// the supplied [`PekoCodegenContext`].
///
/// Every AST variant in [`PekoAST`] implements this trait via the
/// per-variant `build_value` methods in [`expression_gen`],
/// [`statement_gen`], [`declaration_gen`], and [`value_gen`]. The impl on
/// [`PekoAST`] itself just dispatches to the appropriate variant.
pub trait PekoValueBuilder {
    /// Emit IR for `self` and return the resulting value.
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue;
}

/// Dispatch a `PekoAST` to the appropriate `PekoValueBuilder` impl.
///
/// Mirrors what `peko_core::simulator::mod` does for the simulator side.
/// Each non-placeholder AST variant simply delegates to its inner node's
/// `build_value`; only the `Placeholder` variant has dedicated behavior
/// (it emits an error sentinel, since a placeholder reaching codegen
/// means typechecking already failed).
macro_rules! dispatch_build_value {
    ($self:ident, $ctx:ident, [ $($variant:ident),* $(,)? ]) => {
        match $self {
            $(PekoAST::$variant(ast) => ast.build_value($ctx),)*
            PekoAST::Placeholder(_) => $ctx.create_error_value(),
        }
    };
}

impl PekoValueBuilder for PekoAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        dispatch_build_value!(
            self,
            codegen_context,
            [
                Boolean,
                String,
                EncryptedString,
                Array,
                Map,
                Char,
                Number,
                Null,
                NewVariable,
                VariableReassignment,
                VariableReference,
                FunctionDeclaration,
                Closure,
                Return,
                FunctionCall,
                Class,
                Enum,
                ObjectConstruction,
                ObjectAccess,
                ArrayAccess,
                IfStatement,
                Switch,
                WhileLoop,
                ForLoop,
                Break,
                Continue,
                BinaryExpression,
                UnaryExpression,
                ModuleCreation,
                ModuleAccess,
                ImportStatement,
                LinkStatement,
                StyleStatement,
                PlatformStatement,
                Unwrap,
                Cast,
                PekoXTag,
                Range,
            ]
        )
    }
}
