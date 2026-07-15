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

    /// Header pass: create a declaration's LLVM type shell (a named struct for
    /// a class, created once) and register it so a class built earlier can
    /// resolve a class declared later. Only declarations override this; the
    /// rest are no-ops. `build_value` reuses the shell's types rather than
    /// recreating them.
    fn declare(&self, _codegen_context: &mut PekoCodegenContext) {}

    /// Signature pass, run after every shell is declared and before any body:
    /// lay out a class's fields, declare its method LLVM functions, and emit its
    /// static data, without emitting method bodies. This makes a class
    /// dispatchable from another class's body regardless of source order. Only
    /// `ClassAST` overrides this; every other declaration is a no-op here and is
    /// built fully in the body pass.
    fn declare_signatures(&self, _codegen_context: &mut PekoCodegenContext) {}
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
            // Comments are captured only for formatting; ordinary compilation
            // never routes one here.
            PekoAST::Comment(_) => $ctx.create_error_value(),
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
                Destructure,
                VariableReassignment,
                VariableReference,
                FunctionDeclaration,
                Closure,
                Return,
                FunctionCall,
                Class,
                Trait,
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
                DemoStatement,
                Unwrap,
                Cast,
                PekoXTag,
                Range,
            ]
        )
    }

    /// Routes the header pass to the contained AST. Class declarations create a
    /// type shell. Trait declarations register their slot layout here, before
    /// any class signature pass, so a class can emit a witness table and itable
    /// for the traits it implements (the itable drives erased-generic dispatch).
    fn declare(&self, codegen_context: &mut PekoCodegenContext) {
        match self {
            PekoAST::Class(ast) => ast.declare(codegen_context),
            PekoAST::Trait(ast) => {
                ast.build_value(codegen_context);
            }
            PekoAST::Enum(ast) => {
                ast.build_value(codegen_context);
            }
            _ => {}
        }
    }

    /// Routes the signature pass to the contained AST. Class declarations lay
    /// out fields and declare method signatures. Import statements run here too,
    /// before any class signature, so a class can name an imported type in its
    /// fields or method signatures. An import is idempotent across passes, so
    /// the body pass running it again reuses the already-loaded module.
    fn declare_signatures(&self, codegen_context: &mut PekoCodegenContext) {
        match self {
            PekoAST::Class(ast) => ast.declare_signatures(codegen_context),
            PekoAST::FunctionDeclaration(ast) => ast.declare_signatures(codegen_context),
            PekoAST::ImportStatement(ast) => {
                ast.build_value(codegen_context);
            }
            // Register enums in the signature pass as well (registration is
            // idempotent), so a class can name an enum in its attribute and
            // method-parameter types regardless of pass ordering.
            PekoAST::Enum(ast) => {
                ast.build_value(codegen_context);
            }
            _ => {}
        }
    }
}
