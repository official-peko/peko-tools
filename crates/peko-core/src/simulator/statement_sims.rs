//! # Statement AST simulators
//!
//! [`PekoValueSimulator`] implementations for statement-level AST nodes:
//!
//! * Variable reassignment ([`VariableReassignmentAST`])
//! * Control flow: `if` ([`IfStatementAST`]), `while`
//!   ([`WhileLoopAST`]), `for` ([`ForLoopAST`]), `break`
//!   ([`BreakAST`]), `return` ([`ReturnAST`])
//! * Module composition: `import` ([`ImportStatementAST`]), `link`
//!   ([`LinkStatementAST`]), `style` ([`StyleStatementAST`]), `asset`
//!   ([`AssetStatementAST`]), `platform` ([`PlatformStatementAST`])
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::asts::PekoAST;
use crate::asts::data_structures::{PositionData, UnpackItem, VisibilityData};
use crate::asts::statements::*;
use crate::diagnostics;
use crate::execution::ExecutionContextAlgorithms;
use crate::ffi;
use crate::{lexer, parser, types};

use super::PekoValueSimulator;
use super::context::PekoSimulatorContext;
use super::data_structures::{
    Scope, ScopeModule, ScopeSymbol, ScopeVariable, SimulatorModule, SimulatorVariable,
};
use super::value::SimulatorValue;

/// Simulates variable reassignment: type-checks the new value against
/// the variable's declared type (direct assignment) or against the
/// declared operator's overload (compound assignment).
impl PekoValueSimulator for VariableReassignmentAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Simulate the LHS variable reference.
        let variable_reference = self.variable_reference.as_ref().simulate(simulator_context);

        // If this is an attribute set, remove it from the
        // pending-attribute-init list so the constructor knows it's
        // taken care of.
        let variable_name = match self.variable_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                variable_reference.variable_name.value.clone()
            }
            _ => String::new(),
        };

        if simulator_context.previous_was_this
            && simulator_context.attributes_to_set.contains(&variable_name)
        {
            simulator_context.attributes_to_set.remove(
                simulator_context
                    .attributes_to_set
                    .iter()
                    .position(|key| key.as_str() == variable_name)
                    .unwrap(),
            );
        }
        simulator_context.previous_was_this = false;

        // Set the inference type so the RHS knows what it's expected to
        // produce.
        let previous_expected_type = simulator_context.current_expected_type_options.clone();
        simulator_context.current_expected_type_options = Some(vec![variable_reference.get_type()]);

        // Simulate the RHS value and type-check it.
        let variable_value = self.variable_value.as_ref().simulate(simulator_context);
        let variable_type = variable_reference.get_type();

        let current_file = simulator_context.get_current_file();

        // Direct assignment: types must be similar.
        if !simulator_context.types_similar(&variable_value.get_type(), &variable_type)
            && self.assignment_operator.is_none()
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_value.get_start().clone(),
                    self.variable_value.get_end().clone(),
                    format!(
                        "cannot assign value of type `{}` to variable of type `{}`. The right-hand side type is not compatible with the variable's declared type",
                        variable_value.get_type(),
                        variable_type,
                    ),
                    diagnostics::DiagnosticType::Error,
                    current_file.clone(),
                ));
        }

        // Compound assignment (e.g. `+=`): the operator must apply
        // between the variable's current value and the new value.
        if self.assignment_operator.is_some()
            && simulator_context
                .apply_operator(
                    self.assignment_operator.clone().unwrap().as_str(),
                    &variable_reference,
                    &variable_value,
                )
                .is_none()
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_reference.get_start().clone(),
                    self.variable_value.get_end().clone(),
                    format!(
                        "cannot apply operator `{}` between variable of type `{}` and value of type `{}`. There is no operator overload that accepts these two operand types",
                        self.assignment_operator.clone().unwrap(),
                        variable_reference.get_type(),
                        variable_value.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    current_file,
                ));
        }

        simulator_context.current_expected_type_options = previous_expected_type;
        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates an `if` / `elif` / `else` chain.
