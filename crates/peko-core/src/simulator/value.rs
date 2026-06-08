//! # Simulator value
//!
//! Output of simulating an AST.
//!
//! Unlike a real evaluator, the simulator doesn't compute actual values.
//! It only tracks **type information** along the execution paths the user
//! wrote. [`SimulatorValue`] is the variant the simulator pushes onto its
//! virtual operand stack for every value-producing expression: a typed
//! value, a function, a class, or one of three control-flow sentinels
//! (`Return`, `BranchExit`, `Null`).

use crate::asts::data_structures::PositionData;
use crate::execution::data_structures::ExecutionValue;
use crate::types;

use super::data_structures::{SimulatorClass, SimulatorFunction};

/// Output produced by simulating a Pekoscript expression or statement.
///
/// The simulator is a *type-tracking* evaluator: it only carries type
/// information forward, not concrete runtime values. Every variant
/// represents a different shape of result the simulator might encounter
/// at a value-position in source.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum SimulatorValue {
    /// A regular typed value (e.g. the result of an arithmetic expression
    /// or a variable reference).
    Value(types::PekoType),

    /// A function reference, produced by referencing a function name in
    /// value position, or by closure expressions.
    Function(SimulatorFunction),

    /// A class reference, produced by referencing a class name in value
    /// position (e.g. for use in type expressions or static calls).
    Class(SimulatorClass),

    /// Control flow has returned from the enclosing function.
    /// Stops sibling-statement simulation in the containing block.
    Return,

    /// Control flow has exited the enclosing block (e.g. via
    /// `break`).
    BranchExit,

    /// Produced by the `null` literal and by malformed inputs
    /// that simulate as `null` for recovery purposes.
    Null,
}

impl ExecutionValue for SimulatorValue {
    fn get_type(&self) -> types::PekoType {
        // Delegate to the inherent method by fully-qualified path to
        // sidestep any method-resolution ambiguity.
        SimulatorValue::get_type(self)
    }
}

impl SimulatorValue {
    /// Returns `true` if this value is the [`SimulatorValue::BranchExit`]
    /// control-flow sentinel.
    ///
    /// Used by block-level simulators to short-circuit further statement
    /// processing once a branch has exited.
    #[must_use]
    pub fn is_branch_exit(&self) -> bool {
        matches!(self, Self::BranchExit)
    }

    /// Returns the [`PekoType`](types::PekoType) view of this value.
    ///
    /// * `Value(t)` returns `t` directly.
    /// * `Function(f)` reconstructs the equivalent function-type from the
    ///   declared argument and return types.
    /// * `Class(c)` returns the class's own declared type.
    /// * `Null` returns the `opaque` type (Pekoscript's null pointer type).
    /// * `Return` and `BranchExit` return special sentinel-name types
    ///   (`<<returnexit>>` / `<<branchexit>>`) that the simulator
    ///   recognizes as non-value control-flow markers.
    #[must_use]
    pub fn get_type(&self) -> types::PekoType {
        match self {
            SimulatorValue::Value(value) => value.clone(),

            SimulatorValue::Function(function) => {
                let argument_types: Vec<types::PekoType> = function
                    .arguments
                    .iter()
                    .map(|(_, arg)| arg.argument_type.clone())
                    .collect();

                types::PekoType::new(
                    Vec::new(),
                    String::new(),
                    argument_types,
                    0,
                    0,
                    0,
                    Some(function.return_type.clone()),
                    false,
                    PositionData::default(),
                    PositionData::default(),
                )
            }

            SimulatorValue::Class(class) => class.class_type.clone(),

            SimulatorValue::Null => types::PekoType::simple_type("opaque"),

            SimulatorValue::Return => types::PekoType::simple_type("<<returnexit>>"),

            SimulatorValue::BranchExit => types::PekoType::simple_type("<<branchexit>>"),
        }
    }
}
