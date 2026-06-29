//! # Declaration AST simulators
//!
//! [`PekoValueSimulator`] implementations for declaration-level AST
//! nodes:
//!
//! * Variable declarations ([`NewVariableAST`])
//! * Function declarations ([`FunctionDeclarationAST`])
//! * Closures ([`ClosureAST`])
//! * Module declarations ([`ModuleCreationAST`])
//! * Class declarations ([`ClassAST`])
//!
//! Each impl walks its AST while threading mutable state through the
//! [`PekoSimulatorContext`]: type-checking declared types, registering
//! declared symbols in the active module / scope, and (for nodes with
//! bodies) simulating the body under a fresh local scope.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::asts::data_structures::{ClassMethod, DocInfo, PositionData, VisibilityData};
use crate::asts::declarations::*;
use crate::diagnostics;
use crate::execution::ExecutionContextAlgorithms;
use crate::execution::data_structures::{TraitDefinition, TraitMethodSlot};
use crate::simulator::data_structures::{
    ScopeClass, SimulatorClass, SimulatorClassAttribute, SimulatorClassGeneric,
    SimulatorClassVirtualTable,
};
use crate::types::{self, PekoType};

use super::PekoValueSimulator;
use super::context::PekoSimulatorContext;
use super::data_structures::{
    DefinedObject, Scope, ScopeFunction, ScopeModule, ScopeSymbol, ScopeVariable, SimulatorArg,
    SimulatorFunction, SimulatorFunctionGeneric, SimulatorModule, SimulatorVariable,
};
use super::value::SimulatorValue;

/// Simulates a variable declaration.
///
/// Two paths depending on whether we're inside a function body:
///
/// * **Local scope**: the value is simulated, the declared type (if
///   any) is type-checked or used as a cast target, and the variable
///   is added to the scoped-variable map.
/// * **Global scope**: the declared type is required (no inference
///   for globals). External variables go on the `extern` module's
///   variable list; everything else lives on the current module.
impl NewVariableAST {
    /// Registers a local binding into the current scope's symbol map (for
    /// tooling) and the scoped-variable map (for resolution).
    fn register_local(
        &self,
        simulator_context: &mut PekoSimulatorContext,
        variable_type: types::PekoType,
    ) {
        if let Some(scope) = simulator_context.current_scope.as_mut() {
            scope.write().unwrap().symbols.insert(
                self.name.value.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        self.docinfo.clone(),
                        self.name.value.clone(),
                        variable_type.clone(),
                        self.name.start.clone(),
                        self.name.end.clone(),
                        false,
                    ),
                    self.visibility.clone(),
                ),
            );
        }

        simulator_context.scoped_variables.insert(
            self.name.value.clone(),
            SimulatorVariable::new(
                self.start.clone(),
                self.visibility.clone(),
                variable_type.clone(),
                SimulatorValue::Value(variable_type),
                simulator_context.module_context.current_module().clone(),
            ),
        );
    }
}

impl PekoValueSimulator for NewVariableAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Set the expected type so the initializer's simulation knows
        // what type to infer toward.
        let previous_expected_type = simulator_context.current_expected_type_options.clone();
        if let Some(variable_type) = &self.variable_type
            && simulator_context.type_exists(self.variable_type.as_ref().unwrap())
        {
            simulator_context.current_expected_type_options =
                Some(vec![simulator_context.expand_type(variable_type).unwrap()]);
        }

        // Local-scope declarations: simulate value, type-check, register.
        if simulator_context.local_scope {
            // A typed declaration may omit its initializer (`let x: T`). The
            // binding is then uninitialized and must be definitely assigned
            // before it is read.
            let Some(initializer) = &self.variable_value else {
                simulator_context.current_expected_type_options = previous_expected_type;

                let Some(variable_type) = &self.variable_type else {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "a declaration without an initializer must have an explicit type, as in `let x: int`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    return SimulatorValue::Value(types::PekoType::simple_type("default"));
                };

                let variable_type_exists = simulator_context.type_exists(variable_type);
                if !variable_type_exists {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            variable_type.start_position.clone(),
                            variable_type.end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                variable_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                }

                // A `const` binding cannot be assigned after declaration, so it
                // must be initialized where it is declared.
                if variable_type.is_const() {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "`const` binding `{}` must have an initializer. A const value is immutable, so it cannot be assigned after declaration",
                                self.name.value,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                }

                let registered_type = if variable_type_exists {
                    simulator_context
                        .expand_type(variable_type)
                        .unwrap_or_else(types::PekoType::error_type)
                } else {
                    types::PekoType::error_type()
                };

                self.register_local(simulator_context, registered_type);
                simulator_context
                    .uninitialized_variables
                    .insert(self.name.value.clone());
                return SimulatorValue::Value(types::PekoType::simple_type("default"));
            };

            simulator_context.expecting_value = true;
            let mut variable_value = initializer.simulate(simulator_context);
            simulator_context.expecting_value = false;

            // void is not a real value type.
            if variable_value.get_type().to_string() == "void" {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        initializer.get_start().clone(),
                        initializer.get_end().clone(),
                        "variable initializer cannot have type `void`. `void` is the type of expressions that produce no value, so a variable cannot be initialized to one".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                variable_value = simulator_context.create_error_value();
            }

            // If a type was declared explicitly, ensure the initializer
            // matches (or can be cast).
            if let Some(variable_type) = &self.variable_type {
                if !simulator_context.type_exists(variable_type) {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            variable_type.start_position.clone(),
                            variable_type.end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                variable_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    variable_value = simulator_context.create_error_value();
                } else if simulator_context.types_similar(&variable_value.get_type(), variable_type)
                {
                    if !simulator_context.const_compatible(&variable_value.get_type(), variable_type)
                    {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                initializer.get_start().clone(),
                                initializer.get_end().clone(),
                                format!(
                                    "cannot assign a `const` value of type `{}` to a non-const binding of type `{}`. Casting away const requires an explicit `as`",
                                    variable_value.get_type(),
                                    variable_type,
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        variable_value = simulator_context.create_error_value();
                    } else {
                        variable_value = simulator_context
                            .box_value_to_type(variable_type, &variable_value)
                            .unwrap();
                    }
                } else {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            initializer.get_start().clone(),
                            initializer.get_end().clone(),
                            format!(
                                "cannot assign value of type `{}` to variable of type `{}`. The right-hand side type is not compatible with the variable's declared type",
                                variable_value.get_type(),
                                variable_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    variable_value = simulator_context.create_error_value();
                }
            }

            // An initialized binding is definitely assigned.
            self.register_local(simulator_context, variable_value.get_type());
            simulator_context
                .uninitialized_variables
                .remove(&self.name.value);

            simulator_context.current_expected_type_options = previous_expected_type;
            return SimulatorValue::Value(types::PekoType::simple_type("default"));
        }

        // Global-scope declarations: type is required, no inference.
        let unexpanded_type = if self.variable_type.is_none() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "global variable declarations must include an explicit type. Type inference is only available for local variables".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            types::PekoType::error_type()
        } else {
            self.variable_type.clone().unwrap()
        };
        let variable_peko_type = match simulator_context.expand_type(&unexpanded_type) {
            Some(type_expanded) => type_expanded,
            None => {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_type.clone().unwrap().start_position.clone(),
                        self.variable_type.clone().unwrap().end_position.clone(),
                        format!(
                        "type `{}` is not defined. Check the type name and that the type is in scope",
                        self.variable_type.clone().unwrap(),
                    ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                types::PekoType::error_type()
            }
        };

        // External variables get registered on the extern module
        // (preserving their original names), but a scope symbol is
        // still placed in the current scope for visibility. A scoped foreign
        // variable (a `.peko.h` import) stays in its declaring module so it
        // resolves through that module.
        if self.visibility.external && !self.visibility.scoped {
            simulator_context
                .module_context
                .extern_module
                .write()
                .unwrap()
                .scope
                .write()
                .unwrap()
                .symbols
                .insert(
                    self.name.value.clone(),
                    ScopeSymbol::Variable(
                        ScopeVariable::new(
                            self.docinfo.clone(),
                            self.name.value.clone(),
                            variable_peko_type.clone(),
                            self.name.start.clone(),
                            self.name.end.clone(),
                            false,
                        ),
                        self.visibility.clone(),
                    ),
                );

            if let Some(scope) = simulator_context.current_scope.as_mut() {
                scope.write().unwrap().symbols.insert(
                    self.name.value.clone(),
                    ScopeSymbol::Variable(
                        ScopeVariable::new(
                            self.docinfo.clone(),
                            self.name.value.clone(),
                            variable_peko_type.clone(),
                            self.name.start.clone(),
                            self.name.end.clone(),
                            false,
                        ),
                        self.visibility.clone(),
                    ),
                );
            }

            let variable = SimulatorVariable::new(
                self.start.clone(),
                self.visibility.clone(),
                variable_peko_type.clone(),
                SimulatorValue::Value(variable_peko_type),
                simulator_context.module_context.extern_module.clone(),
            );
            simulator_context
                .module_context
                .extern_module
                .write()
                .unwrap()
                .variables
                .insert(self.name.value.clone(), Arc::new(RwLock::new(variable)));

            simulator_context.current_expected_type_options = previous_expected_type;
            return SimulatorValue::Value(types::PekoType::simple_type("default"));
        }

        // Normal (non-external) global: register in current scope and
        // current module.
        if let Some(scope) = simulator_context.current_scope.as_mut() {
            scope.write().unwrap().symbols.insert(
                self.name.value.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        self.docinfo.clone(),
                        self.name.value.clone(),
                        variable_peko_type.clone(),
                        self.name.start.clone(),
                        self.name.end.clone(),
                        false,
                    ),
                    self.visibility.clone(),
                ),
            );
        }

        let variable = SimulatorVariable::new(
            self.start.clone(),
            self.visibility.clone(),
            variable_peko_type.clone(),
            SimulatorValue::Value(variable_peko_type),
            simulator_context.module_context.current_module().clone(),
        );
        simulator_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .variables
            .insert(self.name.value.clone(), Arc::new(RwLock::new(variable)));

        simulator_context.current_expected_type_options = previous_expected_type;
        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates a function declaration.
