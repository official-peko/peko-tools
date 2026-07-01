//! # Value AST simulators
//!
//! [`PekoValueSimulator`] implementations for the lightweight value
//! ASTs: booleans, numbers, characters, null, strings (literal and
//! interpolated), and encrypted strings.
//!
//! Most variants here are trivial: they return a hard-coded
//! [`SimulatorValue`] of the appropriate type. [`StringAST`] is the one
//! exception (interpolated strings need to type-check the embedded
//! expressions and ensure each interpolation site produces a
//! string-compatible value).

use std::sync::{Arc, RwLock};

use crate::asts::data_structures::VisibilityData;
use crate::asts::values::*;
use crate::diagnostics;
use crate::execution::ExecutionContextAlgorithms;
use crate::types;

use super::PekoValueSimulator;
use super::context::PekoSimulatorContext;
use super::data_structures::Scope;
use super::value::SimulatorValue;

/// Booleans simulate as a fresh `bool`-typed value.
impl PekoValueSimulator for BooleanAST {
    fn simulate(&self, _simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        SimulatorValue::Value(types::PekoType::simple_type("bool"))
    }
}

/// Number literals simulate as the boxed `number` value type. Raw machine
/// integers and floats come from FFI values and `constant<T>(...)`, not bare
/// literals.
impl PekoValueSimulator for NumberAST {
    fn simulate(&self, _simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        SimulatorValue::Value(types::PekoType::simple_type("number"))
    }
}

/// Characters simulate as a fresh `char`-typed value.
impl PekoValueSimulator for CharAST {
    fn simulate(&self, _simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        SimulatorValue::Value(types::PekoType::simple_type("char"))
    }
}

/// `null` literals simulate as the [`SimulatorValue::Null`] sentinel.
impl PekoValueSimulator for NullAST {
    fn simulate(&self, _simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        SimulatorValue::Null
    }
}

/// Strings simulate as a `string`-typed value.
///
/// For *interpolated* strings (`"hello {name}"`), each interpolation
/// chunk's embedded expression list is simulated under its own
/// dedicated scope so that interpolation-local variables don't leak
/// into the surrounding scope's symbol table. The last value of each
/// interpolation must be `string`-compatible (anything else produces a
/// diagnostic).
impl PekoValueSimulator for StringAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Non-interpolated string: trivially typed.
        if !self.interpolated {
            return SimulatorValue::Value(types::PekoType::simple_type("string"));
        }

        // Interpolated string: walk every chunk, simulating embedded
        // expressions and checking the final value of each chunk's
        // expression list for string-compatibility.
        for chunk in &self.chunks {
            // Text chunks are already string-typed by construction.
            if chunk.is_text() {
                continue;
            }

            let interpolated_asts = chunk.get_interpolation();

            // Save the currently-active scope so we can restore it
            // after simulating the chunk under a chunk-local scope.
            let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

            // Build a fresh scope spanning the chunk's source range.
            let scope_reference = Arc::new(RwLock::new(Scope::new(
                false,
                false,
                VisibilityData::open_visibility(),
                chunk.start.clone(),
                chunk.end.clone(),
                String::new(),
            )));
            simulator_context.current_scope = Some(Arc::clone(&scope_reference));

            // Simulate every embedded AST under that scope.
            let mut simulated_values = Vec::new();
            for ast in &interpolated_asts {
                simulated_values.push(ast.simulate(simulator_context));
            }

            // Restore the outer scope, then attach the chunk's scope as
            // a child of it so IDE tooling can still see what was
            // declared inside the interpolation.
            simulator_context.current_scope = previous_scope;
            if let Some(outer) = simulator_context.current_scope.as_mut() {
                outer.write().unwrap().scopes.push(scope_reference);
            }

            // Check the last simulated value (that's what gets
            // converted to string for the interpolation output).
            let current_file = simulator_context.get_current_file();

            if simulated_values.is_empty() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        chunk.start.clone(),
                        chunk.end.clone(),
                        "string interpolation site contains no expression. Expected an expression whose value can be converted to a string (e.g. `\"hello {name}\"`)"
                            .to_string(),
                        diagnostics::DiagnosticType::Error,
                        current_file,
                    ));
            } else {
                // Any object converts through its to_string, so a class-typed
                // value is fine. Only a raw FFI scalar, which has no to_string,
                // cannot be interpolated.
                let last_value_type = simulated_values.last().unwrap().get_type();
                let convertible = simulator_context
                    .types_similar(&last_value_type, &types::PekoType::simple_type("string"))
                    || simulator_context.get_class_by_type(&last_value_type).is_some();
                if !convertible && !last_value_type.is_error_type() {
                    let last_ast = interpolated_asts.last().unwrap();
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            last_ast.get_start().clone(),
                            last_ast.get_end().clone(),
                            format!(
                                "cannot interpolate a value of type `{last_value_type}`; it has no to_string. Convert it to a value or object first",
                            ),
                            diagnostics::DiagnosticType::Error,
                            current_file,
                        ));
                }
            }
        }

        SimulatorValue::Value(types::PekoType::simple_type("string"))
    }
}

/// Encrypted strings simulate as a `string`-typed value (the same as
/// regular strings from the type system's perspective).
impl PekoValueSimulator for EncryptedStringAST {
    fn simulate(&self, _simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        SimulatorValue::Value(types::PekoType::simple_type("string"))
    }
}
