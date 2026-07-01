//! `PekoValueBuilder` implementations for the expression-producing AST
//! nodes: literals (arrays, maps, XML tags), construction, member /
//! index / module access, casts and unwraps, variable references, range
//! expressions, function calls, and binary / unary operators.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use itertools::Itertools;
use llvm_sys_180::core;
use llvm_sys_180::prelude::LLVMValueRef;
use peko_core::asts::PekoAST;
use peko_core::asts::data_structures::StringChunkContent;
use peko_core::asts::data_structures::{
    ClassMethod, PositionData, PositionedValue, VisibilityData,
};
use peko_core::asts::expressions::{
    ArrayAST, ArrayAccessAST, BinaryExpressionAST, CastAST, CastKind, FunctionCallAST, MapAST,
    ModuleAccessAST, ObjectAccessAST, ObjectConstructionAST, PekoXTagAST, RangeAST,
    UnaryExpressionAST, UnwrapAST, VariableReferenceAST,
};
use peko_core::asts::values::StringAST;
use peko_core::diagnostics;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::execution::data_structures::ExecutionModule;
use peko_core::types::{PekoType, TypeRestraint};

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::prelude::*;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{
    BooleanOperation, CodegenArg, CodegenFunction, CodegenValue, NumericalOperation,
    is_managed_pointer, managed_pointer_type,
};

/// Builds a call to one of an enum's generated serialization helper functions
/// (`serialize_enum_<E>` / `deserialize_enum_<E>`). Enum-typed `.serialize` and
/// `Enum::deserialize` are routed here, since an enum has no methods of its own.
/// Using a plain function call reuses ordinary cross-module name resolution, so
/// the helper is found whether the enum is local or imported.
fn enum_serde_helper_call(
    helper_name: &str,
    arguments: Vec<(Option<PositionedValue<String>>, PekoAST)>,
    start: PositionData,
    end: PositionData,
) -> FunctionCallAST {
    let reference = PekoAST::VariableReference(VariableReferenceAST::new(PositionedValue::new(
        helper_name.to_string(),
        start.clone(),
        end.clone(),
    )));
    FunctionCallAST::new(start, end, Box::new(reference), Vec::new(), arguments)
}

impl PekoValueBuilder for ArrayAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if self.values.is_empty() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "array literal requires at least one value".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        let mut array_values = Vec::new();
        for item in &self.values {
            let item_value = item.build_value(codegen_context);

            // The first item determines the array's element type; later
            // items must be compatible with it.
            if array_values.is_empty() {
                array_values.push(item_value);
                continue;
            }

            if !codegen_context.types_similar(
                &array_values.first().unwrap().value_type,
                &item_value.value_type,
            ) {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        item.get_start().clone(),
                        item.get_end().clone(),
                        format!(
                            "type of value `{}` does not match the array type of `{}`",
                            item_value.value_type,
                            array_values.first().unwrap().value_type,
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            } else {
                array_values.push(item_value);
            }
        }

        let element_type = array_values.first().unwrap().value_type.clone();
        codegen_context
            .create_standard_array(&element_type, array_values)
            .unwrap_or_else(|| codegen_context.create_error_value())
    }
}

impl PekoValueBuilder for MapAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if self.key_values.is_empty() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "map literal requires at least one key-value pair".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        let mut key_value_pair_values = Vec::new();
        for (key_item, value_item) in &self.key_values {
            let key_item_value = key_item.build_value(codegen_context);
            let value_item_value = value_item.build_value(codegen_context);

            // The first pair determines both the key and value types.
            if key_value_pair_values.is_empty() {
                key_value_pair_values.push((key_item_value, value_item_value));
                continue;
            }

            if !codegen_context.types_similar(
                &key_value_pair_values.first().unwrap().0.value_type,
                &key_item_value.value_type,
            ) {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        key_item.get_start().clone(),
                        key_item.get_end().clone(),
                        format!(
                            "type of key `{}` does not match the map key type of `{}`",
                            key_item_value.value_type,
                            key_value_pair_values.first().unwrap().0.value_type
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                continue;
            }

            if !codegen_context.types_similar(
                &key_value_pair_values.first().unwrap().1.value_type,
                &value_item_value.value_type,
            ) {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        key_item.get_start().clone(),
                        key_item.get_end().clone(),
                        format!(
                            "type of value `{}` does not match the map value type of `{}`",
                            value_item_value.value_type,
                            key_value_pair_values.first().unwrap().1.value_type
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                continue;
            }

            key_value_pair_values.push((key_item_value, value_item_value));
        }

        let key_type = key_value_pair_values.first().unwrap().0.value_type.clone();
        let value_type = key_value_pair_values.first().unwrap().1.value_type.clone();
        codegen_context
            .create_standard_map(&key_type, &value_type, key_value_pair_values)
            .unwrap_or_else(|| codegen_context.create_error_value())
    }
}

impl PekoValueBuilder for PekoXTagAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Build the attribute key-value list, with both keys and values
        // emitted as `string` values.
        let mut attribute_key_value_pairs = Vec::new();
        for (attribute_name, attribute_value) in &self.attributes {
            let attribute_name = codegen_context.create_string(attribute_name);

            attribute_key_value_pairs
                .push((attribute_name, attribute_value.build_value(codegen_context)));
        }

        let element_attributes = codegen_context.create_standard_map(
            &PekoType::simple_type("string"),
            &PekoType::simple_type("string"),
            attribute_key_value_pairs,
        );

        let element_attributes = match element_attributes {
            Some(v) => v,
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.attributes_start.clone(),
                        self.attributes_end.clone(),
                        "one or more values assigned to this tag's attributes are not convertible to strings. All XML attribute values must be `String`-compatible".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
        };

        // Build and type-check the children.
        let mut children = Vec::new();
        for child in &self.children {
            let child_value = child.clone().build_value(codegen_context);

            if codegen_context.types_equal(
                &child_value.value_type,
                &PekoType::from_string("xml::Element", ""),
            ) {
                children.push(child_value);
            } else {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        child.get_start().clone(),
                        child.get_end().clone(),
                        "only XML tags can be interpolated with `{}` syntax inside another tag. Consider using `${}` syntax for non-element interpolation instead".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }
        }

        let element_children = codegen_context
            .create_standard_array(&PekoType::from_string("xml::Element", ""), children)
            .unwrap();

        // The inner text is a synthesized `string`. Passes
        // `interpolated = true` unconditionally. When the chunk list is just one
        // text chunk, the formatting path collapses to a constant emit, so
        // over-marking it is behaviorally neutral.
        let element_inner_text = PekoAST::String(StringAST::new(
            PositionData::default(),
            PositionData::default(),
            true,
            self.inner_text.clone(),
        ))
        .build_value(codegen_context);

        let tag_string = codegen_context.create_string(&self.tag);

        // Build the event handler map; values are closures over the
        // current scope.
        let mut event_key_values = Vec::new();
        for (event_name, event) in &self.events {
            let event_name_value = codegen_context.create_string(event_name);

            let mut event_arguments = IndexMap::new();
            event_arguments.insert(
                PositionedValue::create_no_position("event".to_string()),
                peko_core::asts::data_structures::DeclarationArgumentData::new(
                    PositionData::default(),
                    PositionData::default(),
                    PekoType::from_string("xml::Event", ""),
                    None,
                    VisibilityData::open_visibility(),
                ),
            );

            let event_closure = peko_core::asts::declarations::ClosureAST::new(
                event.start.clone(),
                event.end.clone(),
                event_arguments,
                codegen_context
                    .scoped_variables
                    .keys()
                    .map(|name| PositionedValue::create_no_position(name.clone()))
                    .collect_vec(),
                None,
                event.clone(),
            );

            event_key_values.push((event_name_value, event_closure.build_value(codegen_context)));
        }

        let events_map = codegen_context
            .create_standard_map(
                &PekoType::simple_type("string"),
                &PekoType::from_string("closure(xml::Event) => void", ""),
                event_key_values,
            )
            .unwrap();

        // Materialize the final `xml::Element` object.
        let pekox_tag_object = codegen_context.create_object(
            &PekoType::from_string("xml::Element", ""),
            vec![
                tag_string,
                element_attributes,
                element_children,
                element_inner_text,
                events_map,
            ],
        );

        match pekox_tag_object {
            Some(v) => v,
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        "error in the linkage of the standard library".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
        }
    }
}

