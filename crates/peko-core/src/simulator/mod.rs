//! # Peko Core Simulator
//!
//! Walks Pekoscript ASTs as a *type-tracking* evaluator: instead of
//! computing runtime values, it simulates the type-flow through every
//! expression and statement, collecting diagnostics whenever a step is
//! ill-typed.
//!
//! The simulator is the part of `peko_core` that catches errors before
//! the LLVM backend gets involved. It's also what powers IDE-facing
//! features (hover info, completion, signature help) by recording
//! function-call traces and defined-object spans alongside its
//! type-checking pass.
//!
//! # Architecture
//!
//! * [`PekoValueSimulator`] is the AST-level trait every kind of AST
//!   node implements.
//! * [`context::PekoSimulatorContext`] holds the mutable state threaded
//!   through every `simulate` call.
//! * [`data_structures`] provides the simulator's own `SimulatorModule`,
//!   `SimulatorClass`, etc. The concrete types plugged into the
//!   backend-agnostic [`crate::execution`] machinery.
//! * [`value::SimulatorValue`] is what every `simulate` call returns
//!   (either a typed value or a control-flow sentinel).
//! * The four `*_sims` modules ([`declaration_sims`], [`expression_sims`],
//!   [`statement_sims`], [`value_sims`]) supply the per-AST-variant
//!   implementations.

use crate::asts::PekoAST;
use crate::types;

pub mod context;
pub mod data_structures;
pub mod value;

// AST simulator implementations, grouped by AST category.
pub mod declaration_sims;
pub mod expression_sims;
pub mod statement_sims;
pub mod value_sims;

/// Trait implemented by every AST node to drive type-tracking
/// simulation.
///
/// `simulate` returns a [`value::SimulatorValue`] capturing the
/// expression's tracked type (or a control-flow sentinel for statements
/// that don't produce a value). Errors encountered along the way are
/// pushed onto `simulator_context.diagnostics` and recovered from by
/// returning an error-typed value via
/// [`context::PekoSimulatorContext::create_error_value`].
pub trait PekoValueSimulator {
    /// Simulates this AST node, returning its tracked type and updating
    /// the simulation context with any diagnostics or scope changes
    /// produced along the way.
    fn simulate(
        &self,
        simulator_context: &mut context::PekoSimulatorContext,
    ) -> value::SimulatorValue;

    /// Header pass: register this declaration's name and signature so a later
    /// body pass can resolve forward references regardless of declaration
    /// order. Only type- and function-introducing declarations override this;
    /// everything else is a no-op. A `declare` must not emit diagnostics --
    /// `simulate` is the authoritative pass and overwrites the shell.
    fn declare(&self, _simulator_context: &mut context::PekoSimulatorContext) {}
}

/// Top-level dispatcher: routes a [`PekoAST`] to the appropriate
/// variant's [`PekoValueSimulator`] impl.
///
/// The 34 non-placeholder variants all delegate identically to their
/// inner AST, a one-line macro expansion. Only [`PekoAST::Placeholder`]
/// needs special handling: empty placeholders simulate as `Null`, and
/// named placeholders simulate as a synthetic typed value (the
/// placeholder's name parsed as a type), used by IDE tooling to
/// represent "the user is typing here, treat this as something of type
/// X for now."
impl PekoValueSimulator for PekoAST {
    fn simulate(
        &self,
        simulator_context: &mut context::PekoSimulatorContext,
    ) -> value::SimulatorValue {
        // Compact dispatch for every variant whose simulation is a
        // straightforward delegation to the contained AST node.
        macro_rules! dispatch_simulate {
            ($($variant:ident),+ $(,)?) => {
                match self {
                    $(PekoAST::$variant(ast) => ast.simulate(simulator_context),)+

                    // Placeholders are the only AST kind without a
                    // separate impl (they're handled inline).
                    PekoAST::Placeholder(ast) => {
                        if ast.value.is_empty() {
                            value::SimulatorValue::Null
                        } else {
                            value::SimulatorValue::Value(types::PekoType::from_string(
                                ast.value.as_str(),
                                "",
                            ))
                        }
                    }

                    // Comments are captured only for formatting and carry no
                    // runtime meaning. Ordinary compilation never reaches here.
                    PekoAST::Comment(_) => value::SimulatorValue::Null,
                }
            };
        }

        dispatch_simulate!(
            // Values
            Boolean,
            String,
            EncryptedString,
            Char,
            Number,
            Null,
            // Expressions
            Array,
            Map,
            VariableReference,
            FunctionCall,
            ObjectConstruction,
            ObjectAccess,
            ArrayAccess,
            BinaryExpression,
            UnaryExpression,
            ModuleAccess,
            Unwrap,
            Cast,
            PekoXTag,
            Range,
            // Statements
            VariableReassignment,
            Return,
            IfStatement,
            Switch,
            WhileLoop,
            ForLoop,
            Break,
            Continue,
            ImportStatement,
            LinkStatement,
            StyleStatement,
            PlatformStatement,
            DemoStatement,
            // Declarations
            NewVariable,
            Destructure,
            FunctionDeclaration,
            Closure,
            Class,
            Trait,
            Enum,
            ModuleCreation,
        )
    }

    /// Routes the header pass to the contained AST. Only declarations that
    /// introduce a name override `declare`; the rest keep the trait default.
    fn declare(&self, simulator_context: &mut context::PekoSimulatorContext) {
        match self {
            PekoAST::Class(ast) => ast.declare(simulator_context),
            PekoAST::Trait(ast) => ast.declare(simulator_context),
            PekoAST::Enum(ast) => ast.declare(simulator_context),
            PekoAST::FunctionDeclaration(ast) => ast.declare(simulator_context),
            _ => {}
        }
    }
}