///
/// Tracks whether every branch returns and whether every branch exits
/// (via `break`) so that the simulator can produce the appropriate
/// control-flow sentinel as the if-statement's result.
impl PekoValueSimulator for IfStatementAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // `if` is a statement, so it must be inside a function body.
        if !simulator_context.local_scope {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "`if` statement cannot appear outside a function body. `if` is a statement, not an expression, so it must be inside a function"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // Whether this `if`'s value is consumed. Captured now because building
        // the condition and branch bodies resets `expecting_value`.
        let is_expression = simulator_context.expecting_value;
        simulator_context.expecting_value = false;

        // At the end of simulation, these hold whether the statement
        // can be guaranteed to return / exit. Both require the else
        // branch to exist (otherwise control may fall through).
        let mut every_block_returns = self.else_body.is_some();
        let mut every_block_exits = self.else_body.is_some();

        // The type of each branch's tail expression, used when `if` is an
        // expression. `None` for a branch that returns or exits (it does not
        // reach the merge), so a mixed branch makes the `if` a statement.
        let mut branch_tails: Vec<Option<types::PekoType>> = Vec::new();

        // Simulate every (condition, body) pair.
        for condition_body in &self.conditional_bodies {
            let condition = condition_body.condition.simulate(simulator_context);

            // Condition must be `bool`-compatible.
            if !simulator_context
                .types_similar(&condition.get_type(), &types::PekoType::simple_type("bool"))
            {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        condition_body.condition.get_start().clone(),
                        condition_body.condition.get_end().clone(),
                        format!(
                            "`if` condition has type `{}` but must have a `bool`-compatible type. The condition expression's type does not match the expected `bool`",
                            condition.get_type(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            // Save scope-stacking state before simulating the branch body.
            simulator_context
                .previous_scoped_variables
                .push(simulator_context.scoped_variables.clone());

            let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

            // Create a fresh scope for this branch.
            let scope_reference = Arc::new(RwLock::new(Scope::new(
                false,
                false,
                VisibilityData::open_visibility(),
                condition_body.body.start.clone(),
                condition_body.body.end.clone(),
                String::new(),
            )));
            simulator_context.current_scope = Some(Arc::clone(&scope_reference));

            let mut branch_exited = false;
            let mut branch_returned = false;
            let mut branch_tail: Option<types::PekoType> = None;

            let body_length = condition_body.body.value.len();
            for (index, peko_ast) in condition_body.body.value.iter().enumerate() {
                // Only the tail statement's value is consumed, and only when
                // the `if` itself is an expression.
                simulator_context.expecting_value = is_expression && index + 1 == body_length;

                let value = peko_ast.simulate(simulator_context);
                let value_type = value.get_type().to_string();
                branch_tail = Some(value.get_type());

                if !branch_exited
                    && !branch_returned
                    && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
                {
                    if value_type == "<<branchexit>>" {
                        branch_exited = true;
                    } else {
                        branch_returned = true;
                    }
                } else if branch_exited || branch_returned {
                    // Any AST after the branch has already exited is
                    // unreachable.
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            peko_ast.get_start().clone(),
                            condition_body.body.value.last().unwrap().get_end().clone(),
                            "unreachable code: this statement (and everything after it) cannot run because the branch has already exited via `break` or `return`"
                                .to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    break;
                }
            }

            // Restore scope and attach the branch scope as a child.
            simulator_context.current_scope = previous_scope;

            if let Some(outer) = simulator_context.current_scope.as_mut() {
                outer.write().unwrap().scopes.push(scope_reference);
            }

            if !branch_returned {
                every_block_returns = false;
            }
            if !branch_exited {
                every_block_exits = false;
            }

            branch_tails.push(if branch_returned || branch_exited {
                None
            } else {
                branch_tail
            });

            simulator_context.scoped_variables.clear();
            simulator_context
                .scoped_variables
                .extend(simulator_context.previous_scoped_variables.pop().unwrap());
        }

        // Simulate the else block (if present) with the same
        // scope-stacking dance.
        if let Some(else_body) = &self.else_body {
            simulator_context
                .previous_scoped_variables
                .push(simulator_context.scoped_variables.clone());

            let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

            let scope_reference = Arc::new(RwLock::new(Scope::new(
                false,
                false,
                VisibilityData::open_visibility(),
                self.else_body.clone().unwrap().start.clone(),
                self.else_body.clone().unwrap().end.clone(),
                String::new(),
            )));
            simulator_context.current_scope = Some(Arc::clone(&scope_reference));

            let mut branch_exited = false;
            let mut branch_returned = false;
            let mut branch_tail: Option<types::PekoType> = None;

            let body_length = else_body.value.len();
            for (index, peko_ast) in else_body.value.iter().enumerate() {
                simulator_context.expecting_value = is_expression && index + 1 == body_length;

                let value = peko_ast.simulate(simulator_context);
                let value_type = value.get_type().to_string();
                branch_tail = Some(value.get_type());

                if !branch_exited
                    && !branch_returned
                    && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
                {
                    if value_type == "<<branchexit>>" {
                        branch_exited = true;
                    } else {
                        branch_returned = true;
                    }
                } else if branch_exited || branch_returned {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            peko_ast.get_start().clone(),
                            else_body
                                .value
                                .last()
                                .unwrap()
                                .get_end()
                                .clone(),
                            "unreachable code: this statement (and everything after it) cannot run because the branch has already exited via `break` or `return`"
                                .to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    break;
                }
            }

            simulator_context.current_scope = previous_scope;

            if let Some(outer) = simulator_context.current_scope.as_mut() {
                outer.write().unwrap().scopes.push(scope_reference);
            }

            if !branch_returned {
                every_block_returns = false;
            }
            if !branch_exited {
                every_block_exits = false;
            }

            branch_tails.push(if branch_returned || branch_exited {
                None
            } else {
                branch_tail
            });

            simulator_context.scoped_variables.clear();
            simulator_context
                .scoped_variables
                .extend(simulator_context.previous_scoped_variables.pop().unwrap());
        }

        // An `if` used as an expression yields a value when there is an else
        // and every branch reaches the merge with a tail value of one common,
        // non-void type. The branch tails feed the codegen PHI.
        let if_value_type = if is_expression {
            simulator_context.if_expression_value_type(self.else_body.is_some(), &branch_tails)
        } else {
            None
        };

        // An `if` whose value is consumed must produce one. Report directly
        // when its branches do not agree on a value.
        if is_expression && if_value_type.is_none() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "this `if` is used as an expression, so it must have an `else` and every branch must end in a value of the same type"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // Surface control-flow guarantees or the expression value as the
        // result type.
        if let Some(value_type) = if_value_type {
            SimulatorValue::Value(value_type)
        } else if every_block_returns {
            SimulatorValue::Return
        } else if every_block_exits {
            SimulatorValue::BranchExit
        } else {
            SimulatorValue::Value(types::PekoType::from_string(
                "default",
                simulator_context.get_current_file(),
            ))
        }
    }
}

/// Extracts the variant name from a switch arm pattern of the shape
/// `Enum::Variant`. Returns `None` for any other pattern shape.
fn switch_arm_variant(pattern: &PekoAST) -> Option<String> {
    if let PekoAST::ModuleAccess(module_access) = pattern
        && let PekoAST::VariableReference(variant_reference) = module_access.accessor.as_ref()
    {
        return Some(variant_reference.variable_name.value.clone());
    }

    None
}

/// Simulates a `switch` over an enum.
///
/// The subject must be an enum value. Each arm matches an `Enum::Variant`
/// pattern, or `_` for the default arm. A switch must cover every variant or
/// include the default arm.
impl PekoValueSimulator for SwitchStatementAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // `switch` is a statement, so it must be inside a function body.
        if !simulator_context.local_scope {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "`switch` statement cannot appear outside a function body. `switch` is a statement, so it must be inside a function"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // The subject is a value position.
        simulator_context.expecting_value = true;
        let subject = self.subject.simulate(simulator_context);
        simulator_context.expecting_value = false;
        let subject_type = subject.get_type();

        // The subject must be an enum value. An error-typed subject already
        // reported a diagnostic, so it suppresses the further error.
        let all_variants = simulator_context.get_enum_variants(&subject_type.type_name);
        if all_variants.is_none() && !subject_type.is_error_type {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.subject.get_start().clone(),
                    self.subject.get_end().clone(),
                    format!(
                        "`switch` subject has type `{}` but must be an enum value. `switch` matches over enum variants",
                        subject_type,
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        let all_variants = all_variants.unwrap_or_default();
        let mut covered: Vec<String> = Vec::new();
        let mut has_default = false;

        for arm in &self.arms {
            match &arm.pattern {
                None => {
                    if has_default {
                        simulator_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                arm.start.clone(),
                                arm.end.clone(),
                                "duplicate `_` arm. A `switch` can have at most one default arm"
                                    .to_string(),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ));
                    }
                    has_default = true;
                }
                Some(pattern) => {
                    // Simulating the pattern validates the `Enum::Variant`
                    // shape and that the variant exists.
                    let pattern_type = pattern.simulate(simulator_context).get_type();

                    if !pattern_type.is_error_type
                        && !subject_type.is_error_type
                        && pattern_type.type_name != subject_type.type_name
                    {
                        simulator_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                pattern.get_start().clone(),
                                pattern.get_end().clone(),
                                format!(
                                    "this arm matches an `{}` variant but the `switch` subject is `{}`. Every arm must match the subject's enum",
                                    pattern_type, subject_type,
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ));
                    }

                    if let Some(variant) = switch_arm_variant(pattern) {
                        if covered.contains(&variant) {
                            simulator_context
                                .diagnostics
                                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                    pattern.get_start().clone(),
                                    pattern.get_end().clone(),
                                    format!(
                                        "duplicate arm for variant `{variant}`. Each variant can be matched by at most one arm",
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    simulator_context.get_current_file(),
                                ));
                        } else {
                            covered.push(variant);
                        }
                    }
                }
            }

            // Simulate the arm body under a fresh scope, mirroring the
            // scope-stacking dance used by `if` branches.
            simulator_context
                .previous_scoped_variables
                .push(simulator_context.scoped_variables.clone());

            let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

            let scope_reference = Arc::new(RwLock::new(Scope::new(
                false,
                false,
                VisibilityData::open_visibility(),
                arm.body.start.clone(),
                arm.body.end.clone(),
                String::new(),
            )));
            simulator_context.current_scope = Some(Arc::clone(&scope_reference));

            for peko_ast in &arm.body.value {
                simulator_context.expecting_value = false;
                peko_ast.simulate(simulator_context);
            }

            simulator_context.current_scope = previous_scope;

            if let Some(outer) = simulator_context.current_scope.as_mut() {
                outer.write().unwrap().scopes.push(scope_reference);
            }

            simulator_context.scoped_variables.clear();
            simulator_context
                .scoped_variables
                .extend(simulator_context.previous_scoped_variables.pop().unwrap());
        }

        // A switch must cover every variant or include the default arm.
        if !has_default && !subject_type.is_error_type {
            let missing: Vec<String> = all_variants
                .iter()
                .filter(|variant| !covered.contains(variant))
                .cloned()
                .collect();

            if !missing.is_empty() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "`switch` over `{}` is not exhaustive. Add arms for the missing variants ({}) or a `_` default arm",
                            subject_type.type_name,
                            missing.join(", "),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates a `while` loop: checks the condition is bool-compatible,
/// simulates the body under a fresh scope, and reports unreachable
/// statements after early branch exits.
impl PekoValueSimulator for WhileLoopAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if !simulator_context.local_scope {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "`while` loop cannot appear outside a function body. `while` is a statement, not an expression, so it must be inside a function"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // Loop context: break statements become valid for the duration
        // of body simulation.
        let previous_loop_finish = simulator_context.current_loop_finish;
        simulator_context.current_loop_finish = true;

        let condition = self.conditional_body.condition.simulate(simulator_context);

        if !simulator_context
            .types_similar(&condition.get_type(), &types::PekoType::simple_type("bool"))
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.conditional_body.condition.get_start().clone(),
                    self.conditional_body.condition.get_end().clone(),
                    format!(
                        "`while` condition has type `{}` but must have a `bool`-compatible type. The condition expression's type does not match the expected `bool`",
                        condition.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        simulator_context
            .previous_scoped_variables
            .push(simulator_context.scoped_variables.clone());

        let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

        let scope_reference = Arc::new(RwLock::new(Scope::new(
            false,
            false,
            VisibilityData::open_visibility(),
            self.conditional_body.body.start.clone(),
            self.conditional_body.body.end.clone(),
            String::new(),
        )));
        simulator_context.current_scope = Some(Arc::clone(&scope_reference));

        let mut branch_exited = false;
        let mut branch_returned = false;

        for peko_ast in &self.conditional_body.body.value {
            let value_type = peko_ast.simulate(simulator_context).get_type().to_string();

            if !branch_exited
                && !branch_returned
                && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
            {
                if value_type == "<<branchexit>>" {
                    branch_exited = true;
                } else {
                    branch_returned = true;
                }
            } else if branch_exited || branch_returned {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        peko_ast.get_start().clone(),
                        self.conditional_body.body.value.last().unwrap().get_end().clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the branch has already exited via `break` or `return`"
                            .to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                break;
            }
        }

        simulator_context.current_scope = previous_scope;

        if let Some(outer) = simulator_context.current_scope.as_mut() {
            outer.write().unwrap().scopes.push(scope_reference);
        }

        simulator_context.scoped_variables.clear();
        simulator_context
            .scoped_variables
            .extend(simulator_context.previous_scoped_variables.pop().unwrap());

        simulator_context.current_loop_finish = previous_loop_finish;

        SimulatorValue::Value(types::PekoType::from_string(
            "default",
            simulator_context.get_current_file(),
        ))
    }
}

/// Simulates a `for` loop over an iterable.
///
/// Resolves the iterable's `[operator iterator]` overload, then the
/// resulting iterator's `inrange` and `next` methods, binding the
/// loop variable to `next`'s return type before simulating the body.
impl PekoValueSimulator for ForLoopAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if !simulator_context.local_scope {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "`for` loop cannot appear outside a function body. `for` is a statement, not an expression, so it must be inside a function"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // Get the iterator via the `[operator iterator]` overload.
        let iterable = self.iterator.simulate(simulator_context);
        let get_iterator = simulator_context.call_object_method(
            &iterable,
            String::from("[operator iterator]"),
            Vec::new(),
            None,
        );

        let iterator = match get_iterator {
            Ok(iter) => iter,
            Err(_) => {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.iterator.get_start().clone(),
                        self.iterator.get_end().clone(),
                        format!(
                            "value of type `{}` is not iterable. The type does not implement the `[operator iterator]` overload, which is required for `for` loops",
                            iterable.get_type(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                simulator_context.create_error_value()
            }
        };

        let previous_loop_finish = simulator_context.current_loop_finish;
        simulator_context.current_loop_finish = true;

        // Verify the iterator has an `inrange` method (called every
        // iteration to test loop termination).
        let inrange_call = simulator_context.call_object_method(
            &iterator,
            String::from("inrange"),
            Vec::new(),
            None,
        );

        if inrange_call.is_err() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.iterator.get_start().clone(),
                    self.iterator.get_end().clone(),
                    format!(
                        "iterator of type `{}` does not have a valid `inrange` method. Iterators must declare `inrange(): bool` to be usable in `for` loops",
                        iterable.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        simulator_context
            .previous_scoped_variables
            .push(simulator_context.scoped_variables.clone());

        // Get the iterator item type via the iterator's `next` method.
        let get_next =
            simulator_context.call_object_method(&iterator, String::from("next"), Vec::new(), None);

        let get_next = match get_next {
            Ok(next) => next,
            Err(_) => {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.iterator.get_start().clone(),
                        self.iterator.get_end().clone(),
                        format!(
                            "iterator of type `{}` does not have a valid `next` method. Iterators must declare `next(): T` to be usable in `for` loops",
                            iterable.get_type(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                simulator_context.create_error_value()
            }
        };

        // Create the loop scope, pre-populated with the iterator item
        // binding.
        let mut new_loop_scope = Scope::new(
            false,
            false,
            VisibilityData::open_visibility(),
            self.body.start.clone(),
            self.body.end.clone(),
            String::new(),
        );

        new_loop_scope.symbols.insert(
            self.item_id.value.clone(),
            ScopeSymbol::Variable(
                ScopeVariable::new(
                    None,
                    self.item_id.value.clone(),
                    get_next.get_type(),
                    self.item_id.start.clone(),
                    self.item_id.end.clone(),
                    false,
                ),
                VisibilityData::open_visibility(),
            ),
        );

        simulator_context.scoped_variables.insert(
            self.item_id.value.clone(),
            SimulatorVariable::new(
                self.item_id.start.clone(),
                VisibilityData::open_visibility(),
                get_next.get_type(),
                SimulatorValue::Value(get_next.get_type()),
                simulator_context.module_context.current_module().clone(),
            ),
        );

        let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

        let scope_reference = Arc::new(RwLock::new(new_loop_scope));
        simulator_context.current_scope = Some(Arc::clone(&scope_reference));

        let mut branch_exited = false;
        let mut branch_returned = false;

        for peko_ast in &self.body.value {
            let value_type = peko_ast.simulate(simulator_context).get_type().to_string();

            if !branch_exited
                && !branch_returned
                && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
            {
                if value_type == "<<branchexit>>" {
                    branch_exited = true;
                } else {
                    branch_returned = true;
                }
            } else if branch_exited || branch_returned {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        peko_ast.get_start().clone(),
                        self.body.value.last().unwrap().get_end().clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the branch has already exited via `break` or `return`"
                            .to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                break;
            }
        }

        simulator_context.current_scope = previous_scope;

        if let Some(outer) = simulator_context.current_scope.as_mut() {
            outer.write().unwrap().scopes.push(scope_reference);
        }

        simulator_context.scoped_variables.clear();
        simulator_context
            .scoped_variables
            .extend(simulator_context.previous_scoped_variables.pop().unwrap());

        simulator_context.current_loop_finish = previous_loop_finish;

        SimulatorValue::Value(types::PekoType::from_string(
            "default",
            simulator_context.get_current_file(),
        ))
    }
}

/// Simulates an `import` statement.
///
/// Imports come in three flavors:
///
/// 1. **Local imports**: `import "./relative/path"` resolves to a
///    `.peko` file alongside the current source file.
/// 2. **External imports**: `import some_module` consults the
///    simulator's pre-loaded `external_modules` table (populated from
///    the local and global Peko `Packages/` directories).
/// 3. **Aliased imports**: `import name as alias` binds the module
///    under a different local name.
///
/// The imported module is parsed and simulated under a fresh module
/// context. Any errors from that pass are forwarded onto the main
/// context's diagnostic list (or collapsed to a single "errors in
/// package" diagnostic if `minified_import_errors` is set).
impl PekoValueSimulator for ImportStatementAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let importing_file = simulator_context.get_current_file();

        // Collect the import path segments and the optional version pin.
        let path_ids: Vec<String> = self
            .module_path
            .iter()
            .map(|segment| segment.value.clone())
            .collect();
        let version = self.module_version.as_ref().map(|v| v.value.as_str());

        // The display form of the path for diagnostics.
        let import_display = path_ids.join("::");

        // The bare module name is the last path segment.
        let module_name = path_ids
            .last()
            .cloned()
            .unwrap_or_else(|| String::from("module"));

        // Resolve the import through the shared resolver. Local files take
        // precedence over external packages, and the resolver builds the
        // entry path, the canonical module id, and the root folder to use
        // while the module loads.
        let resolved = match simulator_context.resolve_module(&path_ids, version, &importing_file) {
            Some(resolved) => resolved,
            None => {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "no module named `{import_display}`. Check the module name, and that the module is installed in either the local `Packages/` directory or the global Peko installation",
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }
        };

        let module_entry_file_path = resolved.entry_file.clone();

        // Move the root folder to the resolved module's root for the
        // duration of this import. A registry import points the root at
        // the package directory so its internal `project::` paths and ids
        // stay self-consistent. The previous root is restored once the
        // module finishes loading.
        let previous_root_folder = simulator_context.root_folder.clone();
        simulator_context.root_folder = resolved.new_root_folder.clone();

        // Whether this import has an unpack list (`import { ... } from x`).
        let has_unpack_list = !self.symbols_to_unpack.is_empty();

        // The name the module takes locally. A plain import uses the user
        // alias or the bare module name. An unpack import uses the
        // canonical module id so two unpacks of different files never
        // share an identity.
        let import_as_module_name = if has_unpack_list {
            resolved.module_id.clone()
        } else if self.import_as.is_some() {
            self.import_as.clone().unwrap().value.clone()
        } else {
            module_name.clone()
        };

        // A plain import conflicts only when the current module has already
        // bound a module under this name in this analysis. The current module
        // and its scope are rebuilt for each analysis, so this check carries
        // no state between analyses. A name that exists only in the global
        // registry because another module imported it, for example `json`
        // pulled in by `ui`, is a global module this module has not imported,
        // so it is not a conflict here. Unpack imports cannot conflict because
        // their identity is the unique module id.
        let conflicting_import = if has_unpack_list {
            false
        } else {
            let current_module_arc = simulator_context.module_context.current_module();
            let current_module = current_module_arc.read().unwrap();
            let current_scope = current_module.scope.read().unwrap();
            matches!(
                current_scope.symbols.get(&import_as_module_name),
                Some(ScopeSymbol::Module(..))
            )
        };

        if conflicting_import {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "module name `{import_as_module_name}` is already imported from a different module. Use `as <alias>` to bind one of them under a different name",
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            simulator_context.root_folder = previous_root_folder;
            return simulator_context.create_error_value();
        }

        let module_to_import = if simulator_context
            .module_context
            .top_level_modules
            .contains_key(&import_as_module_name)
        {
            simulator_context.module_context.top_level_modules[&import_as_module_name].clone()
        } else {
            // Read the source file. If it can't be read (e.g. the file
            // disappeared between the .exists() check and now), report
            // a diagnostic and bail out rather than panicking.
            let raw_source = match std::fs::read_to_string(&module_entry_file_path) {
                Ok(source) => source,
                Err(err) => {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "cannot read module entry file `{}` for import `{}`. {err} (check that the file exists and is readable)",
                                module_entry_file_path.display(),
                                import_display,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    simulator_context.root_folder = previous_root_folder;
                    return simulator_context.create_error_value();
                }
            };

            // An FFI header is parsed as a C interop surface and lowered to
            // equivalent external Peko declarations. Parse errors and any
            // unsupported declarations are reported at the import site, then
            // the lowered source flows through the ordinary module path.
            let module_source = if ffi::is_ffi_header(&module_entry_file_path) {
                let parsed = ffi::parse_header(&raw_source);
                for error in &parsed.errors {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "FFI header `{}`: {error}",
                                module_entry_file_path.display(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                }
                ffi::header_to_peko_source(&parsed)
            } else {
                raw_source
            };

            // Parse the module into ASTs.
            let mut parser = parser::PekoParser::new(
                lexer::TokenList::from_source(&module_source, &module_entry_file_path),
                &module_entry_file_path,
            );

            let mut asts = Vec::new();

            // Pull the module's docinfo first if present. An empty module has
            // no tokens, so the docinfo peek is guarded against an empty list.
            let module_docinfo = if parser.tokens.length() != 0
                && parser.tokens.current_token().equals("//!")
            {
                Some(parser.parse_module_doc_info())
            } else {
                None
            };

            while parser.tokens.length() != 0
                && parser.tokens.get_index() != parser.tokens.length() - 1
            {
                loop {
                    if parser.tokens.finished() {
                        break;
                    }

                    match parser.tokens.current_token().get_type() {
                        lexer::TokenType::Comment => {
                            parser.skip_comment();
                        }
                        _ => {
                            if parser.tokens.get_index() < parser.tokens.length()
                                && parser.tokens.current_token().equals(";")
                            {
                                parser.tokens.increase_index();
                            } else {
                                break;
                            }
                        }
                    }
                }

                if parser.tokens.finished() {
                    break;
                }

                asts.push(parser.parse());
            }

            // Forward parser diagnostics into the main context unless
            // we're collapsing them.
            if !simulator_context.minified_import_errors {
                for error in parser.diagnostics.get_diagnostics() {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(error.clone());
                }
            }

            let previous_errors = simulator_context.diagnostics.clone();
            let parser_errored = parser.diagnostics.get_error_count() > 0;

            // Create a new scope for the imported module's top-level
            // declarations.
            let scope_reference = Arc::new(RwLock::new(Scope::new(
                true,
                false,
                VisibilityData::open_visibility(),
                PositionData {
                    file: simulator_context.get_current_file(),
                    ..PositionData::default()
                },
                parser.get_current_position(),
                import_as_module_name.clone(),
            )));

            // Bind the module name into the current scope unless the
            // import was a wildcard unpack (`import x.{All}`).
            if !(self.symbols_to_unpack.len() == 1
                && matches!(self.symbols_to_unpack[0], UnpackItem::All))
            {
                simulator_context
                    .module_context
                    .current_module()
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(
                        import_as_module_name.clone(),
                        ScopeSymbol::Module(
                            ScopeModule::new(
                                module_docinfo.clone(),
                                import_as_module_name.clone(),
                                PositionData {
                                    file: module_entry_file_path.clone(),
                                    ..PositionData::default()
                                },
                                parser.get_current_position(),
                            ),
                            VisibilityData::open_visibility(),
                        ),
                    );
            }

            // Build the SimulatorModule for the import.
            let new_module = Arc::new(RwLock::new(SimulatorModule::new(
                self.start.clone(),
                VisibilityData::open_visibility(),
                module_entry_file_path.clone(),
                module_docinfo.clone(),
                None,
                import_as_module_name.clone(),
                IndexMap::new(),
                IndexMap::new(),
                IndexMap::new(),
                IndexMap::new(),
                IndexMap::new(),
                IndexMap::new(),
                Arc::clone(&scope_reference),
                Vec::new(),
                IndexMap::new(),
            )));

            let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

            simulator_context.current_scope = Some(scope_reference);
            simulator_context
                .module_context
                .move_to_module(Arc::clone(&new_module), false, false);
            simulator_context
                .module_context
                .top_level_modules
                .insert_before(1, import_as_module_name.clone(), Arc::clone(&new_module));

            // Every imported module gets the standard library imports
            // baked in (unless it *is* one of those libraries).
            let default_imports = ["Runtime", "standard", "console", "ui"];
            for import in default_imports {
                if import_as_module_name == "Runtime" {
                    break;
                }

                if import_as_module_name == import
                    || (import_as_module_name == "standard" && import == "console")
                    || !simulator_context
                        .module_context
                        .top_level_modules
                        .contains_key(import)
                {
                    continue;
                }

                let module =
                    Arc::clone(&simulator_context.module_context.top_level_modules[import]);
                simulator_context.import_module(
                    module,
                    if import == "standard" {
                        vec![UnpackItem::All]
                    } else {
                        Vec::new()
                    },
                );
            }

            // Declarations lowered from an FFI header stay external for their
            // raw name and gc-leaf marking, but are scoped to this module so
            // they resolve through it rather than the global extern module.
            if ffi::is_ffi_header(&module_entry_file_path) {
                for ast in asts.iter_mut() {
                    match ast {
                        PekoAST::FunctionDeclaration(declaration) => {
                            declaration.visibility.scoped = true;
                        }
                        PekoAST::NewVariable(declaration) => {
                            declaration.visibility.scoped = true;
                        }
                        _ => {}
                    }
                }
            }

            for ast in &asts {
                ast.simulate(simulator_context);
            }

            simulator_context.module_context.move_out_of_module();
            simulator_context.current_scope = previous_scope;

            // Optionally collapse all per-statement errors from the
            // imported module into a single error at the import site.
            if simulator_context.minified_import_errors {
                simulator_context.diagnostics = previous_errors.clone();

                if simulator_context.diagnostics.get_error_count()
                    != previous_errors.get_error_count()
                    || parser_errored
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "the imported module `{}` contains errors. Disable `minified_import_errors` to see them individually",
                                import_display,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                }
            }

            new_module
        };

        // Restore the root folder now that the imported module is loaded.
        simulator_context.root_folder = previous_root_folder;

        simulator_context.import_module(module_to_import, self.symbols_to_unpack.clone());

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates a `link` statement: ensures the referenced object/lib/
/// archive file actually exists on disk.
impl PekoValueSimulator for LinkStatementAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Resolve the linker file's full path with the appropriate
        // extension.
        let extension = match self.link_as.value.as_str() {
            "object" => ".o",
            "lib" => ".lib",
            "archive" => ".a",
            _ => {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "`{}` is not a valid linker file type. Valid types are `object`, `lib`, and `archive`",
                            self.link_as.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }
        };

        let file_path = simulator_context
            .get_current_file()
            .parent()
            .unwrap()
            .join([self.object.value.as_str(), extension].concat());

        if !file_path.exists() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "cannot link `{}`. File not found. Ensure the linker file exists at this path and that you've spelled the object name correctly",
                        file_path.display(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            return simulator_context.create_error_value();
        }

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates a `style` statement: ensures the referenced `.scss`
/// stylesheet exists and binds it as a `string` global.
///
/// When the stylesheet is missing but a `.css` or `.sass` variant
/// exists at the same path, surfaces a warning suggesting to rename
/// the extension to `.scss`.
impl PekoValueSimulator for StyleStatementAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Strip any path components since the stylesheet variable
        // binds under just the base name.
        let stylesheet_name = if self.stylesheet.value.contains('/') {
            self.stylesheet
                .value
                .split('/')
                .collect::<Vec<&str>>()
                .pop()
                .unwrap()
        } else {
            self.stylesheet.value.as_str()
        };

        let parent_dir = simulator_context
            .get_current_file()
            .parent()
            .unwrap()
            .to_path_buf();

        let file_path = parent_dir.join([self.stylesheet.value.as_str(), ".scss"].concat());

        let alternate_css_file_path =
            parent_dir.join([self.stylesheet.value.as_str(), ".css"].concat());

        let alternate_sass_file_path =
            parent_dir.join([self.stylesheet.value.as_str(), ".sass"].concat());

        if !file_path.exists() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "cannot find stylesheet `{}`. File not found. Check the stylesheet name and path",
                        file_path.display(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            // If the user has a .css or .sass file at the same path,
            // surface a warning explaining that Peko only supports
            // .scss and how to fix it.
            if alternate_css_file_path.exists() || alternate_sass_file_path.exists() {
                let path = if alternate_css_file_path.exists() {
                    alternate_css_file_path
                } else {
                    alternate_sass_file_path
                };

                simulator_context.diagnostics.report_diagnostic(
                    diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "found stylesheet at `{}`. Peko only uses scss stylesheets. To use this stylesheet, change its extension to `.scss`",
                            path.display(),
                        ),
                        diagnostics::DiagnosticType::Warning,
                        simulator_context.get_current_file(),
                    ),
                );
            }

            return simulator_context.create_error_value();
        }

        // Bind a stylesheet-named string global into the current module.
        let variable = SimulatorVariable::new(
            self.start.clone(),
            VisibilityData::open_visibility(),
            types::PekoType::simple_type("string"),
            SimulatorValue::Value(types::PekoType::simple_type("string")),
            simulator_context.module_context.current_module().clone(),
        );
        simulator_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .variables
            .insert(
                String::from(stylesheet_name),
                Arc::new(RwLock::new(variable)),
            );

        simulator_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .scope
            .write()
            .unwrap()
            .symbols
            .insert(
                String::from(stylesheet_name),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        None,
                        stylesheet_name.to_string(),
                        types::PekoType::simple_type("string"),
                        self.start.clone(),
                        self.end.clone(),
                        false,
                    ),
                    VisibilityData::open_visibility(),
                ),
            );

        SimulatorValue::Value(types::PekoType::simple_type("string"))
    }
}