impl PekoValueBuilder for ObjectConstructionAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // The class name may carry a module path (`module::Class`). Parse it so
        // the leading segments become the type's module path, then attach the
        // construction's generic arguments.
        // Generic-argument inference: when the construction omits its type
        // arguments, recover them from a matching expected type at the use
        // site (mirrors the simulator).
        let mut object_generics = self.object_generics.clone();
        if object_generics.is_empty()
            && let Some(options) = &codegen_context.current_expected_type_options
        {
            for expected in options {
                if expected.name() == self.class_name.value && !expected.generics().is_empty() {
                    object_generics = expected.generics().to_vec();
                    break;
                }
            }
        }

        let mut class_type =
            PekoType::from_string(&self.class_name.value, codegen_context.get_current_file());
        *class_type.generics_mut() = object_generics.clone();
        class_type.start_position = self.start.clone();
        class_type.end_position = self.end.clone();

        let mut get_codegen_class = codegen_context.get_class_by_type(&class_type);

        // Backward inference: bind a fresh inference variable (`?N`) per
        // missing argument when the class is generic but received none, so the
        // erased methods resolve. The carriers lower like any generic param.
        if object_generics.is_empty()
            && let Some(class) = &get_codegen_class
            && !class.generic_typenames.is_empty()
        {
            for _ in 0..class.generic_typenames.len() {
                let name = format!("?{}", codegen_context.inference_counter);
                codegen_context.inference_counter += 1;
                object_generics.push(PekoType::generic_type(name, Vec::new()));
            }
            *class_type.generics_mut() = object_generics.clone();
            get_codegen_class = codegen_context.get_class_by_type(&class_type);
        }

        if get_codegen_class.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.class_name.start.clone(),
                    self.class_name.end.clone(),
                    format!(
                        "cannot find class `{}`. Check the class name, that the class is declared, and that it is imported",
                        class_type
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        let allocate_object = codegen_context.allocate_class(get_codegen_class.as_ref().unwrap());
        let allocate_object = match allocate_object {
            Some(v) => v,
            None => return codegen_context.create_error_value(),
        };

        // Either reuse previously generated argument values (generic
        // class path) or generate them now.
        let (constructor_arguments, constructor_keyword_arguments) =
            if codegen_context.generated_kw_args.is_some() {
                (
                    codegen_context.generated_args.clone(),
                    codegen_context.generated_kw_args.clone().unwrap(),
                )
            } else {
                let mut argument_type_options = vec![Vec::new(); self.arguments.len()];

                if get_codegen_class
                    .as_ref()
                    .unwrap()
                    .main_virtual_table
                    .methods
                    .contains_key("constructor")
                {
                    let method_options: Vec<CodegenFunction> = get_codegen_class
                        .as_ref()
                        .unwrap()
                        .main_virtual_table
                        .methods["constructor"]
                        .iter()
                        .map(|option| option.read().unwrap().clone())
                        .collect();

                    for method_option in method_options {
                        if (method_option.arguments.len() - 1) != self.arguments.len()
                            || (self.arguments.len() > (method_option.arguments.len() - 1)
                                && method_option.var_args_type.is_none())
                        {
                            continue;
                        }

                        for (idx, (_, argument)) in
                            method_option.arguments.iter().skip(1).enumerate()
                        {
                            argument_type_options[idx].push(argument.argument_type.clone());
                        }

                        if self.arguments.len() > (method_option.arguments.len() - 1)
                            && method_option.var_args_type.is_some()
                        {
                            for argument_type_option in argument_type_options
                                .iter_mut()
                                .take(self.arguments.len())
                                .skip(method_option.arguments.len() - 1)
                            {
                                argument_type_option
                                    .push(method_option.var_args_type.clone().unwrap());
                            }
                        }
                    }
                } else {
                    // Implicit constructor: each positional argument is expected
                    // to match the corresponding attribute's type, in
                    // declaration order (the synthetic vtable slot is skipped).
                    for (idx, (_, attribute)) in get_codegen_class
                        .as_ref()
                        .unwrap()
                        .attributes
                        .iter()
                        .filter(|(name, _)| name.as_str() != "<main_virtual_table>")
                        .enumerate()
                    {
                        if idx < argument_type_options.len() {
                            argument_type_options[idx].push(attribute.attribute_type.clone());
                        }
                    }
                }

                let mut constructor_arguments = Vec::new();
                let mut constructor_keyword_arguments = HashMap::new();

                let post_stack = codegen_context.module_context.step_back();
                for ((argument_name, argument), expected_type_options) in
                    self.arguments.iter().zip(argument_type_options)
                {
                    let current_expected_types =
                        codegen_context.current_expected_type_options.clone();
                    codegen_context.current_expected_type_options = Some(expected_type_options);

                    constructor_arguments.push(argument.build_value(codegen_context));

                    codegen_context.current_expected_type_options = current_expected_types;

                    if let Some(name) = argument_name {
                        constructor_keyword_arguments.insert(
                            name.value.clone(),
                            constructor_arguments.last().unwrap().clone(),
                        );
                    }
                }
                codegen_context.module_context.step_forward(post_stack);

                (constructor_arguments, constructor_keyword_arguments)
            };

        let (previous_line, previous_file) = codegen_context.track_call_position(
            self.start.file.to_string_lossy().into_owned(),
            self.start.line,
        );

        // A class with a declared `constructor` is built by calling it; a class
        // without one (including a value type or any methodful POD) is built by
        // the implicit attribute-list constructor below.
        let has_constructor = get_codegen_class
            .as_ref()
            .unwrap()
            .main_virtual_table
            .methods
            .contains_key("constructor");

        // Declared-constructor class: call `constructor` and report on overload
        // mismatch.
        if has_constructor
            && codegen_context
                .call_object_method(
                    &allocate_object,
                    "constructor".to_string(),
                    constructor_arguments.clone(),
                    if !constructor_keyword_arguments.is_empty() {
                        Some(constructor_keyword_arguments.clone())
                    } else {
                        None
                    },
                )
                .is_err()
        {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "no constructor of class `{}` matches the supplied argument types. Check the argument types against the class's declared constructors",
                        class_type
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        }

        // No declared constructor: the implicit one-arg-per-attribute or
        // keyword-args form. The synthetic vtable slot is not a user attribute,
        // so it takes no argument and is never written here.
        if !has_constructor {
            let constructor_attributes: Vec<_> = get_codegen_class
                .as_ref()
                .unwrap()
                .attributes
                .iter()
                .filter(|(name, _)| name.as_str() != "<main_virtual_table>")
                .map(|(name, attribute)| (name.clone(), attribute.clone()))
                .collect();

            if constructor_arguments.len() != constructor_attributes.len() {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "wrong number of arguments to implicit constructor of class `{}`. The implicit constructor takes one argument per attribute, in declaration order",
                            class_type
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return allocate_object;
            }

            if constructor_keyword_arguments.is_empty() {
                // Positional form.
                for (idx, ((attribute_name, attribute), attribute_value)) in constructor_attributes
                    .iter()
                    .zip(&constructor_arguments)
                    .enumerate()
                {
                    if !codegen_context.set_object_attribute(
                        &allocate_object,
                        attribute_name,
                        attribute_value,
                    ) {
                        codegen_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                self.arguments[idx].1.get_start().clone(),
                                self.arguments[idx].1.get_end().clone(),
                                format!(
                                    "cannot assign value of type `{}` to attribute of type `{}`. The value's type is not compatible with the attribute's declared type",
                                    attribute_value.value_type,
                                    attribute.attribute_type
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ));
                    }
                }
            } else {
                // Keyword form: missing keys take the attribute's zero value.
                for (idx, (attribute_name, attribute)) in constructor_attributes.iter().enumerate() {
                    let value_to_set = if constructor_keyword_arguments.contains_key(attribute_name)
                    {
                        constructor_keyword_arguments[attribute_name].clone()
                    } else {
                        codegen_context.build_zero_value(&attribute.attribute_type)
                    };

                    if !codegen_context.set_object_attribute(
                        &allocate_object,
                        attribute_name,
                        &value_to_set,
                    ) {
                        codegen_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                self.arguments[idx].1.get_start().clone(),
                                self.arguments[idx].1.get_end().clone(),
                                format!(
                                    "cannot assign value of type `{}` to attribute of type `{}`. The value's type is not compatible with the attribute's declared type",
                                    value_to_set.value_type,
                                    attribute.attribute_type
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ));
                    }
                }
            }
        }

        codegen_context.reset_call_position(&previous_line, &previous_file);
        allocate_object
    }
}

