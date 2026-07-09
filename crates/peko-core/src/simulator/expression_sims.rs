//! # Expression AST simulators
//!
//! [`PekoValueSimulator`] implementations for expression-level AST
//! nodes - anything that produces a value when simulated:
//!
//! * **Literals** - array ([`ArrayAST`]), map ([`MapAST`]), XML tag
//!   ([`PekoXTagAST`]), range ([`RangeAST`]).
//! * **References** - variable ([`VariableReferenceAST`]), module
//!   access ([`ModuleAccessAST`]), object access ([`ObjectAccessAST`]),
//!   array access ([`ArrayAccessAST`]).
//! * **Calls and constructions** - function call ([`FunctionCallAST`]),
//!   object construction ([`ObjectConstructionAST`]).
//! * **Operators** - unary ([`UnaryExpressionAST`]), binary
//!   ([`BinaryExpressionAST`]).
//! * **Conversions** - cast ([`CastAST`]), unwrap ([`UnwrapAST`]).
//!
//! The function-call impl is by far the largest - it handles three
//! distinct syntaxes (normal call, expression call, object
//! construction), overload resolution, and generic type inference,
//! all behind a single [`FunctionCallAST`] node produced by the parser.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::asts::PekoAST;
use crate::asts::data_structures::{ClassMethod, PositionData, PositionedValue, VisibilityData};
use crate::asts::expressions::*;
use crate::asts::values::StringAST;
use crate::diagnostics;
use crate::execution::ExecutionContextAlgorithms;
use crate::simulator::data_structures::{FunctionCall, pointee_type};
use crate::types::{self, PekoType};

use super::PekoValueSimulator;
use super::context::PekoSimulatorContext;
use super::data_structures::{DefinedObject, SimulatorArg, SimulatorVariable};
use super::value::SimulatorValue;

/// Replaces every backward-inference variable (`?N`) in `ty` with its bound
/// concrete type, recursing through generic arguments. A type that is itself
/// an inference variable becomes the bound type, keeping the original depth.
fn substitute_inference_variables(ty: &PekoType, bindings: &HashMap<String, PekoType>) -> PekoType {
    if let Some(concrete) = bindings.get(ty.name()) {
        let mut result = concrete.clone();
        result.array_depth += ty.array_depth;
        result.reference_depth += ty.reference_depth;
        return result;
    }

    let mut result = ty.clone();
    if !result.generics().is_empty() {
        let substituted: Vec<PekoType> = result
            .generics()
            .iter()
            .map(|generic| substitute_inference_variables(generic, bindings))
            .collect();
        *result.generics_mut() = substituted;
    }
    result
}

/// Simulates an array literal `[a, b, c]`.
///
/// All elements must share the first element's type. The result is
/// constructed by delegating to [`ObjectConstructionAST`] with the
/// `Array` class and the inferred element type as the generic.
impl PekoValueSimulator for ArrayAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Empty array literals can't have their element type inferred.
        if self.values.is_empty() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "array literal must contain at least one value. The element type is inferred from the first value, so an empty array literal cannot be type-checked. Add at least one element, or declare an explicit type and use `Array<T>()` instead".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            return SimulatorValue::Value(types::PekoType::error_type());
        }

        // Type-check subsequent values against the first.
        let first_value_type = self.values[0].simulate(simulator_context).get_type();

        for value in self.values.iter().skip(1) {
            let current_value_type = value.simulate(simulator_context).get_type();

            if !simulator_context.types_similar(&current_value_type, &first_value_type) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        value.get_start().clone(),
                        value.get_end().clone(),
                        format!(
                            "array element has type `{}` but the array's element type is `{}`. All elements of an array literal must have the same type as the first element",
                            current_value_type,
                            first_value_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        // Construct as a standard::Array via ObjectConstructionAST.
        ObjectConstructionAST::new(
            self.start.clone(),
            self.end.clone(),
            PositionedValue::create_no_position(String::from("Array")),
            vec![first_value_type],
            Vec::new(),
        )
        .simulate(simulator_context)
    }
}