///
/// Generic functions are *tracked* (registered as a
/// [`SimulatorFunctionGeneric`]) rather than simulated, since they
/// can't be type-checked without concrete type arguments. Non-generic
/// functions:
///
/// 1. Verify the return type and every argument type exist.
/// 2. Insert (or override) the function in the appropriate module's
///    overload set.
/// 3. If a body is present, simulate it under a fresh function-local
///    scope with the arguments bound, checking that all paths return
///    when a non-void return type was declared.
impl PekoValueSimulator for FunctionDeclarationAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Build the simplified argument map used by scope tracking.
        let mut converted_arguments = IndexMap::new();
        for (argument_name, argument_declaration) in &self.arguments {
            converted_arguments.insert(
                argument_name.value.clone(),
                (
                    argument_declaration.visibility.clone(),
                    argument_declaration.argument_type.clone(),
                ),
            );
        }

        // Generic functions: track but don't simulate.
        if !self.generic_types.is_empty() {
            let mut self_reference = self.clone();
            self_reference.generic_types.clear();

            let current_module = simulator_context.module_context.current_module().clone();

            let function = SimulatorFunctionGeneric::new(
                self.visibility.clone(),
                self.generic_types.clone(),
                self_reference,
                current_module,
            );
            simulator_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .function_generics
                .insert(
                    self.function_name.value.clone(),
                    Arc::new(RwLock::new(function)),
                );

            if let Some(scope) = simulator_context.current_scope.as_mut() {
                scope.write().unwrap().symbols.insert(
                    self.function_name.value.clone(),
                    ScopeSymbol::Function(
                        ScopeFunction::new(
                            self.docinfo.clone(),
                            self.function_name.value.clone(),
                            if self.return_type.is_some() {
                                self.return_type.clone().unwrap()
                            } else {
                                types::PekoType::simple_type("void")
                            },
                            self.start.clone(),
                            self.end.clone(),
                            true,
                            converted_arguments,
                            self.generic_types
                                .iter()
                                .map(|type_string| type_string.value.clone())
                                .collect::<Vec<String>>(),
                        ),
                        self.visibility.clone(),
                    ),
                );
            }

            return SimulatorValue::Value(types::PekoType::from_string(
                "default",
                simulator_context.get_current_file(),
            ));
        }

        // Save context state so we can restore at the end (or on the
        // body-less early return).
        let scoped_variables = simulator_context.scoped_variables.clone();
        simulator_context.scoped_variables.clear();

        let current_this = simulator_context.current_this.clone();
        simulator_context.current_this = None;

        let local_scope = simulator_context.local_scope;
        simulator_context.local_scope = true;

        let current_return_type = simulator_context.current_return_type.clone();
        let function_return_type = if let Some(function_return_type) = &self.return_type {
            let expanded = simulator_context.expand_type(function_return_type);
            expanded.unwrap_or_else(PekoType::error_type)
        } else {
            PekoType::simple_type("void")
        };

        simulator_context.current_return_type = Some(function_return_type.clone());

        // Verify the declared return type exists.
        if self.return_type.is_some()
            && !simulator_context.type_exists(self.return_type.as_ref().unwrap())
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_type.clone().unwrap().start_position.clone(),
                    self.return_type.clone().unwrap().end_position.clone(),
                    format!(
                    "type `{}` is not defined. Check the type name and that the type is in scope",
                    self.return_type.clone().unwrap(),
                ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        // First pass over arguments: collect their types and verify
        // each one exists.
        let mut argument_types = Vec::new();
        let mut argument_keywords = HashMap::new();

        for (argument_name, arg_declaration) in &self.arguments {
            if !simulator_context.type_exists(&arg_declaration.argument_type) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        arg_declaration.argument_type.start_position.clone(),
                        arg_declaration.argument_type.end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            arg_declaration.argument_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            argument_types.push(arg_declaration.argument_type.clone());

            if arg_declaration.default_value.is_some() {
                argument_keywords
                    .insert(argument_name.clone(), arg_declaration.argument_type.clone());
            }
        }

        // Append the variadic-args type (wrapped in standard::Array) if
        // the function declares variadic arguments.
        if let Some(var_args_type) = &self.varargs_type {
            argument_types.push(if !simulator_context.type_exists(var_args_type) {
                simulator_context.diagnostics.report_diagnostic(
                    diagnostics::PekoDiagnostic::new(
                        self.varargs_type.clone().unwrap().start_position.clone(),
                        self.varargs_type.clone().unwrap().end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            var_args_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ),
                );

                types::PekoType::from_string(
                    format!("standard::Array<{}>", types::PekoType::error_type()).as_str(),
                    simulator_context.get_current_file(),
                )
            } else {
                types::PekoType::from_string(
                    format!("standard::Array<{}>", var_args_type).as_str(),
                    simulator_context.get_current_file(),
                )
            });
        }

        // Second pass over arguments: build the IndexMap of expanded
        // SimulatorArg values for storage on the function.
        let mut argument_types: IndexMap<String, SimulatorArg> = IndexMap::new();

        for (argument_name, arg_declaration) in &self.arguments {
            let type_expanded = simulator_context.expand_type(&arg_declaration.argument_type);
            let type_expanded = type_expanded.unwrap_or_else(types::PekoType::error_type);

            argument_types.insert(
                argument_name.value.clone(),
                SimulatorArg::new(
                    arg_declaration.start.clone(),
                    arg_declaration.visibility.clone(),
                    type_expanded,
                    arg_declaration.default_value.is_some(),
                ),
            );
        }

        // Functions named OnStart in main get auto-promoted to extern
        // so the linker can find them as entry points.
        let is_onstart = simulator_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .name
            == "main"
            && self.function_name.value == "OnStart";

        // External functions live in the global extern module, except a
        // scoped foreign symbol (a `.peko.h` import) which stays in its
        // declaring module so it resolves through that module.
        let function_module = if (self.visibility.external && !self.visibility.scoped) || is_onstart
        {
            simulator_context.module_context.extern_module.clone()
        } else {
            simulator_context.module_context.current_module().clone()
        };

        let peko_function = SimulatorFunction::new(
            self.start.clone(),
            self.visibility.clone(),
            self.docinfo.clone(),
            function_return_type.clone(),
            argument_types.clone(),
            self.varargs_type.clone(),
            function_module.clone(),
        );

        let function_exists = function_module
            .as_ref()
            .read()
            .unwrap()
            .functions
            .contains_key(&self.function_name.value);

        if !function_exists {
            function_module
                .write()
                .unwrap()
                .functions
                .insert(self.function_name.value.clone(), Vec::new());
        }

        // Check whether this declaration overrides an existing overload
        // (signature must be exactly equal, not just similar).
        let function_choices = function_module.write().unwrap().functions
            [&self.function_name.value]
            .iter()
            .map(|f| f.read().unwrap().clone())
            .collect();
        let find_function_definition = simulator_context.choose_function_and_index(
            function_choices,
            argument_types
                .iter()
                .map(|(_, argument)| argument.argument_type.clone())
                .collect::<Vec<_>>(),
            None,
            false,
        );

        let find_function_definition: Option<(usize, SimulatorFunction)> =
            if let Some(function_definition) = &find_function_definition {
                let mut all_types_equal = true;
                for ((_, choice_arg), (_, actual_argument)) in function_definition
                    .1
                    .arguments
                    .iter()
                    .zip(argument_types.iter())
                {
                    if !simulator_context
                        .types_equal(&choice_arg.argument_type, &actual_argument.argument_type)
                    {
                        all_types_equal = false;
                        break;
                    }
                }

                if all_types_equal {
                    find_function_definition
                } else {
                    None
                }
            } else {
                None
            };

        // Override the existing overload (or append a new one).
        if let Some(function_definition) = &find_function_definition {
            function_module
                .write()
                .unwrap()
                .functions
                .get_mut(&self.function_name.value)
                .unwrap()[function_definition.0] = Arc::new(RwLock::new(peko_function));
        } else {
            function_module
                .write()
                .unwrap()
                .functions
                .get_mut(&self.function_name.value)
                .unwrap()
                .push(Arc::new(RwLock::new(peko_function)));
        }

        // No body: nothing to simulate. Restore state and exit.
        if self.function_body.is_none() {
            simulator_context.scoped_variables.clear();
            simulator_context.scoped_variables.extend(scoped_variables);
            simulator_context.current_this = current_this;
            simulator_context.local_scope = local_scope;
            simulator_context.current_return_type = current_return_type;

            return SimulatorValue::Value(types::PekoType::simple_type("default"));
        }

        // Build the function's body scope.
        let mut new_function_scope = Scope::new(
            false,
            simulator_context.current_scope.is_some()
                && !simulator_context
                    .current_scope
                    .as_ref()
                    .unwrap()
                    .read()
                    .unwrap()
                    .top_level,
            VisibilityData::open_visibility(),
            self.function_body.clone().unwrap().start.clone(),
            self.function_body.clone().unwrap().end.clone(),
            format!("function-{}", self.function_name.value),
        );

        // Bind each parameter into both the scope's symbol table and
        // the simulator's scoped-variable map.
        for (argument_name, argument_declaration) in &self.arguments {
            new_function_scope.symbols.insert(
                argument_name.value.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        if let Some(docinfo) = &self.docinfo
                            && docinfo.parameter_docs.contains_key(&argument_name.value)
                        {
                            Some(DocInfo::new(
                                docinfo.parameter_docs[&argument_name.value].clone(),
                                HashMap::new(),
                                Vec::new(),
                            ))
                        } else {
                            None
                        },
                        argument_name.value.clone(),
                        argument_declaration.argument_type.clone(),
                        argument_declaration.start.clone(),
                        argument_declaration.end.clone(),
                        false,
                    ),
                    VisibilityData::open_visibility(),
                ),
            );

            simulator_context.scoped_variables.insert(
                argument_name.value.clone(),
                SimulatorVariable::new(
                    argument_declaration.start.clone(),
                    argument_declaration.visibility.clone(),
                    argument_declaration.argument_type.clone(),
                    SimulatorValue::Value(argument_declaration.argument_type.clone()),
                    simulator_context.module_context.current_module().clone(),
                ),
            );
        }

        // Bind the variadic argument under its declared name if present.
        if self.varargs_type.is_some() {
            let argument_type = types::PekoType::from_string(
                format!("standard::Array<{}>", self.varargs_type.clone().unwrap()).as_str(),
                simulator_context.get_current_file(),
            );

            new_function_scope.symbols.insert(
                self.varargs_name.value.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        None,
                        self.varargs_name.value.clone(),
                        argument_type.clone(),
                        self.varargs_name.start.clone(),
                        self.varargs_name.end.clone(),
                        false,
                    ),
                    VisibilityData::open_visibility(),
                ),
            );

            simulator_context.scoped_variables.insert(
                self.varargs_name.value.clone(),
                SimulatorVariable::new(
                    self.varargs_name.start.clone(),
                    VisibilityData::open_visibility(),
                    argument_type.clone(),
                    SimulatorValue::Value(argument_type),
                    simulator_context.module_context.current_module().clone(),
                ),
            );
        }

        let previous_scope = simulator_context.current_scope.clone();

        let scope_reference = Arc::new(RwLock::new(new_function_scope));
        simulator_context.current_scope = Some(Arc::clone(&scope_reference));

        // Simulate the body, tracking branch exits / returns for
        // reachability analysis.
        let mut branch_exits = false;
        let mut branch_returns = false;
        for ast in &self.function_body.as_ref().unwrap().value {
            let ast_value = ast.simulate(simulator_context).get_type();

            if !branch_exits
                && !branch_returns
                && (ast_value.to_string() == "<<branchexit>>"
                    || ast_value.to_string() == "<<returnexit>>")
            {
                branch_exits = ast_value.to_string() == "<<branchexit>>";
                branch_returns = ast_value.to_string() == "<<returnexit>>";
            } else if branch_exits {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        ast.get_start().clone(),
                        self.function_body.as_ref().unwrap().end.clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                break;
            }
        }

        // Check the function returns on all paths if a non-void return
        // type was declared.
        if !branch_returns
            && self.return_type.is_some()
            && self.return_type.as_ref().unwrap().to_string() != "void"
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "function `{}` does not return on all paths. The declared return type is `{}`, but at least one execution path can reach the end of the function without returning",
                        self.function_name.value,
                        self.return_type.clone().unwrap(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        simulator_context.current_scope = previous_scope;

        // Register the function symbol in the appropriate scope.
        if let Some(scope) = simulator_context.current_scope.as_mut() {
            if self.visibility.external && !is_onstart {
                simulator_context
                    .module_context
                    .extern_module
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(
                        self.function_name.value.clone(),
                        ScopeSymbol::Function(
                            ScopeFunction::new(
                                self.docinfo.clone(),
                                self.function_name.value.clone(),
                                function_return_type.clone(),
                                self.start.clone(),
                                self.end.clone(),
                                false,
                                converted_arguments.clone(),
                                Vec::new(),
                            ),
                            self.visibility.clone(),
                        ),
                    );
            } else {
                scope.write().unwrap().symbols.insert(
                    self.function_name.value.clone(),
                    ScopeSymbol::Function(
                        ScopeFunction::new(
                            self.docinfo.clone(),
                            self.function_name.value.clone(),
                            function_return_type,
                            self.start.clone(),
                            self.end.clone(),
                            false,
                            converted_arguments,
                            Vec::new(),
                        ),
                        self.visibility.clone(),
                    ),
                );
            }

            // Attach the function's body scope as a child of the
            // enclosing scope for IDE tooling.
            scope.write().unwrap().scopes.push(scope_reference);
        }

        // Restore context state.
        simulator_context.scoped_variables.clear();
        simulator_context.scoped_variables.extend(scoped_variables);
        simulator_context.current_this = current_this;
        simulator_context.local_scope = local_scope;
        simulator_context.current_return_type = current_return_type;

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates a closure expression.
///
/// Closures capture named local variables explicitly (no implicit
/// capture). The closure body is simulated under a fresh scope that
/// shadows the surrounding one with just the captures and arguments
/// bound.
impl PekoValueSimulator for ClosureAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if !simulator_context.local_scope {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "closures can only be created inside a function body. Move this closure declaration inside a function or method".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        let current_this = simulator_context.current_this.clone();
        simulator_context.current_this = None;

        // Resolve each capture against the surrounding scope.
        let mut captured_variables: IndexMap<String, SimulatorVariable> = IndexMap::new();

        for capture in &self.captures {
            if !simulator_context
                .scoped_variables
                .contains_key(&capture.value)
            {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        capture.start.clone(),
                        capture.end.clone(),
                        format!(
                            "cannot find captured variable `{}` in the current scope. Closures can only capture local variables defined in their enclosing scope",
                            capture.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            } else {
                captured_variables.insert(
                    capture.value.clone(),
                    simulator_context.scoped_variables[&capture.value].clone(),
                );
                simulator_context.scoped_variables.insert(
                    capture.value.clone(),
                    simulator_context.scoped_variables[&capture.value].clone(),
                );
            }
        }

        // Verify the closure's declared return type exists.
        if self.return_type.is_some()
            && !simulator_context.type_exists(self.return_type.as_ref().unwrap())
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_type.clone().unwrap().start_position.clone(),
                    self.return_type.clone().unwrap().end_position.clone(),
                    format!(
                    "type `{}` is not defined. Check the type name and that the type is in scope",
                    self.return_type.clone().unwrap(),
                ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        // Verify each argument type exists.
        for (_, argument_declaration) in &self.arguments {
            if !simulator_context.type_exists(&argument_declaration.argument_type) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        argument_declaration.argument_type.start_position.clone(),
                        argument_declaration.argument_type.end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            argument_declaration.argument_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        let mut new_closure_scope = Scope::new(
            false,
            simulator_context.current_scope.is_some()
                && !simulator_context
                    .current_scope
                    .as_ref()
                    .unwrap()
                    .read()
                    .unwrap()
                    .top_level,
            VisibilityData::open_visibility(),
            self.closure_body.start.clone(),
            self.closure_body.end.clone(),
            String::new(),
        );

        // Save and replace scoped-variable / local-scope state.
        let scoped_variables = simulator_context.scoped_variables.clone();
        simulator_context.scoped_variables.clear();

        let local_scope = simulator_context.local_scope;
        simulator_context.local_scope = true;

        let current_return_type = simulator_context.current_return_type.clone();
        let return_type_expanded = if let Some(return_type_expanded) = &self.return_type {
            simulator_context.expand_type(return_type_expanded)
        } else {
            Some(PekoType::simple_type("void"))
        };

        simulator_context.current_return_type =
            Some(return_type_expanded.unwrap_or_else(PekoType::error_type));

        // Bind every capture into the closure's scope (and detect a
        // captured `this` for method-context-aware code generation).
        for (captured_variable_name, captured_variable) in &captured_variables {
            new_closure_scope.symbols.insert(
                captured_variable_name.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        None,
                        captured_variable_name.clone(),
                        captured_variable.variable_type.clone(),
                        captured_variable.position.clone(),
                        captured_variable.position.clone(),
                        false,
                    ),
                    VisibilityData::open_visibility(),
                ),
            );

            let captured_variable = captured_variable.clone();
            simulator_context
                .scoped_variables
                .insert(captured_variable_name.clone(), captured_variable.clone());

            if captured_variable_name == "this" {
                simulator_context.current_this = Some(captured_variable);
            }
        }

        // Bind each parameter the same way as for function bodies.
        for (argument_name, argument_declaration) in &self.arguments {
            new_closure_scope.symbols.insert(
                argument_name.value.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        None,
                        argument_name.value.clone(),
                        argument_declaration.argument_type.clone(),
                        argument_name.start.clone(),
                        argument_name.end.clone(),
                        false,
                    ),
                    argument_declaration.visibility.clone(),
                ),
            );

            simulator_context.scoped_variables.insert(
                argument_name.value.clone(),
                SimulatorVariable::new(
                    argument_declaration.start.clone(),
                    argument_declaration.visibility.clone(),
                    argument_declaration.argument_type.clone(),
                    SimulatorValue::Value(argument_declaration.argument_type.clone()),
                    simulator_context.module_context.current_module().clone(),
                ),
            );
        }

        let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

        let scope_reference = Arc::new(RwLock::new(new_closure_scope));
        simulator_context.current_scope = Some(Arc::clone(&scope_reference));

        // Simulate the body and track reachability.
        let mut branch_exits = false;
        let mut branch_returns = false;
        for ast in &self.closure_body.value {
            let ast_value = ast.simulate(simulator_context).get_type();

            if !branch_exits
                && !branch_returns
                && (ast_value.to_string() == "<<branchexit>>"
                    || ast_value.to_string() == "<<returnexit>>")
            {
                branch_exits = ast_value.to_string() == "<<branchexit>>";
                branch_returns = ast_value.to_string() == "<<returnexit>>";
            } else if branch_exits {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        ast.get_start().clone(),
                        self.closure_body.end.clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the closure or branch has already exited via `break` or `return`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                break;
            }
        }

        if !branch_returns
            && self.return_type.is_some()
            && self.return_type.as_ref().unwrap().to_string() != "void"
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "closure does not return on all paths. The declared return type is `{}`, but at least one execution path can reach the end of the closure without returning",
                        self.return_type.clone().unwrap(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        simulator_context.current_scope = previous_scope;
        if let Some(outer) = simulator_context.current_scope.as_mut() {
            outer.write().unwrap().scopes.push(scope_reference);
        }

        simulator_context.scoped_variables.clear();
        simulator_context.scoped_variables.extend(scoped_variables);
        simulator_context.current_this = current_this;
        simulator_context.local_scope = local_scope;
        simulator_context.current_return_type = current_return_type;

        // Build the closure's type for the resulting value.
        let mut argument_types = Vec::new();
        for (_, argument_declaration) in &self.arguments {
            argument_types.push(argument_declaration.argument_type.clone());
        }

        let return_type = if self.return_type.is_some() {
            self.return_type.clone()
        } else {
            Some(types::PekoType::simple_type("void"))
        };

        let closure_type = types::PekoType::new(
            Vec::new(),
            String::new(),
            argument_types,
            0,
            0,
            0,
            return_type,
            true,
            PositionData::default(),
            PositionData::default(),
        );

        // Closures are objects (technically). Track this one in defined_objects
        // so IDE tooling can attribute hover info to it.
        simulator_context.defined_objects.push(DefinedObject::new(
            false,
            closure_type.clone(),
            self.end.clone(),
        ));

        SimulatorValue::Value(closure_type)
    }
}