impl PekoValueBuilder for ObjectAccessAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let previous_return_references = codegen_context.return_references;
        codegen_context.return_references = false;

        // Mark / unmark "calling_object_method" state so the object
        // build_value below knows whether to leave the method-table
        // open.
        if let PekoAST::FunctionCall(_) = self.access.as_ref() {
            if !codegen_context
                .state
                .contains(&"calling_object_method".to_string())
            {
                codegen_context
                    .state
                    .push("calling_object_method".to_string());
            }
        } else if codegen_context
            .state
            .contains(&"calling_object_method".to_string())
            && let Some(idx) = codegen_context
                .state
                .iter()
                .position(|s| s == "calling_object_method")
        {
            codegen_context.state.remove(idx);
        }

        let object = self.object.build_value(codegen_context);

        let current_primary_object = codegen_context.primary_object.clone();
        let current_accessing_state = codegen_context.accessed_state.clone();

        if let Some(idx) = codegen_context
            .state
            .iter()
            .position(|s| s == "calling_object_method")
        {
            codegen_context.state.remove(idx);
        }

        codegen_context.return_references = previous_return_references;

        match self.access.as_ref() {
            PekoAST::FunctionCall(function_call) => {
                // The method name must be a plain identifier.
                let function_name = match function_call.function_reference.as_ref() {
                    PekoAST::VariableReference(variable_reference) => {
                        variable_reference.variable_name.value.clone()
                    }
                    _ => {
                        codegen_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                function_call.function_reference.get_start().clone(),
                                function_call.function_reference.get_end().clone(),
                                "expected identifier for method call".to_string(),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ),
                        );
                        return codegen_context.create_error_value();
                    }
                };

                // `enumValue.serialize(serializer)` has no method to dispatch
                // (an enum is not a class). Route it to the enum's generated
                // serialize helper `serialize_enum_<E>(value, serializer)`,
                // which writes the variant identifier string.
                if function_name == "serialize"
                    && codegen_context
                        .get_enum_variants(object.value_type.name())
                        .is_some()
                {
                    codegen_context.primary_object = None;
                    codegen_context.accessed_state = None;
                    let helper_call = enum_serde_helper_call(
                        &format!("serialize_enum_{}", object.value_type.name()),
                        std::iter::once((None, self.object.as_ref().clone()))
                            .chain(function_call.arguments.iter().cloned())
                            .collect(),
                        PositionData::default(),
                        PositionData::default(),
                    );
                    return helper_call.build_value(codegen_context);
                }

                // A method call on an erased generic-parameter value is
                // bound-driven. An `impl Trait` bound that declares the method
                // dispatches through the runtime itable scan on the thin
                // object. Anything else (a `from Class` bound, an unbounded
                // parameter, or an inherited Object method) retypes the thin
                // object to its carrier class and dispatches virtually, so a
                // concrete override is still selected.
                if object.value_type.is_generic_param() {
                    let mut arguments = Vec::new();
                    for (_, argument) in &function_call.arguments {
                        arguments.push(argument.build_value(codegen_context));
                    }
                    codegen_context.primary_object = None;
                    codegen_context.accessed_state = None;

                    let restraints =
                        codegen_context.generic_param_restraints(&object.value_type);
                    for restraint in &restraints {
                        if let TypeRestraint::Impl(trait_type) = restraint
                            && codegen_context
                                .get_trait(trait_type.name())
                                .map(|trait_definition| {
                                    trait_definition
                                        .methods
                                        .iter()
                                        .any(|method| method.name == function_name)
                                })
                                .unwrap_or(false)
                        {
                            return codegen_context.call_trait_method_erased(
                                &object,
                                trait_type,
                                &function_name,
                                arguments,
                            );
                        }
                    }

                    let carrier_class = restraints
                        .iter()
                        .find_map(|restraint| match restraint {
                            TypeRestraint::From(class_type) => Some(class_type.clone()),
                            TypeRestraint::Impl(_) => None,
                        })
                        .unwrap_or_else(|| PekoType::simple_type("Object"));
                    let mut as_carrier = object.clone();
                    as_carrier.value_type = carrier_class;
                    return match codegen_context.call_object_method(
                        &as_carrier,
                        &function_name,
                        arguments,
                        None,
                    ) {
                        Ok(value) => value,
                        Err(message) => {
                            codegen_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    self.access.get_start().clone(),
                                    self.access.get_end().clone(),
                                    message,
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ),
                            );
                            codegen_context.create_error_value()
                        }
                    };
                }

                // A method call on a trait-typed value dispatches through the
                // fat pointer's witness table rather than a known class vtable.
                if codegen_context
                    .get_trait(object.value_type.name())
                    .is_some()
                {
                    let mut arguments = Vec::new();
                    for (_, argument) in &function_call.arguments {
                        arguments.push(argument.build_value(codegen_context));
                    }
                    codegen_context.primary_object = None;
                    codegen_context.accessed_state = None;
                    return codegen_context.call_trait_method(&object, &function_name, arguments);
                }

                let class = match codegen_context.get_class_by_type(&object.value_type) {
                    Some(c) => c,
                    None => return codegen_context.create_error_value(),
                };

                // Method-named attribute on a class: treat as a function
                // attribute and call it directly.
                if class.attributes.contains_key(&function_name)
                    && (class.attributes[&function_name].attribute_type.is_closure()
                        || class.attributes[&function_name]
                            .attribute_type
                            .is_function())
                {
                    let attribute_function = codegen_context
                        .get_object_attribute(&object, function_name.clone(), true)
                        .unwrap();

                    codegen_context.accessed_state = None;
                    codegen_context.primary_object = None;

                    let argument_types = attribute_function.value_type.generics().to_vec();

                    // Attribute functions don't support overloading or
                    // varargs: argument counts must match exactly.
                    if function_call.arguments.len() != argument_types.len() {
                        codegen_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                self.access.get_start().clone(),
                                self.access.get_end().clone(),
                                format!(
                                    "wrong number of arguments to attribute function. The attribute's function type declares `{}` parameters but `{}` arguments were provided",
                                    argument_types.len(),
                                    function_call.arguments.len(),
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ));
                        return codegen_context.create_error_value();
                    }

                    let mut arguments = Vec::new();
                    let mut keyword_arguments = HashMap::new();

                    for ((argument_name, argument), expected_type) in
                        function_call.arguments.iter().zip(&argument_types)
                    {
                        let current_expected_types =
                            codegen_context.current_expected_type_options.clone();
                        codegen_context.current_expected_type_options =
                            Some(vec![expected_type.clone()]);
                        arguments.push(argument.build_value(codegen_context));
                        codegen_context.current_expected_type_options = current_expected_types;

                        if let Some(name) = argument_name {
                            keyword_arguments
                                .insert(name.value.clone(), arguments.last().unwrap().clone());
                        }
                    }

                    let mut boxed_arguments = Vec::new();
                    for (argument_index, (argument, argument_type)) in
                        arguments.iter().zip(argument_types.iter()).enumerate()
                    {
                        let boxed_argument =
                            codegen_context.box_value_to_type(argument_type, argument);

                        if boxed_argument.is_none() {
                            codegen_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    function_call.arguments[argument_index]
                                        .1
                                        .get_start()
                                        .clone(),
                                    function_call.arguments[argument_index].1.get_end().clone(),
                                    format!(
                                        "argument of type `{}` does not match expected type `{}`",
                                        argument.value_type, argument_type
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ),
                            );
                        }

                        boxed_arguments.push(boxed_argument.unwrap());
                    }

                    let (previous_line, previous_file) = codegen_context.track_call_position(
                        self.access.get_start().file.to_string_lossy().into_owned(),
                        self.access.get_start().line,
                    );

                    let attribute_call = codegen_context.call_function(
                        &attribute_function.value_type,
                        false,
                        attribute_function.llvm_value,
                        boxed_arguments,
                    );

                    codegen_context.reset_call_position(&previous_line, &previous_file);

                    return attribute_call;
                } else if !class
                    .main_virtual_table
                    .methods
                    .contains_key(&function_name)
                {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.access.get_start().clone(),
                            self.access.get_end().clone(),
                            format!(
                                "no method named `{}` on class `{}`. Check the method name and that it is declared on this class or a parent",
                                function_name,
                                class.class_type
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                let previous_primary_object = codegen_context.primary_object.clone();
                let previous_accessing_state = codegen_context.accessed_state.clone();

                codegen_context.primary_object = current_primary_object;
                codegen_context.accessed_state = current_accessing_state;

                let (previous_line, previous_file) = if !class.main_virtual_table.methods
                    [&function_name][0]
                    .read()
                    .unwrap()
                    .visibility
                    .notrack
                {
                    codegen_context.track_call_position(
                        self.access.get_start().file.to_string_lossy().into_owned(),
                        self.access.get_start().line,
                    )
                } else {
                    (
                        codegen_context.create_null_pointer(),
                        codegen_context.create_null_pointer(),
                    )
                };

                // Collect the expected argument-type sets across all
                // overloads, so each call-site argument can be
                // type-inferred against its valid type options.
                let method_options: Vec<CodegenFunction> = class.main_virtual_table.methods
                    [&function_name]
                    .iter()
                    .map(|option| option.read().unwrap().clone())
                    .collect();
                let mut argument_type_options = vec![Vec::new(); function_call.arguments.len()];

                for method_option in method_options {
                    if (method_option.arguments.len() - 1) != function_call.arguments.len()
                        || (function_call.arguments.len() > (method_option.arguments.len() - 1)
                            && method_option.var_args_type.is_none())
                    {
                        continue;
                    }

                    for (idx, (_, argument)) in method_option.arguments.iter().skip(1).enumerate() {
                        argument_type_options[idx].push(argument.argument_type.clone());
                    }

                    if function_call.arguments.len() > (method_option.arguments.len() - 1)
                        && method_option.var_args_type.is_some()
                    {
                        for argument_type_option in argument_type_options
                            .iter_mut()
                            .take(function_call.arguments.len())
                            .skip(method_option.arguments.len() - 1)
                        {
                            argument_type_option.push(method_option.var_args_type.clone().unwrap());
                        }
                    }
                }

                let mut arguments = Vec::new();
                let mut keyword_arguments = HashMap::new();

                for ((argument_name, argument), expected_type_options) in
                    function_call.arguments.iter().zip(&argument_type_options)
                {
                    let current_expected_types =
                        codegen_context.current_expected_type_options.clone();
                    codegen_context.current_expected_type_options =
                        Some(expected_type_options.clone());
                    arguments.push(argument.build_value(codegen_context));
                    codegen_context.current_expected_type_options = current_expected_types;

                    if let Some(name) = argument_name {
                        keyword_arguments
                            .insert(name.value.clone(), arguments.last().unwrap().clone());
                    }
                }

                let keyword_values = if keyword_arguments.is_empty() {
                    None
                } else {
                    Some(keyword_arguments)
                };

                let method_call = codegen_context.call_object_method(
                    &object,
                    function_name.clone(),
                    arguments,
                    keyword_values,
                );

                if !class.main_virtual_table.methods[&function_name][0]
                    .read()
                    .unwrap()
                    .visibility
                    .notrack
                {
                    codegen_context.reset_call_position(&previous_line, &previous_file);
                }

                codegen_context.primary_object = previous_primary_object;
                codegen_context.accessed_state = previous_accessing_state;

                match method_call {
                    Err(err_msg) => {
                        codegen_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.object.get_start().clone(),
                                self.access.get_end().clone(),
                                err_msg,
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ),
                        );
                        codegen_context.create_error_value()
                    }
                    Ok(v) => v,
                }
            }

            PekoAST::VariableReference(variable_reference) => {
                let variable_name = variable_reference.variable_name.value.clone();

                // Closures have just two pseudo-attributes (`function`
                // and `context`) that expose the underlying parts of
                // the closure pair.
                if object.value_type.is_closure() {
                    match variable_name.as_str() {
                        "function" => {
                            let mut function_type = object.value_type.clone();
                            function_type.set_closure(false);
                            function_type
                                .generics_mut()
                                .insert(0, managed_pointer_type(PekoType::simple_type("void")));
                            if !function_type.is_function() {
                                function_type
                                    .set_function_return(Some(PekoType::simple_type("void")));
                            }

                            let closure_function_element =
                                codegen_context.get_struct_element(&object, &function_type, 1);
                            return codegen_context.load_value(&closure_function_element);
                        }
                        "context" => {
                            let closure_context_element = codegen_context.get_struct_element(
                                &object,
                                &managed_pointer_type(PekoType::simple_type("void")),
                                0,
                            );
                            return codegen_context.load_value(&closure_context_element);
                        }
                        _ => {
                            codegen_context
                                .diagnostics
                                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                    variable_reference.variable_name.start.clone(),
                                    variable_reference.variable_name.end.clone(),
                                    format!(
                                        "`{}` is not a valid attribute of a closure. Closures only have `function` and `context` attributes",
                                        variable_name
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ));
                            return codegen_context.create_error_value();
                        }
                    }
                }

                let class = match codegen_context.get_class_by_type(&object.value_type) {
                    Some(c) => c,
                    None => return codegen_context.create_error_value(),
                };

                // Method-named identifier: return the vtable method pointer.
                if class
                    .main_virtual_table
                    .methods
                    .contains_key(&variable_name)
                {
                    let object_vtable = codegen_context.get_object_vtable(&object, true);
                    let first_method = class.main_virtual_table.methods[&variable_name][0]
                        .read()
                        .unwrap()
                        .clone();
                    return codegen_context.get_vtable_method(
                        &object_vtable,
                        class.main_virtual_table.llvm_type,
                        &first_method.get_type(),
                        first_method.virtual_table_index,
                        true,
                    );
                }

                let reference = codegen_context.get_object_attribute(
                    &object,
                    variable_name,
                    !codegen_context.return_references,
                );

                match reference {
                    Err(err_msg) => {
                        codegen_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variable_reference.variable_name.start.clone(),
                                variable_reference.variable_name.end.clone(),
                                err_msg,
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ),
                        );
                        codegen_context.create_error_value()
                    }
                    Ok(v) => v,
                }
            }

            PekoAST::VariableReassignment(variable_reassignment) => {
                // Object attribute reassignment, e.g. `obj.attr = value`.
                let variable_name = match variable_reassignment.variable_reference.as_ref() {
                    PekoAST::VariableReference(variable_reference) => {
                        variable_reference.variable_name.value.clone()
                    }
                    _ => {
                        codegen_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                variable_reassignment.variable_reference.get_start().clone(),
                                variable_reassignment.variable_reference.get_end().clone(),
                                "expected identifier for attribute".to_string(),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ),
                        );
                        return codegen_context.create_error_value();
                    }
                };

                // Reassigning `this.attr` in a constructor satisfies
                // the still-needs-to-be-set tracker.
                let object_name = match self.object.as_ref() {
                    PekoAST::VariableReference(variable_reference) => {
                        Some(variable_reference.variable_name.value.clone())
                    }
                    _ => None,
                };

                if object_name.as_deref() == Some("this")
                    && codegen_context.attributes_to_set.contains(&variable_name)
                    && let Some(pos) = codegen_context
                        .attributes_to_set
                        .iter()
                        .position(|key| key.as_str() == variable_name)
                {
                    codegen_context.attributes_to_set.remove(pos);
                }

                let object_class = match codegen_context.get_class_by_type(&object.value_type) {
                    Some(c) => c,
                    None => return codegen_context.create_error_value(),
                };

                let previous_expected_type = codegen_context.current_expected_type_options.clone();
                if object_class.attributes.contains_key(&variable_name) {
                    codegen_context.current_expected_type_options = Some(vec![
                        object_class.attributes[&variable_name]
                            .attribute_type
                            .clone(),
                    ]);
                }

                let mut variable_value = variable_reassignment
                    .variable_value
                    .build_value(codegen_context);

                if let Some(assignment_op) = &variable_reassignment.assignment_operator {
                    let attribute =
                        codegen_context.get_object_attribute(&object, variable_name.clone(), true);

                    let attribute = match attribute {
                        Err(err_msg) => {
                            codegen_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    variable_reassignment.variable_reference.get_start().clone(),
                                    variable_reassignment.variable_reference.get_end().clone(),
                                    err_msg,
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ),
                            );
                            codegen_context.accessed_state = None;
                            codegen_context.primary_object = None;
                            return codegen_context.create_error_value();
                        }
                        Ok(v) => v,
                    };

                    let try_operator =
                        codegen_context.apply_operator(assignment_op, &attribute, &variable_value);

                    match try_operator {
                        None => {
                            codegen_context
                                .diagnostics
                                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                    variable_reassignment.variable_reference.get_start().clone(),
                                    variable_reassignment.variable_reference.get_end().clone(),
                                    format!(
                                        "cannot apply operator `{}` between attribute of type `{}` and value of type `{}`. There is no operator overload that accepts these two operand types",
                                        assignment_op,
                                        attribute.value_type,
                                        variable_value.value_type
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ));
                            codegen_context.accessed_state = None;
                            codegen_context.primary_object = None;
                            return codegen_context.create_error_value();
                        }
                        Some(v) => variable_value = v,
                    }
                }

                codegen_context.current_expected_type_options = previous_expected_type;

                if !codegen_context.set_object_attribute(&object, variable_name, &variable_value) {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            variable_reassignment.variable_reference.get_start().clone(),
                            variable_reassignment.variable_reference.get_end().clone(),
                            format!(
                                "cannot assign value of type `{}` to attribute of type `{}`. The value's type is not compatible with the attribute's declared type",
                                variable_value.value_type,
                                // Best-effort: we don't have the attribute type readily
                                // available here, so fall back to the value's type.
                                variable_value.value_type
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    codegen_context.accessed_state = None;
                    codegen_context.primary_object = None;
                    return codegen_context.create_error_value();
                }

                codegen_context.accessed_state = None;
                codegen_context.primary_object = None;
                codegen_context.create_null_pointer()
            }
            _ => codegen_context.create_error_value(),
        }
    }
}

impl PekoValueBuilder for ArrayAccessAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let return_references = codegen_context.return_references;
        codegen_context.return_references = false;

        let array = self.array.build_value(codegen_context);
        let access = self.access.build_value(codegen_context);

        codegen_context.return_references = return_references;

        // Object case: dispatch to the Index / IndexRef trait method.
        if (array.value_type.array_depth == 0 && array.value_type.reference_depth == 0)
            && codegen_context
                .get_class_by_type(&array.value_type)
                .is_some()
        {
            let method = if codegen_context.return_references {
                String::from("index_ref")
            } else {
                String::from("index")
            };

            let (previous_line, previous_file) = codegen_context.track_call_position(
                self.start.file.to_string_lossy().into_owned(),
                self.start.line,
            );

            let access_call =
                codegen_context.call_object_method(&array, method, vec![access.clone()], None);

            codegen_context.reset_call_position(&previous_line, &previous_file);

            return match access_call {
                Err(_) => {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.array.get_start().clone(),
                            self.array.get_end().clone(),
                            format!("cannot index into value of type `{}`", array.value_type,),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    codegen_context.create_error_value()
                }
                Ok(v) => v,
            };
        } else if !array.value_type.is_pointer() && !is_managed_pointer(&array.value_type) {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.array.get_start().clone(),
                    self.array.get_end().clone(),
                    format!("value of type `{}` is not an array", array.value_type,),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // Plain pointer-array case: box the index to `int64` and emit a GEP.
        let access_boxed =
            codegen_context.box_value_to_type(&PekoType::simple_type("i64"), &access);

        if access_boxed.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.access.get_start().clone(),
                    self.access.get_end().clone(),
                    format!(
                        "cannot index into array with index of type `{}`",
                        access.value_type,
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        let array_element = codegen_context.get_array_element(&array, &access_boxed.unwrap());
        if codegen_context.return_references {
            array_element
        } else {
            codegen_context.load_value(&array_element)
        }
    }
}