/// Simulates a map literal `{k: v, k: v, ...}`.
///
/// Keys must share a type and values must share a type; both are
/// inferred from the first pair.
impl PekoValueSimulator for MapAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if self.key_values.is_empty() {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "map literal must contain at least one key-value pair. The key and value types are inferred from the first pair, so an empty map literal cannot be type-checked".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            return SimulatorValue::Value(types::PekoType::error_type());
        }

        let key_type = self.key_values[0].0.simulate(simulator_context).get_type();
        let value_type = self.key_values[0].1.simulate(simulator_context).get_type();

        for (key, value) in self.key_values.iter().skip(1) {
            let current_key_type = key.simulate(simulator_context).get_type();
            let current_value_type = value.simulate(simulator_context).get_type();

            if !simulator_context.types_similar(&current_key_type, &key_type) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        value.get_start().clone(),
                        value.get_end().clone(),
                        format!(
                            "map key has type `{}` but the map's key type is `{}`. All keys of a map literal must have the same type as the first key",
                            current_key_type,
                            key_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            // Check the current value's type against the map value type.
            if !simulator_context.types_similar(&current_value_type, &value_type) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        value.get_start().clone(),
                        value.get_end().clone(),
                        format!(
                            "map value has type `{}` but the map's value type is `{}`. All values of a map literal must have the same type as the first value",
                            current_value_type,
                            value_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        ObjectConstructionAST::new(
            self.start.clone(),
            self.end.clone(),
            PositionedValue::create_no_position(String::from("Map")),
            vec![key_type, value_type],
            Vec::new(),
        )
        .simulate(simulator_context)
    }
}

/// Simulates a PekoX (XML-style) tag literal `<Tag attr="...">...</Tag>`.
///
/// Builds the equivalent `xml::Element` object: tag name + string map of
/// attributes + array of children + inner text + events map.
impl PekoValueSimulator for PekoXTagAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Build (attribute_name, attribute_value) pairs, each as
        // SimulatorValues, to feed into a string-to-string map.
        let mut attribute_key_value_pairs = Vec::new();

        for attribute_value in self.attributes.values() {
            let attribute_name = SimulatorValue::Value(types::PekoType::simple_type("string"));

            attribute_key_value_pairs
                .push((attribute_name, attribute_value.simulate(simulator_context)));
        }

        let element_attributes = simulator_context.create_standard_map(
            &types::PekoType::simple_type("string"),
            &types::PekoType::simple_type("string"),
            attribute_key_value_pairs,
        );

        let element_attributes = if let Some(attrs) = element_attributes {
            attrs
        } else {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.attributes_start.clone(),
                    self.attributes_end.clone(),
                    "one or more values assigned to this tag's attributes are not convertible to strings. All XML attribute values must be `String`-compatible".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            SimulatorValue::Value(types::PekoType::error_type())
        };

        let current_file = simulator_context.get_current_file();

        // Simulate the children, requiring each to be a ui::Element.
        let mut children = Vec::new();

        for child in &self.children {
            let child_value = child.clone().simulate(simulator_context);

            if simulator_context.types_equal(
                &child_value.get_type(),
                &types::PekoType::from_string("xml::Element", &current_file),
            ) {
                children.push(child_value);
            } else {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        child.get_start().clone(),
                        child.get_end().clone(),
                        "only XML tags can be interpolated with `{}` syntax inside another tag. Consider using `${}` syntax for non-element interpolation instead".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        let element_children = simulator_context
            .create_standard_array(
                &types::PekoType::from_string("xml::Element", &current_file),
                children,
            )
            .unwrap();

        // Build the inner-text `string` value.
        let element_inner_text = PekoAST::String(StringAST::new(
            PositionData::default(),
            PositionData::default(),
            true,
            self.inner_text.clone(),
        ))
        .simulate(simulator_context);

        let tag_string = SimulatorValue::Value(types::PekoType::simple_type("string"));

        let events_map = simulator_context
            .create_standard_map(
                &types::PekoType::simple_type("string"),
                &types::PekoType::from_string("closure(xml::Event) => void", &current_file),
                Vec::new(),
            )
            .unwrap();

        // Build the final `xml::Element` object.
        let pekox_tag_object = simulator_context.create_object(
            &types::PekoType::from_string("xml::Element", &current_file),
            vec![
                tag_string,
                element_attributes,
                element_children,
                element_inner_text,
                events_map,
            ],
        );

        if let Some(obj) = pekox_tag_object {
            obj
        } else {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "internal error: failed to construct `xml::Element` for this XML tag. The standard library may not be properly linked".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            SimulatorValue::Value(types::PekoType::error_type())
        }
    }
}

/// Simulates a range expression `start..end`.
///
/// Both endpoints must be `int`-compatible. The result is a
/// `standard::RangeIterator`.
impl PekoValueSimulator for RangeAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let range_start = self.range_from.clone().simulate(simulator_context);
        if !simulator_context.types_similar(
            &range_start.get_type(),
            &types::PekoType::simple_type("i32"),
        ) {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.range_from.get_start().clone(),
                    self.range_from.get_end().clone(),
                    format!(
                        "range start expression has type `{}` but must be `int`-compatible. Range expressions iterate over integers",
                        range_start.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        let range_end = self.range_to.clone().simulate(simulator_context);
        if !simulator_context
            .types_similar(&range_end.get_type(), &types::PekoType::simple_type("i32"))
        {
            // Bug fix vs original: the original referenced
            // `self.range_from` for both endpoints' diagnostics, so a
            // bad range_to would highlight the range_from expression.
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.range_to.get_start().clone(),
                    self.range_to.get_end().clone(),
                    format!(
                        "range end expression has type `{}` but must be `int`-compatible. Range expressions iterate over integers",
                        range_end.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        SimulatorValue::Value(types::PekoType::from_string("standard::RangeIterator", ""))
    }
}

/// Simulates a variable reference.
///
/// Handles four cases in order:
///
/// 1. The pseudo-identifiers `None` and `Default`, which produce
///    values whose type is taken from the surrounding inference
///    context.
/// 2. Local variables in the current block's scoped-variable map.
/// 3. Attribute references on the current `this`.
/// 4. Global variables and global functions in the current module's
///    hierarchy. (When the reference resolves to a function name,
///    the result is a function-typed value usable in expressions.)
impl PekoValueSimulator for VariableReferenceAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // --- None pseudo-identifier --- //
        if self.variable_name.value == "None" {
            // None needs a type hint to know which Option<T> it is.
            if simulator_context.current_expected_type_options.is_none() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        "cannot determine the type of `None`. `None` requires a type hint from the surrounding context, such as a variable declaration with a declared `Option<T>` type".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));

                return SimulatorValue::Value(types::PekoType::error_type());
            }

            for expected_type in &simulator_context
                .current_expected_type_options
                .clone()
                .unwrap()
            {
                let expand_option = simulator_context.expand_type(expected_type);
                let expand_option = expand_option.unwrap();

                // Only Option<T> can hold None.
                if expand_option.name() == "Option" && expand_option.generics().len() == 1 {
                    simulator_context.defined_objects.push(DefinedObject::new(
                        false,
                        expand_option.clone(),
                        self.variable_name.end.clone(),
                    ));

                    return SimulatorValue::Value(expand_option);
                }
            }

            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_name.start.clone(),
                    self.variable_name.end.clone(),
                    "cannot create a `None` value when the inferred type is not `Option<T>`. `None` is only valid where an `Option` is expected".to_string(),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));

            return SimulatorValue::Value(types::PekoType::error_type());
        }

        // --- Default pseudo-identifier --- //
        if self.variable_name.value == "Default" {
            if simulator_context.current_expected_type_options.is_none() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        "cannot determine the type of `Default`. `Default` requires a type hint from the surrounding context, such as a variable declaration with an explicit type".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return SimulatorValue::Value(types::PekoType::error_type());
            }

            let main_expected_type = simulator_context
                .current_expected_type_options
                .clone()
                .unwrap()[0]
                .clone();

            // Built-in types have built-in default values.
            let default_value = if main_expected_type.is_builtin_type() {
                match main_expected_type.to_string().as_str() {
                    "i32" => SimulatorValue::Value(types::PekoType::simple_type("i32")),
                    "i16" => SimulatorValue::Value(types::PekoType::simple_type("i16")),
                    "i128" => SimulatorValue::Value(types::PekoType::simple_type("i128")),
                    "i64" => SimulatorValue::Value(types::PekoType::simple_type("i64")),
                    "f32" => SimulatorValue::Value(types::PekoType::simple_type("f32")),
                    "f64" => SimulatorValue::Value(types::PekoType::simple_type("f64")),
                    "char" => SimulatorValue::Value(types::PekoType::simple_type("char")),
                    "string" => SimulatorValue::Value(types::PekoType::simple_type("string")),
                    _ => SimulatorValue::Value(types::PekoType::simple_type("bool")),
                }
            } else {
                // Otherwise the inferred type must exist for Default
                // to produce a value of that type.
                if !simulator_context.type_exists(&main_expected_type)
                    && !main_expected_type.is_error_type()
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.variable_name.start.clone(),
                            self.variable_name.end.clone(),
                            format!(
                                "cannot create a `Default` value for type `{}` because that type is not defined",
                                main_expected_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    return simulator_context.create_error_value();
                }

                SimulatorValue::Value(main_expected_type)
            };

            simulator_context.record_defined_object(
                &default_value.get_type(),
                false,
                self.variable_name.start.clone(),
            );

            return default_value;
        }

        // --- Normal variable lookup --- //
        // Record the reference for usage tracking before resolving it.
        simulator_context.mark_symbol_used(&self.variable_name.value);

        // Reading a local that was declared without an initializer and never
        // assigned on this path is a use-before-init error.
        if !simulator_context.simulating_assignment_target
            && simulator_context
                .scoped_variables
                .contains_key(&self.variable_name.value)
            && simulator_context
                .uninitialized_variables
                .contains(&self.variable_name.value)
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_name.start.clone(),
                    self.variable_name.end.clone(),
                    format!(
                        "`{}` is used before it is initialized. The binding may not be assigned on every path that reaches this point",
                        self.variable_name.value,
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        // First: scoped (local) variables, then attributes on `this`,
        // then globals and functions in the module hierarchy.
        let variable_reference = if simulator_context
            .scoped_variables
            .contains_key(&self.variable_name.value)
        {
            simulator_context.scoped_variables[&self.variable_name.value].clone()
        } else if simulator_context.current_this.is_some()
            && simulator_context
                .get_class_by_type(
                    &simulator_context
                        .current_this
                        .clone()
                        .unwrap()
                        .variable_type,
                )
                .is_some()
            && simulator_context
                .get_class_by_type(
                    &simulator_context
                        .current_this
                        .clone()
                        .unwrap()
                        .variable_type,
                )
                .unwrap()
                .attributes
                .contains_key(&self.variable_name.value)
        {
            // Resolve as an attribute on `this`.
            let attribute = simulator_context
                .get_object_attribute(
                    &simulator_context
                        .current_this
                        .clone()
                        .unwrap()
                        .variable_value,
                    self.variable_name.value.clone(),
                    false,
                )
                .unwrap();

            simulator_context.previous_was_this = true;

            // Drop one level of reference depth so the value reads
            // as a non-reference attribute.
            let mut value_type = attribute.get_type();
            if value_type.reference_depth > 0 {
                value_type.reference_depth -= 1;
            } else if value_type.array_depth > 0 {
                value_type.array_depth -= 1;
            }

            SimulatorVariable::new(
                PositionData::default(),
                VisibilityData::open_visibility(),
                value_type.clone(),
                SimulatorValue::Value(value_type),
                simulator_context.module_context.current_module().clone(),
            )
        } else {
            // Globals - and if not a variable, try resolving as a
            // function reference (functions are first-class values).
            let find_global = match simulator_context
                .find_global_variable_in_current(&self.variable_name.value)
            {
                Some(global) => global,
                None => {
                    match simulator_context.find_function_in_current(&self.variable_name.value) {
                        Some(global_function) => {
                            let mut function_argument_types = Vec::new();

                            for (_, arg) in &global_function[0].arguments {
                                function_argument_types.push(arg.argument_type.clone());
                            }

                            let function_type = types::PekoType::new(
                                Vec::new(),
                                String::new(),
                                function_argument_types,
                                0,
                                0,
                                0,
                                Some(global_function[0].return_type.clone()),
                                false,
                                PositionData::default(),
                                PositionData::default(),
                            );

                            return SimulatorValue::Value(function_type);
                        }
                        _ => {
                            simulator_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    self.variable_name.start.clone(),
                                    self.variable_name.end.clone(),
                                    format!(
                                        "cannot find symbol `{}` in the current scope. Check the spelling, that the symbol is declared, and that it is imported into this module",
                                        self.variable_name.value,
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    simulator_context.get_current_file(),
                                ),
                            );
                            return simulator_context.create_error_value();
                        }
                    }
                }
            };

            if find_global.variable_visibility.private
                && simulator_context.module_context.accessing_current_module()
            {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        format!(
                            "cannot access private global variable `{}` of module `{}` from outside that module. Mark the variable `pub` or access it from within its declaring module",
                            self.variable_name.value,
                            simulator_context
                                .module_context
                                .current_module()
                                .read()
                                .unwrap()
                                .name
                                .clone(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            find_global
        };

        // A call on `this` itself (not only on an attribute of `this`)
        // propagates `[mutates]` to the enclosing method, exactly as a call on a
        // `this` attribute does (24.2 rule 2). Marking the receiver here lets
        // the object-access path treat `this.method()` as a mutation site.
        if self.variable_name.value == "this" && simulator_context.current_this.is_some() {
            simulator_context.previous_was_this = true;
        }

        // Mark class-typed references as defined objects for IDE tooling. A
        // generic-parameter-typed reference is also recorded so completion can
        // offer the members reachable through the parameter's bounds.
        simulator_context.record_defined_object(
            &variable_reference.variable_type,
            self.variable_name.value == "this",
            self.variable_name.end.clone(),
        );

        // Reference context: return a reference-depth-increased type
        // for mutable contexts, after checking constness.
        if simulator_context.return_references {
            if variable_reference.variable_visibility.constant {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        format!(
                            "cannot mutate constant variable `{}`. Constants cannot be reassigned or used in mutable reference contexts",
                            self.variable_name.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            let mut reference_type = variable_reference.variable_value.get_type();
            reference_type.reference_depth += 1;

            return SimulatorValue::Value(reference_type);
        }

        variable_reference.variable_value
    }
}

/// Simulates a function call.
///
/// This is the most complex AST simulator - [`FunctionCallAST`]
/// represents three distinct surface syntaxes that the parser
/// produces with the same node shape:
///
/// 1. **Identifier calls** - `name(args)` where `name` is a
///    declared function.
/// 2. **Expression calls** - `expr(args)` where `expr` evaluates to
///    a function-typed value (e.g. `closures[0](x)`).
/// 3. **Object constructions** - `ClassName(args)`, identical at
///    parse time to identifier calls. This impl detects the class
///    case and re-delegates to [`ObjectConstructionAST`].
///
/// On top of those three syntaxes, function calls can be generic
/// (with inference from arguments and/or the surrounding expected
/// type), and non-generic functions can be overloaded - which
/// requires running the overload-selection algorithm on every call.
///
/// The flow is:
///
/// 1. Build a `FunctionCall` trace record for IDE signature help.
/// 2. Handle built-in functions (`sizeof`, `Error`) directly.
/// 3. Collect the function identifier and locate the declaring
///    module by walking up the module hierarchy.
/// 4. If the name resolves to a class, delegate to
///    [`ObjectConstructionAST`].
/// 5. Otherwise, infer generic type parameters from the arguments
///    and/or expected type, then resolve the overload.
/// 6. Type-check the arguments against the chosen signature and
///    return the function's declared return type.
impl PekoValueSimulator for FunctionCallAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Build a trace record for IDE signature help. This is
        // populated as the function is resolved and added to the
        // simulator's function_calls list once we know we're not
        // re-delegating to ObjectConstructionAST.
        let function_call_info = Arc::new(RwLock::new(FunctionCall::new(
            self.start.clone(),
            self.end.clone(),
            None,
            String::new(),
            Vec::new(),
            IndexMap::new(),
            PekoType::simple_type("void"),
        )));

        let mut argument_positions = Vec::new();
        for (_, argument) in &self.arguments {
            argument_positions.push((argument.get_start().clone(), argument.get_end().clone()));
        }
        function_call_info.write().unwrap().argument_positions = argument_positions;

        // --- Function identifier collection (step 2 in module doc) --- //
        // Detect built-in pseudo-functions first.
        let function_name = match self.function_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                Some(variable_reference.variable_name.clone())
            }
            _ => None,
        };

        // Record the called function for usage tracking.
        if let Some(name) = &function_name {
            simulator_context.mark_symbol_used(&name.value);
        }

        // Built-in `deserialize<T>(source)`: lowers to
        // `T::deserialize(source as Deserializer)` and yields `T?`.
        if let Some(name) = &function_name
            && name.value == "deserialize"
            && self.function_generics.len() == 1
            && self.arguments.len() == 1
        {
            self.arguments[0].1.simulate(simulator_context);
            let target = self.function_generics[0].clone();
            return SimulatorValue::Value(types::PekoType::option_of(target));
        }

        // --- Built-in functions: sizeof, Error, __rt_peko_alloc, cstring --- //
        if function_name.is_some()
            && (function_name.clone().unwrap().value == "sizeof"
                || function_name.clone().unwrap().value == "Error"
                || function_name.clone().unwrap().value == "__rt_peko_alloc"
                || function_name.clone().unwrap().value == "cstring")
        {
            // Save and reset the function call context. Arguments
            // simulate in the call module (which may differ from the
            // module being called into via module access).
            let previous_function_call = simulator_context.current_function_call.clone();
            let mut topmost_module = simulator_context.module_context.current_module().clone();

            while topmost_module.read().unwrap().parent.is_some() {
                let parent = topmost_module
                    .read()
                    .unwrap()
                    .parent
                    .as_ref()
                    .unwrap()
                    .clone();
                topmost_module = parent;
            }

            if simulator_context.in_main() {
                simulator_context.current_function_call = Some(Arc::clone(&function_call_info));
            }

            // Simulate arguments in the call module's context.
            let mut arguments = Vec::new();
            let mut argument_types = Vec::new();
            let mut keyword_args = HashMap::new();
            let mut keyword_arg_types = HashMap::new();
            let post_stack = simulator_context.module_context.step_back();

            for (argument_name, argument) in &self.arguments {
                let generated_argument = argument.simulate(simulator_context);

                arguments.push(generated_argument.clone());
                argument_types.push(generated_argument.get_type());

                if argument_name.is_some() {
                    keyword_args.insert(
                        argument_name.clone().unwrap().value.clone(),
                        generated_argument.clone(),
                    );
                    keyword_arg_types.insert(
                        argument_name.clone().unwrap().value.clone(),
                        generated_argument.get_type(),
                    );
                }
            }

            simulator_context.module_context.step_forward(post_stack);
            simulator_context.current_function_call = previous_function_call;

            // sizeof<T>() returns int64 - size of T in bytes, used
            // for low-level allocations.
            if function_name.is_some() && function_name.clone().unwrap().value == "sizeof" {
                if self.function_generics.len() != 1 {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`sizeof` requires exactly one type as a generic parameter, e.g. `sizeof<int>()`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                if !simulator_context.type_exists(&self.function_generics[0]) {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.function_generics[0].start_position.clone(),
                            self.function_generics[0].end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                self.function_generics[0],
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                return SimulatorValue::Value(types::PekoType::simple_type("i64"));

            // __rt_peko_alloc<T>(count) allocates `count` elements of
            // type `T` on the gc heap and returns a `Pointer<T>`. It
            // requires exactly one generic type and one `int`-compatible
            // count argument.
            } else if function_name.is_some()
                && function_name.clone().unwrap().value == "__rt_peko_alloc"
            {
                if self.function_generics.len() != 1 {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`__rt_peko_alloc` requires exactly one type as a generic parameter, e.g. `__rt_peko_alloc<int>(count)`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                if !simulator_context.type_exists(&self.function_generics[0]) {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.function_generics[0].start_position.clone(),
                            self.function_generics[0].end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                self.function_generics[0],
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                // Require exactly one count argument that is int-compatible.
                if arguments.len() != 1
                    || !simulator_context.types_similar(
                        &arguments[0].get_type(),
                        &types::PekoType::simple_type("i32"),
                    )
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`__rt_peko_alloc` requires exactly one `int` argument for the element count, e.g. `__rt_peko_alloc<int>(8)`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                // Build the managed pointer type `Pointer<T>`.
                let mut allocated_type = types::PekoType::simple_type("pointer");
                allocated_type
                    .generics_mut()
                    .push(self.function_generics[0].clone());

                return SimulatorValue::Value(allocated_type);

            // cstring("literal") wraps a raw string literal as a `cstr`.
            // For the simulator we only verify that the first argument is
            // present and is a string literal, then return a `cstr` value.
            } else if function_name.is_some() && function_name.clone().unwrap().value == "cstring" {
                let first_argument_is_string_literal = self
                    .arguments
                    .first()
                    .is_some_and(|(_, argument)| matches!(argument, PekoAST::String(_)));

                if !first_argument_is_string_literal {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`cstring` requires a string literal as its first argument, e.g. `cstring(\"text\")`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                return SimulatorValue::Value(types::PekoType::simple_type("cstr"));

            // Error("msg") returns an error optional. Bug fix vs
            // original: the original `!arguments.len() == 1` parsed
            // as a bitwise-NOT and never fired correctly; corrected
            // to `arguments.len() != 1`.
            } else if let Some(function_name_value) = function_name
                && function_name_value.value == "Error"
            {
                if arguments.len() != 1
                    || !simulator_context.types_similar(
                        &arguments[0].get_type(),
                        &types::PekoType::simple_type("string"),
                    )
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`Error` requires exactly one `string` argument as the error message, e.g. `Error(\"failed to parse\")`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return simulator_context.create_error_value();
                }

                if simulator_context.current_expected_type_options.is_none() {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "cannot determine the type of `Error(...)`. `Error` requires a type hint from the surrounding context, such as a variable declaration with a declared `Option<T>` type".to_string(),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return SimulatorValue::Value(types::PekoType::error_type());
                }

                for expected_type in &simulator_context
                    .current_expected_type_options
                    .clone()
                    .unwrap()
                {
                    let expand_option = simulator_context.expand_type(expected_type).unwrap();

                    // The inference type must be an Option with one
                    // generic parameter to hold the error.
                    if expand_option.name() == "Option" && expand_option.generics().len() == 1 {
                        simulator_context.defined_objects.push(DefinedObject::new(
                            false,
                            expand_option.clone(),
                            self.end.clone(),
                        ));

                        return SimulatorValue::Value(expand_option);
                    }
                }

                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        "cannot create an `Error` value when the inferred type is not `Option<T>`. `Error(...)` is only valid where an `Option` is expected".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));

                return SimulatorValue::Value(types::PekoType::error_type());
            }
        }

        // --- Resolve function reference and locate declaring module --- //
        let function_type: types::PekoType;
        let function_visibility: VisibilityData;
        let function_var_args_type: Option<types::PekoType>;
        let function_argument_types: IndexMap<String, SimulatorArg>;

        let mut function_from_expression = false;
        let mut function_full_name = String::new();
        let mut function_base_name = String::new();

        // Step 2b: pull the function name if it's an identifier
        // reference. Anything else means we're in case 2 (expression
        // call) and the reference will need to be simulated.
        match self.function_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                function_base_name = variable_reference.variable_name.value.clone();
                function_full_name = variable_reference.variable_name.value.clone();
            }
            _ => {
                function_from_expression = true;
            }
        }

        function_call_info.write().unwrap().name = function_full_name.clone();

        // Walk up the module hierarchy looking for the function /
        // class / generic. Also tracks whether the name is actually
        // an in-scope variable (case 2 - the function reference is a
        // variable holding a function value).
        let mut function_module = simulator_context.module_context.current_module().clone();
        let mut no_parent = false;

        if !function_full_name.is_empty() {
            let mut function_is_valid_variable = false;

            while !function_module
                .read()
                .unwrap()
                .functions
                .contains_key(&function_full_name)
                && !function_module
                    .read()
                    .unwrap()
                    .classes
                    .contains_key(&function_full_name)
                && !simulator_context
                    .module_has_function_template(&function_module, &function_base_name)
                && !simulator_context
                    .module_has_class_template(&function_module, &function_base_name)
            {
                // Remember if the name matches a variable so we can
                // later treat this AST as case 2.
                if function_module
                    .read()
                    .unwrap()
                    .variables
                    .contains_key(&function_full_name)
                {
                    function_is_valid_variable = true;
                }

                let parent = function_module.read().unwrap().parent.clone();
                if let Some(p) = parent {
                    function_module = p;
                } else {
                    no_parent = true;
                    break;
                }
            }

            // Couldn't find a function, class, or variable with this
            // name in the module hierarchy.
            if no_parent
                && !function_is_valid_variable
                && !simulator_context
                    .scoped_variables
                    .contains_key(&function_full_name)
            {
                // Last chance: if we're inside a method, try calling
                // the name as a method on `this`.
                if simulator_context.current_this.is_some() {
                    let this_class = simulator_context
                        .get_class_by_type(
                            &simulator_context
                                .current_this
                                .clone()
                                .unwrap()
                                .variable_type,
                        )
                        .unwrap();

                    if this_class
                        .main_virtual_table
                        .methods
                        .contains_key(&function_full_name)
                    {
                        let method_options: Vec<_> = this_class.main_virtual_table.methods
                            [&function_full_name]
                            .iter()
                            .map(|function| function.read().unwrap().clone())
                            .collect();
                        let mut argument_type_options = vec![Vec::new(); self.arguments.len()];

                        for method_option in method_options {
                            // Only add type options for the correct number of arguments.
                            if method_option.arguments.len() != self.arguments.len()
                                || (self.arguments.len() > method_option.arguments.len()
                                    && method_option.var_args_type.is_none())
                            {
                                continue;
                            }

                            for (idx, (_, argument)) in method_option.arguments.iter().enumerate() {
                                argument_type_options[idx].push(argument.argument_type.clone());
                            }

                            if self.arguments.len() > method_option.arguments.len()
                                && method_option.var_args_type.is_some()
                            {
                                for arg in argument_type_options
                                    .iter_mut()
                                    .take(self.arguments.len())
                                    .skip(method_option.arguments.len())
                                {
                                    arg.push(method_option.var_args_type.clone().unwrap());
                                }
                            }
                        }

                        let previous_function_call =
                            simulator_context.current_function_call.clone();
                        let mut topmost_module =
                            simulator_context.module_context.current_module().clone();

                        while topmost_module.read().unwrap().parent.is_some() {
                            let parent = topmost_module
                                .read()
                                .unwrap()
                                .parent
                                .as_ref()
                                .unwrap()
                                .clone();
                            topmost_module = parent;
                        }

                        if simulator_context.in_main() {
                            simulator_context.current_function_call =
                                Some(Arc::clone(&function_call_info));
                        }

                        let mut arguments = Vec::new();
                        let mut argument_types = Vec::new();
                        let mut keyword_args = HashMap::new();
                        let mut keyword_arg_types = HashMap::new();

                        let post_stack = simulator_context.module_context.step_back();
                        for ((argument_name, argument), expected_type_options) in
                            self.arguments.iter().zip(&argument_type_options)
                        {
                            let current_expected_types =
                                simulator_context.current_expected_type_options.clone();

                            simulator_context.current_expected_type_options =
                                Some(expected_type_options.clone());

                            let generated_argument = argument.simulate(simulator_context);

                            arguments.push(generated_argument.clone());
                            argument_types.push(generated_argument.get_type());

                            simulator_context.current_expected_type_options =
                                current_expected_types;

                            if argument_name.is_some() {
                                keyword_args.insert(
                                    argument_name.clone().unwrap().value.clone(),
                                    generated_argument.clone(),
                                );
                                keyword_arg_types.insert(
                                    argument_name.clone().unwrap().value.clone(),
                                    generated_argument.get_type(),
                                );
                            }
                        }
                        simulator_context.module_context.step_forward(post_stack);

                        simulator_context.current_function_call = previous_function_call;

                        let call_on_this = simulator_context.call_object_method(
                            &simulator_context
                                .current_this
                                .as_ref()
                                .unwrap()
                                .variable_value
                                .clone(),
                            function_full_name.clone(),
                            arguments.clone(),
                            if !keyword_args.is_empty() {
                                Some(keyword_args.clone())
                            } else {
                                None
                            },
                        );

                        if let Ok(call_result) = call_on_this {
                            // A bare method call resolves to a call on `this`, so
                            // it propagates `[mutates]` exactly like an explicit
                            // `this.method()` call (24.2 rule 2). The direct check
                            // handles an already-known mutating callee; the
                            // recorded edge lets the fixpoint handle a callee
                            // whose `[mutates]` is inferred only later.
                            if simulator_context.last_called_method_mutates {
                                simulator_context.current_method_mutates = true;
                            }
                            if let Some(this_variable) = &simulator_context.current_this
                                && let Some(caller_method) =
                                    simulator_context.current_method_name.clone()
                            {
                                let this_class =
                                    this_variable.variable_type.name().to_string();
                                simulator_context.mutates_call_edges.push((
                                    this_class.clone(),
                                    caller_method,
                                    this_class,
                                    function_full_name.clone(),
                                ));
                            }
                            return call_result;
                        }
                    }

                    // If the name is an attribute on `this`, treat
                    // it as case 2 (the attribute is a callable
                    // value).
                    let object_class = simulator_context.get_class_by_type(
                        &simulator_context
                            .current_this
                            .as_ref()
                            .unwrap()
                            .variable_value
                            .get_type(),
                    );
                    if object_class.is_some()
                        && object_class
                            .unwrap()
                            .attributes
                            .contains_key(&function_full_name)
                    {
                        function_from_expression = true;
                    }
                }

                if !function_from_expression {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.function_reference.get_start().clone(),
                            self.function_reference.get_end().clone(),
                            format!(
                                "cannot find function `{function_full_name}`. Check the function name, that the function is declared, and that it is imported into this module"
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    return simulator_context.create_error_value();
                }
            }

            // If the name resolves to a variable, mark as expression-call.
            function_from_expression = function_from_expression
                || (function_is_valid_variable
                    || simulator_context
                        .scoped_variables
                        .contains_key(&function_full_name));
        }

        // --- Generic type inference --- //
        let mut function_generics = self.function_generics.clone();

        // If this names a generic function/class with no explicit type
        // parameters, attempt to infer them: first from the argument
        // types, then from the expected return/binding type.
        if !no_parent
            && (simulator_context
                .module_has_function_template(&function_module, &function_base_name)
                || simulator_context
                    .module_has_class_template(&function_module, &function_base_name))
            && self.function_generics.is_empty()
        {
            let previous_function_call = simulator_context.current_function_call.clone();
            let mut topmost_module = simulator_context.module_context.current_module().clone();

            while topmost_module.read().unwrap().parent.is_some() {
                let parent = topmost_module
                    .read()
                    .unwrap()
                    .parent
                    .as_ref()
                    .unwrap()
                    .clone();
                topmost_module = parent;
            }

            if simulator_context.in_main() {
                simulator_context.current_function_call = Some(Arc::clone(&function_call_info));
            }

            // Simulate arguments in the call module's context.
            let mut arguments = Vec::new();
            let mut argument_types = Vec::new();
            let mut keyword_args = HashMap::new();
            let mut keyword_arg_types = HashMap::new();

            let post_stack = simulator_context.module_context.step_back();
            for (argument_name, argument) in &self.arguments {
                let generated_argument = argument.simulate(simulator_context);

                arguments.push(generated_argument.clone());
                argument_types.push(generated_argument.get_type());

                if argument_name.is_some() {
                    keyword_args.insert(
                        argument_name.clone().unwrap().value.clone(),
                        generated_argument.clone(),
                    );
                    keyword_arg_types.insert(
                        argument_name.clone().unwrap().value.clone(),
                        generated_argument.get_type(),
                    );
                }
            }
            simulator_context.module_context.step_forward(post_stack);

            simulator_context.current_function_call = previous_function_call;

            // Collect the generic type-parameter names from the
            // declaration.
            let is_function_template = simulator_context
                .module_has_function_template(&function_module, &function_base_name);
            let needed_generics = if is_function_template {
                simulator_context
                    .module_function_template(&function_module, &function_base_name)
                    .unwrap()
                    .generic_typenames
                    .iter()
                    .map(|arg_type| arg_type.value.clone())
                    .collect::<Vec<String>>()
            } else {
                function_module.read().unwrap().classes[&function_base_name]
                    .read()
                    .unwrap()
                    .generic_typenames
                    .iter()
                    .map(|arg_type| arg_type.value.clone())
                    .collect::<Vec<String>>()
            };

            // Pull the declared argument-type names from either the
            // function's parameter list or the matching constructor.
            let argument_declaration_types: Vec<String> = if is_function_template {
                let generic = simulator_context
                    .module_function_template(&function_module, &function_base_name)
                    .unwrap();
                let source = generic.source_function.clone().unwrap();
                source
                    .arguments
                    .iter()
                    .map(|(_, argument_declaration_info)| {
                        argument_declaration_info.argument_type.to_string()
                    })
                    .collect()
            } else {
                let generic = function_module.read().unwrap().classes[&function_base_name].clone();
                let generic = generic.read().unwrap();
                let source = generic.source_class.clone().unwrap();
                let find_matching_constructor = source.methods.iter().find(|method| match method {
                    ClassMethod::Constructor(constructor_info, _) => {
                        constructor_info.arguments.len() == argument_types.len()
                    }
                    _ => false,
                });

                if find_matching_constructor.is_none() {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "no constructor of generic class `{function_full_name}` accepts `{}` arguments. Check the argument count against the class's declared constructors",
                                argument_types.len(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    return simulator_context.create_error_value();
                }

                match find_matching_constructor.unwrap() {
                    ClassMethod::Constructor(constructor_info, _) => constructor_info
                        .arguments
                        .iter()
                        .map(|(_, argument_declaration_info)| {
                            argument_declaration_info.argument_type.to_string()
                        })
                        .collect(),
                    _ => panic!("matching constructor must be a Constructor variant"),
                }
            };

            let mut needed_generics_count = needed_generics.len();

            // Map of generic typename -> inferred concrete type.
            let mut collected_generic_types = IndexMap::new();

            // Walk the provided argument types against the declared
            // parameter types, collecting any generic-type matches.
            argument_types
                .iter()
                .zip(argument_declaration_types.iter())
                .for_each(|(provided_argument_type, generic_typename)| {
                    if !needed_generics.contains(generic_typename)
                        || collected_generic_types.contains_key(generic_typename)
                    {
                        return;
                    }

                    collected_generic_types
                        .insert(generic_typename.clone(), provided_argument_type.clone());
                    needed_generics_count -= 1;
                });

            // Fallback: try to fill remaining generics from the
            // expected type (e.g. `var x: Array<int> = Array()` -
            // the `int` is inferred from the binding's type).
            let find_expected_type = if let Some(current_expected_types) =
                &simulator_context.current_expected_type_options
                && needed_generics_count > 0
            {
                current_expected_types.iter().find(|expected| {
                    expected.name() == function_base_name
                        && expected.generics().len() == needed_generics_count
                })
            } else {
                None
            };

            if let Some(find_expected_type) = find_expected_type {
                function_generics = find_expected_type.generics().to_vec();
            } else if needed_generics_count == 0 {
                needed_generics.iter().for_each(|generic_typename| {
                    function_generics.push(collected_generic_types[generic_typename].clone());
                });
            } else {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "could not infer type parameters for generic `{function_full_name}`. Provide explicit type parameters with `{function_full_name}<T, U, ...>`"
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }
        }

        // Object construction is its own AST node, produced by the parser from
        // `new Class(args)`. A bare `Class(args)` that names a class is the
        // missing-`new` mistake, reported here so it does not fall through to
        // function-call resolution.
        if !no_parent
            && (simulator_context.module_has_class_template(&function_module, &function_base_name)
                || function_module
                    .read()
                    .unwrap()
                    .classes
                    .contains_key(&function_full_name))
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "`{function_base_name}` is a class. Construct it with `new {function_base_name}(...)`"
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // --- Argument simulation in call module's context --- //
        let previous_function_call = simulator_context.current_function_call.clone();
        let mut topmost_module = simulator_context.module_context.current_module().clone();

        while topmost_module.read().unwrap().parent.is_some() {
            let parent = topmost_module
                .read()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .clone();
            topmost_module = parent;
        }

        if simulator_context.in_main() {
            simulator_context.current_function_call = Some(Arc::clone(&function_call_info));
        }

        let mut arguments = Vec::new();
        let mut argument_types = Vec::new();
        let mut keyword_args = HashMap::new();
        let mut keyword_arg_types = HashMap::new();

        let post_stack = simulator_context.module_context.step_back();
        for (argument_name, argument) in &self.arguments {
            let generated_argument = argument.simulate(simulator_context);

            arguments.push(generated_argument.clone());
            argument_types.push(generated_argument.get_type());

            if argument_name.is_some() {
                keyword_args.insert(
                    argument_name.clone().unwrap().value.clone(),
                    generated_argument.clone(),
                );
                keyword_arg_types.insert(
                    argument_name.clone().unwrap().value.clone(),
                    generated_argument.get_type(),
                );
            }
        }
        simulator_context.module_context.step_forward(post_stack);

        simulator_context.current_function_call = previous_function_call;

        // Record this call into the trace (either as a sub-call or
        // as a top-level call).
        let mut topmost_module = simulator_context.module_context.current_module().clone();

        while topmost_module.read().unwrap().parent.is_some() {
            let parent = topmost_module
                .read()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .clone();
            topmost_module = parent;
        }

        let current_fcall = simulator_context.current_function_call.is_some();

        if simulator_context.in_main() {
            if current_fcall {
                simulator_context
                    .current_function_call
                    .as_mut()
                    .unwrap()
                    .write()
                    .unwrap()
                    .subcalls
                    .push(Arc::clone(&function_call_info));
            } else {
                simulator_context
                    .function_calls
                    .push(Arc::clone(&function_call_info));
            }
        }

        // Append `<T, U, ...>` to function_full_name for generic
        // overload lookup.
        if !function_generics.is_empty() {
            function_full_name.push('<');
            for generic in &function_generics {
                let expand_generic = simulator_context.expand_type(generic);

                let expand_generic = if let Some(g) = expand_generic {
                    g
                } else {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            generic.start_position.clone(),
                            generic.end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                generic,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    types::PekoType::error_type()
                };

                function_full_name.push_str(expand_generic.to_string().as_str());
                function_full_name.push(',');
            }
            function_full_name.pop();
            function_full_name.push('>');
        }

        // --- Step 2a: expression call --- //
        if function_from_expression {
            let function_from_expression =
                self.function_reference.as_ref().simulate(simulator_context);

            if !function_from_expression.get_type().is_function() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.function_reference.get_start().clone(),
                        self.function_reference.get_end().clone(),
                        "value is not callable. The expression's type is not a function or closure type, so it cannot be called".to_string(),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }

            function_call_info.write().unwrap().return_type =
                if !function_from_expression.get_type().is_function() {
                    PekoType::simple_type("void")
                } else {
                    function_from_expression
                        .get_type()
                        .function_return()
                        .unwrap()
                        .clone()
                };

            for argument_type in function_from_expression.get_type().generics() {
                function_call_info
                    .write()
                    .unwrap()
                    .signature_arguments
                    .insert(String::new(), argument_type.clone());
            }

            function_type = function_from_expression.get_type();
            function_visibility = VisibilityData::open_visibility();
            function_var_args_type = None;
            function_argument_types = IndexMap::new();
        } else {
            // --- Step 2b: identifier call - overload resolution --- //

            // Lazily instantiate a generic function if needed.
            if simulator_context.module_has_function_template(&function_module, &function_base_name)
                && !function_module
                    .read()
                    .unwrap()
                    .functions
                    .contains_key(&function_full_name)
            {
                let function_reference = simulator_context
                    .module_function_template(&function_module, &function_base_name)
                    .unwrap();
                let generated_function = simulator_context
                    .create_generic_function(&function_reference, function_generics);

                if generated_function.is_none() {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "could not instantiate generic `{function_full_name}` with the provided type parameters. Check that the type parameters match the generic's declared bounds"
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    return simulator_context.create_error_value();
                }
            }

            let function_choices: Vec<_> = function_module.read().unwrap().functions
                [&function_full_name]
                .iter()
                .map(|function| function.read().unwrap().clone())
                .collect();

            let post_stack = simulator_context.module_context.step_back();
            let function_choice = simulator_context.choose_function(
                function_choices.clone(),
                argument_types.clone(),
                if !keyword_arg_types.is_empty() {
                    Some(keyword_arg_types.clone())
                } else {
                    None
                },
                false,
            );
            simulator_context.module_context.step_forward(post_stack);

            if function_choice.is_none() {
                // Diagnostic recovery: surface the best partial match
                // so the IDE can still show a signature.
                let best_signature_choice = simulator_context.choose_most_similar_function(
                    function_choices,
                    argument_types.clone(),
                    if !keyword_arg_types.is_empty() {
                        Some(keyword_arg_types.clone())
                    } else {
                        None
                    },
                    false,
                );

                if best_signature_choice.is_none() {
                    return simulator_context.create_error_value();
                }

                let best_signature_choice = best_signature_choice.unwrap();

                function_call_info.write().unwrap().return_type =
                    best_signature_choice.return_type.clone();
                for (argument_name, argument_info) in &best_signature_choice.arguments {
                    function_call_info
                        .write()
                        .unwrap()
                        .signature_arguments
                        .insert(argument_name.clone(), argument_info.argument_type.clone());
                }
                function_call_info.write().unwrap().docinfo = best_signature_choice.docinfo.clone();

                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "no overload of `{function_full_name}` matches the supplied argument types. Check the argument types against the declared overloads"
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }

            let function_choice = function_choice.unwrap();

            // Reject illegal accesses to private functions.
            if function_choice.visibility.private
                && simulator_context.module_context.accessing_current_module()
            {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.function_reference.get_start().clone(),
                        self.function_reference.get_end().clone(),
                        format!(
                            "cannot access private function `{function_full_name}` from outside its declaring module"
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }

            function_call_info.write().unwrap().return_type = function_choice.return_type.clone();
            for (argument_name, argument_info) in &function_choice.arguments {
                function_call_info
                    .write()
                    .unwrap()
                    .signature_arguments
                    .insert(argument_name.clone(), argument_info.argument_type.clone());
            }
            function_call_info.write().unwrap().docinfo = function_choice.docinfo.clone();

            function_type = {
                let mut new_type = types::PekoType::simple_type("");
                new_type.set_function_return(Some(function_choice.return_type.clone()));

                for (_, arg) in &function_choice.arguments {
                    new_type.generics_mut().push(arg.argument_type.clone());
                }

                new_type
            };

            function_visibility = function_choice.visibility.clone();
            function_var_args_type = function_choice.var_args_type.clone();
            function_argument_types = function_choice.arguments.clone();
        }

        // --- Step 3: type-check arguments and produce return value --- //
        let post_stack = simulator_context.module_context.step_back();

        // Keyword-call mode: only allowed when every parameter has a
        // default value. Otherwise we fall through to positional
        // type-checking below.
        let mut all_args_keywords = !function_argument_types.is_empty();
        for (_, argument) in &function_argument_types {
            if !argument.default_value {
                all_args_keywords = false;
                break;
            }
        }

        let return_type = function_type.function_return().unwrap().clone();

        // Variadic-argument type-check: if extras were passed, they
        // all need to fit into an Array<varargs_type>.
        if let Some(function_var_args) = &function_var_args_type
            && function_type.generics().len() - 1 < arguments.len()
        {
            let mut variable_arguments = Vec::new();
            for arg in arguments.iter().skip(function_type.generics().len() - 1) {
                variable_arguments.push(arg.clone());
            }

            let create_array =
                simulator_context.create_standard_array(function_var_args, variable_arguments);

            if create_array.is_none() {
                let index = function_type.generics().len() - 1;

                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.arguments[index].1.get_start().clone(),
                        self.arguments.last().unwrap().1.get_end().clone(),
                        format!(
                            "variadic arguments have incorrect types. All variadic arguments must have type `{}`",
                            function_var_args,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        // Record the return type as a defined-object site for IDE tooling. A
        // function returning a generic parameter records through its carrier.
        simulator_context.record_defined_object(&return_type, false, self.end.clone());

        // Keyword-arguments call: type-check each parameter by name.
        if all_args_keywords
            && (!keyword_args.is_empty()
                || (arguments.len() != function_argument_types.len() && arguments.is_empty()))
        {
            for (index, (argument_name, arg)) in function_argument_types.iter().enumerate() {
                let argument_value = if keyword_args.contains_key(argument_name) {
                    keyword_args[argument_name].clone()
                } else {
                    SimulatorValue::Value(arg.argument_type.clone())
                };

                if !simulator_context.types_similar(&argument_value.get_type(), &arg.argument_type)
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.arguments[index].1.get_start().clone(),
                            self.arguments[index].1.get_end().clone(),
                            format!(
                                "argument of type `{}` does not match expected type `{}`",
                                argument_value.get_type(),
                                arg.argument_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                }
            }

            return SimulatorValue::Value(return_type);
        }

        // Positional-arguments call: zip arguments against the
        // function's declared types and type-check each pair.
        for (index, (argument, argument_type)) in arguments
            .iter()
            .zip(function_type.generics().iter())
            .enumerate()
        {
            if function_var_args_type.is_some() && index == function_type.generics().len() - 1 {
                break;
            }

            if !simulator_context.types_similar(&argument.get_type(), argument_type) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.arguments[index].1.get_start().clone(),
                        self.arguments[index].1.get_end().clone(),
                        format!(
                            "argument of type `{}` does not match expected type `{}`",
                            argument.get_type(),
                            argument_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        simulator_context.module_context.step_forward(post_stack);

        if function_visibility.private
            && simulator_context.module_context.accessing_current_module()
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.function_reference.get_start().clone(),
                    self.function_reference.get_end().clone(),
                    format!(
                        "cannot call private function `{function_full_name}` from outside its declaring module"
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
        }

        SimulatorValue::Value(return_type)
    }
}

/// Simulates a module access `mod::sub::expr`.
///
/// Walks the module chain (starting from either the current module's
/// children or the top-level imported modules), reporting "module not
/// found" or "module is private" diagnostics as appropriate. Once the
/// terminal module is reached, the accessor expression is simulated
/// under that module's context.
impl PekoValueSimulator for ModuleAccessAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Enum variant access: `Enum::Variant`. The head names an enum (not a
        // module) and the accessor is a bare variant name. Resolves to a value
        // of the enum type.
        if self.module_names.len() == 1
            && let Some(variants) = simulator_context.get_enum_variants(&self.module_names[0].value)
            && let PekoAST::VariableReference(variant_reference) = self.accessor.as_ref()
        {
            let enum_name = &self.module_names[0].value;
            let variant_name = &variant_reference.variable_name.value;

            if !variants.contains(variant_name) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        variant_reference.variable_name.start.clone(),
                        variant_reference.variable_name.end.clone(),
                        format!(
                            "enum `{enum_name}` has no variant `{variant_name}`. Check the variant name and that it is declared in the enum",
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }

            return SimulatorValue::Value(types::PekoType::simple_type(enum_name));
        }

        // Enum deserialize: `Enum::deserialize(d)` routes to the enum's
        // deserialize helper. Type-check the arguments and yield `Enum?`.
        if self.module_names.len() == 1
            && simulator_context
                .get_enum_variants(&self.module_names[0].value)
                .is_some()
            && let PekoAST::FunctionCall(call) = self.accessor.as_ref()
            && let PekoAST::VariableReference(method_reference) = call.function_reference.as_ref()
            && method_reference.variable_name.value == "deserialize"
        {
            for (_, argument) in &call.arguments {
                argument.simulate(simulator_context);
            }
            return SimulatorValue::Value(types::PekoType::option_of(
                types::PekoType::simple_type(&self.module_names[0].value),
            ));
        }

        // Static method access: `Type::method(args)`. The head names a class
        // (not a module) that has a `static` method, and the accessor is a call.
        // A static method takes no receiver, so it resolves at the type level to
        // the concrete class named here. Its `Self` return type was already
        // bound to that class when the method was registered.
        if self.module_names.len() == 1
            && let PekoAST::FunctionCall(call) = self.accessor.as_ref()
            && let PekoAST::VariableReference(method_reference) = call.function_reference.as_ref()
        {
            let type_name = self.module_names[0].value.clone();
            let class_type = types::PekoType::simple_type(&type_name);

            if let Some(class) = simulator_context.get_class_by_type(&class_type) {
                let method_name = method_reference.variable_name.value.clone();

                if let Some(overloads) = class.main_virtual_table.methods.get(&method_name) {
                    let static_overloads: Vec<_> = overloads
                        .iter()
                        .map(|function| function.read().unwrap().clone())
                        .filter(|function| function.is_static)
                        .collect();

                    if !static_overloads.is_empty() {
                        let mut argument_types = Vec::new();
                        for (_, argument) in &call.arguments {
                            argument_types.push(argument.simulate(simulator_context).get_type());
                        }

                        let chosen = simulator_context.choose_function(
                            static_overloads,
                            argument_types,
                            None,
                            true,
                        );

                        return match chosen {
                            Some(method) => SimulatorValue::Value(method.return_type.clone()),
                            None => {
                                simulator_context.diagnostics.report_diagnostic(
                                    diagnostics::PekoDiagnostic::new(
                                        method_reference.variable_name.start.clone(),
                                        method_reference.variable_name.end.clone(),
                                        format!(
                                            "no overload of static method `{method_name}` on type `{type_name}` matches the supplied argument types",
                                        ),
                                        diagnostics::DiagnosticType::Error,
                                        simulator_context.get_current_file(),
                                    ),
                                );
                                simulator_context.create_error_value()
                            }
                        };
                    }

                    // The method exists but has no static overload: an instance
                    // method cannot be called at the type level.
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            method_reference.variable_name.start.clone(),
                            method_reference.variable_name.end.clone(),
                            format!(
                                "method `{method_name}` on type `{type_name}` is not static, so it cannot be called as `{type_name}::{method_name}(...)`. Call it on an instance instead",
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    return simulator_context.create_error_value();
                }
            }
        }

        // Resolve the first module in the chain - try the importing module's
        // own aliases first (so a local import name wins over a global one),
        // then its child modules, then top-level imports.
        let next_module = if simulator_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .module_aliases
            .contains_key(&self.module_names[0].value)
        {
            Some(
                simulator_context
                    .module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .module_aliases[&self.module_names[0].value]
                    .clone(),
            )
        } else if simulator_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .modules
            .contains_key(&self.module_names[0].value)
        {
            Some(
                simulator_context
                    .module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .modules[&self.module_names[0].value]
                    .clone(),
            )
        } else if simulator_context
            .module_context
            .top_level_modules
            .contains_key(&self.module_names[0].value)
            && (simulator_context.module_context.top_level_modules[&self.module_names[0].value]
                .read()
                .unwrap()
                .is_imported_by(simulator_context.module_context.current_module().clone())
                || self.module_names[0].value == "extern")
        {
            Some(Arc::clone(
                &simulator_context.module_context.top_level_modules[&self.module_names[0].value],
            ))
        } else {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.module_names[0].start.clone(),
                    self.module_names[0].end.clone(),
                    format!(
                        "cannot find module `{}` in the current scope. Check the module name, that the module is declared, and that it is imported",
                        &self.module_names[0].value,
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            None
        };

        let Some(mut next_module) = next_module else {
            return simulator_context.create_error_value();
        };

        // Walk the remaining names in the chain. A name matching the
        // current module's own name terminates the walk early.
        for i in 1..self.module_names.len() {
            if !next_module
                .read()
                .unwrap()
                .modules
                .contains_key(&self.module_names[i].value)
            {
                if next_module.read().unwrap().name == self.module_names[i].value {
                    break;
                }

                // The final segment may name an enum in this module rather
                // than a submodule, so `module::Enum::Variant` resolves here.
                if i == self.module_names.len() - 1
                    && let PekoAST::VariableReference(variant_reference) = self.accessor.as_ref()
                    && let Some(definition) = next_module
                        .read()
                        .unwrap()
                        .enums
                        .get(&self.module_names[i].value)
                        .cloned()
                {
                    let enum_name = &self.module_names[i].value;
                    let variant_name = &variant_reference.variable_name.value;

                    // A qualified path reaches this enum from another module,
                    // so a `[private]` enum is out of bounds.
                    if definition.private {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.module_names[i].start.clone(),
                                variant_reference.variable_name.end.clone(),
                                format!(
                                    "cannot access private enum `{enum_name}` from outside its module. Remove the `[private]` modifier to export it",
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }

                    if !definition.variants.contains(variant_name) {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variant_reference.variable_name.start.clone(),
                                variant_reference.variable_name.end.clone(),
                                format!(
                                    "enum `{enum_name}` has no variant `{variant_name}`. Check the variant name and that it is declared in the enum",
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }
                    // Qualify the value's type with the module path to the enum
                    // (`module::Enum`). A bare enum name does not resolve in the
                    // importer, so the qualification lets it reconcile with the
                    // bare parameter type declared in the enum's own module.
                    let mut enum_type = types::PekoType::simple_type(enum_name);
                    *enum_type.module_names_mut() = self.module_names[..i]
                        .iter()
                        .map(|module| module.value.clone())
                        .collect();
                    return SimulatorValue::Value(enum_type);
                }

                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.module_names[i].start.clone(),
                        self.module_names[i].end.clone(),
                        format!(
                            "cannot find module `{}` in the current scope. Check the module name, that the module is declared, and that it is imported",
                            &self.module_names[i].value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));

                return simulator_context.create_error_value();
            }

            let next = next_module.read().unwrap().modules[&self.module_names[i].value].clone();
            next_module = next;

            // Private modules still resolve (so the rest of the
            // expression can simulate), but the access is reported
            // as a diagnostic.
            if next_module.as_ref().read().unwrap().visibility.private {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.module_names[i].start.clone(),
                        self.module_names[i].end.clone(),
                        format!(
                            "cannot access private module `{}` from outside its parent module",
                            &self.module_names[i].value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        }

        // Switch to the resolved module's context, simulate the
        // accessor expression, then switch back.
        simulator_context
            .module_context
            .move_to_module(next_module, true, false);

        let accessor = self.accessor.as_ref().simulate(simulator_context);

        simulator_context.module_context.move_out_of_module();

        accessor
    }
}

/// Simulates an object construction `ClassName(args)` (or
/// `ClassName<T>(args)`).
///
/// Resolves the class, simulates each argument under the union of
/// the constructor overloads' parameter types (so each argument has
/// the maximum possible inference context), then resolves the
/// constructor overload and type-checks. Falls back to attribute-
/// list-based construction for classes without declared
/// constructors.
impl PekoValueSimulator for ObjectConstructionAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // Generic-argument inference: when `new Class()` omits its type
        // arguments, recover them from a matching expected type at the use
        // site, so `let x: Array<number> = new Array()` constructs an
        // `Array<number>`.
        let mut object_generics = self.object_generics.clone();
        if object_generics.is_empty()
            && let Some(options) = &simulator_context.current_expected_type_options
        {
            for expected in options {
                if expected.name() == self.class_name.value && !expected.generics().is_empty() {
                    object_generics = expected.generics().to_vec();
                    break;
                }
            }
        }

        let mut class_name_to_type =
            types::PekoType::from_string(self.class_name.value.as_str(), "");
        class_name_to_type
            .generics_mut()
            .extend(object_generics.clone());
        let class_to_create = simulator_context.get_class_by_type(&class_name_to_type);

        let Some(mut class_to_create) = class_to_create else {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "cannot find class `{}`. Check the class name, that the class is declared, and that it is imported",
                        class_name_to_type,
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        };

        // Backward inference: still no type arguments but the class is
        // generic. Bind a fresh inference variable (`?N`) per missing
        // argument so the instance is usable now; a later constraining call
        // resolves them and back-patches the variable's type.
        if object_generics.is_empty() && !class_to_create.generic_typenames.is_empty() {
            for _ in 0..class_to_create.generic_typenames.len() {
                let name = format!("?{}", simulator_context.inference_counter);
                simulator_context.inference_counter += 1;
                object_generics.push(types::PekoType::generic_type(name, Vec::new()));
            }
            class_name_to_type.generics_mut().clear();
            class_name_to_type
                .generics_mut()
                .extend(object_generics.clone());
            if let Some(reinstantiated) = simulator_context.get_class_by_type(&class_name_to_type) {
                class_to_create = reinstantiated;
            }
        }

        // Build a trace record for IDE signature help.
        let function_call_info = Arc::new(RwLock::new(FunctionCall::new(
            self.start.clone(),
            self.end.clone(),
            None,
            class_name_to_type.to_string(),
            Vec::new(),
            IndexMap::new(),
            PekoType::simple_type("void"),
        )));

        let previous_function_call = simulator_context.current_function_call.clone();
        let mut topmost_module = simulator_context.module_context.current_module().clone();

        while topmost_module.read().unwrap().parent.is_some() {
            let parent = topmost_module
                .read()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .clone();
            topmost_module = parent;
        }

        let current_fcall = simulator_context.current_function_call.is_some();

        if simulator_context.in_main() {
            if current_fcall {
                simulator_context
                    .current_function_call
                    .as_mut()
                    .unwrap()
                    .write()
                    .unwrap()
                    .subcalls
                    .push(Arc::clone(&function_call_info));
            } else {
                simulator_context
                    .function_calls
                    .push(Arc::clone(&function_call_info));
            }

            simulator_context.current_function_call = Some(Arc::clone(&function_call_info));
        }

        // Build a per-argument list of candidate types from every
        // constructor overload (used as expected-type options for
        // argument simulation, enabling better inference for nested
        // literals like `Array()`).
        let method_options: Vec<_> = class_to_create
            .main_virtual_table
            .methods
            .get("constructor")
            .map(|overloads| {
                overloads
                    .iter()
                    .map(|function| function.read().unwrap().clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut argument_type_options = vec![Vec::new(); self.arguments.len()];

        for method_option in method_options {
            // Only add type options for the correct number of arguments.
            if method_option.arguments.len() != self.arguments.len()
                || (self.arguments.len() > method_option.arguments.len()
                    && method_option.var_args_type.is_none())
            {
                continue;
            }

            for (idx, (_, argument)) in method_option.arguments.iter().enumerate() {
                argument_type_options[idx].push(argument.argument_type.clone());
            }

            if self.arguments.len() > method_option.arguments.len()
                && method_option.var_args_type.is_some()
            {
                for type_option in argument_type_options
                    .iter_mut()
                    .take(self.arguments.len())
                    .skip(method_option.arguments.len())
                {
                    type_option.push(method_option.var_args_type.clone().unwrap());
                }
            }
        }

        // Simulate the arguments under the union of overload types.
        let mut argument_types = Vec::new();
        let mut keyword_types = HashMap::new();

        let post_stack = simulator_context.module_context.step_back();
        for ((argument_name, argument), expected_type_options) in
            self.arguments.iter().zip(&argument_type_options)
        {
            function_call_info
                .write()
                .unwrap()
                .argument_positions
                .push((argument.get_start().clone(), argument.get_end().clone()));

            let current_expected_types = simulator_context.current_expected_type_options.clone();
            simulator_context.current_expected_type_options = Some(expected_type_options.clone());

            let generated_argument = argument.simulate(simulator_context);
            argument_types.push(generated_argument.get_type());

            simulator_context.current_expected_type_options = current_expected_types;

            if argument_name.is_some() {
                keyword_types.insert(
                    argument_name.clone().unwrap().value,
                    generated_argument.get_type(),
                );
            }
        }
        simulator_context.module_context.step_forward(post_stack);

        simulator_context.current_function_call = previous_function_call;

        if class_to_create
            .main_virtual_table
            .methods
            .contains_key("constructor")
        {
            // Try the strict overload selector first.
            let post_stack = simulator_context.module_context.step_back();
            let constructor_choice = simulator_context.choose_function(
                class_to_create.main_virtual_table.methods["constructor"]
                    .iter()
                    .map(|function| function.read().unwrap().clone())
                    .collect(),
                argument_types.clone(),
                if keyword_types.is_empty() {
                    None
                } else {
                    Some(keyword_types.clone())
                },
                true,
            );
            simulator_context.module_context.step_forward(post_stack);

            if let Some(constructor_choice_value) = &constructor_choice {
                for (argument_name, argument_info) in &constructor_choice_value.arguments {
                    function_call_info
                        .write()
                        .unwrap()
                        .signature_arguments
                        .insert(argument_name.clone(), argument_info.argument_type.clone());
                }
            } else {
                // Fall back to the diagnostic-recovery selector so
                // the IDE can still surface a signature.
                let best_signature_choice = simulator_context.choose_most_similar_function(
                    class_to_create.main_virtual_table.methods["constructor"]
                        .iter()
                        .map(|function| function.read().unwrap().clone())
                        .collect(),
                    argument_types,
                    if keyword_types.is_empty() {
                        None
                    } else {
                        Some(keyword_types)
                    },
                    true,
                );

                let Some(best_signature_choice) = best_signature_choice else {
                    return simulator_context.create_error_value();
                };

                for (argument_name, argument_info) in &best_signature_choice.arguments {
                    function_call_info
                        .write()
                        .unwrap()
                        .signature_arguments
                        .insert(argument_name.clone(), argument_info.argument_type.clone());
                }

                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "no constructor of class `{}` matches the supplied argument types. Check the argument types against the class's declared constructors",
                            class_to_create.class_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
        } else {
            // No declared constructor - use the implicit attribute-list
            // constructor. The synthetic vtable slot is not a user attribute,
            // so it takes no constructor argument.
            let constructor_attributes: Vec<_> = class_to_create
                .attributes
                .iter()
                .filter(|(name, _)| name.as_str() != "<main_virtual_table>")
                .collect();

            for (attribute_name, attribute) in &constructor_attributes {
                function_call_info
                    .write()
                    .unwrap()
                    .signature_arguments
                    .insert((*attribute_name).clone(), attribute.attribute_type.clone());
            }

            // The implicit constructor needs exactly as many
            // arguments as there are attributes.
            if argument_types.len() != constructor_attributes.len() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "wrong number of arguments to implicit constructor of class `{}`. The implicit constructor takes one argument per attribute, in declaration order",
                            class_to_create.class_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return SimulatorValue::Value(class_to_create.class_type.clone());
            }

            // Positional implicit-constructor type-check.
            let post_stack = simulator_context.module_context.step_back();
            if keyword_types.is_empty() {
                for (idx, ((_, attribute), attribute_value_type)) in constructor_attributes
                    .iter()
                    .zip(&argument_types)
                    .enumerate()
                {
                    if !simulator_context
                        .types_similar(&attribute.attribute_type, attribute_value_type)
                    {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.arguments[idx].1.get_start().clone(),
                                self.arguments[idx].1.get_end().clone(),
                                format!(
                                    "cannot assign value of type `{}` to attribute of type `{}`. The value's type is not compatible with the attribute's declared type",
                                    attribute_value_type,
                                    attribute.attribute_type,
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                    }
                }
                simulator_context.module_context.step_forward(post_stack);
            } else {
                // Keyword implicit-constructor type-check.
                let post_stack = simulator_context.module_context.step_back();
                for (idx, (attribute_name, attribute)) in constructor_attributes.iter().enumerate()
                {
                    let value_to_set_type = if keyword_types.contains_key(*attribute_name) {
                        &keyword_types[*attribute_name]
                    } else {
                        &attribute.attribute_type
                    };

                    if !simulator_context
                        .types_similar(&attribute.attribute_type, value_to_set_type)
                    {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.arguments[idx].1.get_start().clone(),
                                self.arguments[idx].1.get_end().clone(),
                                format!(
                                    "cannot assign value of type `{}` to attribute of type `{}`. The value's type is not compatible with the attribute's declared type",
                                    value_to_set_type,
                                    attribute.attribute_type,
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                    }
                }
                simulator_context.module_context.step_forward(post_stack);
            }
        }

        simulator_context.defined_objects.push(DefinedObject::new(
            false,
            class_to_create.class_type.clone(),
            self.end.clone(),
        ));

        SimulatorValue::Value(class_to_create.class_type.clone())
    }
}

/// Simulates an object access `object.access`.
///
/// Three cases dispatched on the kind of access:
///
/// * **Method call** (`object.method(args)`) - resolves and calls
///   the method, with special handling for attribute fields that
///   hold function-typed values.
/// * **Attribute read** (`object.attr`) - resolves and returns the
///   attribute's value, with special handling for the `function` and
///   `context` pseudo-attributes on closures.
/// * **Attribute write** (`object.attr = value`) - type-checks the
///   assignment, with support for compound assignment operators.
impl PekoValueSimulator for ObjectAccessAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // The object expression must simulate as a value, not a
        // reference - references are introduced only at the final
        // step of access if needed.
        let return_references = simulator_context.return_references;
        simulator_context.return_references = false;

        let object = self.object.as_ref().simulate(simulator_context);

        simulator_context.return_references = return_references;

        // Whether the receiver is an attribute of `this`. Captured now because
        // the flag is reset by the argument simulation that follows. Used to
        // propagate `[mutates]` from a called method (24.2 rule 2).
        let object_is_this_attribute = simulator_context.previous_was_this;

        // Reassigning an attribute on a `const` object is not allowed (21.2).
        // An attribute set parses as `object.(attr = value)`, so the access
        // node is a reassignment.
        if matches!(self.access.as_ref(), PekoAST::VariableReassignment(_)) {
            let object_type = object.get_type();
            if object_type.is_const() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.object.get_start().clone(),
                        self.access.get_end().clone(),
                        format!(
                            "cannot reassign an attribute on a `const` value of type `{object_type}`. A `const` value is immutable; cast it to a mutable type with `as` first",
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
            // Rule 1 of 24.2: assigning through an attribute of `this` (the
            // explicit `this.attr = value` form) marks the enclosing method
            // `[mutates]`, exactly as the bare `attr = value` form does.
            if object_is_this_attribute {
                simulator_context.current_method_mutates = true;
            }
        }

        match self.access.as_ref() {
            // --- Method invocation --- //
            PekoAST::FunctionCall(function_call) => {
                // The method name must be an identifier (no
                // arbitrary expression here).
                let function_name = match function_call.function_reference.as_ref() {
                    PekoAST::VariableReference(variable_reference) => {
                        variable_reference.variable_name.value.clone()
                    }
                    _ => {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                function_call.function_reference.get_start().clone(),
                                function_call.function_reference.get_end().clone(),
                                "expected an identifier for the method name. Method calls must use the form `object.method(...)`".to_string(),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }
                };

                // `enumValue.serialize(...)` has no method to resolve; it routes
                // to the enum's serialize helper, which yields void.
                if function_name == "serialize"
                    && simulator_context
                        .get_enum_variants(object.get_type().name())
                        .is_some()
                {
                    for (_, argument) in &function_call.arguments {
                        argument.simulate(simulator_context);
                    }
                    return SimulatorValue::Value(types::PekoType::simple_type("void"));
                }

                // A method call on a trait-typed value, or on an erased carrier,
                // resolves against the trait's slots: the result is the slot's
                // return type. A carrier bound by several traits searches each of
                // its bounds, so `KT: impl Hash, impl Equals` resolves methods
                // from both.
                let receiver_type = object.get_type();
                let candidate_traits: Vec<_> =
                    if let Some(definition) = simulator_context.get_trait(receiver_type.name()) {
                        vec![definition]
                    } else if receiver_type.is_generic_param() {
                        receiver_type
                            .restraints()
                            .iter()
                            .filter_map(|restraint| match restraint {
                                types::TypeRestraint::Impl(trait_type) => {
                                    simulator_context.get_trait(trait_type.name())
                                }
                                types::TypeRestraint::From(_) => None,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                if !candidate_traits.is_empty() {
                    for (_, argument) in &function_call.arguments {
                        argument.simulate(simulator_context);
                    }

                    for trait_definition in &candidate_traits {
                        if let Some(slot) = trait_definition
                            .methods
                            .iter()
                            .find(|method| method.name == function_name)
                        {
                            return SimulatorValue::Value(slot.return_type.clone());
                        }
                    }

                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.access.get_start().clone(),
                            self.access.get_end().clone(),
                            format!(
                                "type `{}` has no method `{}` among its bounds. Check the method name against the trait declarations",
                                receiver_type,
                                function_name,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                    return simulator_context.create_error_value();
                }

                let function_call_info = Arc::new(RwLock::new(FunctionCall::new(
                    self.access.get_start().clone(),
                    self.access.get_end().clone(),
                    None,
                    function_name.clone(),
                    Vec::new(),
                    IndexMap::new(),
                    PekoType::simple_type("void"),
                )));

                let previous_function_call = simulator_context.current_function_call.clone();
                let mut topmost_module = simulator_context.module_context.current_module().clone();

                while topmost_module.read().unwrap().parent.is_some() {
                    let parent = topmost_module
                        .read()
                        .unwrap()
                        .parent
                        .as_ref()
                        .unwrap()
                        .clone();
                    topmost_module = parent;
                }

                let current_fcall = simulator_context.current_function_call.is_some();

                if simulator_context.in_main() {
                    if current_fcall {
                        simulator_context
                            .current_function_call
                            .as_mut()
                            .unwrap()
                            .write()
                            .unwrap()
                            .subcalls
                            .push(Arc::clone(&function_call_info));
                    } else {
                        simulator_context
                            .function_calls
                            .push(Arc::clone(&function_call_info));
                    }

                    simulator_context.current_function_call = Some(Arc::clone(&function_call_info));
                }

                let class = simulator_context.get_class_by_type(&object.get_type());
                let Some(class) = class else {
                    return simulator_context.create_error_value();
                };

                // Attribute-as-function path: if the method name
                // matches an attribute whose type is a function or
                // closure, call it as a function value.
                if class.attributes.contains_key(&function_name)
                    && (class.attributes[&function_name].attribute_type.is_closure()
                        || class.attributes[&function_name]
                            .attribute_type
                            .is_function())
                {
                    let attribute_function = simulator_context
                        .get_object_attribute(&object, function_name.clone(), true)
                        .unwrap();

                    function_call_info.write().unwrap().return_type =
                        if attribute_function.get_type().is_function() {
                            attribute_function
                                .get_type()
                                .function_return()
                                .unwrap()
                                .clone()
                        } else {
                            PekoType::simple_type("void")
                        };

                    for argument_type in attribute_function.get_type().generics() {
                        function_call_info
                            .write()
                            .unwrap()
                            .signature_arguments
                            .insert(String::new(), argument_type.clone());
                    }

                    let argument_types = attribute_function.get_type().generics().to_vec();

                    // Attribute-functions don't support variadic
                    // parameters, so the argument count must match.
                    if function_call.arguments.len() != argument_types.len() {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.access.get_start().clone(),
                                self.access.get_end().clone(),
                                format!(
                                    "wrong number of arguments to attribute function. The attribute's function type declares `{}` parameters but `{}` arguments were provided",
                                    argument_types.len(),
                                    function_call.arguments.len(),
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );

                        return simulator_context.create_error_value();
                    }

                    // Simulate the call's arguments under the
                    // expected parameter types.
                    let mut arguments = Vec::new();
                    let mut keyword_values = HashMap::new();
                    let post_stack = simulator_context.module_context.step_back();
                    for ((argument_name, argument), expected_type) in
                        function_call.arguments.iter().zip(&argument_types)
                    {
                        let current_expected_types =
                            simulator_context.current_expected_type_options.clone();
                        simulator_context.current_expected_type_options =
                            Some(vec![expected_type.clone()]);

                        arguments.push(argument.simulate(simulator_context));
                        function_call_info
                            .write()
                            .unwrap()
                            .argument_positions
                            .push((argument.get_start().clone(), argument.get_end().clone()));

                        simulator_context.current_expected_type_options = current_expected_types;

                        if argument_name.is_some() {
                            keyword_values.insert(
                                argument_name.clone().unwrap().value,
                                arguments.last().unwrap().clone(),
                            );
                        }
                    }
                    simulator_context.current_function_call = previous_function_call;

                    for (argument_index, (argument, argument_type)) in
                        arguments.iter().zip(argument_types.iter()).enumerate()
                    {
                        if !simulator_context.types_similar(argument_type, &argument.get_type()) {
                            simulator_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    function_call.arguments[argument_index]
                                        .1
                                        .get_start()
                                        .clone(),
                                    function_call.arguments[argument_index].1.get_end().clone(),
                                    format!(
                                        "argument of type `{}` does not match expected type `{}`",
                                        argument.get_type(),
                                        argument_type,
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    simulator_context.get_current_file(),
                                ),
                            );
                        }
                    }
                    simulator_context.module_context.step_forward(post_stack);

                    let function_return_type = attribute_function
                        .get_type()
                        .function_return()
                        .unwrap()
                        .clone();

                    simulator_context.record_defined_object(
                        &function_return_type,
                        false,
                        self.access.get_end().clone(),
                    );

                    return SimulatorValue::Value(function_return_type);

                // Method-not-found error.
                } else if !class
                    .main_virtual_table
                    .methods
                    .contains_key(&function_name)
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.access.get_start().clone(),
                            self.access.get_end().clone(),
                            format!(
                                "no method named `{}` on class `{}`. Check the method name and that it is declared on this class or a parent",
                                function_name,
                                class.class_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );

                    return SimulatorValue::Value(types::PekoType::error_type());
                }

                // Build per-argument expected-type options from the
                // method's overload set.
                let method_options: Vec<_> = class.main_virtual_table.methods[&function_name]
                    .iter()
                    .map(|function| function.read().unwrap().clone())
                    .collect();
                let mut argument_type_options = vec![Vec::new(); function_call.arguments.len()];

                for method_option in method_options {
                    // Only add type options for the correct number of arguments.
                    if method_option.arguments.len() != function_call.arguments.len()
                        || (function_call.arguments.len() > method_option.arguments.len()
                            && method_option.var_args_type.is_none())
                    {
                        continue;
                    }

                    for (idx, (_, argument)) in method_option.arguments.iter().enumerate() {
                        argument_type_options[idx].push(argument.argument_type.clone());
                    }

                    if function_call.arguments.len() > method_option.arguments.len()
                        && method_option.var_args_type.is_some()
                    {
                        for arg_option in argument_type_options
                            .iter_mut()
                            .take(function_call.arguments.len())
                            .skip(method_option.arguments.len())
                        {
                            arg_option.push(method_option.var_args_type.clone().unwrap());
                        }
                    }
                }

                let mut arguments = Vec::new();
                let mut keyword_arguments = HashMap::new();
                let post_stack = simulator_context.module_context.step_back();
                for ((argument_name, argument), expected_type_options) in
                    function_call.arguments.iter().zip(&argument_type_options)
                {
                    let current_expected_types =
                        simulator_context.current_expected_type_options.clone();

                    simulator_context.current_expected_type_options =
                        Some(expected_type_options.clone());
                    arguments.push(argument.simulate(simulator_context));

                    simulator_context.current_expected_type_options = current_expected_types;

                    if argument_name.is_some() {
                        keyword_arguments.insert(
                            argument_name.clone().unwrap().value,
                            arguments.last().unwrap().clone(),
                        );
                    }
                }

                let keyword_values = if keyword_arguments.is_empty() {
                    None
                } else {
                    Some(keyword_arguments)
                };

                let mut argument_types = Vec::new();
                for argument in &arguments {
                    argument_types.push(argument.get_type());
                }

                let provided_function_argument_types = if let Some(kv) = &keyword_values {
                    let mut arguments = HashMap::new();

                    for (argument_name, argument_value) in kv {
                        arguments.insert(argument_name.clone(), argument_value.get_type());
                    }

                    Some(arguments)
                } else {
                    None
                };

                // Always populate signature info via the
                // diagnostic-recovery selector (always returns Some).
                let best_signature_choice = simulator_context.choose_most_similar_function(
                    class.main_virtual_table.methods[&function_name]
                        .iter()
                        .map(|function| function.read().unwrap().clone())
                        .collect(),
                    argument_types,
                    provided_function_argument_types,
                    true,
                );

                if let Some(signature_function) = best_signature_choice {
                    function_call_info.write().unwrap().return_type =
                        signature_function.return_type.clone();
                    for (argument_name, argument_info) in &signature_function.arguments {
                        function_call_info
                            .write()
                            .unwrap()
                            .signature_arguments
                            .insert(argument_name.clone(), argument_info.argument_type.clone());
                    }
                    function_call_info.write().unwrap().docinfo =
                        signature_function.docinfo.clone();
                }
                simulator_context.module_context.step_forward(post_stack);

                // Dispatch to the actual method call.
                let method_call = simulator_context.call_object_method(
                    &object,
                    function_name.clone(),
                    arguments,
                    keyword_values,
                );

                // call_object_method's Err carries the user-facing
                // diagnostic message it produced.
                let method_call = match method_call {
                    Ok(value) => value,
                    Err(message) => {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.object.get_start().clone(),
                                self.access.get_end().clone(),
                                message,
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );

                        return simulator_context.create_error_value();
                    }
                };

                // Backward inference: a constraining call on a variable holding
                // inference variables resolves them. Substitute the discovered
                // bindings into the receiver variable's stored type so later
                // uses see the concrete type.
                if !simulator_context.last_call_inference.is_empty()
                    && let PekoAST::VariableReference(receiver) = self.object.as_ref()
                {
                    let name = receiver.variable_name.value.clone();
                    let current = simulator_context
                        .scoped_variables
                        .get(&name)
                        .map(|variable| variable.variable_type.clone());
                    if let Some(current) = current {
                        let updated = substitute_inference_variables(
                            &current,
                            &simulator_context.last_call_inference,
                        );
                        if let Some(variable) = simulator_context.scoped_variables.get_mut(&name) {
                            variable.variable_type = updated;
                        }
                    }
                }

                // Rule 2 of 24.2: calling a [mutates] method on an attribute of
                // `this` makes the enclosing method [mutates] too.
                if object_is_this_attribute && simulator_context.last_called_method_mutates {
                    simulator_context.current_method_mutates = true;
                }

                // Record the call edge so a fixpoint after simulation can
                // propagate [mutates] through forward references that the
                // single pass above cannot see yet.
                if object_is_this_attribute
                    && let Some(this_variable) = &simulator_context.current_this
                    && let Some(caller_method) = simulator_context.current_method_name.clone()
                {
                    let caller_class = this_variable.variable_type.name().to_string();
                    let callee_class = object.get_type().name().to_string();
                    simulator_context.mutates_call_edges.push((
                        caller_class,
                        caller_method,
                        callee_class,
                        function_name.clone(),
                    ));
                }

                simulator_context.record_defined_object(
                    &method_call.get_type(),
                    false,
                    function_call.end.clone(),
                );

                method_call
            }

            // --- Attribute read --- //
            PekoAST::VariableReference(variable_reference) => {
                let variable_name = variable_reference.variable_name.value.clone();

                // Closures have two pseudo-attributes: `function`
                // (the function pointer) and `context` (the
                // captured-variables blob). Both are opaque.
                if object.get_type().is_closure() {
                    let mut context_type = types::PekoType::simple_type("pointer");
                    context_type
                        .generics_mut()
                        .push(types::PekoType::simple_type("void"));

                    match variable_name.as_str() {
                        "function" => {
                            let mut function_type = object.get_type();
                            function_type.set_closure(false);
                            function_type.generics_mut().insert(0, context_type);

                            return SimulatorValue::Value(function_type);
                        }
                        "context" => {
                            // The closure's captured-context pointer. This
                            // is a managed `Pointer<void>` in codegen, so
                            // the simulator reports the same type to keep
                            // overload resolution consistent across backends.
                            return SimulatorValue::Value(context_type);
                        }
                        _ => {
                            simulator_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    variable_reference.variable_name.start.clone(),
                                    variable_reference.variable_name.end.clone(),
                                    format!(
                                        "`{variable_name}` is not a valid attribute of a closure. Closures only have `function` and `context` attributes"
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    simulator_context.get_current_file(),
                                ),
                            );

                            return SimulatorValue::Value(types::PekoType::error_type());
                        }
                    }
                }

                let class = simulator_context.get_class_by_type(&object.get_type());
                let Some(class) = class else {
                    return simulator_context.create_error_value();
                };

                // Methods accessed without invocation surface as
                // opaque function pointers.
                if class
                    .main_virtual_table
                    .methods
                    .contains_key(&variable_name)
                {
                    return SimulatorValue::Value(types::PekoType::simple_type("opaque"));
                }

                let reference = simulator_context.get_object_attribute(
                    &object,
                    variable_name.clone(),
                    !simulator_context.return_references,
                );

                let reference = match reference {
                    Ok(value) => value,
                    Err(message) => {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variable_reference.variable_name.start.clone(),
                                variable_reference.variable_name.end.clone(),
                                message,
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }
                };

                // Records the attribute type as a completion source. Expansion
                // inside the helper resolves a generic-parameter attribute
                // (`dat: T`) to its bound carrier, so `this.dat.` surfaces the
                // same members as a bare `dat`.
                simulator_context.record_defined_object(
                    &reference.get_type(),
                    false,
                    variable_reference.variable_name.end.clone(),
                );

                reference
            }

            // --- Attribute write --- //
            PekoAST::VariableReassignment(variable_reassignment) => {
                let variable_name = match variable_reassignment.variable_reference.as_ref() {
                    PekoAST::VariableReference(variable_reference) => {
                        variable_reference.variable_name.value.clone()
                    }
                    _ => {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variable_reassignment.variable_reference.get_start().clone(),
                                variable_reassignment.variable_reference.get_end().clone(),
                                "expected an identifier for the attribute name. Attribute assignments must use the form `object.attribute = value`".to_string(),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }
                };

                // If we're setting an attribute on `this` and that
                // attribute was in the "must-set" list, mark it done.
                let object_name = match self.object.as_ref() {
                    PekoAST::VariableReference(variable_reference) => {
                        Some(variable_reference.variable_name.value.clone())
                    }
                    _ => None,
                };

                if object_name.is_some()
                    && object_name.unwrap() == "this"
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

                let class = simulator_context.get_class_by_type(&object.get_type());
                let Some(class) = class else {
                    return simulator_context.create_error_value();
                };

                let previous_expected_type =
                    simulator_context.current_expected_type_options.clone();
                if class.attributes.contains_key(&variable_name) {
                    simulator_context.current_expected_type_options = Some(vec![
                        class.attributes[&variable_name].attribute_type.clone(),
                    ]);
                }

                let variable_value = variable_reassignment
                    .variable_value
                    .simulate(simulator_context);

                let attribute =
                    simulator_context.get_object_attribute(&object, variable_name.clone(), true);

                let attribute = match attribute {
                    Ok(value) => value,
                    Err(message) => {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variable_reassignment.variable_reference.get_start().clone(),
                                variable_reassignment.variable_reference.get_end().clone(),
                                message,
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }
                };

                // Compound-assignment: apply the operator and bail.
                if variable_reassignment.assignment_operator.is_some() {
                    let try_operator = simulator_context.apply_operator(
                        variable_reassignment
                            .clone()
                            .assignment_operator
                            .unwrap()
                            .as_str(),
                        &attribute,
                        &variable_value,
                    );

                    if try_operator.is_none() {
                        simulator_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variable_reassignment.variable_reference.get_start().clone(),
                                variable_reassignment.variable_reference.get_end().clone(),
                                format!(
                                    "cannot apply operator `{}` between attribute of type `{}` and value of type `{}`. There is no operator overload that accepts these two operand types",
                                    variable_reassignment.clone().assignment_operator.unwrap(),
                                    attribute.get_type(),
                                    variable_value.get_type(),
                                ),
                                diagnostics::DiagnosticType::Error,
                                simulator_context.get_current_file(),
                            ),
                        );
                        return simulator_context.create_error_value();
                    }

                    return SimulatorValue::Null;
                }

                simulator_context.current_expected_type_options = previous_expected_type;

                // Direct assignment type-check.
                if !simulator_context
                    .types_similar(&variable_value.get_type(), &attribute.get_type())
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            variable_reassignment.variable_reference.get_start().clone(),
                            variable_reassignment.variable_reference.get_end().clone(),
                            format!(
                                "cannot assign value of type `{}` to attribute of type `{}`. The value's type is not compatible with the attribute's declared type",
                                variable_value.get_type(),
                                attribute.get_type(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                }

                SimulatorValue::Null
            }

            _ => SimulatorValue::Value(types::PekoType::error_type()),
        }
    }
}

/// Simulates a unary expression `op operand`. Three operators are
/// recognized: `!` (logical not), `&` (address-of/reference), and
/// `-` (negation, implemented as `operand * (-1)`).
impl PekoValueSimulator for UnaryExpressionAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        match self.operator.as_str() {
            "!" => {
                let negate = self.get_operand().simulate(simulator_context);

                // An object operand routes through the Not trait; a raw i1
                // negates in place.
                if simulator_context
                    .get_class_by_type(&negate.get_type())
                    .is_some()
                {
                    if let Ok(value) =
                        simulator_context.call_object_method(&negate, "not", Vec::new(), None)
                    {
                        return value;
                    }
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.operand.get_start().clone(),
                            self.operand.get_end().clone(),
                            format!(
                                "the `!` operator requires a raw i1 or a type that implements `Not`, but `{}` does neither",
                                negate.get_type(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    return simulator_context.create_error_value();
                }

                if !simulator_context
                    .types_similar(&negate.get_type(), &types::PekoType::simple_type("i1"))
                {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.operand.get_start().clone(),
                            self.operand.get_end().clone(),
                            format!(
                                "the `!` (logical not) operator requires a bool or raw i1 operand, but the operand has type `{}`",
                                negate.get_type(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                }

                SimulatorValue::Value(types::PekoType::simple_type("i1"))
            }
            "*" => {
                // Dereference a pointer. Mirrors `ptr[0]` indexing but
                // expresses the operation as a unary prefix; emits a
                // single load on the operand value.
                let value = self.operand.simulate(simulator_context);
                let value_type = simulator_context.expand_type(&value.get_type()).unwrap();

                if value_type.array_depth == 0
                    && value_type.reference_depth == 0
                    && value_type.name() != "pointer"
                {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.operand.get_start().clone(),
                            self.operand.get_end().clone(),
                            format!(
                                "cannot dereference value of type `{}` with the unary `*` operator. Only pointer or reference types can be dereferenced",
                                value_type,
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file().to_path_buf(),
                        ));
                    return simulator_context.create_error_value();
                }

                SimulatorValue::Value(pointee_type(&value_type))
            }
            "&" => {
                // Take a reference - simulate the operand in
                // reference-returning mode.
                let return_references = simulator_context.return_references;
                simulator_context.return_references = true;

                let value = self.operand.simulate(simulator_context);

                simulator_context.return_references = return_references;

                simulator_context.record_defined_object(
                    &value.get_type(),
                    false,
                    self.operand.get_end().clone(),
                );

                value
            }
            "-" => {
                // Negate via `operand * -1`, leveraging the operator overload
                // for `*`. The minus-one carries the operand's own type so the
                // overload matches: a `number` operand multiplies by a `number`
                // minus-one, a raw scalar by a raw machine minus-one.
                let value = self.operand.simulate(simulator_context);
                let negative_type = if value.get_type().name() == "number" {
                    types::PekoType::simple_type("number")
                } else {
                    types::PekoType::simple_type("i32")
                };
                let negative_value = SimulatorValue::Value(negative_type);

                let evaluated = simulator_context.apply_operator("*", &value, &negative_value);

                let Some(evaluated) = evaluated else {
                    simulator_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.operand.get_start().clone(),
                            self.operand.get_end().clone(),
                            format!(
                                "cannot negate value of type `{}` with the unary `-` operator. The type does not implement the `*` operator with an `int` operand",
                                value.get_type(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ),
                    );
                    return simulator_context.create_error_value();
                };

                simulator_context.record_defined_object(
                    &evaluated.get_type(),
                    false,
                    self.operand.get_end().clone(),
                );

                evaluated
            }
            _ => {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.operand.get_start().clone(),
                        self.operand.get_end().clone(),
                        format!(
                            "operator `{}` is not a unary operator. Only `!`, `&`, and `-` can be used as unary operators",
                            self.operator,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));

                simulator_context.create_error_value()
            }
        }
    }
}

/// Simulates a binary expression `lhs op rhs`.
///
/// The `..` operator is re-routed to [`RangeAST`] since the parser
/// produces a [`BinaryExpressionAST`] for range expressions too.
/// Other operators are dispatched through [`apply_operator`] which
/// consults the type's declared operator overloads.
///
/// [`apply_operator`]: ExecutionContextAlgorithms::apply_operator
/// True when `ast` is the bare `None` literal. The parser represents None as a
/// variable reference named None.
fn is_none_literal_ast(ast: &PekoAST) -> bool {
    matches!(ast, PekoAST::VariableReference(reference) if reference.variable_name.value == "None")
}

impl PekoValueSimulator for BinaryExpressionAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        if self.operator == ".." {
            return PekoAST::Range(RangeAST::new(self.lhs.clone(), self.rhs.clone()))
                .simulate(simulator_context);
        }

        // `opt == None` / `opt != None` test an optional for emptiness. None has
        // no type of its own here, so the comparison resolves to a bool on the
        // optional operand without forcing None to name a type.
        if self.operator == "==" || self.operator == "!=" {
            let lhs_is_none = is_none_literal_ast(self.get_lhs());
            let rhs_is_none = is_none_literal_ast(self.get_rhs());
            if lhs_is_none ^ rhs_is_none {
                let operand_ast = if lhs_is_none {
                    self.get_rhs()
                } else {
                    self.get_lhs()
                };
                let operand = operand_ast.simulate(simulator_context);
                if operand.get_type().name() == "Option" {
                    return SimulatorValue::Value(types::PekoType::simple_type("bool"));
                }
                if !operand.get_type().is_error_type() {
                    simulator_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.lhs.get_start().clone(),
                            self.rhs.get_end().clone(),
                            format!(
                                "cannot compare `{}` against None. A None comparison requires an optional value",
                                operand.get_type()
                            ),
                            diagnostics::DiagnosticType::Error,
                            simulator_context.get_current_file(),
                        ));
                }
                return simulator_context.create_error_value();
            }
        }

        let lhs = self.get_lhs().simulate(simulator_context);
        let rhs = self.get_rhs().simulate(simulator_context);

        let evaluated = simulator_context.apply_operator(self.operator.as_str(), &lhs, &rhs);

        let evaluated = if let Some(e) = evaluated {
            e
        } else {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.lhs.get_start().clone(),
                    self.rhs.get_end().clone(),
                    format!(
                        "cannot apply binary operator `{}` to values of type `{}` and `{}`. There is no operator overload that accepts these two operand types",
                        self.operator,
                        lhs.get_type(),
                        rhs.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        };

        simulator_context.record_defined_object(
            &evaluated.get_type(),
            false,
            self.rhs.get_end().clone(),
        );

        evaluated
    }
}

/// Simulates an array access `array[index]`.
///
/// If the array's type is a class, the access is dispatched through the
/// Index trait (`index` for a read, `index_ref` for a write). Otherwise the
/// array must be a pointer/reference type, and the index must be
/// `int64`-compatible.
impl PekoValueSimulator for ArrayAccessAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        // The array and index expressions simulate as values, not
        // references.
        let return_references = simulator_context.return_references;
        simulator_context.return_references = false;

        let array = self.array.simulate(simulator_context);
        let access = self.access.simulate(simulator_context);

        simulator_context.return_references = return_references;

        // Class-with-indexing-overload path.
        if (array.get_type().array_depth == 0 && array.get_type().reference_depth == 0)
            && simulator_context
                .get_class_by_type(&array.get_type())
                .is_some()
        {
            // Route indexing to the Index / IndexRef trait methods. A write
            // context (the slot is needed) dispatches `index_ref`; a read
            // context dispatches `index`.
            let method = if simulator_context.return_references {
                String::from("index_ref")
            } else {
                String::from("index")
            };

            let access_call =
                simulator_context.call_object_method(&array, method, vec![access.clone()], None);

            if access_call.is_err() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.array.get_start().clone(),
                        self.array.get_end().clone(),
                        format!(
                            "cannot index into value of type `{}`. The type does not implement the `Index` trait",
                            array.get_type(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }

            return access_call.unwrap();
        } else if !array.get_type().is_pointer() {
            // No indexing overload and not a pointer - can't be
            // indexed.
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.array.get_start().clone(),
                    self.array.get_end().clone(),
                    format!(
                        "cannot index into value of type `{}` with `[]`. The value is not an array, pointer, or class with an indexing operator",
                        array.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        if !simulator_context
            .types_similar(&access.get_type(), &types::PekoType::simple_type("i64"))
        {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.access.get_start().clone(),
                    self.access.get_end().clone(),
                    format!(
                        "cannot use value of type `{}` as an array index. Array indices must be `int`-compatible",
                        access.get_type(),
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // Compute the item type: one pointer/reference depth less
        // than the array, except for strings/opaque values which
        // produce `char`.
        let mut value_type = pointee_type(&array.get_type());

        simulator_context.record_defined_object(&value_type, false, self.end.clone());

        if simulator_context.return_references {
            value_type.reference_depth += 1;
            SimulatorValue::Value(value_type)
        } else {
            SimulatorValue::Value(value_type)
        }
    }
}

/// Simulates a value cast `(value as Type)`.
impl PekoValueSimulator for CastAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let value = self.value.simulate(simulator_context);
        let value_type = value.get_type();

        // A forced `danger_cast<T>(value)` or a `constant<T>(value)` performs no
        // safety check; the result simply takes the target type.
        if matches!(self.kind, CastKind::Forced | CastKind::Constant) {
            if !simulator_context.type_exists(&self.cast_to) {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.cast_to.start_position.clone(),
                        self.cast_to.end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            self.cast_to,
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
                return simulator_context.create_error_value();
            }
            let expanded = simulator_context
                .expand_type(&self.cast_to)
                .unwrap_or_else(|| self.cast_to.clone());
            return SimulatorValue::Value(expanded);
        }

        // Casting an object to a trait it implements is a safe upcast: the
        // result is a trait object. Allowed when the value's class carries the
        // trait; otherwise the escape hatch is `danger_cast<Trait>`.
        if let Some(trait_definition) = simulator_context.get_trait(self.cast_to.name()) {
            let carries_trait = simulator_context
                .get_class_by_type(&value_type)
                .map(|class| class.implements.contains(&trait_definition.name))
                .unwrap_or(false);

            if carries_trait {
                return SimulatorValue::Value(self.cast_to.clone());
            }

            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.value.get_start().clone(),
                    self.value.get_end().clone(),
                    format!(
                        "value of type `{}` does not statically implement trait `{}`, so it cannot be cast with `as`. Use `danger_cast<{}>(...)` to force it",
                        value_type, self.cast_to, self.cast_to,
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        }

        // Casting between two classes on the same inheritance chain. An upcast
        // (a derived value to one of its base classes) is always safe. A
        // downcast (a base value to one of its derived classes) is the intended
        // use of `as`: it reinterprets the object as the more specific type, the
        // way JSON code narrows a `JsonValue` to a `JsonObject` after checking
        // its kind. `classes_connect` is directional, so test both orders.
        let value_class = simulator_context.get_class_by_type(&value_type);
        let target_class = simulator_context.get_class_by_type(&self.cast_to);
        if let (Some(from_class), Some(to_class)) = (&value_class, &target_class)
            && (simulator_context.classes_connect(from_class, to_class)
                || simulator_context.classes_connect(to_class, from_class))
        {
            let expanded = simulator_context
                .expand_type(&self.cast_to)
                .unwrap_or_else(|| self.cast_to.clone());
            return SimulatorValue::Value(expanded);
        }

        let box_value = simulator_context.box_value_to_type(&self.cast_to, &value);

        let Some(boxed) = box_value else {
            simulator_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.value.get_start().clone(),
                    self.value.get_end().clone(),
                    format!(
                        "value of type `{}` cannot be cast to type `{}`. The cast is not defined between these types",
                        value_type,
                        self.cast_to,
                    ),
                    diagnostics::DiagnosticType::Error,
                    simulator_context.get_current_file(),
                ));
            return simulator_context.create_error_value();
        };

        boxed
    }
}

/// Simulates an unwrap expression `optional?`. The optional's
/// `[operator ?]` overload is invoked to produce the unwrapped
/// value; non-optionals produce a diagnostic.
impl PekoValueSimulator for UnwrapAST {
    fn simulate(&self, simulator_context: &mut PekoSimulatorContext) -> SimulatorValue {
        let optional = self.optional.simulate(simulator_context);

        // The operand must be an optional. `?` yields its held type T.
        if optional.get_type().name() != "Option" {
            if !optional.get_type().is_error_type() {
                simulator_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.optional.get_start().clone(),
                        self.optional.get_end().clone(),
                        format!(
                            "cannot unwrap value of type `{}` with `?`. The value is not an `Option<T>`; only optional values can be unwrapped",
                            optional.get_type(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        simulator_context.get_current_file(),
                    ));
            }
            return simulator_context.create_error_value();
        }

        let inner_type = optional
            .get_type()
            .optional_get_inner_type()
            .unwrap_or_else(types::PekoType::error_type);

        // Type-check a fallback block. Its statements simulate for their own
        // diagnostics; the block yields the fallback value.
        if let Some(else_body) = &self.else_body {
            for statement in &else_body.value {
                statement.simulate(simulator_context);
            }
        }

        simulator_context.record_defined_object(&inner_type, false, self.end.clone());

        SimulatorValue::Value(inner_type)
    }
}