/// Simulates a `return` statement: type-checks the return value (if
/// any) against the enclosing function's declared return type.
impl PekoValueSimulator for ReturnAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if !simulator_context.local_scope {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_value.clone().unwrap().get_start().clone(),
                    self.return_value.clone().unwrap().get_end().clone(),
                    "`return` cannot appear outside a function body. `return` can only be used to return from a function"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return SimulatorValue::Return;
        }

        // Bare `return`, no type check needed.
        if self.return_value.is_none() {
            return SimulatorValue::Return;
        }

        let previous_expected_type = simulator_context.current_expected_type_options.clone();

        // Set the expected type so the return expression simulates with
        // the right inference context.
        if simulator_context.current_return_type.is_some()
            && simulator_context
                .current_return_type
                .as_ref()
                .unwrap()
                .to_string()
                != "void"
        {
            simulator_context.current_expected_type_options =
                Some(vec![simulator_context.current_return_type.clone().unwrap()]);
        } else if simulator_context.current_return_type.is_none()
            || simulator_context
                .current_return_type
                .as_ref()
                .unwrap()
                .to_string()
                == "void"
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_value.clone().unwrap().get_start().clone(),
                    self.return_value.clone().unwrap().get_end().clone(),
                    "cannot return a value from a `void` function. The enclosing function's return type is `void` (or unset), so no value should be returned"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return SimulatorValue::Return;
        }

        // Simulate and type-check the return value.
        simulator_context.expecting_value = true;
        let return_value = self
            .return_value
            .clone()
            .unwrap()
            .as_ref()
            .simulate(simulator_context);
        simulator_context.expecting_value = false;

        if !simulator_context.types_similar(
            &return_value.get_type(),
            &simulator_context.current_return_type.clone().unwrap(),
        ) {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_value.clone().unwrap().get_start().clone(),
                    self.return_value.clone().unwrap().get_end().clone(),
                    format!(
                        "cannot return value of type `{}`. The enclosing function's declared return type is `{}`, and the returned value's type is not compatible",
                        return_value.get_type(),
                        simulator_context.current_return_type.as_ref().unwrap(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        simulator_context.current_expected_type_options = previous_expected_type;
        SimulatorValue::Return
    }
}

/// Simulates `break`: must be inside a loop body.
impl PekoValueSimulator for BreakAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if !simulator_context.current_loop_finish {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "`break` cannot appear outside a loop body. `break` is only valid inside `for` and `while` loops"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        SimulatorValue::BranchExit
    }
}