/// Simulates a module declaration: creates the submodule, moves into
/// it, simulates the body under the new module's scope, then
/// restores the previous context and registers the module symbol.
impl PekoValueSimulator for ModuleCreationAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let current_module = Arc::clone(&simulator_context.module_context.current_module());

        let previous_scope = simulator_context.current_scope.as_ref().map(Arc::clone);

        let scope_reference = Arc::new(RwLock::new(Scope::new(
            true,
            false,
            self.visibility.clone(),
            self.module_body.start.clone(),
            self.module_body.end.clone(),
            self.module_name.value.clone(),
        )));

        let new_module = SimulatorModule::new(
            self.start.clone(),
            self.visibility.clone(),
            simulator_context.get_current_file(),
            self.docinfo.clone(),
            Some(Arc::clone(&current_module)),
            self.module_name.value.clone(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            Arc::clone(&scope_reference),
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
        );

        let new_module_ref = Arc::new(RwLock::new(new_module));
        let new_module_scope = scope_reference;

        simulator_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .modules
            .insert(self.module_name.value.clone(), new_module_ref.clone());

        simulator_context
            .module_context
            .move_to_module(new_module_ref, false, false);
        simulator_context.current_scope = Some(Arc::clone(&new_module_scope));

        // Header pass over the module body, then the body pass, so a
        // declaration can reference another in the same module regardless of
        // order.
        for ast in &self.module_body.value {
            ast.declare(simulator_context);
        }
        for ast in &self.module_body.value {
            ast.simulate(simulator_context);
        }

        simulator_context.module_context.move_out_of_module();
        simulator_context.current_scope = previous_scope;

        // Attach the module's scope as a child of the parent scope
        // and register the module symbol there.
        if let Some(parent_scope) = simulator_context.current_scope.as_mut() {
            parent_scope.write().unwrap().scopes.push(new_module_scope);

            parent_scope.write().unwrap().symbols.insert(
                self.module_name.value.clone(),
                ScopeSymbol::Module(
                    ScopeModule::new(
                        self.docinfo.clone(),
                        self.module_name.value.clone(),
                        self.start.clone(),
                        self.end.clone(),
                    ),
                    VisibilityData::open_visibility(),
                ),
            );
        }

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Simulates a class declaration.
///
/// Classes have three phases of simulation:
///
/// 1. **Virtual table and attribute collection**: derive attributes
///    and methods from the parent class (if any), then add the
///    class's own attributes. Reports diagnostics for missing parent
///    classes, duplicate attribute names, and multiple inheritance.
/// 2. **Method registration**: build a `SimulatorFunction` for each
///    declared method and insert it into the class's virtual table,
///    overriding parent methods that have matching signatures.
/// 3. **Method body simulation**: simulate each method's body under
///    a fresh scope with `this` and parameter bindings.
///
/// Simulates an enum declaration.
///
/// Checks variant names are unique, then registers the enum and its variants
/// in the current module so the name resolves as a type and `Enum::Variant`
/// resolves to a value of that type.
impl PekoValueSimulator for EnumDeclarationAST {
    /// Header pass: register the enum name and variants so it resolves as a
    /// type regardless of declaration order. Diagnostic-free; the dup-variant
    /// check happens in `simulate`.
    fn declare(&self, simulator_context: &mut PekoSimulatorContext) {
        let mut variant_names: Vec<String> = Vec::new();
        for variant in &self.variants {
            if !variant_names.contains(&variant.value) {
                variant_names.push(variant.value.clone());
            }
        }
        simulator_context.register_enum(self.enum_name.value.clone(), variant_names);
    }

    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let mut variant_names: Vec<String> = Vec::new();

        for variant in &self.variants {
            if variant_names.contains(&variant.value) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        variant.start.clone(),
                        variant.end.clone(),
                        format!(
                            "duplicate enum variant `{}`. Each variant in an enum must have a unique name",
                            variant.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                continue;
            }

            variant_names.push(variant.value.clone());
        }

        simulator_context.register_enum(self.enum_name.value.clone(), variant_names);

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

impl PekoValueSimulator for TraitDeclarationAST {
    /// Header pass: register the trait's slot layout so a class declared in the
    /// same module can implement it regardless of order. Diagnostic-free; the
    /// authoritative dup-method check happens in `simulate`.
    fn declare(&self, simulator_context: &mut PekoSimulatorContext) {
        let generics: Vec<String> = self
            .generics
            .iter()
            .map(|generic| generic.value.clone())
            .collect();

        let mut slots: Vec<TraitMethodSlot> = Vec::new();
        for method in &self.methods {
            let method_name = method.function_name.value.clone();
            if slots.iter().any(|slot| slot.name == method_name) {
                continue;
            }

            let argument_types: Vec<types::PekoType> = method
                .arguments
                .values()
                .map(|argument| argument.argument_type.clone())
                .collect();
            let return_type = method
                .return_type
                .clone()
                .unwrap_or_else(|| types::PekoType::simple_type("void"));

            slots.push(TraitMethodSlot {
                name: method_name,
                argument_types,
                return_type,
                has_default: method.function_body.is_some(),
            });
        }

        simulator_context.register_trait(TraitDefinition {
            name: self.trait_name.value.clone(),
            generics,
            methods: slots,
        });
    }

    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let generics: Vec<String> = self
            .generics
            .iter()
            .map(|generic| generic.value.clone())
            .collect();

        let mut slots: Vec<TraitMethodSlot> = Vec::new();

        for method in &self.methods {
            let method_name = method.function_name.value.clone();

            if slots.iter().any(|slot| slot.name == method_name) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        method.function_name.start.clone(),
                        method.function_name.end.clone(),
                        format!(
                            "duplicate trait method `{}`. Each method in a trait must have a unique name",
                            method_name,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                continue;
            }

            let argument_types: Vec<types::PekoType> = method
                .arguments
                .values()
                .map(|argument| argument.argument_type.clone())
                .collect();

            let return_type = method
                .return_type
                .clone()
                .unwrap_or_else(|| types::PekoType::simple_type("void"));

            slots.push(TraitMethodSlot {
                name: method_name,
                argument_types,
                return_type,
                has_default: method.function_body.is_some(),
            });
        }

        simulator_context.register_trait(TraitDefinition {
            name: self.trait_name.value.clone(),
            generics,
            methods: slots,
        });

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}

/// Generic class declarations are tracked but not simulated until
/// instantiated with concrete type parameters.
impl PekoValueSimulator for ClassAST {
    /// Header pass: register a class shell so other declarations can resolve
    /// this name before its body is simulated. The shell carries the class
    /// type, its own attributes, and its method signatures, all with raw
    /// (unexpanded) types so a forward reference inside a signature is fine.
    /// Inheritance merge, type expansion, body simulation, and conformance all
    /// happen later in `simulate`, which overwrites this shell.
    fn declare(&self, simulator_context: &mut PekoSimulatorContext) {
        // A generic class registers as a generic, exactly as `simulate` does.
        if !self.generics.is_empty() {
            let mut self_reference = self.clone();
            self_reference.generics.clear();

            let current_module = simulator_context.module_context.current_module().clone();
            let current_scope = current_module.read().unwrap().scope.clone();
            let current_file = simulator_context.get_current_file();

            simulator_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .class_generics
                .insert(
                    self.class_name.value.clone(),
                    Arc::new(RwLock::new(SimulatorClassGeneric::new(
                        self.visibility.clone(),
                        self.generics.clone(),
                        self_reference,
                        current_module,
                        current_scope,
                        current_file,
                    ))),
                );
            return;
        }

        // Build the fully-qualified class type from the module chain.
        let mut class_type = types::PekoType::from_string(
            self.class_name.value.as_str(),
            simulator_context.get_current_file(),
        );
        let mut next_module = simulator_context.module_context.current_module().clone();
        loop {
            class_type
                .module_names_mut()
                .insert(0, next_module.read().unwrap().name.clone());
            let parent = next_module.read().unwrap().parent.clone();
            if let Some(parent) = parent {
                next_module = parent;
            } else {
                break;
            }
        }

        // Own attributes, with their raw declared types.
        let mut attributes = IndexMap::new();
        for (attribute_name, attribute) in &self.attributes {
            attributes.insert(
                attribute_name.value.clone(),
                SimulatorClassAttribute::new(
                    attribute_name.start.clone(),
                    attribute.visibility.clone(),
                    attribute.docinfo.clone(),
                    attribute.attribute_type.as_ref().clone(),
                ),
            );
        }

        // Method signatures, grouped by name into overload lists.
        let mut methods: IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>> = IndexMap::new();
        for class_method in &self.methods {
            let mut arguments = IndexMap::new();
            for (argument_name, argument) in &class_method.get_info().arguments {
                arguments.insert(
                    argument_name.value.clone(),
                    SimulatorArg::new(
                        argument.start.clone(),
                        argument.visibility.clone(),
                        argument.argument_type.clone(),
                        argument.default_value.is_some(),
                    ),
                );
            }

            let simulator_function = SimulatorFunction::new(
                class_method.get_info().start.clone(),
                class_method.get_info().visibility.clone(),
                class_method.get_info().docinfo.clone(),
                class_method.get_return_type(),
                arguments,
                class_method.get_info().varargs_type.clone(),
                simulator_context.module_context.current_module().clone(),
            );

            methods
                .entry(class_method.get_info().name.value.clone())
                .or_default()
                .push(Arc::new(RwLock::new(simulator_function)));
        }

        let shell = SimulatorClass::new(
            self.start.clone(),
            class_type,
            None,
            attributes,
            SimulatorClassVirtualTable::new(methods),
            Vec::new(),
            simulator_context.module_context.current_module().clone(),
        );

        simulator_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .classes
            .insert(self.class_name.value.clone(), Arc::new(RwLock::new(shell)));
    }

    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let mut new_class_scope = Scope::new(
            false,
            false,
            VisibilityData::open_visibility(),
            self.start.clone(),
            self.end.clone(),
            format!("class-{}", self.class_name.value),
        );

        // Generic class: track but don't simulate.
        if !self.generics.is_empty() {
            let mut self_reference = self.clone();
            self_reference.generics.clear();

            let current_module = simulator_context.module_context.current_module().clone();
            let current_scope = simulator_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .scope
                .clone();

            let current_file = simulator_context.get_current_file();

            simulator_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .class_generics
                .insert(
                    self.class_name.value.clone(),
                    Arc::new(RwLock::new(SimulatorClassGeneric::new(
                        self.visibility.clone(),
                        self.generics.clone(),
                        self_reference,
                        current_module,
                        current_scope,
                        current_file,
                    ))),
                );

            // Surface attribute symbols on the class scope.
            for (attribute_name, attribute) in &self.attributes {
                new_class_scope.symbols.insert(
                    attribute_name.value.clone(),
                    ScopeSymbol::Variable(
                        ScopeVariable::new(
                            attribute.docinfo.clone(),
                            attribute_name.value.clone(),
                            attribute.attribute_type.as_ref().clone(),
                            attribute_name.start.clone(),
                            attribute_name.end.clone(),
                            true,
                        ),
                        attribute.visibility.clone(),
                    ),
                );
            }

            // Surface method symbols on the class scope and create a
            // child scope for each method body.
            for class_method in &self.methods {
                let mut simplified_args = IndexMap::new();
                for (arg_name, arg_info) in &class_method.get_info().arguments {
                    simplified_args.insert(
                        arg_name.value.clone(),
                        (arg_info.visibility.clone(), arg_info.argument_type.clone()),
                    );
                }

                new_class_scope.symbols.insert(
                    class_method.get_info().name.value.clone(),
                    ScopeSymbol::Function(
                        ScopeFunction::new(
                            class_method.get_info().docinfo.clone(),
                            class_method.get_info().name.value.clone(),
                            class_method.get_return_type(),
                            class_method.get_info().start.clone(),
                            class_method.get_info().end.clone(),
                            false,
                            simplified_args,
                            Vec::new(),
                        ),
                        class_method.get_info().visibility.clone(),
                    ),
                );

                new_class_scope.scopes.push(Arc::new(RwLock::new(Scope::new(
                    false,
                    false,
                    class_method.get_info().visibility.clone(),
                    class_method.get_info().body.start.clone(),
                    class_method.get_info().body.end.clone(),
                    format!("function-{}", class_method.get_info().name.value),
                ))));
            }

            // Attach the class's scope to its declaration scope.
            let scope_reference = Arc::new(RwLock::new(new_class_scope));
            if let Some(scope) = simulator_context.current_scope.as_mut() {
                scope
                    .write()
                    .unwrap()
                    .scopes
                    .push(Arc::clone(&scope_reference));
            }

            // Find a constructor in this class or its parents to seed
            // the ScopeClass's first-constructor-args (used by IDE
            // signature help). If no constructor is declared anywhere
            // in the inheritance chain, fall back to the implicit
            // "all-attributes" constructor.
            let mut constructor_arguments = IndexMap::new();
            let mut found_constructor = false;
            let mut parent = Some(self.clone());
            let mut inherited_attributes = IndexMap::new();

            while !found_constructor && parent.is_some() {
                for (attribute_name, attribute_info) in
                    parent.as_ref().unwrap().attributes.iter().rev()
                {
                    inherited_attributes.insert_before(
                        0,
                        attribute_name.value.clone(),
                        attribute_info.attribute_type.as_ref().clone(),
                    );
                }

                for method in &parent.as_ref().unwrap().methods {
                    if let ClassMethod::Constructor(constructor_info, _) = method {
                        for (argument, argument_info) in &constructor_info.arguments {
                            constructor_arguments.insert(
                                argument.value.clone(),
                                argument_info.argument_type.clone(),
                            );
                        }

                        found_constructor = true;
                        break;
                    }
                }

                if !parent.as_ref().unwrap().derives_from.is_empty() {
                    if !self.derives_from[0].generics().is_empty() {
                        let mut generic_class_simple = self.derives_from[0].clone();
                        generic_class_simple.generics_mut().clear();

                        let find_generic = simulator_context
                            .find_class_generic_in_current(generic_class_simple.to_string());
                        if find_generic.is_none() {
                            break;
                        }

                        parent = Some(find_generic.unwrap().class.clone());
                    } else {
                        let parent_class =
                            simulator_context.get_class_by_type(&self.derives_from[0]);
                        if parent_class.is_none()
                            || !parent_class
                                .as_ref()
                                .unwrap()
                                .main_virtual_table
                                .methods
                                .contains_key("constructor")
                        {
                            for (attribute_name, attribute_info) in
                                &parent_class.unwrap().attributes
                            {
                                inherited_attributes.insert(
                                    attribute_name.clone(),
                                    attribute_info.attribute_type.clone(),
                                );
                            }

                            break;
                        }

                        for (argument_name, argument_info) in &parent_class
                            .as_ref()
                            .unwrap()
                            .main_virtual_table
                            .methods["constructor"][0]
                            .read()
                            .unwrap()
                            .arguments
                        {
                            constructor_arguments
                                .insert(argument_name.clone(), argument_info.argument_type.clone());
                        }

                        found_constructor = true;
                    }
                } else {
                    // No parent to walk to, so the inheritance search ends.
                    parent = None;
                }
            }

            if !found_constructor {
                for (attribute_name, attribute_info) in &self.attributes {
                    inherited_attributes.insert(
                        attribute_name.value.clone(),
                        attribute_info.attribute_type.as_ref().clone(),
                    );
                }
                constructor_arguments = inherited_attributes;
            }

            // Register the generic class symbol.
            simulator_context
                .current_scope
                .as_mut()
                .unwrap()
                .write()
                .unwrap()
                .symbols
                .insert(
                    self.class_name.value.clone(),
                    ScopeSymbol::Class(
                        ScopeClass::new(
                            self.docinfo.clone(),
                            self.class_name.value.clone(),
                            self.start.clone(),
                            self.end.clone(),
                            true,
                            self.generics
                                .iter()
                                .map(|generic| generic.value.clone())
                                .collect::<Vec<String>>(),
                            constructor_arguments,
                        ),
                        self.visibility.clone(),
                    ),
                );

            return SimulatorValue::Value(types::PekoType::from_string(
                "default",
                simulator_context.get_current_file(),
            ));
        }

        // Non-generic class: simulate in three phases.

        // Build the fully-qualified class type with its module path.
        let mut class_type = types::PekoType::from_string(
            self.class_name.value.as_str(),
            simulator_context.get_current_file(),
        );

        let mut next_module = simulator_context.module_context.current_module().clone();
        loop {
            class_type
                .module_names_mut()
                .insert(0, next_module.read().unwrap().name.clone());
            let parent = next_module.read().unwrap().parent.clone();
            if let Some(p) = parent {
                next_module = p;
            } else {
                break;
            }
        }

        let mut virtual_table_methods = IndexMap::new();
        let mut class_attributes = IndexMap::new();
        let mut parent_class: Option<Box<SimulatorClass>> = None;

        // Reserve an opaque slot for the virtual table on classes with
        // methods or inheritance.
        if !self.methods.is_empty() || self.derives_from.len() == 1 {
            class_attributes.insert(
                "<main_virtual_table>".to_string(),
                SimulatorClassAttribute::new(
                    PositionData::default(),
                    VisibilityData::open_visibility(),
                    None,
                    types::PekoType::simple_type("opaque"),
                ),
            );
        }

        // Inherit attributes and methods from the parent class.
        if self.derives_from.len() == 1 {
            let find_parent_class = simulator_context.get_class_by_type(&self.derives_from[0]);

            if let Some(find_parent_class) = &find_parent_class {
                parent_class = Some(Box::new(find_parent_class.clone()));

                for (attribute_name, attribute) in &find_parent_class.attributes {
                    if attribute_name != "<main_virtual_table>" {
                        class_attributes.insert(attribute_name.clone(), attribute.clone());
                    }

                    new_class_scope.symbols.insert(
                        attribute_name.clone(),
                        ScopeSymbol::Variable(
                            ScopeVariable::new(
                                attribute.docinfo.clone(),
                                attribute_name.clone(),
                                attribute.attribute_type.clone(),
                                attribute.position.clone(),
                                attribute.position.clone(),
                                true,
                            ),
                            attribute.visibility.clone(),
                        ),
                    );
                }

                virtual_table_methods.extend(find_parent_class.main_virtual_table.methods.clone());
            } else {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.derives_from[0].start_position.clone(),
                        self.derives_from[0].end_position.clone(),
                        format!(
                            "cannot find class `{}`. Check the class name, that the class is in scope, and that it has been declared before this point",
                            self.derives_from[0],
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        } else if self.derives_from.len() > 1 {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.derives_from[0].start_position.clone(),
                    self.derives_from.last().unwrap().end_position.clone(),
                    "cannot inherit from multiple classes. Pekoscript supports single inheritance only; specify just one parent class after `from`".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        // Pre-register the class so it can be referenced from its own
        // attribute / method bodies during simulation.
        let class = SimulatorClass::new(
            self.start.clone(),
            class_type.clone(),
            parent_class.clone(),
            class_attributes.clone(),
            SimulatorClassVirtualTable::new(virtual_table_methods.clone()),
            Vec::new(),
            simulator_context.module_context.current_module().clone(),
        );
        simulator_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .classes
            .insert(self.class_name.value.clone(), Arc::new(RwLock::new(class)));

        // Simulate each attribute: type-check, then add to both the
        // class's attribute map and the scope's symbol table.
        for (attribute_name, attribute) in &self.attributes {
            let expanded_attribute = if !simulator_context
                .type_exists(attribute.attribute_type.as_ref())
            {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        attribute.attribute_type.start_position.clone(),
                        attribute.attribute_type.end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            attribute.attribute_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                types::PekoType::error_type()
            } else {
                simulator_context
                    .expand_type(attribute.attribute_type.as_ref())
                    .unwrap()
            };

            if class_attributes.contains_key(&attribute_name.value) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        attribute_name.start.clone(),
                        attribute_name.end.clone(),
                        format!(
                            "cannot declare multiple attributes with the same name `{}` on a class. Attribute names must be unique within a class",
                            attribute_name.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                continue;
            }

            simulator_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .classes
                .get_mut(&self.class_name.value)
                .unwrap()
                .write()
                .unwrap()
                .attributes
                .insert(
                    attribute_name.value.clone(),
                    SimulatorClassAttribute::new(
                        attribute_name.start.clone(),
                        attribute.visibility.clone(),
                        attribute.docinfo.clone(),
                        expanded_attribute,
                    ),
                );

            new_class_scope.symbols.insert(
                attribute_name.value.clone(),
                ScopeSymbol::Variable(
                    ScopeVariable::new(
                        attribute.docinfo.clone(),
                        attribute_name.value.clone(),
                        attribute.attribute_type.as_ref().clone(),
                        attribute_name.start.clone(),
                        attribute_name.end.clone(),
                        true,
                    ),
                    attribute.visibility.clone(),
                ),
            );
        }

        // Method registration (phase 2): build a SimulatorFunction
        // for each method and insert it into the class's virtual
        // table, overriding by signature equality.
        for class_method in &self.methods {
            let class_ref = simulator_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .classes[&self.class_name.value]
                .clone();

            if !class_ref
                .read()
                .unwrap()
                .main_virtual_table
                .methods
                .contains_key(&class_method.get_info().name.value)
            {
                class_ref
                    .write()
                    .unwrap()
                    .main_virtual_table
                    .methods
                    .insert(class_method.get_info().name.value.clone(), Vec::new());
            }

            let mut simulator_arguments = IndexMap::new();
            for (argument_name, argument_declaration) in &class_method.get_info().arguments {
                let argument_type_expanded =
                    simulator_context.expand_type(&argument_declaration.argument_type);

                simulator_arguments.insert(
                    argument_name.value.clone(),
                    SimulatorArg::new(
                        argument_declaration.start.clone(),
                        argument_declaration.visibility.clone(),
                        argument_type_expanded.unwrap_or_else(PekoType::error_type),
                        argument_declaration.default_value.is_some(),
                    ),
                );
            }

            let return_type_expanded =
                simulator_context.expand_type(&class_method.get_return_type());

            let simulator_function = SimulatorFunction::new(
                class_method.get_info().start.clone(),
                class_method.get_info().visibility.clone(),
                class_method.get_info().docinfo.clone(),
                return_type_expanded
                    .clone()
                    .unwrap_or_else(PekoType::error_type),
                simulator_arguments,
                class_method.get_info().varargs_type.clone(),
                simulator_context.module_context.current_module().clone(),
            );

            let mut function_added_to_vtable = false;

            // Look for an existing overload with an exact signature
            // match. method_index = -1 means no match found.
            let existing_overloads: Vec<SimulatorFunction> =
                class_ref.read().unwrap().main_virtual_table.methods
                    [&class_method.get_info().name.value]
                    .iter()
                    .map(|function| function.read().unwrap().clone())
                    .collect();

            if !existing_overloads.is_empty() {
                let mut method_index = -1;

                for (current_method_index, method_choice) in existing_overloads.iter().enumerate() {
                    if method_choice.arguments.len() != class_method.get_info().arguments.len() {
                        continue;
                    }

                    let mut signatures_equal = true;
                    for ((_, method_choice_argument), (_, method_to_create_argument)) in
                        method_choice
                            .arguments
                            .iter()
                            .zip(class_method.get_info().arguments.iter())
                    {
                        if !simulator_context.types_equal(
                            &method_choice_argument.argument_type,
                            &method_to_create_argument.argument_type,
                        ) {
                            signatures_equal = false;
                            break;
                        }
                    }

                    if signatures_equal {
                        method_index = current_method_index as i32;
                        break;
                    }
                }

                if method_index >= 0 {
                    function_added_to_vtable = true;
                    class_ref.write().unwrap().main_virtual_table.methods
                        [&class_method.get_info().name.value][method_index as usize] =
                        Arc::new(RwLock::new(simulator_function.clone()));
                }
            }

            if !function_added_to_vtable {
                class_ref
                    .write()
                    .unwrap()
                    .main_virtual_table
                    .methods
                    .get_mut(&class_method.get_info().name.value)
                    .unwrap()
                    .push(Arc::new(RwLock::new(simulator_function)));
            }
        }

        // Method body simulation (phase 3): walk every method again,
        // this time simulating their bodies under a fresh scope with
        // `this` and parameters bound.
        for class_method in &self.methods {
            let mut simplified_args = IndexMap::new();
            for (argument_name, argument_declaration) in &class_method.get_info().arguments {
                simplified_args.insert(
                    argument_name.value.clone(),
                    (
                        argument_declaration.visibility.clone(),
                        argument_declaration.argument_type.clone(),
                    ),
                );
            }

            new_class_scope.symbols.insert(
                class_method.get_info().name.value.clone(),
                ScopeSymbol::Function(
                    ScopeFunction::new(
                        class_method.get_info().docinfo.clone(),
                        class_method.get_info().name.value.clone(),
                        class_method.get_return_type(),
                        class_method.get_info().start.clone(),
                        class_method.get_info().end.clone(),
                        false,
                        simplified_args,
                        Vec::new(),
                    ),
                    class_method.get_info().visibility.clone(),
                ),
            );

            let method_scope = Arc::new(RwLock::new(Scope::new(
                false,
                false,
                class_method.get_info().visibility.clone(),
                class_method.get_info().body.start.clone(),
                class_method.get_info().body.end.clone(),
                format!("function-{}", class_method.get_info().name.value),
            )));

            let previous_scope = simulator_context.current_scope.clone();
            simulator_context.current_scope = Some(method_scope.clone());

            let previous_scoped_variables = simulator_context.scoped_variables.clone();
            simulator_context.scoped_variables.clear();

            let local_scope_previous = simulator_context.local_scope;
            simulator_context.local_scope = true;

            let return_type_expanded =
                simulator_context.expand_type(&class_method.get_return_type());

            let previous_return_type = simulator_context.current_return_type.clone();
            simulator_context.current_return_type =
                if class_method.get_return_type().to_string() != "void" {
                    return_type_expanded
                } else {
                    None
                };

            let current_this = simulator_context.current_this.clone();
            let current_this_variable = SimulatorVariable::new(
                class_method.get_info().start.clone(),
                VisibilityData::open_visibility(),
                class_type.clone(),
                SimulatorValue::Value(class_type.clone()),
                simulator_context.module_context.current_module().clone(),
            );

            simulator_context
                .scoped_variables
                .insert(String::from("this"), current_this_variable.clone());

            simulator_context.current_this = Some(current_this_variable);

            simulator_context
                .current_scope
                .as_mut()
                .unwrap()
                .write()
                .unwrap()
                .symbols
                .insert(
                    String::from("this"),
                    ScopeSymbol::Variable(
                        ScopeVariable::new(
                            None,
                            String::from("this"),
                            class_type.clone(),
                            class_method.get_info().start.clone(),
                            class_method.get_info().end.clone(),
                            false,
                        ),
                        VisibilityData::open_visibility(),
                    ),
                );

            // Bind each method argument.
            for (argument_name, argument_info) in &class_method.get_info().arguments {
                let argument_type = if !simulator_context.type_exists(&argument_info.argument_type)
                {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            argument_info.argument_type.start_position.clone(),
                            argument_info.argument_type.end_position.clone(),
                            format!(
                                "argument type `{}` is not defined. Check the type name and that the type is in scope",
                                argument_info.argument_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    types::PekoType::error_type()
                } else {
                    argument_info.argument_type.clone()
                };

                simulator_context.scoped_variables.insert(
                    argument_name.value.clone(),
                    SimulatorVariable::new(
                        argument_info.start.clone(),
                        argument_info.visibility.clone(),
                        argument_type.clone(),
                        SimulatorValue::Value(argument_type.clone()),
                        simulator_context.module_context.current_module().clone(),
                    ),
                );

                simulator_context
                    .current_scope
                    .as_mut()
                    .unwrap()
                    .write()
                    .unwrap()
                    .symbols
                    .insert(
                        argument_name.value.clone(),
                        ScopeSymbol::Variable(
                            ScopeVariable::new(
                                if class_method.get_info().docinfo.is_some()
                                    && class_method
                                        .get_info()
                                        .docinfo
                                        .as_ref()
                                        .unwrap()
                                        .parameter_docs
                                        .contains_key(&argument_name.value)
                                {
                                    Some(DocInfo::new(
                                        class_method
                                            .get_info()
                                            .docinfo
                                            .as_ref()
                                            .unwrap()
                                            .parameter_docs[&argument_name.value]
                                            .clone(),
                                        HashMap::new(),
                                        Vec::new(),
                                    ))
                                } else {
                                    None
                                },
                                argument_name.value.clone(),
                                argument_type.clone(),
                                argument_name.start.clone(),
                                argument_name.end.clone(),
                                false,
                            ),
                            argument_info.visibility.clone(),
                        ),
                    );
            }

            // Variadic argument binding.
            if class_method.get_info().varargs_type.is_some() {
                simulator_context.scoped_variables.insert(
                    class_method.get_info().varargs_name.value.clone(),
                    SimulatorVariable::new(
                        class_method.get_info().varargs_name.start.clone(),
                        VisibilityData::open_visibility(),
                        class_method.get_info().varargs_type.clone().unwrap(),
                        SimulatorValue::Value(
                            class_method.get_info().varargs_type.clone().unwrap(),
                        ),
                        simulator_context.module_context.current_module().clone(),
                    ),
                );

                simulator_context
                    .current_scope
                    .as_mut()
                    .unwrap()
                    .write()
                    .unwrap()
                    .symbols
                    .insert(
                        class_method.get_info().varargs_name.value.clone(),
                        ScopeSymbol::Variable(
                            ScopeVariable::new(
                                None,
                                class_method.get_info().varargs_name.value.clone(),
                                class_method.get_info().varargs_type.clone().unwrap(),
                                class_method.get_info().varargs_name.start.clone(),
                                class_method.get_info().varargs_name.end.clone(),
                                false,
                            ),
                            VisibilityData::open_visibility(),
                        ),
                    );
            }

            // Constructor `super(...)` calls: type-check their
            // arguments against the parent class's constructor
            // overloads.
            if let ClassMethod::Constructor(_, super_call) = class_method {
                if super_call.is_some() && parent_class.is_none() {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            super_call.clone().unwrap().start.clone(),
                            super_call.clone().unwrap().end.clone(),
                            "cannot use `super(...)` in the constructor of a class that does not inherit from any other class. Add a `from ParentClass` clause if you intend to derive from one".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                } else if super_call.is_some() {
                    let mut argument_types = Vec::new();
                    let mut super_call_keyword_types = HashMap::new();

                    for (argument_name, argument) in &super_call.as_ref().unwrap().arguments {
                        let argument_value = argument.simulate(simulator_context);
                        argument_types.push(argument_value.get_type());

                        if argument_name.is_some() {
                            super_call_keyword_types.insert(
                                argument_name.clone().unwrap().value,
                                argument_value.get_type(),
                            );
                        }
                    }

                    let super_constructor_overloads =
                        parent_class.clone().unwrap().main_virtual_table.methods["constructor"]
                            .iter()
                            .map(|function| function.read().unwrap().clone())
                            .collect();

                    let best_super_overload = simulator_context.choose_function(
                        super_constructor_overloads,
                        argument_types,
                        if super_call_keyword_types.is_empty() {
                            None
                        } else {
                            Some(super_call_keyword_types)
                        },
                        true,
                    );

                    if best_super_overload.is_none() {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                super_call.clone().unwrap().start.clone(),
                                super_call.clone().unwrap().end.clone(),
                                "arguments to `super(...)` do not match any constructor overload of the parent class. Check the argument types against the parent's declared constructors".to_string(),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                    }
                }
            }

            // Track whether this method's body reassigns an attribute of
            // `this`, seeding from any hand-written `[mutates]` modifier.
            let previous_method_mutates = simulator_context.current_method_mutates;
            simulator_context.current_method_mutates =
                class_method.get_info().visibility.mutates;

            let previous_method_name = simulator_context.current_method_name.clone();
            simulator_context.current_method_name =
                Some(class_method.get_info().name.value.clone());

            // A constructor must initialize every attribute the class declares.
            // Seed the to-set list with the class's own attributes; each
            // `this.attr = ...` removes one, and any left over after the body
            // is an uninitialized attribute.
            let is_constructor = matches!(class_method, ClassMethod::Constructor(_, _));
            if is_constructor {
                simulator_context.attributes_to_set =
                    self.attributes.keys().map(|name| name.value.clone()).collect();
            }

            // Simulate the method body and check for unreachable code.
            let mut branch_exits = false;
            let mut branch_returns = false;

            for ast in &class_method.get_info().body.value {
                let ast_value = ast.simulate(simulator_context).get_type();

                if !branch_exits
                    && !branch_returns
                    && (ast_value.to_string() == "<<branchexit>>"
                        || ast_value.to_string() == "<<returnexit>>")
                {
                    branch_exits = ast_value.to_string() == "<<branchexit>>";
                    branch_returns = ast_value.to_string() == "<<returnexit>>";
                } else if branch_exits {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            ast.get_start().clone(),
                            class_method.get_info().body.value.last().unwrap().get_end().clone(),
                            "unreachable code: this statement (and everything after it) cannot run because the method or branch has already exited via `break` or `return`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    break;
                }
            }

            // A constructor that leaves attributes unset is an error.
            if is_constructor && !simulator_context.attributes_to_set.is_empty() {
                let uninitialized = simulator_context.attributes_to_set.join(", ");
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        class_method.get_info().start.clone(),
                        class_method.get_info().end.clone(),
                        format!(
                            "constructor of class `{}` does not initialize every attribute. The following attributes are never set: {uninitialized}",
                            self.class_name.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
            simulator_context.attributes_to_set = Vec::new();

            // Non-void methods must return on all paths.
            if !branch_returns && class_method.get_return_type().to_string() != "void" {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        class_method.get_info().start.clone(),
                        class_method.get_info().end.clone(),
                        format!(
                            "method `{}` does not return on all paths. The declared return type is `{}`, but at least one execution path can reach the end of the method without returning",
                            class_method.get_info().name.value,
                            class_method.get_return_type(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            simulator_context.current_scope = previous_scope;
            new_class_scope.scopes.push(method_scope);

            simulator_context.scoped_variables = previous_scoped_variables;

            simulator_context.local_scope = local_scope_previous;
            simulator_context.current_return_type = previous_return_type;

            simulator_context.current_this = current_this;

            // Persist the inferred `[mutates]` onto the matching vtable
            // overload so call sites and codegen can read it. The overload is
            // matched by signature, the same way phase 2 builds the table.
            if simulator_context.current_method_mutates {
                let class_ref = simulator_context
                    .module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .classes[&self.class_name.value]
                    .clone();

                let overloads = class_ref.read().unwrap().main_virtual_table.methods
                    [&class_method.get_info().name.value]
                    .clone();

                let ast_argument_types: Vec<PekoType> = class_method
                    .get_info()
                    .arguments
                    .iter()
                    .map(|(_, argument)| argument.argument_type.clone())
                    .collect();

                for overload in &overloads {
                    let overload_argument_types: Vec<PekoType> = overload
                        .read()
                        .unwrap()
                        .arguments
                        .iter()
                        .map(|(_, argument)| argument.argument_type.clone())
                        .collect();

                    if overload_argument_types.len() != ast_argument_types.len() {
                        continue;
                    }

                    let mut signatures_equal = true;
                    for (vtable_type, ast_type) in
                        overload_argument_types.iter().zip(ast_argument_types.iter())
                    {
                        if !simulator_context.types_equal(vtable_type, ast_type) {
                            signatures_equal = false;
                            break;
                        }
                    }

                    if signatures_equal {
                        overload.write().unwrap().visibility.mutates = true;
                    }
                }
            }

            simulator_context.current_method_mutates = previous_method_mutates;
            simulator_context.current_method_name = previous_method_name;
        }

        // Trait conformance and coherence. The class's virtual table now holds
        // every method (inherited and own), so a required trait method is
        // satisfied by any method of the same name in scope.
        let conformance_class = simulator_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .classes[&self.class_name.value]
            .clone();
        let class_method_names: std::collections::HashSet<String> = conformance_class
            .read()
            .unwrap()
            .main_virtual_table
            .methods
            .keys()
            .cloned()
            .collect();

        let mut implemented_traits: Vec<String> = Vec::new();
        for implemented in &self.implements {
            let trait_name = implemented.name().to_string();

            let Some(trait_definition) = simulator_context.get_trait(&trait_name) else {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        implemented.start_position.clone(),
                        implemented.end_position.clone(),
                        format!(
                            "`{trait_name}` is not a trait. The `impl` clause lists traits the class conforms to",
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                continue;
            };

            // Coherence: a class implements a given trait at most once.
            if implemented_traits.contains(&trait_name) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        implemented.start_position.clone(),
                        implemented.end_position.clone(),
                        format!(
                            "class `{}` implements trait `{trait_name}` more than once. A type implements a given trait at most once",
                            self.class_name.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                continue;
            }
            implemented_traits.push(trait_name.clone());

            // Every required slot (one with no default body) must be provided.
            for slot in &trait_definition.methods {
                if !slot.has_default && !class_method_names.contains(&slot.name) {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            implemented.start_position.clone(),
                            implemented.end_position.clone(),
                            format!(
                                "class `{}` does not implement method `{}` required by trait `{trait_name}`. Provide a `fn {}(...)` method or a trait default",
                                self.class_name.value, slot.name, slot.name,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                }
            }
        }

        // Persist the implemented trait names on the class so a safe
        // `value as Trait` cast can check that the static type carries it.
        simulator_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .classes[&self.class_name.value]
            .write()
            .unwrap()
            .implements = implemented_traits;

        // Collect the first-constructor arguments (or attribute list
        // for the implicit constructor) for IDE signature help.
        let mut constructor_arguments = IndexMap::new();
        let class_ref = simulator_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .classes[&self.class_name.value]
            .clone();
        if class_ref
            .read()
            .unwrap()
            .main_virtual_table
            .methods
            .contains_key("constructor")
        {
            let first_constructor = class_ref
                .read()
                .unwrap()
                .main_virtual_table
                .methods
                .get("constructor")
                .unwrap()[0]
                .read()
                .unwrap()
                .clone();

            for (argument_name, argument_info) in first_constructor.arguments {
                constructor_arguments.insert(argument_name, argument_info.argument_type);
            }
        } else {
            for (attribute_name, attribute_info) in class_ref.read().unwrap().attributes.clone() {
                constructor_arguments.insert(attribute_name, attribute_info.attribute_type);
            }
        }

        // Register the class symbol in the current scope.
        simulator_context
            .current_scope
            .as_mut()
            .unwrap()
            .write()
            .unwrap()
            .symbols
            .insert(
                self.class_name.value.clone(),
                ScopeSymbol::Class(
                    ScopeClass::new(
                        self.docinfo.clone(),
                        self.class_name.value.clone(),
                        self.start.clone(),
                        self.end.clone(),
                        false,
                        Vec::new(),
                        constructor_arguments,
                    ),
                    self.visibility.clone(),
                ),
            );

        simulator_context
            .current_scope
            .as_mut()
            .unwrap()
            .write()
            .unwrap()
            .scopes
            .push(Arc::new(RwLock::new(new_class_scope)));

        SimulatorValue::Value(types::PekoType::simple_type("default"))
    }
}