impl PekoValueBuilder for UnwrapAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let optional = self.optional.build_value(codegen_context);

        // The operand must be an optional. Everything else is a type error.
        let is_option = codegen_context
            .expand_type(&optional.value_type)
            .map(|expanded| expanded.name() == "Option")
            .unwrap_or(false);
        if !is_option {
            if !optional.value_type.is_error_type() {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.optional.get_start().clone(),
                        self.optional.get_end().clone(),
                        format!(
                            "cannot unwrap non-optional value of type `{}`",
                            optional.value_type
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }
            return codegen_context.create_error_value();
        }

        // The held type T, recovered for the result and the fallback phi.
        let inner_type = optional
            .value_type
            .optional_get_inner_type()
            .unwrap_or_else(|| PekoType::simple_type("Object"));

        // Branch on is_value: the value block unwraps, the fail block
        // propagates, halts, or runs the fallback.
        let is_value_call = codegen_context
            .call_object_method(&optional, "is_value", Vec::new(), None)
            .unwrap_or_else(|_| codegen_context.create_error_value());
        let is_value_raw = codegen_context.to_raw_bool(&is_value_call);

        let value_block = codegen_context.create_new_block(None);
        let fail_block = codegen_context.create_new_block(None);
        let merge_block = codegen_context.create_new_block(None);

        codegen_context.build_conditional_branch(&is_value_raw, value_block, fail_block);

        // Value block: unwrap the held value.
        codegen_context.goto_block_end(value_block);
        let unwrapped = codegen_context
            .call_object_method(&optional, "unwrap", Vec::new(), None)
            .unwrap_or_else(|_| codegen_context.create_error_value());
        let value_incoming = codegen_context.current_basic_block.unwrap();
        codegen_context.build_branch(merge_block);

        // Fail block.
        codegen_context.goto_block_end(fail_block);

        if let Some(else_body) = &self.else_body {
            // `expr? else { ... }`: the block yields a fallback value on a None
            // or Error. No backtrace frame is added; the failure is handled.
            let previous_expecting = codegen_context.expecting_value;
            let body_length = else_body.value.len();
            let mut fallback: CodegenValue = codegen_context.create_null_pointer();
            for (index, statement) in else_body.value.iter().enumerate() {
                codegen_context.expecting_value = index + 1 == body_length;
                fallback = statement.build_value(codegen_context);
            }
            codegen_context.expecting_value = previous_expecting;

            // A fallback block that diverges, such as `else { return ... }`,
            // already carries its own terminator. It never reaches the merge
            // block and contributes no phi incoming. The unwrapped value flows
            // straight through, matching the propagate and halt paths.
            let fail_incoming = codegen_context.current_basic_block.unwrap();
            let fail_diverges =
                unsafe { !core::LLVMGetBasicBlockTerminator(fail_incoming).is_null() };
            if fail_diverges {
                codegen_context.goto_block_end(merge_block);
                return unwrapped;
            }

            let fallback = codegen_context
                .box_value_to_type(&inner_type, &fallback)
                .unwrap_or(fallback);
            codegen_context.build_branch(merge_block);

            codegen_context.goto_block_end(merge_block);
            let phi_type = unsafe { core::LLVMTypeOf(unwrapped.llvm_value) };
            let phi = unsafe {
                core::LLVMBuildPhi(codegen_context.llvm_builder, phi_type, c"".as_ptr())
            };
            unsafe {
                core::LLVMAddIncoming(
                    phi,
                    vec![unwrapped.llvm_value, fallback.llvm_value].as_mut_ptr(),
                    vec![value_incoming, fail_incoming].as_mut_ptr(),
                    2,
                );
            }
            return CodegenValue::new(phi, inner_type);
        }

        // Propagate or halt: record this site in the backtrace first.
        let file_value =
            codegen_context.create_string(self.start.file.to_string_lossy().into_owned());
        let line_value = codegen_context.create_constant_int(self.start.line as i32);
        let column_value = codegen_context.create_constant_int(self.start.column as i32);
        let _ = codegen_context.call_object_method(
            &optional,
            "add_context",
            vec![file_value, line_value, column_value],
            None,
        );

        // A function returning an optional propagates; any other halts.
        let returns_optional = codegen_context
            .current_return_type
            .clone()
            .and_then(|return_type| codegen_context.expand_type(&return_type))
            .map(|expanded| expanded.name() == "Option")
            .unwrap_or(false);

        if returns_optional {
            let return_type = codegen_context.current_return_type.clone().unwrap();
            let propagated = codegen_context
                .box_value_to_type(&return_type, &optional)
                .unwrap_or_else(|| optional.clone());
            codegen_context.build_return(Some(propagated));
        } else {
            let _ =
                codegen_context.call_object_method(&optional, "halt", Vec::new(), None);
            unsafe {
                core::LLVMBuildUnreachable(codegen_context.llvm_builder);
            }
        }

        // The fail block diverged, so the merge block's only predecessor is the
        // value block; the unwrapped value flows straight through.
        codegen_context.goto_block_end(merge_block);
        unwrapped
    }
}

impl PekoValueBuilder for CastAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // A `constant<T>(value)` emits a compile-time LLVM constant of the FFI
        // type `T` directly from a literal.
        if self.kind == CastKind::Constant {
            let Some(target_llvm) = codegen_context.get_llvm_type(&self.cast_to) else {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.cast_to.start_position.clone(),
                        self.cast_to.end_position.clone(),
                        format!("type `{}` is not defined", self.cast_to),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            };

            let constant_value = match self.value.as_ref() {
                // A numeric literal is emitted at the target FFI type: a float
                // target is a real constant (so `constant<f64>(7)` is 7.0, not
                // the integer 7 reinterpreted as float bits), an integer target
                // is an integer constant.
                PekoAST::Number(number) => unsafe {
                    if self.cast_to.is_float() {
                        core::LLVMConstReal(target_llvm, number.value.value)
                    } else {
                        core::LLVMConstInt(target_llvm, number.value.value as i64 as u64, 1)
                    }
                },
                PekoAST::Boolean(boolean) => unsafe {
                    core::LLVMConstInt(target_llvm, boolean.value.value as u64, 0)
                },
                PekoAST::Char(character) => unsafe {
                    core::LLVMConstInt(target_llvm, character.value.value as u64, 0)
                },
                PekoAST::String(string) if !string.interpolated => {
                    // A string literal becomes a C string constant (a pointer
                    // into a global `i8` array), reinterpreted to the target
                    // pointer type.
                    let text: String = string
                        .chunks
                        .iter()
                        .filter_map(|chunk| match &chunk.content {
                            StringChunkContent::Text(piece) => Some(piece.clone()),
                            StringChunkContent::Interpolation(_) => None,
                        })
                        .collect();
                    let cstring = codegen_context.create_cstring(text);
                    return CodegenValue::new(cstring.llvm_value, self.cast_to.clone());
                }
                _ => {
                    // A non-literal value is built normally and reinterpreted to
                    // the target type.
                    return CodegenValue::new(
                        self.value.build_value(codegen_context).llvm_value,
                        self.cast_to.clone(),
                    );
                }
            };

            return CodegenValue::new(constant_value, self.cast_to.clone());
        }

        let value = self.value.build_value(codegen_context);

        // Casting an object to a trait builds a fat pointer { self, witness }.
        // This covers both `value as Trait` (when the static type carries the
        // trait) and `danger_cast<Trait>(value)`.
        if codegen_context.get_trait(self.cast_to.name()).is_some() {
            return codegen_context.build_trait_object(&value, &self.cast_to);
        }

        // A forced `danger_cast<T>(value)` numerically converts or reinterprets
        // without a safety check.
        if self.kind == CastKind::Forced {
            return codegen_context.typecast_number_value(&value, &self.cast_to);
        }

        let boxed_value = codegen_context.box_value_to_type(&self.cast_to, &value);

        match boxed_value {
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.value.get_start().clone(),
                        self.value.get_end().clone(),
                        format!(
                            "value of type `{}` cannot be cast to type `{}`",
                            value.value_type, self.cast_to
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
            Some(v) => v,
        }
    }
}