/// Simulates `continue`: must be inside a loop body.
impl PekoValueSimulator for ContinueAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if !simulator_context.current_loop_finish {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "`continue` cannot appear outside a loop body. `continue` is only valid inside `for` and `while` loops"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        SimulatorValue::BranchExit
    }
}

/// Simulates a `platform` block: only simulates its body if the current
/// compilation target matches one of the listed platforms (either
/// architecture or operating system depending on the AST's
/// `architecture_test` flag).
impl PekoValueSimulator for PlatformStatementAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let targets = self
            .targets
            .iter()
            .map(|target_value| target_value.value.clone())
            .collect::<Vec<String>>();

        let matches_architecture = self.architecture_test
            && targets.contains(&simulator_context.target.architecture.to_string());

        let matches_os = !self.architecture_test
            && targets.contains(&simulator_context.target.operating_system.to_string());

        // Special case for windowsgui, only matches when on Windows
        // with the windowsgui flag set.
        let matches_windowsgui = !self.architecture_test
            && targets.contains(&"windowsgui".to_owned())
            && simulator_context.target.operating_system.to_string() == "windows"
            && simulator_context.windowsgui;

        if matches_architecture || matches_os || matches_windowsgui {
            for ast in &self.body.value {
                ast.simulate(simulator_context);
            }
        }

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}