impl PekoValueBuilder for ModuleAccessAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Enum variant access: `Enum::Variant` lowers to the variant's
        // zero-based index as a 32-bit integer, typed as the enum.
        if self.module_names.len() == 1
            && let Some(variants) = codegen_context.get_enum_variants(&self.module_names[0].value)
            && let PekoAST::VariableReference(variant_reference) = self.accessor.as_ref()
        {
            let index = variants
                .iter()
                .position(|variant| variant == &variant_reference.variable_name.value)
                .unwrap_or(0);

            let constant = codegen_context.create_constant_int(index as i32);
            return CodegenValue::new(
                constant.llvm_value,
                PekoType::simple_type(&self.module_names[0].value),
            );
        }

        // Enum deserialize: `Enum::deserialize(d)`. An enum has no static
        // method, so route it to the enum's generated deserialize helper
        // `deserialize_enum_<E>(d)`, which reads a variant identifier string
        // back (Error on an unknown one).
        if self.module_names.len() == 1
            && codegen_context
                .get_enum_variants(&self.module_names[0].value)
                .is_some()
            && let PekoAST::FunctionCall(call) = self.accessor.as_ref()
            && let PekoAST::VariableReference(method_reference) = call.function_reference.as_ref()
            && method_reference.variable_name.value == "deserialize"
        {
            let helper_call = enum_serde_helper_call(
                &format!("deserialize_enum_{}", self.module_names[0].value),
                call.arguments.clone(),
                PositionData::default(),
                        PositionData::default(),
            );
            return helper_call.build_value(codegen_context);
        }

        // Static method access: `Type::method(args)`. The head names a class
        // (not a module) and the accessor is a call. A class with a matching
        // `static` method dispatches directly to that concrete type with no
        // receiver; a class whose method is instance-only is an error here.
        if self.module_names.len() == 1
            && let PekoAST::FunctionCall(call) = self.accessor.as_ref()
            && let PekoAST::VariableReference(method_reference) = call.function_reference.as_ref()
        {
            let class_type = PekoType::from_string(
                self.module_names[0].value.as_str(),
                codegen_context.get_current_file(),
            );
            let method_name = method_reference.variable_name.value.clone();

            if let Some(class) = codegen_context.get_class_by_type(&class_type) {
                let type_name = &self.module_names[0].value;
                let overloads = class.main_virtual_table.methods.get(&method_name);
                let has_static = overloads.is_some_and(|overloads| {
                    overloads
                        .iter()
                        .any(|function| function.read().unwrap().is_static)
                });

                if has_static {
                    let mut argument_values = Vec::new();
                    for (_, argument) in &call.arguments {
                        argument_values.push(argument.build_value(codegen_context));
                    }

                    return match codegen_context.call_static_method(
                        &class_type,
                        &method_name,
                        argument_values,
                    ) {
                        Ok(value) => value,
                        Err(message) => {
                            codegen_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    method_reference.variable_name.start.clone(),
                                    method_reference.variable_name.end.clone(),
                                    message,
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ),
                            );
                            codegen_context.create_error_value()
                        }
                    };
                }

                // The head is a class, so this is a type-level call, not a
                // module access. Report a precise error instead of falling
                // through to module resolution.
                let message = if overloads.is_some() {
                    format!(
                        "method `{method_name}` on type `{type_name}` is not static, so it cannot be called as `{type_name}::{method_name}(...)`. Call it on an instance instead",
                    )
                } else {
                    format!(
                        "type `{type_name}` has no static method `{method_name}`",
                    )
                };
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        method_reference.variable_name.start.clone(),
                        method_reference.variable_name.end.clone(),
                        message,
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }
        }

        // Resolve the first segment of the path: the importing module's own
        // aliases first (so a local import name wins), then a submodule, then
        // a top-level imported module.
        let mut next_module = if codegen_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .module_aliases
            .contains_key(&self.module_names[0].value)
        {
            codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .module_aliases[&self.module_names[0].value]
                .clone()
        } else if codegen_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .modules
            .contains_key(&self.module_names[0].value)
        {
            codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .modules[&self.module_names[0].value]
                .clone()
        } else if codegen_context
            .module_context
            .top_level_modules
            .contains_key(&self.module_names[0].value)
            && codegen_context.module_context.top_level_modules[&self.module_names[0].value]
                .read()
                .unwrap()
                .get_top_level()
                .unwrap()
                .is_imported_by(
                    codegen_context
                        .module_context
                        .current_module()
                        .read()
                        .unwrap()
                        .get_uuid()
                        .unwrap(),
                )
        {
            codegen_context.module_context.top_level_modules[&self.module_names[0].value].clone()
        } else {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.module_names[0].start.clone(),
                    self.module_names[0].end.clone(),
                    format!(
                        "cannot find module `{}` in the current scope. Check the module name, that the module is declared, and that it is imported",
                        self.module_names[0].value
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        };

        // Walk the rest of the path one segment at a time.
        for i in 1..self.module_names.len() {
            if !next_module
                .read()
                .unwrap()
                .get_modules()
                .contains_key(&self.module_names[i].value)
            {
                // Self-reference of the path: skip and exit (the access
                // is the module itself).
                if next_module.read().unwrap().get_name() == self.module_names[i].value {
                    break;
                }

                // `module::Enum::Variant`: the final segment names an enum in
                // this module, lowering to the variant's zero-based index.
                if i == self.module_names.len() - 1
                    && let PekoAST::VariableReference(variant_reference) = self.accessor.as_ref()
                    && let Some(definition) = next_module
                        .read()
                        .unwrap()
                        .get_enums()
                        .get(&self.module_names[i].value)
                        .cloned()
                {
                    // A qualified path reaches this enum from another module,
                    // so a `[private]` enum is out of bounds.
                    if definition.private {
                        codegen_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                self.module_names[i].start.clone(),
                                variant_reference.variable_name.end.clone(),
                                format!(
                                    "cannot access private enum `{}` from outside its module. Remove the `[private]` modifier to export it",
                                    self.module_names[i].value,
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ));
                        return codegen_context.create_error_value();
                    }

                    let index = definition
                        .variants
                        .iter()
                        .position(|variant| variant == &variant_reference.variable_name.value)
                        .unwrap_or(0);
                    let constant = codegen_context.create_constant_int(index as i32);
                    return CodegenValue::new(
                        constant.llvm_value,
                        PekoType::simple_type(&self.module_names[i].value),
                    );
                }

                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.module_names[i].start.clone(),
                        self.module_names[i].end.clone(),
                        format!(
                            "cannot find module `{}` in the current scope. Check the module name, that the module is declared, and that it is imported",
                            self.module_names[i].value
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }

            next_module = Arc::clone(
                &Arc::clone(&next_module).read().unwrap().get_modules()
                    [&self.module_names[i].value],
            );

            // Report private-module access but continue to allow the
            // access to be simulated (so further errors still surface).
            if next_module.read().unwrap().get_visibility().private {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.module_names[i].start.clone(),
                        self.module_names[i].end.clone(),
                        format!(
                            "cannot access private module `{}`",
                            self.module_names[i].value
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }
        }

        codegen_context
            .module_context
            .move_to_module(next_module, true, false);

        let accessor = self.accessor.as_ref().build_value(codegen_context);

        codegen_context.module_context.move_out_of_module();
        accessor
    }
}

impl PekoValueBuilder for VariableReferenceAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        codegen_context.accessed_state = None;
        codegen_context.primary_object = None;

        // `None` literal: requires an inference type that is some
        // `Option<T>`, against which we construct an empty optional.
        if self.variable_name.value == "None" {
            if codegen_context.current_expected_type_options.is_none() {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        "cannot infer current type for None value".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }

            for expected_type_option in &codegen_context
                .current_expected_type_options
                .clone()
                .unwrap()
            {
                if !codegen_context.type_exists(expected_type_option) {
                    continue;
                }

                let expand_option = codegen_context.expand_type(expected_type_option).unwrap();
                if expand_option.name() == "Option" && expand_option.generics().len() == 1 {
                    let create_option = codegen_context.create_object(&expand_option, Vec::new());
                    if let Some(v) = create_option {
                        capture_optional_origin(
                            codegen_context,
                            &v,
                            &self.variable_name.start,
                        );
                        return v;
                    }
                }
            }

            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_name.start.clone(),
                    self.variable_name.end.clone(),
                    "cannot infer current type for None value".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();

        // `Default` literal: requires an inference type, and produces
        // its zero value.
        } else if self.variable_name.value == "Default" {
            codegen_context.accessed_state = None;
            codegen_context.primary_object = None;

            if codegen_context.current_expected_type_options.is_none() {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        "cannot create Default value without an inference type".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }

            let expected_type = codegen_context
                .current_expected_type_options
                .as_ref()
                .unwrap()[0]
                .clone();
            return codegen_context.build_zero_value(&expected_type);
        }

        // Locate the variable in (in order): the local scope, the
        // current module's globals, the current `this` object, or
        // upward in enclosing scopes.
        let variable_reference = if codegen_context
            .scoped_variables
            .contains_key(&self.variable_name.value)
        {
            codegen_context.accessed_state = None;
            codegen_context.primary_object = None;
            codegen_context.scoped_variables[&self.variable_name.value].clone()
        } else if codegen_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_variables()
            .contains_key(&self.variable_name.value)
        {
            codegen_context.accessed_state = None;
            codegen_context.primary_object = None;

            let variable_reference = codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .get_variables()[&self.variable_name.value]
                .read()
                .unwrap()
                .clone();

            if variable_reference.variable_visibility.private
                && codegen_context.module_context.accessing_current_module()
            {
                let module_name = codegen_context
                    .module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .get_name()
                    .to_owned();
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        format!(
                            "cannot access private global variable `{}` of module `{}` from outside that module. Mark the variable `pub` or access it from within its declaring module",
                            self.variable_name.value, module_name,
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }

            variable_reference
        } else if codegen_context.current_this.is_some()
            && codegen_context
                .get_class_by_type(&codegen_context.current_this.clone().unwrap().variable_type)
                .is_some()
            && codegen_context
                .get_class_by_type(&codegen_context.current_this.clone().unwrap().variable_type)
                .unwrap()
                .attributes
                .contains_key(&self.variable_name.value)
        {
            let current_uuid = codegen_context.get_owning_module_uuid();
            let load_current_this = codegen_context.load_value(
                &codegen_context.current_this.clone().unwrap().variable_value[&current_uuid],
            );
            let attribute = codegen_context
                .get_object_attribute(
                    &load_current_this,
                    self.variable_name.value.clone(),
                    !codegen_context.return_references,
                )
                .unwrap();

            codegen_context.previous_was_this = true;
            return attribute;
        } else {
            // Search upward for a global variable or function with this name.
            let find_global = match codegen_context
                .find_global_variable_in_current(&self.variable_name.value)
            {
                Some(global) => global,
                None => match codegen_context.find_function_in_current(&self.variable_name.value) {
                    Some(global_function) => {
                        return global_function[0].function_value
                            [&codegen_context.get_owning_module_uuid()]
                            .clone();
                    }
                    None => {
                        codegen_context
                                .diagnostics
                                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                    self.variable_name.start.clone(),
                                    self.variable_name.end.clone(),
                                    format!(
                                        "cannot find symbol `{}` in the current scope. Check the spelling, that the symbol is declared, and that it is imported into this module",
                                        self.variable_name.value
                                    ),
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ));
                        return codegen_context.create_error_value();
                    }
                },
            };

            if find_global.variable_visibility.private
                && codegen_context.module_context.accessing_current_module()
            {
                let module_name = codegen_context
                    .module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .name
                    .clone();
                let module_file = codegen_context
                    .module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .file
                    .clone();
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        format!(
                            "cannot access private global variable `{}` of module `{}` from outside that module. Mark the variable `pub` or access it from within its declaring module",
                            self.variable_name.value, module_name,
                        ),
                        diagnostics::DiagnosticType::Error,
                        module_file,
                    ));
            }

            find_global
        };

        // Reference path: return the variable's allocation directly,
        // unless the variable is constant (constants cannot be mutated
        // and therefore cannot be borrowed as references).
        if codegen_context.return_references {
            if variable_reference.variable_visibility.constant {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_name.start.clone(),
                        self.variable_name.end.clone(),
                        format!(
                            "cannot mutate constant variable `{}`",
                            self.variable_name.value
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }

            return variable_reference.variable_value[&codegen_context.get_owning_module_uuid()]
                .clone();
        }

        // Value path: load through the allocation.
        let current_uuid = codegen_context.get_owning_module_uuid();
        codegen_context.load_value(&variable_reference.variable_value[&current_uuid].clone())
    }
}

impl PekoValueBuilder for RangeAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let range_start = self.range_from.build_value(codegen_context);
        let range_start_boxed =
            codegen_context.box_value_to_type(&PekoType::simple_type("i32"), &range_start);

        if range_start_boxed.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.range_from.get_start().clone(),
                    self.range_from.get_end().clone(),
                    format!(
                        "type of range start, `{}`, is not compatible with expected `int` type",
                        range_start.value_type
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        }

        let range_end = self.range_to.build_value(codegen_context);
        let range_end_boxed =
            codegen_context.box_value_to_type(&PekoType::simple_type("i32"), &range_end);

        if range_end_boxed.is_none() {
            // Note: positions previously pointed to `range_from` here
            // (a long-standing bug; diagnostic for the `range_to`
            // error pointed to the start instead). Fixed to use
            // `range_to`.
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.range_to.get_start().clone(),
                    self.range_to.get_end().clone(),
                    format!(
                        "type of range end, `{}`, is not compatible with expected `int` type",
                        range_end.value_type
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        }

        codegen_context
            .call_named_function(
                "core::range",
                vec![range_start_boxed.unwrap(), range_end_boxed.unwrap()],
            )
            .unwrap_or_else(|| codegen_context.create_error_value())
    }
}

impl PekoValueBuilder for FunctionCallAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        codegen_context.accessed_state = None;
        codegen_context.primary_object = None;

        // The expected type at the call site, before building arguments clobbers
        // it. The Error built-in infers its Option type from this.
        let entry_expected_type_options = codegen_context.current_expected_type_options.clone();

        // FunctionCallASTs represent three syntactic cases:
        //
        //  1. Normal call by identifier:    `defined_function(arg1, arg2)`
        //  2. Call by expression:           `function_list[0](arg1, arg2)`
        //  3. Object instantiation:         `Class1(constructor_args)`
        //
        // Each case has different lookup and dispatch logic. On top of
        // that, generic calls can have type parameters inferred from
        // the argument types or the surrounding inference type, and
        // non-generic functions can be overloaded.
        //
        // We do this in three steps:
        //
        //  1. Simulate the arguments in the call's module context.
        //  2. Resolve the function info via one of:
        //     a. Function call from expression: simulate, pull type info.
        //     b. Function call from identifier: find the function, pick
        //        the best overload, save its info.
        //     c. Object instantiation: convert to an
        //        ObjectConstructionAST and simulate that.
        //  3. Using the collected function info, simulate the call.

        // --- Step 2: Function information collection ---

        let function_name = match self.function_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                Some(variable_reference.variable_name.clone())
            }
            _ => None,
        };

        // Built-in `deserialize<T>(source)`: reads a fresh `T` from a
        // Deserializer. It lowers to `T::deserialize(source as Deserializer)`,
        // so `source` is any Deserializer (for example `json::reader(value)`).
        // T is concrete here, so the static call resolves.
        if let Some(function_name) = &function_name
            && function_name.value == "deserialize"
            && self.function_generics.len() == 1
            && self.arguments.len() == 1
        {
            let target_type = self.function_generics[0].clone();
            let source = self.arguments[0].1.build_value(codegen_context);
            let deserializer_type = PekoType::simple_type("Deserializer");
            let deserializer = codegen_context.build_trait_object(&source, &deserializer_type);

            return match codegen_context.call_static_method(
                &target_type,
                "deserialize",
                vec![deserializer],
            ) {
                Ok(value) => value,
                Err(message) => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            message,
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    codegen_context.create_error_value()
                }
            };
        }

        // Built-in pseudo-functions: `sizeof<T>()`, `Error(msg)`,
        // `__rt_peko_alloc<T>(count)`, and `cstring("literal")`.
        if let Some(function_name) = &function_name
            && (function_name.value == "sizeof"
                || function_name.value == "Error"
                || function_name.value == "__rt_peko_alloc"
                || function_name.value == "cstring")
        {
            // `cstring("literal")` is handled before generic argument
            // building: it must read the raw literal text from the argument
            // AST and emit a raw (address space 0) C string, rather than let
            // the argument be built into a managed `string` first.
            if function_name.value == "cstring" {
                if self.arguments.len() != 1 {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`cstring` requires exactly one string-literal argument, e.g. `cstring(\"text\")`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                // The argument must be a plain (non-interpolated) string
                // literal: a raw C string has no place to evaluate an
                // interpolation.
                if let PekoAST::String(string_ast) = &self.arguments[0].1
                    && !string_ast.interpolated
                {
                    let text = if string_ast.chunks.is_empty() {
                        String::new()
                    } else {
                        string_ast.chunks[0].get_text()
                    };
                    return codegen_context.create_cstring(text);
                }

                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        "`cstring` requires a plain string literal (no interpolation), e.g. `cstring(\"text\")`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }

            // Arguments are generated in the call module (the
            // surrounding module-access may have shifted current
            // module to the definition module; we step back to the
            // *call* module here).
            let mut arguments = Vec::new();
            let mut keyword_args = HashMap::new();

            let post_stack = codegen_context.module_context.step_back();
            for (argument_name, argument) in self.arguments.iter() {
                let generated_argument = argument.build_value(codegen_context);
                arguments.push(generated_argument.clone());

                if let Some(name) = argument_name {
                    keyword_args.insert(name.value.clone(), generated_argument);
                }
            }
            codegen_context.module_context.step_forward(post_stack);

            if function_name.value == "sizeof" {
                if self.function_generics.len() != 1 {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`sizeof` requires exactly one type as a generic parameter, e.g. `sizeof<int>()`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                if !codegen_context.type_exists(&self.function_generics[0]) {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.function_generics[0].start_position.clone(),
                            self.function_generics[0].end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                self.function_generics[0]
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                let type_size = codegen_context.get_type_size(&self.function_generics[0], false);
                return codegen_context.create_constant_int64(type_size as i32);
            } else if function_name.value == "__rt_peko_alloc" {
                // __rt_peko_alloc<T>(count): allocate a managed buffer of
                // `count` elements of type T, returning a Pointer<T>. Used
                // by the standard library Array and Map classes for their
                // backing storage.
                if self.function_generics.len() != 1 {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`__rt_peko_alloc` requires exactly one type as a generic parameter, e.g. `__rt_peko_alloc<int>(count)`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                if !codegen_context.type_exists(&self.function_generics[0]) {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.function_generics[0].start_position.clone(),
                            self.function_generics[0].end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                self.function_generics[0]
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                if arguments.len() != 1
                    || !codegen_context
                        .types_similar(&arguments[0].value_type, &PekoType::simple_type("i32"))
                {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`__rt_peko_alloc` requires exactly one `int` argument as the element count, e.g. `__rt_peko_alloc<int>(8)`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                let element_type = self.function_generics[0].clone();

                // The element stride and element descriptor are chosen together
                // so the collector's per-element count (payload / stride) and
                // per-element tracing match the buffer layout. A managed element
                // (class instance, string, closure, or Pointer<T>) is stored as
                // one address-space-1 pointer: stride is the pointer size and the
                // managed field is at offset 0. An unmanaged element (int, char,
                // other builtin) is stored inline at its ABI size with no tracing.
                let element_is_managed = codegen_context.is_managed(&element_type);
                let stride = if element_is_managed {
                    codegen_context.get_type_size(&element_type, false)
                } else {
                    codegen_context.get_type_size(&element_type, true)
                };

                let element_descriptor = if element_is_managed {
                    codegen_context.emit_type_descriptor(
                        &format!("array_elem_{}", element_type.to_mangled_string()),
                        0,
                        vec![0],
                    )
                } else {
                    CodegenValue::new(
                        unsafe {
                            llvm_sys_180::core::LLVMConstPointerNull(
                                llvm_sys_180::core::LLVMPointerType(
                                    llvm_sys_180::core::LLVMInt8Type(),
                                    0,
                                ),
                            )
                        },
                        PekoType::simple_type("opaque"),
                    )
                };

                let array_descriptor = codegen_context.emit_array_descriptor(
                    &format!("array_{}", element_type.to_mangled_string()),
                    stride,
                    &element_descriptor,
                );

                // Total byte count is count * stride, computed at runtime.
                let stride_value = codegen_context.create_constant_int(stride as i32);
                let byte_count = codegen_context.build_int_operation(
                    NumericalOperation::Multiplication,
                    &arguments[0],
                    &stride_value,
                );

                let buffer_type = managed_pointer_type(element_type);
                return codegen_context
                    .allocate_managed_object_sized(&array_descriptor, &byte_count, &buffer_type)
                    .unwrap_or_else(|| codegen_context.create_error_value());
            } else if function_name.value == "Error" {
                // Note: original `!arguments.len() == 1` was a bitwise-NOT
                // bug that never fired correctly. Fixed to `!= 1`.
                if arguments.len() != 1
                    || !codegen_context
                        .types_similar(&arguments[0].value_type, &PekoType::simple_type("string"))
                {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "`Error` requires exactly one `string` argument as the error message, e.g. `Error(\"failed to parse\")`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                // The error-state flag is the bool object the Option
                // constructor expects, not a raw i1.
                let is_error_flag = codegen_context.create_constant_boolean(true);
                let is_error_flag = codegen_context
                    .box_value_to_type(&PekoType::simple_type("bool"), &is_error_flag)
                    .unwrap_or(is_error_flag);
                arguments.push(is_error_flag);

                // Building the message argument clobbered the call-site hint;
                // restore it so the Option type infers from the surrounding
                // context (a declared type or the function return type).
                codegen_context.current_expected_type_options =
                    entry_expected_type_options.clone();

                if codegen_context.current_expected_type_options.is_none() {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "cannot infer current type for Error value".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    return codegen_context.create_error_value();
                }

                for expected_type in &codegen_context
                    .current_expected_type_options
                    .clone()
                    .unwrap()
                {
                    if !codegen_context.type_exists(expected_type) {
                        continue;
                    }

                    let expand_option = codegen_context.expand_type(expected_type).unwrap();

                    if expand_option.name() == "Option" || expand_option.generics().len() == 1
                    {
                        let create_error_optional =
                            codegen_context.create_object(&expand_option, arguments.clone());
                        if let Some(v) = create_error_optional {
                            capture_optional_origin(codegen_context, &v, &self.start);
                            return v;
                        }
                    }
                }

                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        "cannot infer current type for Error value".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }
        }

        // Information required to call the function.
        let function_type: PekoType;
        let function_value: LLVMValueRef;
        let function_visibility: VisibilityData;
        let function_var_args_type: Option<PekoType>;
        let function_argument_types: IndexMap<String, CodegenArg>;

        let mut function_from_expression = false;
        let mut function_full_name = String::new();
        let mut function_base_name = String::new();

        // Step 2b: pull the function identifier if there is one.
        match self.function_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                function_base_name = variable_reference.variable_name.value.clone();
                function_full_name = variable_reference.variable_name.value.clone();
            }
            _ => {
                function_from_expression = true;
            }
        }

        // Locate the module containing the function or class.
        let mut function_module = codegen_context.module_context.current_module().clone();
        let mut found_function = true;

        if !function_full_name.is_empty() {
            let mut function_is_valid_variable = false;

            // Walk upward through enclosing modules until we find the
            // symbol (function, class, or one of their generics).
            while !function_module
                .read()
                .unwrap()
                .get_functions()
                .contains_key(&function_full_name)
                && !function_module
                    .read()
                    .unwrap()
                    .get_classes()
                    .contains_key(&function_full_name)
                && !codegen_context.module_has_function_template(&function_module, &function_base_name)
                && !codegen_context.module_has_class_template(&function_module, &function_base_name)
            {
                // Remember if the name resolves to a variable (case 2,
                // call by expression).
                if function_module
                    .read()
                    .unwrap()
                    .get_variables()
                    .contains_key(&function_full_name)
                {
                    function_is_valid_variable = true;
                }

                let parent = function_module.read().unwrap().get_parent().cloned();
                match parent {
                    Some(p) => function_module = p,
                    None => {
                        found_function = false;
                        break;
                    }
                }
            }

            // If the name didn't resolve anywhere, try calling it as a
            // method on the current `this`, or fall back to error.
            if !found_function
                && !function_is_valid_variable
                && !codegen_context
                    .scoped_variables
                    .contains_key(&function_full_name)
            {
                if codegen_context.current_this.is_some() {
                    let current_uuid = codegen_context.get_owning_module_uuid();
                    let this_value = codegen_context
                        .current_this
                        .as_ref()
                        .unwrap()
                        .variable_value[&current_uuid]
                        .clone();
                    let loaded_this = codegen_context.load_value(&this_value);

                    let this_class = codegen_context
                        .get_class_by_type(&loaded_this.value_type)
                        .unwrap();

                    if this_class
                        .main_virtual_table
                        .methods
                        .contains_key(&function_full_name)
                    {
                        let method_options: Vec<CodegenFunction> =
                            this_class.main_virtual_table.methods[&function_full_name]
                                .iter()
                                .map(|option| option.read().unwrap().clone())
                                .collect();
                        let mut argument_type_options = vec![Vec::new(); self.arguments.len()];

                        for method_option in method_options {
                            if (method_option.arguments.len() - 1) != self.arguments.len()
                                || (self.arguments.len() > (method_option.arguments.len() - 1)
                                    && method_option.var_args_type.is_none())
                            {
                                continue;
                            }

                            for (idx, (_, argument)) in
                                method_option.arguments.iter().skip(1).enumerate()
                            {
                                argument_type_options[idx].push(argument.argument_type.clone());
                            }

                            if self.arguments.len() > (method_option.arguments.len() - 1)
                                && method_option.var_args_type.is_some()
                            {
                                for argument_type_option in argument_type_options
                                    .iter_mut()
                                    .take(self.arguments.len())
                                    .skip(method_option.arguments.len() - 1)
                                {
                                    argument_type_option
                                        .push(method_option.var_args_type.clone().unwrap());
                                }
                            }
                        }

                        let mut arguments = Vec::new();
                        let mut keyword_args = HashMap::new();

                        let post_stack = codegen_context.module_context.step_back();
                        for ((argument_name, argument), expected_type_options) in
                            self.arguments.iter().zip(&argument_type_options)
                        {
                            let current_expected_types =
                                codegen_context.current_expected_type_options.clone();
                            codegen_context.current_expected_type_options =
                                Some(expected_type_options.clone());

                            let generated_argument = argument.build_value(codegen_context);
                            arguments.push(generated_argument.clone());

                            codegen_context.current_expected_type_options = current_expected_types;

                            if let Some(name) = argument_name {
                                keyword_args.insert(name.value.clone(), generated_argument);
                            }
                        }
                        codegen_context.module_context.step_forward(post_stack);

                        let (previous_line, previous_file) =
                            if !this_class.main_virtual_table.methods[&function_full_name][0]
                                .read()
                                .unwrap()
                                .visibility
                                .notrack
                            {
                                codegen_context.track_call_position(
                                    self.start.file.to_string_lossy().into_owned(),
                                    self.start.line,
                                )
                            } else {
                                (
                                    codegen_context.create_null_pointer(),
                                    codegen_context.create_null_pointer(),
                                )
                            };

                        let call_on_this = codegen_context.call_object_method(
                            &loaded_this,
                            function_full_name.clone(),
                            arguments,
                            if !keyword_args.is_empty() {
                                Some(keyword_args)
                            } else {
                                None
                            },
                        );

                        if let Ok(v) = call_on_this {
                            return v;
                        }

                        if !this_class.main_virtual_table.methods[&function_full_name][0]
                            .read()
                            .unwrap()
                            .visibility
                            .notrack
                        {
                            codegen_context.reset_call_position(&previous_line, &previous_file);
                        }
                    }

                    let object_class = codegen_context.get_class_by_type(&loaded_this.value_type);
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
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.function_reference.get_start().clone(),
                            self.function_reference.get_end().clone(),
                            format!(
                                "cannot find symbol `{}` in the current scope. Check the spelling, that the symbol is declared, and that it is imported into this module",
                                function_full_name
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }
            }

            function_from_expression = function_from_expression
                || (function_is_valid_variable
                    || codegen_context
                        .scoped_variables
                        .contains_key(&function_full_name));
        }

        // Collect generic type info, either explicitly or by inference.
        let mut function_generics = self.function_generics.clone();

        // Infer generic types when none were supplied and the function
        // resolves to a generic function or class.
        if found_function
            && (codegen_context.module_has_function_template(&function_module, &function_base_name)
                || codegen_context.module_has_class_template(&function_module, &function_base_name))
            && self.function_generics.is_empty()
        {
            let mut arguments = Vec::new();
            let mut keyword_args = HashMap::new();

            let post_stack = codegen_context.module_context.step_back();
            for (argument_name, argument) in self.arguments.iter() {
                let generated_argument = argument.build_value(codegen_context);
                arguments.push(generated_argument.clone());

                if let Some(name) = argument_name {
                    keyword_args.insert(name.value.clone(), generated_argument);
                }
            }
            codegen_context.module_context.step_forward(post_stack);

            // Collect the generic typenames declared on the function/class.
            let is_function_template = codegen_context
                .module_has_function_template(&function_module, &function_base_name);
            let needed_generics = if is_function_template {
                codegen_context
                    .module_function_template(&function_module, &function_base_name)
                    .unwrap()
                    .generic_typenames
                    .iter()
                    .map(|arg_type| arg_type.value.clone())
                    .collect::<Vec<String>>()
            } else {
                function_module.read().unwrap().get_classes()[&function_base_name]
                    .read()
                    .unwrap()
                    .generic_typenames
                    .iter()
                    .map(|arg_type| arg_type.value.clone())
                    .collect::<Vec<String>>()
            };

            let argument_declaration_types: Vec<_> = if is_function_template {
                let function_generic = codegen_context
                    .module_function_template(&function_module, &function_base_name)
                    .unwrap();
                let source = function_generic.source_function.clone().unwrap();
                source
                    .arguments
                    .values()
                    .map(|argument_declaration_info| {
                        argument_declaration_info.argument_type.clone()
                    })
                    .collect()
            } else {
                let class_generic = function_module.read().unwrap().get_classes()
                    [&function_base_name]
                    .clone();
                let class_generic_read = class_generic.read().unwrap();
                let source = class_generic_read.source_class.clone().unwrap();
                let find_matching_constructor =
                    source
                        .methods
                        .iter()
                        .find(|method| match method {
                            ClassMethod::Constructor(constructor_info, _) => {
                                constructor_info.arguments.len() == arguments.len()
                            }
                            _ => false,
                        });

                if find_matching_constructor.is_none() {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "no constructor of class `{}` matches the supplied argument types. Check the argument types against the class's declared constructors",
                                function_full_name
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                match find_matching_constructor.unwrap() {
                    ClassMethod::Constructor(constructor_info, _) => constructor_info
                        .arguments
                        .values()
                        .map(|argument_declaration_info| {
                            argument_declaration_info.argument_type.clone()
                        })
                        .collect(),
                    _ => panic!("this error is impossible"),
                }
            };

            let mut needed_generics_count = needed_generics.len();
            let mut collected_generic_types = IndexMap::new();

            // Pass 1: walk the supplied arguments against the function's
            // declared argument types, recording each declared generic
            // typename's match to a provided argument type.
            arguments
                .iter()
                .zip(argument_declaration_types.iter())
                .for_each(|(provided_argument, declared_type)| {
                    let generic_typename = declared_type.to_string();

                    if !needed_generics.contains(&generic_typename)
                        || collected_generic_types.contains_key(&generic_typename)
                    {
                        return;
                    }

                    collected_generic_types
                        .insert(generic_typename, provided_argument.value_type.clone());
                    needed_generics_count -= 1;
                });

            // Pass 2: if any generics are still unresolved, try to
            // pull them from the inference type (e.g. assigning to
            // `Array<int>` infers `int` for the generic).
            let find_expected_type = if needed_generics_count > 0
                && let Some(expected_types) = &codegen_context.current_expected_type_options
            {
                expected_types.iter().find(|expected| {
                    expected.name() == function_base_name
                        && expected.generics().len() == needed_generics_count
                })
            } else {
                None
            };

            if let Some(expected) = find_expected_type {
                function_generics = expected.generics().to_vec();
            } else if needed_generics_count == 0 {
                needed_generics.iter().for_each(|generic_typename| {
                    function_generics.push(collected_generic_types[generic_typename].clone());
                });
            } else {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "type parameters cannot be properly inferred for generic `{}`",
                            function_full_name
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }

            // Stash the generated argument values for the
            // ObjectConstructionAST path to reuse.
            if found_function
                && (codegen_context.module_has_class_template(&function_module, &function_base_name)
                    || function_module
                        .read()
                        .unwrap()
                        .get_classes()
                        .contains_key(&function_full_name))
            {
                codegen_context.generated_args = arguments;
                codegen_context.generated_kw_args = Some(keyword_args);
            }
        }

        // Format the generic-suffixed function name.
        if !function_generics.is_empty() {
            function_full_name.push('<');
            for generic in function_generics.iter() {
                let expand_generic = codegen_context.expand_type(generic);

                let expand_generic = match expand_generic {
                    None => {
                        codegen_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                generic.start_position.clone(),
                                generic.end_position.clone(),
                                format!(
                                    "type `{}` is not defined. Check the type name and that the type is in scope",
                                    generic
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ));
                        PekoType::error_type()
                    }
                    Some(t) => t,
                };

                function_full_name.push_str(expand_generic.to_string().as_str());
                function_full_name.push(',');
            }
            function_full_name.pop();
            function_full_name.push('>');
        }

        // An erased generic function compiles once under a name carrying its
        // typenames, not the concrete arguments. Target that single symbol; the
        // concrete arguments stay in `function_generics` to retype the result.
        let generic_function_typenames: Option<Vec<String>> = codegen_context
            .module_function_template(&function_module, &function_base_name)
            .map(|generic| {
                generic
                    .generic_typenames
                    .iter()
                    .map(|name| name.value.clone())
                    .collect()
            });
        if let Some(typenames) = &generic_function_typenames {
            function_full_name =
                crate::codegen::context::erased_generic_symbol(&function_base_name, typenames);
        }

        // Object construction is its own AST node, produced by the parser from
        // `new Class(args)`. A bare `Class(args)` that names a class is the
        // missing-`new` mistake, reported here so it does not fall through to
        // function-call resolution.
        if found_function
            && (codegen_context.module_has_class_template(&function_module, &function_base_name)
                || function_module
                    .read()
                    .unwrap()
                    .get_classes()
                    .contains_key(&function_full_name))
        {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "`{function_base_name}` is a class. Construct it with `new {function_base_name}(...)`"
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // Generate the call-site arguments in the call module.
        let mut arguments = Vec::new();
        let mut keyword_args = HashMap::new();

        let post_stack = codegen_context.module_context.step_back();
        for (argument_name, argument) in self.arguments.iter() {
            let generated_argument = argument.build_value(codegen_context);
            arguments.push(generated_argument.clone());

            if let Some(name) = argument_name {
                keyword_args.insert(name.value.clone(), generated_argument);
            }
        }
        codegen_context.module_context.step_forward(post_stack);

        // Step 2a: pull function info from an expression value.
        if function_from_expression {
            let function_from_expression = self
                .function_reference
                .as_ref()
                .build_value(codegen_context);

            if !function_from_expression.value_type.is_function() {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.function_reference.get_start().clone(),
                        self.function_reference.get_end().clone(),
                        "value is not callable. The expression's type is not a function or closure type, so it cannot be called".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }

            function_type = function_from_expression.value_type;
            function_value = function_from_expression.llvm_value;
            function_visibility = VisibilityData::open_visibility();
            function_var_args_type = None;
            function_argument_types = IndexMap::new();
        } else {
            // Step 2b: resolve a function overload from the module's
            // function list, materializing the generic if needed.
            if codegen_context.module_has_function_template(&function_module, &function_base_name)
                && !function_module
                    .read()
                    .unwrap()
                    .get_functions()
                    .contains_key(&function_full_name)
            {
                let function_reference = codegen_context
                    .module_function_template(&function_module, &function_base_name)
                    .unwrap();

                let generated_function = codegen_context
                    .create_generic_function(&function_reference, function_generics.clone());

                if generated_function.is_none() {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "couldn't create generic `{}` due to incorrect type parameters",
                                function_full_name
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    return codegen_context.create_error_value();
                }
            }

            let function_choices: Vec<CodegenFunction> =
                function_module.read().unwrap().get_functions()[&function_full_name]
                    .iter()
                    .map(|option| option.read().unwrap().clone())
                    .collect();

            let function_choice = codegen_context.choose_function(
                function_choices,
                arguments
                    .iter()
                    .map(|arg| arg.value_type.clone())
                    .collect_vec(),
                if !keyword_args.is_empty() {
                    Some(
                        keyword_args
                            .iter()
                            .map(|(kw, kw_val)| (kw.clone(), kw_val.value_type.clone()))
                            .collect(),
                    )
                } else {
                    None
                },
                false,
            );

            let function_choice = match function_choice {
                Some(c) => c,
                None => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!(
                                "no overload of `{}` matches the supplied argument types. Check the argument types against the declared overloads",
                                function_full_name
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }
            };

            if function_choice.visibility.private
                && codegen_context.module_context.accessing_current_module()
            {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.function_reference.get_start().clone(),
                        self.function_reference.get_end().clone(),
                        format!(
                            "cannot access private function `{}` from outside its declaring module",
                            function_full_name
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }

            function_type = {
                let mut new_type = PekoType::simple_type("");
                new_type.set_function_return(Some(function_choice.return_type.clone()));

                for (_, arg) in &function_choice.arguments {
                    new_type.generics_mut().push(arg.argument_type.clone());
                }

                new_type
            };

            // The current module may not hold a declaration of the chosen
            // function (it was defined in another module and imported after this
            // call site was reached - a class body in a late-importing module
            // calling an erased generic, for example). Declare it on demand,
            // mirroring the import path.
            let owning_uuid = codegen_context.get_owning_module_uuid();
            function_value = match function_choice.function_value.get(&owning_uuid) {
                Some(value) => value.llvm_value,
                None => {
                    let owner = function_choice.parent.clone();
                    codegen_context
                        .declare_function_in_current(&function_choice, &owner)
                        .llvm_value
                }
            };

            function_visibility = function_choice.visibility.clone();
            function_var_args_type = function_choice.var_args_type.clone();
            function_argument_types = function_choice.arguments.clone();
        }

        // --- Step 3: Call the function ---

        // Determine whether all arguments are passed as keywords (only
        // legal when every parameter has a default value).
        let mut all_args_keywords = !function_argument_types.is_empty();
        for (_, argument) in &function_argument_types {
            if argument.default_value.is_none() {
                all_args_keywords = false;
                break;
            }
        }

        let mut argument_values = Vec::new();
        let mut arguments_errored = false;

        if all_args_keywords
            && (!keyword_args.is_empty()
                || (arguments.len() != function_argument_types.len() && arguments.is_empty()))
        {
            // Keyword form: walk the parameter list in declaration order.
            for (index, (argument_name, arg)) in function_argument_types.iter().enumerate() {
                let argument_value = if keyword_args.contains_key(argument_name) {
                    keyword_args[argument_name].clone()
                } else {
                    arg.default_value
                        .as_ref()
                        .unwrap()
                        .build_value(codegen_context)
                };

                if let Some(boxed_argument) =
                    codegen_context.box_value_to_type(&arg.argument_type, &argument_value)
                {
                    argument_values.push(boxed_argument);
                } else {
                    arguments_errored = true;
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.arguments[index].1.get_start().clone(),
                            self.arguments[index].1.get_end().clone(),
                            format!(
                                "argument of type `{}` does not match expected type `{}`",
                                argument_value.value_type, arg.argument_type
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                }
            }
        } else {
            // Positional form.
            for (index, (argument, argument_type)) in arguments
                .iter()
                .zip(function_type.generics().iter())
                .enumerate()
            {
                if function_var_args_type.is_some()
                    && index == function_type.generics().len() - 1
                {
                    break;
                }

                if let Some(boxed_argument) =
                    codegen_context.box_value_to_type(argument_type, argument)
                {
                    argument_values.push(boxed_argument);
                } else {
                    arguments_errored = true;
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.arguments[index].1.get_start().clone(),
                            self.arguments[index].1.get_end().clone(),
                            format!(
                                "argument of type `{}` does not match expected type `{}`",
                                argument.value_type, argument_type
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                }
            }

            // C-style variadics: pass extra arguments through unchecked.
            if arguments.len() > function_type.generics().len() && function_visibility.variadic {
                for argument in &arguments[function_type.generics().len()..] {
                    argument_values.push(argument.clone());
                }
            }
        }

        if function_visibility.private && codegen_context.module_context.accessing_current_module()
        {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.function_reference.get_start().clone(),
                    self.function_reference.get_end().clone(),
                    format!(
                        "cannot call private function `{}` from outside its declaring module",
                        function_full_name
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        }

        // Peko-style variadics: collect into a typed array.
        if let Some(function_var_args_type) = &function_var_args_type
            && function_type.generics().len() - 1 < arguments.len()
        {
            let mut variable_arguments = Vec::new();
            for argument in arguments.iter().skip(function_type.generics().len() - 1) {
                variable_arguments.push(argument.clone());
            }

            if let Some(create_array) =
                codegen_context.create_standard_array(function_var_args_type, variable_arguments)
            {
                argument_values.push(create_array);
            } else {
                arguments_errored = true;
                let index = function_type.generics().len() - 1;
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.arguments[index].1.get_start().clone(),
                        self.arguments.last().unwrap().1.get_end().clone(),
                        format!(
                            "variadic arguments have incorrect types. All variadic arguments must have type `{}`",
                            function_var_args_type
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }
        }

        if arguments_errored {
            return codegen_context.create_error_value();
        }

        let (previous_line, previous_file) = if !function_visibility.notrack {
            codegen_context.track_call_position(
                self.start.file.to_string_lossy().into_owned(),
                self.start.line,
            )
        } else {
            (
                codegen_context.create_null_pointer(),
                codegen_context.create_null_pointer(),
            )
        };

        let mut function_call = codegen_context.call_function(
            &function_type,
            function_visibility.variadic,
            function_value,
            argument_values,
        );

        if !function_visibility.notrack {
            codegen_context.reset_call_position(&previous_line, &previous_file);
        }

        // Retype the result of an erased generic call. The single compiled body
        // returns a generic-parameter-typed thin object; substitute the call's
        // concrete type arguments so the caller observes the instantiated type
        // (for example `Option<number>` rather than `Option<T>`).
        if let Some(typenames) = &generic_function_typenames {
            let mut substitution = HashMap::new();
            for (typename, concrete) in typenames.iter().zip(function_generics.iter()) {
                if let Some(expanded) = codegen_context.expand_type(concrete) {
                    substitution.insert(typename.clone(), expanded);
                }
            }
            function_call.value_type = crate::codegen::context::substitute_generic_params(
                &function_call.value_type,
                &substitution,
            );
        }

        function_call
    }
}

/// True when `ast` is the bare `None` literal. The parser represents None as a
/// variable reference named None.
fn is_none_literal(ast: &PekoAST) -> bool {
    matches!(ast, PekoAST::VariableReference(reference) if reference.variable_name.value == "None")
}

/// Records where a None or Error originated as the first frame of its failure
/// backtrace. Only emitted inside a function body, where a method call can run.
fn capture_optional_origin(
    codegen_context: &mut PekoCodegenContext,
    option_value: &CodegenValue,
    position: &peko_core::asts::data_structures::PositionData,
) {
    if !codegen_context.local_scope {
        return;
    }
    let file_value =
        codegen_context.create_string(position.file.to_string_lossy().into_owned());
    let line_value = codegen_context.create_constant_int(position.line as i32);
    let column_value = codegen_context.create_constant_int(position.column as i32);
    let _ = codegen_context.call_object_method(
        option_value,
        "add_context",
        vec![file_value, line_value, column_value],
        None,
    );
}

impl PekoValueBuilder for BinaryExpressionAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // `a..b` is parsed as a binary expression by the parser; we
        // re-route to RangeAST here.
        if self.operator == ".." {
            return PekoAST::Range(RangeAST::new(self.lhs.clone(), self.rhs.clone()))
                .build_value(codegen_context);
        }

        // `&&` and `||` short-circuit, which requires emitting branches
        // rather than a single operator overload.
        if (self.operator == "&&" || self.operator == "||") && codegen_context.local_scope {
            let evaluate = codegen_context.short_circuit_boolean_operation(
                BooleanOperation::from(&self.operator).unwrap(),
                self.lhs.as_ref(),
                self.rhs.as_ref(),
            );

            return match evaluate {
                None => {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.lhs.get_start().clone(),
                            self.rhs.get_end().clone(),
                            "expected boolean types for boolean operation".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    codegen_context.create_error_value()
                }
                Some(v) => v,
            };
        }

        // `opt == None` / `opt != None` test an optional for emptiness. A None
        // literal has no type of its own to compare against, so the comparison
        // lowers to is_none / is_value on the optional operand. This also lets
        // the optional operand keep its inferred generic type without forcing
        // None to name one.
        if self.operator == "==" || self.operator == "!=" {
            let lhs_is_none = is_none_literal(self.get_lhs());
            let rhs_is_none = is_none_literal(self.get_rhs());
            if lhs_is_none ^ rhs_is_none {
                let operand_ast = if lhs_is_none {
                    self.get_rhs()
                } else {
                    self.get_lhs()
                };
                let operand = operand_ast.build_value(codegen_context);
                let is_option = codegen_context
                    .expand_type(&operand.value_type)
                    .map(|expanded| expanded.name() == "Option")
                    .unwrap_or(false);

                if is_option {
                    let method = if self.operator == "==" {
                        "is_none"
                    } else {
                        "is_value"
                    };
                    return codegen_context
                        .call_object_method(&operand, method, Vec::new(), None)
                        .unwrap_or_else(|_| codegen_context.create_error_value());
                }

                if !operand.value_type.is_error_type() {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.lhs.get_start().clone(),
                            self.rhs.get_end().clone(),
                            format!(
                                "cannot compare `{}` against None. A None comparison requires an optional value",
                                operand.value_type
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                }
                return codegen_context.create_error_value();
            }
        }

        let lhs = self.get_lhs().build_value(codegen_context);
        let rhs = self.get_rhs().build_value(codegen_context);

        let (previous_line, previous_file) = codegen_context.track_call_position(
            self.get_lhs()
                .get_start()
                .file
                .to_string_lossy()
                .into_owned(),
            self.get_lhs().get_start().line,
        );

        let evaluated = codegen_context.apply_operator(self.operator.as_str(), &lhs, &rhs);

        codegen_context.reset_call_position(&previous_line, &previous_file);

        match evaluated {
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.lhs.get_start().clone(),
                        self.rhs.get_end().clone(),
                        format!(
                            "cannot apply binary operator `{}` to values of type `{}` and `{}`. There is no operator overload that accepts these two operand types",
                            self.operator,
                            lhs.value_type,
                            rhs.value_type
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
            Some(v) => v,
        }
    }
}

impl PekoValueBuilder for UnaryExpressionAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Only `!`, `&`, and `-` are valid unary operators.
        match self.operator.as_str() {
            "!" => {
                let negate = self.get_operand().build_value(codegen_context);

                // An object operand routes through the Not trait; a raw i1 is
                // negated by comparing it against false.
                if codegen_context.get_class_by_type(&negate.value_type).is_some()
                    && let Ok(value) =
                        codegen_context.call_object_method(&negate, "not", Vec::new(), None)
                {
                    return value;
                }

                let negate_raw = codegen_context.to_raw_bool(&negate);
                let false_value = codegen_context.create_constant_boolean(false);
                codegen_context.build_int_operation(
                    NumericalOperation::Equals,
                    &negate_raw,
                    &false_value,
                )
            }
            "&" => {
                // Address-of: build the operand in reference-returning
                // mode and reset state afterwards.
                let return_references = codegen_context.return_references;
                codegen_context.return_references = true;

                let value = self.operand.build_value(codegen_context);

                codegen_context.return_references = return_references;
                codegen_context.accessed_state = None;
                codegen_context.primary_object = None;

                value
            }
            "-" => {
                // Unary negation: multiply the operand by -1 via the type's
                // `*` operator overload. The minus-one carries the operand's
                // own type so the overload matches: a `number` operand boxes a
                // `number` minus-one, a raw scalar uses a machine minus-one.
                let value = self.operand.build_value(codegen_context);
                let negative_value = if value.value_type.name() == "number" {
                    let raw = codegen_context.create_constant_double(-1.0);
                    codegen_context
                        .box_value_to_type(&PekoType::simple_type("number"), &raw)
                        .unwrap_or_else(|| codegen_context.create_error_value())
                } else {
                    codegen_context.create_constant_int(-1)
                };

                let (previous_line, previous_file) = codegen_context.track_call_position(
                    self.get_operand()
                        .get_start()
                        .file
                        .to_string_lossy()
                        .into_owned(),
                    self.get_operand().get_start().line,
                );

                let evaluated = codegen_context.apply_operator("*", &value, &negative_value);

                codegen_context.reset_call_position(&previous_line, &previous_file);

                match evaluated {
                    None => {
                        codegen_context
                            .diagnostics
                            .report_diagnostic(diagnostics::PekoDiagnostic::new(
                                self.operand.get_start().clone(),
                                self.operand.get_end().clone(),
                                format!(
                                    "cannot negate value of type `{}` with the unary `-` operator. The type does not implement the `*` operator with an `int` operand",
                                    value.value_type
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ));
                        codegen_context.create_error_value()
                    }
                    Some(v) => v,
                }
            }
            "*" => {
                // Dereference a pointer. Mirrors `ptr[0]` indexing but
                // expresses the operation as a unary prefix; emits a
                // single load on the operand value.
                let value = self.operand.build_value(codegen_context);
                let value_type = value.value_type.clone();

                if value_type.array_depth == 0
                    && value_type.reference_depth == 0
                    && value_type.name() != "pointer"
                {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.operand.get_start().clone(),
                            self.operand.get_end().clone(),
                            format!(
                                "cannot dereference value of type `{}` with the unary `*` operator. Only pointer or reference types can be dereferenced",
                                value_type
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    return codegen_context.create_error_value();
                }

                codegen_context.build_pointer_dereference(&value)
            }
            _ => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.operand.get_start().clone(),
                        self.operand.get_end().clone(),
                        format!(
                            "operator `{}` is not a unary operator. Only `!`, `&`, `-`, and `*` can be used as unary operators",
                            self.operator
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
        }
    }
}
