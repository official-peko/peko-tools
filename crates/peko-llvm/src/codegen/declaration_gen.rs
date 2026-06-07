//! `PekoValueBuilder` implementations for the declaration-producing AST
//! nodes: variable declarations, function declarations, closures,
//! classes, and module declarations.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use itertools::Itertools;
use peko_core::asts::data_structures::{ClassMethod, PositionData, VisibilityData};
use peko_core::asts::declarations::{
    ClassAST, ClosureAST, FunctionDeclarationAST, ModuleCreationAST, NewVariableAST,
};
use peko_core::diagnostics;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::execution::data_structures::ExecutionModule;
use peko_core::types::PekoType;

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::prelude::*;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{
    CodegenArg, CodegenClass, CodegenClassAttribute, CodegenClassGeneric, CodegenFunction,
    CodegenFunctionGeneric, CodegenModule, CodegenValue, CodegenVariable, CodegenVirtualTable,
    GlobalVariable, is_managed_pointer, managed_pointer_type,
};
use crate::codegen::symbol::SymbolName;

impl PekoValueBuilder for NewVariableAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Set the expected-type hint for type inference if this
        // declaration has an explicit type.
        let previous_expected_type = codegen_context.current_expected_type_options.clone();
        if self.variable_type.is_some()
            && codegen_context.type_exists(self.variable_type.as_ref().unwrap())
        {
            codegen_context.current_expected_type_options = Some(vec![
                codegen_context
                    .expand_type(self.variable_type.as_ref().unwrap())
                    .unwrap(),
            ]);
        }

        // Local-scope path: stack-alloc, store, and add to the scope.
        if codegen_context.local_scope {
            let mut variable_value = self.variable_value.build_value(codegen_context);

            // `void` is not a value-bearing type.
            if variable_value.value_type.to_string() == "void" {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_value.get_start().clone(),
                        self.variable_value.get_end().clone(),
                        "variable value cannot be of type void".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                variable_value = codegen_context.create_error_value();
            } else if self.variable_type.is_some()
                && !codegen_context.type_exists(self.variable_type.as_ref().unwrap())
            {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_type.clone().unwrap().start_position.clone(),
                        self.variable_type.clone().unwrap().end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            self.variable_type.clone().unwrap().to_string()
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            } else if self.variable_type.is_some() {
                let (previous_line, previous_file) = codegen_context.track_call_position(
                    self.variable_value
                        .as_ref()
                        .get_start()
                        .file
                        .to_string_lossy()
                        .into_owned(),
                    self.variable_value.as_ref().get_start().line,
                );

                let value_boxed = codegen_context
                    .box_value_to_type(self.variable_type.as_ref().unwrap(), &variable_value);

                codegen_context.reset_call_position(&previous_line, &previous_file);

                match value_boxed {
                    Some(boxed) => variable_value = boxed,
                    None => {
                        codegen_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                self.variable_value.get_start().clone(),
                                self.variable_value.get_end().clone(),
                                format!(
                                    "cannot assign value of type `{}` to variable of type `{}`. The right-hand side type is not compatible with the variable's declared type",
                                    variable_value.value_type.to_string(),
                                    self.variable_type.clone().unwrap().to_string()
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ),
                        );
                        variable_value = codegen_context.create_error_value();
                    }
                }
            }

            let variable_type = match &self.variable_type {
                Some(declared) => {
                    if codegen_context.type_exists(declared) {
                        codegen_context.expand_type(declared).unwrap()
                    } else {
                        variable_value.value_type.clone()
                    }
                }
                None => variable_value.value_type.clone(),
            };

            // Stack-allocate the variable and store its initial value.
            let allocate_variable = codegen_context.build_stack_allocation(&variable_type);
            if !variable_value.value_type.is_error_type && !variable_type.is_error_type {
                codegen_context.build_store(&allocate_variable, &variable_value);
            }

            // Add to the current local scope.
            let qualified_allocation = codegen_context.qualify_value_to_current(allocate_variable);
            codegen_context.scoped_variables.insert(
                self.name.value.clone(),
                CodegenVariable::new(
                    self.visibility.clone(),
                    variable_type,
                    qualified_allocation,
                    None,
                    codegen_context.module_context.current_module().clone(),
                    None,
                ),
            );

            codegen_context.current_expected_type_options = previous_expected_type;
            return codegen_context.create_null_pointer();
        }

        // Global-variable path. Global variables must have an explicit type.
        let unexpanded_type = match &self.variable_type {
            Some(declared) => declared.clone(),
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        "variable type is required for global variables".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                PekoType::error_type()
            }
        };
        let global_type = match codegen_context.expand_type(&unexpanded_type) {
            Some(expanded_type) => expanded_type,
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.variable_type.clone().unwrap().start_position.clone(),
                        self.variable_type.clone().unwrap().end_position.clone(),
                        format!(
                        "type `{}` is not defined. Check the type name and that the type is in scope",
                        self.variable_type.clone().unwrap().to_string()
                    ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                PekoType::error_type()
            }
        };

        // Mangle the global name with its module path unless it is
        // exported as external.
        let global_name = if self.visibility.external {
            self.name.value.clone()
        } else {
            let mut final_name = self.name.value.clone();
            let mut next_module = codegen_context.module_context.current_module().clone();
            loop {
                final_name.insert_str(
                    0,
                    [next_module.read().unwrap().get_name(), "::"]
                        .concat()
                        .as_str(),
                );
                let parent = next_module.read().unwrap().get_parent().cloned();
                match parent {
                    Some(p) => next_module = p,
                    None => break,
                }
            }
            final_name
        };

        let global_symbol_name = SymbolName::from(None, None, &global_name, None);
        let global_declaration = codegen_context.create_named_global(
            Some(global_symbol_name.clone()),
            &global_type,
            self.visibility.external,
            true,
        );
        let global_variable = CodegenVariable::new(
            self.visibility.clone(),
            global_type.clone(),
            codegen_context.qualify_value_to_current(global_declaration.clone()),
            Some(global_symbol_name),
            codegen_context.module_context.current_module().clone(),
            None,
        );

        // External variables live in the extern module; the rest live
        // in the current module.
        if self.visibility.external {
            codegen_context
                .module_context
                .extern_module
                .write()
                .unwrap()
                .get_variables_mut()
                .insert(self.name.value.clone(), global_variable);
        } else {
            codegen_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .get_variables_mut()
                .insert(self.name.value.clone(), global_variable);
        }

        let current_file = codegen_context.get_current_file().to_path_buf();

        CodegenModule::get_top_parent(codegen_context.module_context.current_module())
            .write()
            .unwrap()
            .top_level_info
            .as_mut()
            .unwrap()
            .globals_info
            .globals_to_set
            .push((
                GlobalVariable::new(global_declaration, global_type, global_name, current_file),
                self.variable_value.as_ref().clone(),
            ));

        codegen_context.current_expected_type_options = previous_expected_type;
        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for FunctionDeclarationAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Generic function declarations are tracked, not emitted; the
        // emit happens when a concrete instantiation is requested.
        if !self.generic_types.is_empty() {
            let mut self_reference = self.clone();
            self_reference.generic_types.clear();

            let current_module = codegen_context.module_context.current_module().clone();
            let current_file = codegen_context.get_current_file().to_path_buf();
            codegen_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .get_function_generics_mut()
                .insert(
                    self.function_name.value.clone(),
                    CodegenFunctionGeneric::new(
                        self.visibility.clone(),
                        self.generic_types.clone(),
                        self_reference,
                        current_module,
                        current_file,
                        None,
                    ),
                );

            return codegen_context.create_null_pointer();
        }

        // Save the outer scope state. Function body codegen mutates
        // these on the context and we'll restore at the end.
        let scoped_variables = codegen_context.scoped_variables.clone();
        codegen_context.scoped_variables.clear();

        let current_this = codegen_context.current_this.clone();
        codegen_context.current_this = None;

        let local_scope = codegen_context.local_scope;
        codegen_context.local_scope = true;

        let current_return_type = codegen_context.current_return_type.clone();

        // --- Collect function type info ---

        let return_type = if self.return_type.is_some() {
            let expanded_type = codegen_context.expand_type(self.return_type.as_ref().unwrap());

            let ret_ty = match expanded_type {
                Some(t) => t,
                None => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.return_type.clone().unwrap().start_position.clone(),
                            self.return_type.clone().unwrap().end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                self.return_type.clone().unwrap().to_string(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    PekoType::simple_type("void")
                }
            };

            codegen_context.current_return_type = Some(ret_ty.clone());
            ret_ty
        } else {
            codegen_context.current_return_type = None;
            PekoType::simple_type("void")
        };

        // Analyze positional arguments.
        let mut arguments = IndexMap::new();
        for (argument_name, arg_declaration) in self.arguments.iter() {
            let expanded_argument_type =
                codegen_context.expand_type(&arg_declaration.argument_type);
            let argument_type = match expanded_argument_type {
                Some(t) => t,
                None => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            arg_declaration.argument_type.start_position.clone(),
                            arg_declaration.argument_type.end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                arg_declaration.argument_type.to_string(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    PekoType::error_type()
                }
            };

            arguments.insert(
                argument_name.value.clone(),
                CodegenArg::new(
                    arg_declaration.visibility.clone(),
                    argument_type,
                    arg_declaration.default_value.clone(),
                ),
            );
        }

        // Append the var-args parameter if present.
        if self.varargs_type.is_some() {
            let varargs_array_type = if !codegen_context
                .type_exists(self.varargs_type.as_ref().unwrap())
            {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.varargs_type.clone().unwrap().start_position.clone(),
                        self.varargs_type.clone().unwrap().end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            self.varargs_type.clone().unwrap().to_string(),
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));

                PekoType::from_string(
                    format!("standard::Array<{}>", PekoType::error_type().to_string()).as_str(),
                    codegen_context.get_current_file(),
                )
            } else {
                PekoType::from_string(
                    format!(
                        "standard::Array<{}>",
                        codegen_context
                            .expand_type(self.varargs_type.as_ref().unwrap())
                            .unwrap()
                            .to_string()
                    )
                    .as_str(),
                    codegen_context.get_current_file(),
                )
            };

            arguments.insert(
                self.varargs_name.value.clone(),
                CodegenArg::new(VisibilityData::open_visibility(), varargs_array_type, None),
            );
        }

        // `OnStart` in the `main` module is the program entry point and
        // is force-exported via the extern module.
        let is_onstart = codegen_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_name()
            == "main"
            && self.function_name.value == "OnStart";

        let function_qualified_name = if self.visibility.external || is_onstart {
            self.function_name.value.clone()
        } else {
            let mut function_module_names = Vec::new();
            let mut parent_module = codegen_context.module_context.current_module().clone();
            loop {
                function_module_names
                    .insert(0, parent_module.read().unwrap().get_name().to_owned());
                let parent = parent_module.read().unwrap().get_parent().cloned();
                match parent {
                    Some(p) => parent_module = p,
                    None => break,
                }
            }

            [
                function_module_names.join("::").as_str(),
                "::",
                self.function_name.value.as_str(),
            ]
            .concat()
        };

        let argument_types = arguments
            .iter()
            .map(|(_, arg)| arg.argument_type.clone())
            .collect_vec();

        // --- Create function definition ---

        let function_module = if self.visibility.external || is_onstart {
            codegen_context.module_context.extern_module.clone()
        } else {
            codegen_context.module_context.current_module().clone()
        };

        let function_exists = function_module
            .read()
            .unwrap()
            .functions
            .contains_key(&self.function_name.value);

        if !function_exists {
            function_module
                .write()
                .unwrap()
                .get_functions_mut()
                .insert(self.function_name.value.clone(), Vec::new());
        }

        // Look for an existing definition with exactly-matching arg types.
        let find_function_definition = codegen_context.choose_function(
            function_module.read().unwrap().get_functions()[&self.function_name.value].clone(),
            argument_types.clone(),
            None,
            false,
        );

        let find_function_definition = match find_function_definition {
            Some(found) => {
                let mut all_types_equal = true;
                for ((_, choice_arg), actual_type) in
                    found.arguments.iter().zip(argument_types.iter())
                {
                    if !codegen_context.types_equal(&choice_arg.argument_type, actual_type) {
                        all_types_equal = false;
                        break;
                    }
                }
                if all_types_equal { Some(found) } else { None }
            }
            None => None,
        };

        // Build the full CodegenFunction reference.
        let peko_function = match find_function_definition {
            Some(existing) => existing,
            None => {
                let function_symbol_name = SymbolName::from(
                    None,
                    None,
                    function_qualified_name,
                    Some(argument_types.clone()),
                );

                let function_value = codegen_context.create_function(
                    Some(function_symbol_name.clone()),
                    argument_types.clone(),
                    &return_type,
                    self.visibility.variadic,
                    self.visibility.external,
                    self.function_body.is_none()
                        || (codegen_context.outside_declarations_only
                            && codegen_context.outside_primary_module),
                );

                // External functions are leaf (not safepoints) by default, so
                // RewriteStatepointsForGC does not wrap calls to them. A call
                // must remain a safepoint only when a collection can occur
                // during it while the caller holds live managed pointers: such
                // functions are opted back in with the `gcsafe` visibility.
                // The GC allocation entrypoints always allocate (and so can
                // collect), so they are never marked leaf regardless of how
                // they are declared.
                let is_gc_alloc_entrypoint = self.function_name.value == "peko_gc_alloc_object"
                    || self.function_name.value == "peko_gc_alloc";
                if self.visibility.external
                    && !self.visibility.gc_safepoint
                    && !is_gc_alloc_entrypoint
                {
                    crate::codegen::builders::functions::set_gc_leaf_attribute(
                        function_value.llvm_value,
                    );
                }

                let function_reference = CodegenFunction::new(
                    self.visibility.clone(),
                    return_type.clone(),
                    arguments.clone(),
                    if self.varargs_type.is_some() {
                        Some(
                            if codegen_context.type_exists(self.varargs_type.as_ref().unwrap()) {
                                codegen_context
                                    .expand_type(self.varargs_type.as_ref().unwrap())
                                    .unwrap()
                            } else {
                                PekoType::error_type()
                            },
                        )
                    } else {
                        None
                    },
                    codegen_context.qualify_value_to_current(function_value),
                    0,
                    function_symbol_name,
                    codegen_context.module_context.current_module().clone(),
                    None,
                    None,
                );

                function_module
                    .write()
                    .unwrap()
                    .get_functions_mut()
                    .get_mut(&self.function_name.value)
                    .unwrap()
                    .push(function_reference.clone());
                function_reference
            }
        };

        let function_value =
            peko_function.function_value[&codegen_context.get_owning_module_uuid()].clone();

        // Pure declarations and declarations-only mode stop here.
        if self.function_body.is_none()
            || (codegen_context.outside_declarations_only && codegen_context.outside_primary_module)
        {
            codegen_context.scoped_variables.clear();
            codegen_context.scoped_variables.extend(scoped_variables);
            codegen_context.current_this = current_this;
            codegen_context.local_scope = local_scope;
            codegen_context.current_return_type = current_return_type;
            return codegen_context.create_null_pointer();
        }

        // --- Generate function body ---

        let previous_function = codegen_context.current_llvm_function;
        codegen_context.current_llvm_function = Some(function_value.llvm_value);

        let entry_block = codegen_context.create_new_block(Some("entry".to_string()));
        let previous_entry_block = codegen_context.current_entry_block;
        let previous_block = codegen_context.current_basic_block;
        codegen_context.current_entry_block = Option::Some(entry_block);
        codegen_context.goto_block_end(entry_block);

        // Copy parameters into the local scope.
        for (idx, (argument_name, argument_declaration)) in arguments.iter().enumerate() {
            let argument_value =
                codegen_context.get_allocated_function_argument(&function_value, idx);

            let qualified_argument = codegen_context.qualify_value_to_current(argument_value);
            codegen_context.scoped_variables.insert(
                argument_name.clone(),
                CodegenVariable::new(
                    argument_declaration.visibility.clone(),
                    argument_declaration.argument_type.clone(),
                    qualified_argument,
                    None,
                    codegen_context.module_context.current_module().clone(),
                    None,
                ),
            );
        }

        // Walk the body, emitting each statement and watching for an
        // early branch exit (return / break). Anything after the first
        // branch exit is unreachable.
        let mut branch_returns = false;
        for ast in &self.function_body.clone().unwrap().value {
            if !branch_returns
                && ast.build_value(codegen_context).value_type.to_string() == "<<returnexit>>"
            {
                branch_returns = true;
            } else if branch_returns {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        ast.get_start().clone(),
                        ast.get_end().clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                break;
            }
        }

        // A non-void function must return on every path; a void
        // function gets an implicit `ret void` if execution falls off
        // the end.
        if !branch_returns && return_type.to_string() != "void" {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "function `{}` does not return on all paths. The declared return type is `{}`, but at least one execution path can reach the end of the function without returning",
                        self.function_name.value,
                        self.return_type.clone().unwrap().to_string()
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        }

        if !branch_returns && return_type.to_string() == "void" {
            codegen_context.build_return(None);
        }

        // Restore outer scope state.
        codegen_context.scoped_variables.clear();
        codegen_context.scoped_variables.extend(scoped_variables);
        codegen_context.current_this = current_this;
        codegen_context.local_scope = local_scope;
        codegen_context.current_return_type = current_return_type;
        codegen_context.current_llvm_function = previous_function;
        match previous_block {
            Some(block) => codegen_context.goto_block_end(block),
            None => codegen_context.current_basic_block = None,
        }

        codegen_context.current_entry_block = previous_entry_block;

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for ClosureAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if !codegen_context.local_scope && codegen_context.current_basic_block.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "cannot create closures outside of a local scope".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // The closure has its own scope; reset `this` so the closure
        // body doesn't see the enclosing method's `this` unless it was
        // explicitly captured.
        let current_this = codegen_context.current_this.clone();
        codegen_context.current_this = None;

        // Collect all captured variables. Each capture is heap-allocated
        // so its lifetime is independent of the enclosing function's
        // stack frame.
        let mut captured_values = IndexMap::new();
        let mut context_types = Vec::new();

        for capture in self.captures.iter() {
            if !codegen_context
                .scoped_variables
                .contains_key(&capture.value)
            {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        capture.start.clone(),
                        capture.end.clone(),
                        format!(
                            "cannot find symbol `{}` in the current scope. Check the spelling, that the symbol is declared, and that it is imported into this module",
                            capture.value
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                continue;
            }

            let captured_type = codegen_context.scoped_variables[&capture.value]
                .variable_type
                .clone();

            // Each captured variable is converted from a local alloca to a
            // heap box so it outlives the enclosing scope. The context
            // therefore stores a managed pointer to the box (a
            // Pointer<captured_type>), not the value itself.
            let box_type = managed_pointer_type(captured_type.clone());
            context_types.push(codegen_context.get_llvm_type(&box_type).unwrap());

            // The box holds one value at offset 0. Its descriptor traces
            // that value only when the value is itself managed.
            let box_offsets = if codegen_context.is_managed(&captured_type) {
                vec![0]
            } else {
                Vec::new()
            };
            let box_descriptor = codegen_context.emit_type_descriptor(
                &format!(
                    "closure_box_{}_{}_{}",
                    self.start.line,
                    self.start.column,
                    captured_values.len()
                ),
                0,
                box_offsets,
            );

            // The box stores the captured value as it lives in a variable:
            // a pointer for managed captures, the value for primitives.
            // base_type=false collapses managed/pointer types to the
            // machine pointer size.
            let uuid = codegen_context.get_owning_module_uuid();
            let box_size = codegen_context.get_type_size(&captured_type, false);

            // only heap allocate the variable once
            let heap_allocated_variable = if is_managed_pointer(
                &codegen_context.scoped_variables[&capture.value].variable_value[&uuid].value_type,
            ) {
                codegen_context.scoped_variables[&capture.value].variable_value[&uuid].clone()
            } else {
                match codegen_context.allocate_managed_object(&box_descriptor, box_size, &box_type)
                {
                    Some(value) => value,
                    None => continue,
                }
            };

            let variable_value = codegen_context.load_value(
                &codegen_context.scoped_variables[&capture.value].variable_value[&uuid].clone(),
            );
            codegen_context.build_store(&heap_allocated_variable, &variable_value);
            captured_values.insert(capture.value.clone(), heap_allocated_variable);
        }

        let closure_context_type = codegen_context.create_struct_type(context_types);

        // Build the context descriptor. Every context element is a managed
        // box pointer, so the descriptor traces all of them.
        let context_offsets = {
            let datalayout = unsafe {
                llvm_sys_180::target::LLVMGetModuleDataLayout(
                    codegen_context
                        .module_context
                        .current_module()
                        .read()
                        .unwrap()
                        .get_top_level()
                        .unwrap()
                        .llvm_module,
                )
            };
            (0..captured_values.len())
                .map(|index| unsafe {
                    llvm_sys_180::target::LLVMOffsetOfElement(
                        datalayout,
                        closure_context_type,
                        index as u32,
                    ) as usize
                })
                .collect::<Vec<_>>()
        };
        let context_descriptor = codegen_context.emit_type_descriptor(
            &format!("closure_ctx_{}_{}", self.start.line, self.start.column),
            0,
            context_offsets,
        );

        // Heap-allocate the context as a managed object. Its pointer is a
        // managed Pointer<void>. Size from the real struct layout so any
        // alignment padding is included.
        let context_alloc_size = (unsafe {
            llvm_sys_180::target::LLVMABISizeOfType(
                llvm_sys_180::target::LLVMGetModuleDataLayout(
                    codegen_context
                        .module_context
                        .current_module()
                        .read()
                        .unwrap()
                        .get_top_level()
                        .unwrap()
                        .llvm_module,
                ),
                closure_context_type,
            )
        } as usize)
            .max(8);
        let allocated_context = codegen_context
            .allocate_managed_object(
                &context_descriptor,
                context_alloc_size,
                &managed_pointer_type(PekoType::simple_type("void")),
            )
            .unwrap();

        // Store the captured box pointers into the context.
        for (index, (_, value)) in captured_values.iter().enumerate() {
            let context_element = codegen_context.get_closure_context_element(
                &allocated_context,
                closure_context_type,
                &value.value_type,
                index,
            );
            codegen_context.build_store(&context_element, value);
        }

        // Resolve the return type.
        let return_type = match &self.return_type {
            Some(declared) => {
                if !codegen_context.type_exists(declared) {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            declared.start_position.clone(),
                            declared.end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                declared.to_string(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    PekoType::simple_type("void")
                } else {
                    codegen_context.expand_type(declared).unwrap()
                }
            }
            None => PekoType::simple_type("void"),
        };

        // The first argument is a managed pointer to the captured-context
        // struct (Pointer<void>); positional args follow.
        let mut closure_argument_types = vec![managed_pointer_type(PekoType::simple_type("void"))];

        for (_, argument_declaration) in self.arguments.iter() {
            if !codegen_context.type_exists(&argument_declaration.argument_type) {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        argument_declaration.argument_type.start_position.clone(),
                        argument_declaration.argument_type.end_position.clone(),
                        format!(
                            "type `{}` is not defined. Check the type name and that the type is in scope",
                            argument_declaration.argument_type.to_string()
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            } else {
                closure_argument_types.push(
                    codegen_context
                        .expand_type(&argument_declaration.argument_type)
                        .unwrap(),
                );
            }
        }

        let closure_function = codegen_context.create_function(
            None,
            closure_argument_types,
            &return_type,
            false,
            false,
            false,
        );

        // Snapshot state, then switch into the closure's body.
        let previous_function = codegen_context.current_llvm_function;
        let previous_scoped_variables = codegen_context.scoped_variables.clone();
        let previous_local_scope = codegen_context.local_scope;
        let previous_return_type = codegen_context.current_return_type.clone();
        let previous_block = codegen_context.current_basic_block.unwrap();
        let previous_entry_block = codegen_context.current_entry_block;
        codegen_context.scoped_variables.clear();
        codegen_context.local_scope = true;
        codegen_context.current_return_type = Some(return_type.clone());
        codegen_context.current_llvm_function = Some(closure_function.llvm_value);

        let entry_block = codegen_context.create_new_block(Some("entry".to_string()));
        codegen_context.current_entry_block = Option::Some(entry_block);
        codegen_context.goto_block_end(entry_block);

        let context_argument =
            codegen_context.get_allocated_function_argument(&closure_function, 0);

        // Load each captured value out of the context and bind it as a
        // local in the closure body's scope.
        for (capture_index, (capture_name, capture_value)) in captured_values.iter().enumerate() {
            let loaded_context = codegen_context.load_value(&context_argument);

            let context_element = codegen_context.get_closure_context_element(
                &loaded_context,
                closure_context_type,
                &capture_value.value_type,
                capture_index,
            );

            let captured_variable_value = codegen_context.load_value(&context_element);
            let mut pointee_type = capture_value.value_type.clone();
            pointee_type.decrease_pointer_depth();

            let captured_variable = CodegenVariable::new(
                VisibilityData::open_visibility(),
                pointee_type,
                codegen_context.qualify_value_to_current(captured_variable_value),
                None,
                codegen_context.module_context.current_module().clone(),
                None,
            );

            if capture_name == "this" {
                codegen_context.current_this = Some(captured_variable.clone());
            }

            codegen_context
                .scoped_variables
                .insert(capture_name.clone(), captured_variable);
        }

        // Bind the positional arguments.
        for (argument_index, (argument_name, argument_declaration)) in
            self.arguments.iter().enumerate()
        {
            let current_argument = codegen_context
                .get_allocated_function_argument(&closure_function, argument_index + 1);

            let qualified_argument = codegen_context.qualify_value_to_current(current_argument);
            codegen_context.scoped_variables.insert(
                argument_name.value.clone(),
                CodegenVariable::new(
                    argument_declaration.visibility.clone(),
                    argument_declaration.argument_type.clone(),
                    qualified_argument,
                    None,
                    codegen_context.module_context.current_module().clone(),
                    None,
                ),
            );
        }

        // Walk the closure body, watching for an early branch exit.
        let mut branch_returns = false;
        for ast in &self.closure_body.value {
            if !branch_returns
                && ast.build_value(codegen_context).value_type.to_string() == "<<returnexit>>"
            {
                branch_returns = true;
            } else if branch_returns {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        ast.get_start().clone(),
                        ast.get_end().clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                break;
            }
        }

        if !branch_returns && return_type.to_string() != "void" {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "closure does not return, expected to return a value of type `{}`",
                        return_type.to_string()
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        } else if !branch_returns && return_type.to_string() == "void" {
            codegen_context.build_return(None);
        }

        // Restore outer state.
        codegen_context.scoped_variables.clear();
        codegen_context
            .scoped_variables
            .extend(previous_scoped_variables);
        codegen_context.current_this = current_this;
        codegen_context.local_scope = previous_local_scope;
        codegen_context.current_return_type = previous_return_type;
        codegen_context.current_llvm_function = previous_function;
        codegen_context.current_entry_block = previous_entry_block;

        codegen_context.goto_block_end(previous_block);

        // Construct the closure value: { context: i8*, function: fn-ptr }.
        let argument_types = self
            .arguments
            .iter()
            .map(|(_, arg)| arg.argument_type.clone())
            .collect_vec();

        let closure_type = PekoType::new(
            Vec::new(),
            String::new(),
            argument_types,
            0,
            0,
            0,
            Some(return_type),
            true,
            PositionData::default(),
            PositionData::default(),
        );

        // Build the closure descriptor. The closure struct is
        // { context: ptr, function: fn-ptr }. Only the context pointer
        // (element 0) is traced; the function pointer is code, not a
        // managed reference.
        let closure_struct_type = codegen_context
            .get_llvm_type_full(&closure_type, true, false)
            .unwrap();
        let closure_offsets = {
            let datalayout = unsafe {
                llvm_sys_180::target::LLVMGetModuleDataLayout(
                    codegen_context
                        .module_context
                        .current_module()
                        .read()
                        .unwrap()
                        .get_top_level()
                        .unwrap()
                        .llvm_module,
                )
            };

            vec![unsafe {
                llvm_sys_180::target::LLVMOffsetOfElement(datalayout, closure_struct_type, 0)
                    as usize
            }]
        };
        let closure_descriptor = codegen_context.emit_type_descriptor(
            &format!("closure_{}_{}", self.start.line, self.start.column),
            0,
            closure_offsets,
        );

        // Size the closure from its real struct layout: a context
        // pointer plus a function pointer, including any padding.
        let closure_alloc_size = unsafe {
            llvm_sys_180::target::LLVMABISizeOfType(
                llvm_sys_180::target::LLVMGetModuleDataLayout(
                    codegen_context
                        .module_context
                        .current_module()
                        .read()
                        .unwrap()
                        .get_top_level()
                        .unwrap()
                        .llvm_module,
                ),
                closure_struct_type,
            )
        } as usize;
        let mut allocated_closure = codegen_context
            .allocate_managed_object(&closure_descriptor, closure_alloc_size, &closure_type)
            .unwrap();
        allocated_closure.value_type = closure_type.clone();

        let context_element = codegen_context.get_struct_element(
            &allocated_closure,
            &managed_pointer_type(PekoType::simple_type("void")),
            0,
        );
        codegen_context.build_store(&context_element, &allocated_context);

        let context_function =
            codegen_context.get_struct_element(&allocated_closure, &closure_function.value_type, 1);
        codegen_context.build_store(&context_function, &closure_function);

        CodegenValue::new(allocated_closure.llvm_value, closure_type)
    }
}

impl PekoValueBuilder for ClassAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Generic class declarations are tracked, not emitted.
        if !self.generics.is_empty() {
            let mut self_reference = self.clone();
            self_reference.generics.clear();

            let current_module = codegen_context.module_context.current_module().clone();
            let current_file = codegen_context.get_current_file().to_path_buf();
            codegen_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .get_class_generics_mut()
                .insert(
                    self.class_name.value.clone(),
                    CodegenClassGeneric::new(
                        self.visibility.clone(),
                        self.generics.clone(),
                        self_reference,
                        current_module,
                        current_file,
                        None,
                    ),
                );
            return codegen_context.create_null_pointer();
        }

        // Codegening a class has three primary steps:
        //
        // 1. Create the main virtual table, derive the super virtual
        //    table; then contextualize the vtables.
        // 2. Simulate the arguments and contextualize them.
        // 3. Contextualize and simulate the various methods.
        //
        // Peko classes have three method kinds: constructors,
        // operators, and normal methods. They are differentiated during
        // parsing; for codegen we only need the generic information.

        // --- Collect virtual table and attribute info ---

        let mut class_type = PekoType::from_string(
            self.class_name.value.as_str(),
            codegen_context.get_current_file(),
        );

        // Prepend all enclosing module names to the class type.
        let mut next_module = codegen_context.module_context.current_module().clone();
        loop {
            class_type
                .module_names
                .insert(0, next_module.read().unwrap().get_name().to_owned());
            let parent = next_module.read().unwrap().get_parent().cloned();
            match parent {
                Some(p) => next_module = p,
                None => break,
            }
        }

        let mut virtual_table_methods = IndexMap::new();
        let mut class_attributes = IndexMap::new();
        let mut parent_class = None;

        let main_virtual_table_struct_type = codegen_context.create_named_struct(format!(
            "{}::<<main_virtual_table>>",
            class_type.to_string()
        ));

        // Only add the vtable slot when the class actually has methods
        // or inherits from a class that does. The vtable is a managed
        // pointer (Pointer<void>), so the GC traces it like any other
        // managed attribute and can relocate the vtable allocation.
        if !self.methods.is_empty() || self.derives_from.len() == 1 {
            class_attributes.insert(
                "<main_virtual_table>".to_string(),
                CodegenClassAttribute::new(
                    VisibilityData::open_visibility(),
                    managed_pointer_type(PekoType::simple_type("void")),
                    0,
                    codegen_context.managed_pointer_type_of(main_virtual_table_struct_type),
                ),
            );
        }

        // Single inheritance: derive parent's attributes and methods.
        if self.derives_from.len() == 1 {
            let find_parent_class = codegen_context.get_class_by_type(&self.derives_from[0]);

            match find_parent_class {
                None => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.derives_from[0].start_position.clone(),
                            self.derives_from[0].end_position.clone(),
                            format!(
                                "cannot find class `{}`. Check the class name, that the class is declared, and that it is imported",
                                self.derives_from[0].to_string()
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                }
                Some(parent) => {
                    parent_class = Some(Box::new(parent.clone()));

                    // Inherit attributes (except the vtable slot).
                    for (attribute_name, attribute) in &parent.attributes {
                        if attribute_name != "<main_virtual_table>" {
                            class_attributes.insert(attribute_name.clone(), attribute.clone());
                        }
                    }

                    // Inherit methods.
                    virtual_table_methods.extend(parent.main_virtual_table.methods);
                }
            }
        } else if self.derives_from.len() > 1 {
            // Multiple inheritance is not currently supported.
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.derives_from[0].start_position.clone(),
                    self.derives_from.last().unwrap().end_position.clone(),
                    "cannot inherit from multiple classes".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        }

        // Pre-add the class to the context so methods can reference the
        // class type during their own codegen (self-reference).
        let class_struct_type = codegen_context.create_named_struct(class_type.to_string());
        let class = CodegenClass::new(
            class_type.clone(),
            parent_class.clone(),
            class_attributes.clone(),
            CodegenVirtualTable::new(virtual_table_methods, 0, main_virtual_table_struct_type),
            class_struct_type,
            codegen_context.module_context.current_module().clone(),
            None,
            // Descriptors are emitted per module after the struct body is
            // materialized (owner) or at import time (importer); start
            // empty.
            HashMap::new(),
        );
        codegen_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .get_classes_mut()
            .insert(self.class_name.value.clone(), class);

        // --- Materialize the class struct body (layout pass) ---
        //
        // This runs BEFORE method-signature generation. A method that refers to
        // the class by value (e.g. returns `Option<Self>`) triggers a layout
        // query on the class during signature gen; if the class struct body is
        // still opaque at that point the query returns a wrong/zero size and the
        // instance is under-allocated, so later field writes overflow into the
        // next heap object and corrupt the heap. Setting the struct body here
        // guarantees the layout exists before any such query.
        //
        // CRITICAL: this pass must NOT call expand_type or get_llvm_type on the
        // attribute types. For a generic field those force instantiation of the
        // generic class (the very recursion we are avoiding). Each own-declared
        // field is mapped to a layout PLACEHOLDER via layout_placeholder_type,
        // which takes the RAW (unexpanded) attribute type and resolves only
        // generic type parameters via the cheap generic-context lookup -- never
        // expand_type. A placeholder has the same size/alignment as the field's
        // real lowering, so the struct layout (size + field offsets) is exact.
        //
        // The attributes map already contains element 0 (the vtable slot, when
        // present) and any inherited attributes, inserted at class creation. We
        // append the class's own declared attributes here -- storing the RAW
        // attribute type for now (expansion is deferred to the post-method-gen
        // "Resolve attributes" pass) plus the placeholder llvm_type and the
        // struct index -- then build the body from the complete map in order, so
        // the body has the vtable slot, inherited fields, and own fields in the
        // same order and count the rest of codegen expects.
        for (attribute_name, attribute) in &self.attributes {
            // Reject duplicate attribute names (against the slots already in the
            // map: vtable + inherited).
            let already_present = codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .get_classes()[&self.class_name.value]
                .attributes
                .contains_key(&attribute_name.value);
            if already_present || class_attributes.contains_key(&attribute_name.value) {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        attribute_name.start.clone(),
                        attribute_name.end.clone(),
                        format!(
                            "cannot declare multiple attributes with the same name `{}` on a class. Attribute names must be unique within a class",
                            attribute_name.value
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                continue;
            }

            // struct index = current attribute count (vtable + inherited + the
            // own attributes appended so far), matching declaration order.
            let current_struct_index = codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .get_classes()[&self.class_name.value]
                .attributes
                .len();

            // Placeholder layout type from the RAW attribute type (no expand).
            let attribute_llvm_type =
                codegen_context.layout_placeholder_type(attribute.attribute_type.as_ref());

            // Store the RAW (unexpanded) attribute type for now; the real
            // expanded type is filled in by the "Resolve attributes" pass after
            // method-signature generation, when expansion is safe.
            let raw_attribute_type = attribute.attribute_type.as_ref().clone();

            codegen_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .get_classes_mut()
                .get_mut(&self.class_name.value)
                .unwrap()
                .attributes
                .insert(
                    attribute_name.value.clone(),
                    CodegenClassAttribute::new(
                        attribute.visibility.clone(),
                        raw_attribute_type,
                        current_struct_index,
                        attribute_llvm_type,
                    ),
                );
        }

        // Build the struct body from the COMPLETE attributes map (vtable slot,
        // inherited attributes, own attributes) in order -- the same source the
        // original code used. Snapshot the cached placeholder llvm_types under
        // the read lock; they are layout-correct, so no expansion is needed.
        let placeholder_field_types = codegen_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_classes()[&self.class_name.value]
            .attributes
            .iter()
            .map(|(_, attr)| attr.llvm_type)
            .collect_vec();

        codegen_context.set_struct_body(class_struct_type, placeholder_field_types);

        // --- Generate method signatures ---

        let mut function_values = Vec::new();

        for class_method in &self.methods {
            // Initialize the method's overload list when it is first seen.
            if !codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .get_classes()
                .get(&self.class_name.value)
                .unwrap()
                .main_virtual_table
                .methods
                .contains_key(&class_method.get_info().name.value)
            {
                codegen_context
                    .module_context
                    .current_module()
                    .write()
                    .unwrap()
                    .get_classes_mut()
                    .get_mut(&self.class_name.value)
                    .unwrap()
                    .main_virtual_table
                    .methods
                    .insert(class_method.get_info().name.value.clone(), Vec::new());
            }

            // The first method argument is always `this` of class type.
            let mut method_arguments = IndexMap::new();
            method_arguments.insert(
                "this".to_string(),
                CodegenArg::new(VisibilityData::open_visibility(), class_type.clone(), None),
            );

            for (argument_name, argument_declaration) in &class_method.get_info().arguments {
                let argument_expanded =
                    codegen_context.expand_type(&argument_declaration.argument_type);
                let argument_type = match argument_expanded {
                    Some(t) => t,
                    None => {
                        codegen_context.diagnostics.report_diagnostic(
                            diagnostics::PekoDiagnostic::new(
                                argument_declaration.argument_type.start_position.clone(),
                                argument_declaration.argument_type.end_position.clone(),
                                format!(
                                    "type `{}` is not defined. Check the type name and that the type is in scope",
                                    argument_declaration.argument_type.to_string(),
                                ),
                                diagnostics::DiagnosticType::Error,
                                codegen_context.get_current_file().to_path_buf(),
                            ),
                        );
                        PekoType::error_type()
                    }
                };

                method_arguments.insert(
                    argument_name.value.clone(),
                    CodegenArg::new(
                        argument_declaration.visibility.clone(),
                        argument_type,
                        argument_declaration.default_value.clone(),
                    ),
                );
            }

            // Add the var-args parameter if present.
            if class_method.get_info().varargs_type.is_some() {
                let varargs_type_expanded = codegen_context
                    .expand_type(class_method.get_info().varargs_type.as_ref().unwrap());

                let varargs_inner = match varargs_type_expanded {
                    Some(t) => t.to_string(),
                    None => PekoType::error_type().to_string(),
                };

                method_arguments.insert(
                    class_method.get_info().varargs_name.value.clone(),
                    CodegenArg::new(
                        VisibilityData::open_visibility(),
                        PekoType::from_string(
                            format!("Array<{}>", varargs_inner).as_str(),
                            codegen_context.get_current_file(),
                        ),
                        None,
                    ),
                );
            }

            let argument_types = method_arguments
                .iter()
                .map(|(_, arg)| arg.argument_type.clone())
                .collect_vec();

            let class_return_type = codegen_context.expand_type(&class_method.get_return_type());
            let class_return_type = match class_return_type {
                Some(t) => t,
                None => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            class_method.get_return_type().start_position.clone(),
                            class_method.get_return_type().end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                class_method.get_return_type().to_string(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    PekoType::error_type()
                }
            };

            let mut method_function_symbol_name = SymbolName::from(
                None,
                Some(self.class_name.value.clone()),
                class_method.get_info().name.value.as_str(),
                Some(argument_types.clone()),
            );
            method_function_symbol_name
                .module_names
                .extend(class_type.module_names.clone());

            let method_function = codegen_context.create_function(
                Some(method_function_symbol_name.clone()),
                argument_types.clone(),
                &class_return_type,
                false,
                false,
                false,
            );

            // Look for an existing overload that this method should
            // replace (override).
            let current_method_overloads = codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .get_classes()
                .get(&self.class_name.value)
                .unwrap()
                .main_virtual_table
                .methods[&class_method.get_info().name.value]
                .clone();

            let mut method_base_info = if current_method_overloads.is_empty() {
                None
            } else {
                codegen_context.choose_function_and_index(
                    current_method_overloads,
                    argument_types.clone(),
                    None,
                    true,
                )
            };

            // The override match must use exactly the same argument types.
            if method_base_info.is_some() {
                for ((_, argument), expected_argument_type) in method_base_info
                    .as_ref()
                    .unwrap()
                    .1
                    .arguments
                    .iter()
                    .skip(1)
                    .zip(argument_types.iter().skip(1))
                {
                    if !codegen_context.types_equal(&argument.argument_type, expected_argument_type)
                    {
                        method_base_info = None;
                        break;
                    }
                }
            }

            let codegen_function = CodegenFunction::new(
                class_method.get_info().visibility.clone(),
                class_return_type,
                method_arguments,
                if class_method.get_info().varargs_type.is_some() {
                    Some(
                        if codegen_context
                            .type_exists(class_method.get_info().varargs_type.as_ref().unwrap())
                        {
                            class_method
                                .get_info()
                                .varargs_type
                                .as_ref()
                                .unwrap()
                                .clone()
                        } else {
                            PekoType::error_type()
                        },
                    )
                } else {
                    None
                },
                codegen_context.qualify_value_to_current(method_function),
                // Override: reuse the parent's vtable index. New method:
                // assign the next available slot.
                if method_base_info.is_some() {
                    method_base_info.as_ref().unwrap().1.virtual_table_index
                } else {
                    let mut current_methods_length = 0;
                    for (_, method_list) in &codegen_context
                        .module_context
                        .current_module()
                        .write()
                        .unwrap()
                        .get_classes_mut()
                        .get_mut(&self.class_name.value)
                        .unwrap()
                        .main_virtual_table
                        .methods
                    {
                        current_methods_length += method_list.len();
                    }
                    current_methods_length
                },
                method_function_symbol_name,
                codegen_context.module_context.current_module().clone(),
                None,
                Some((
                    codegen_context
                        .module_context
                        .current_module()
                        .read()
                        .unwrap()
                        .classes[&self.class_name.value]
                        .class_type
                        .clone(),
                    codegen_context.module_context.current_module().clone(),
                )),
            );

            // Install the method either as an override or as a new
            // entry on the overload list.
            if let Some((base_idx, _)) = method_base_info {
                codegen_context
                    .module_context
                    .current_module()
                    .write()
                    .unwrap()
                    .get_classes_mut()
                    .get_mut(&self.class_name.value)
                    .unwrap()
                    .main_virtual_table
                    .methods[&class_method.get_info().name.value][base_idx as usize] =
                    codegen_function.clone();
            } else {
                codegen_context
                    .module_context
                    .current_module()
                    .write()
                    .unwrap()
                    .get_classes_mut()
                    .get_mut(&self.class_name.value)
                    .unwrap()
                    .main_virtual_table
                    .methods
                    .get_mut(&class_method.get_info().name.value)
                    .unwrap()
                    .push(codegen_function.clone());
            }

            function_values.push(codegen_function);
        }

        // Materialize the vtable struct body from the function types.
        let all_main_functions = codegen_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .get_classes_mut()
            .get_mut(&self.class_name.value)
            .unwrap()
            .main_virtual_table
            .methods
            .clone();

        let mut function_llvm_types = Vec::new();
        for (_, func_overloads) in &all_main_functions {
            for func in func_overloads {
                if let Some(llvm_type) = codegen_context.get_llvm_type(&func.get_type()) {
                    function_llvm_types.push(llvm_type);
                }
            }
        }

        codegen_context.set_struct_body(main_virtual_table_struct_type, function_llvm_types);

        // --- Resolve attributes (real types) ---
        //
        // The class struct body and the attribute map entries (with indices and
        // placeholder llvm_types) were already created in the layout pass before
        // method-signature generation, so the layout (size and field offsets) is
        // fixed and correct. This pass resolves each own-declared attribute's
        // REAL type via expand_type -- safe here because any generic class the
        // fields reference can be fully instantiated at this point -- and UPDATES
        // the existing map entry's attribute_type in place. It does NOT reinsert
        // (which would change indices/order) and does NOT rebuild the struct body
        // (the placeholder layout is already identical). Later passes (the GC
        // descriptor emission below, and method-body codegen) read these real
        // types for is_managed checks, descriptor offsets, and field access.
        for (attribute_name, attribute) in &self.attributes {
            let expanded_attribute_type =
                codegen_context.expand_type(attribute.attribute_type.as_ref());

            let attribute_type = match expanded_attribute_type {
                Some(t) => t,
                None => {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            attribute.attribute_type.start_position.clone(),
                            attribute.attribute_type.end_position.clone(),
                            format!(
                                "type `{}` is not defined. Check the type name and that the type is in scope",
                                attribute.attribute_type.to_string(),
                            ),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    PekoType::error_type()
                }
            };

            // Update the real type on the already-present entry, preserving its
            // struct index and placeholder llvm_type. A missing entry here means
            // the layout pass rejected it as a duplicate; skip it consistently.
            if let Some(existing) = codegen_context
                .module_context
                .current_module()
                .write()
                .unwrap()
                .get_classes_mut()
                .get_mut(&self.class_name.value)
                .unwrap()
                .attributes
                .get_mut(&attribute_name.value)
            {
                existing.attribute_type = attribute_type;
            }
        }

        // Emit the static GC type descriptor for this class. It lists the
        // byte offsets of every managed (address space 1) element: the
        // vtable slot (itself a managed allocation) and any attribute whose
        // type is a class instance, closure, or `Pointer<T>`. The collector
        // reads this to trace the object's children. Stored in the class so
        // allocation can write it into each instance's header.

        // Snapshot the attribute types and struct indices (and the module's
        // data layout) under the lock, then release it before calling
        // `is_managed` / `emit_type_descriptor`, which need `&mut self`.
        let (attribute_info, datalayout) = {
            let module = codegen_context.module_context.current_module();
            let class_ref = module.read().unwrap();
            let class_entry = &class_ref.get_classes()[&self.class_name.value];
            let datalayout = unsafe {
                llvm_sys_180::target::LLVMGetModuleDataLayout(
                    class_ref.get_top_level().unwrap().llvm_module,
                )
            };
            let info = class_entry
                .attributes
                .iter()
                .map(|(_, attr)| (attr.attribute_type.clone(), attr.struct_index))
                .collect_vec();
            (info, datalayout)
        };

        let mut managed_offsets = Vec::new();
        for (attribute_type, struct_index) in &attribute_info {
            if codegen_context.is_managed(attribute_type) {
                managed_offsets.push(unsafe {
                    llvm_sys_180::target::LLVMOffsetOfElement(
                        datalayout,
                        class_struct_type,
                        *struct_index as u32,
                    )
                } as usize);
            }
        }

        let descriptor =
            codegen_context.emit_class_descriptor(&class_type.to_mangled_string(), managed_offsets);

        let owning_uuid = codegen_context.get_owning_module_uuid();
        codegen_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .get_classes_mut()
            .get_mut(&self.class_name.value)
            .unwrap()
            .type_descriptor
            .insert(owning_uuid, descriptor);

        // --- Generate method bodies ---

        for (class_method, method_value) in self.methods.iter().zip(function_values) {
            if codegen_context.outside_declarations_only && codegen_context.outside_primary_module {
                break;
            }

            let previous_scoped_variables = codegen_context.scoped_variables.clone();
            codegen_context.scoped_variables.clear();

            let previous_local_scope = codegen_context.local_scope;
            codegen_context.local_scope = true;

            let previous_basic_block = codegen_context.current_basic_block;
            let previous_entry_block = codegen_context.current_entry_block;
            let previous_function = codegen_context.current_llvm_function;

            codegen_context.current_llvm_function = Some(
                method_value.function_value[&codegen_context.get_owning_module_uuid()].llvm_value,
            );

            let entry_block = codegen_context.create_new_block(Some("entry".to_string()));
            codegen_context.current_entry_block = Option::Some(entry_block);
            codegen_context.goto_block_end(entry_block);

            let method_codegen_value =
                method_value.function_value[&codegen_context.get_owning_module_uuid()].clone();

            let previous_return_type = codegen_context.current_return_type.clone();
            codegen_context.current_return_type =
                if class_method.get_return_type().to_string() != "void" {
                    Some(method_value.return_type.clone())
                } else {
                    None
                };

            // `this` is always the first allocated argument.
            let this_argument =
                codegen_context.get_allocated_function_argument(&method_codegen_value, 0);

            let this_variable = CodegenVariable::new(
                VisibilityData::open_visibility(),
                class_type.clone(),
                codegen_context.qualify_value_to_current(this_argument.clone()),
                None,
                codegen_context.module_context.current_module().clone(),
                None,
            );

            codegen_context
                .scoped_variables
                .insert(String::from("this"), this_variable.clone());

            let previous_current_this = codegen_context.current_this.clone();
            codegen_context.current_this = Some(this_variable);

            // Bind the remaining positional arguments.
            for (idx, (argument_name, argument_info)) in
                class_method.get_info().arguments.iter().enumerate()
            {
                let argument_type = if !codegen_context.type_exists(&argument_info.argument_type) {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            argument_info.argument_type.start_position.clone(),
                            argument_info.argument_type.end_position.clone(),
                            "argument type doesn't exist".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    PekoType::error_type()
                } else {
                    codegen_context
                        .expand_type(&argument_info.argument_type)
                        .unwrap()
                };

                let current_argument =
                    codegen_context.get_allocated_function_argument(&method_codegen_value, idx + 1);

                let qualified_argument = codegen_context.qualify_value_to_current(current_argument);
                codegen_context.scoped_variables.insert(
                    argument_name.value.clone(),
                    CodegenVariable::new(
                        argument_info.visibility.clone(),
                        argument_type,
                        qualified_argument,
                        None,
                        codegen_context.module_context.current_module().clone(),
                        None,
                    ),
                );
            }

            // Bind the var-args parameter if present.
            if class_method.get_info().varargs_type.is_some() {
                let varargs_argument = codegen_context.get_allocated_function_argument(
                    &method_codegen_value,
                    class_method.get_info().arguments.len() + 2,
                );

                let varargs_type = if codegen_context
                    .type_exists(class_method.get_info().varargs_type.as_ref().unwrap())
                {
                    class_method
                        .get_info()
                        .varargs_type
                        .as_ref()
                        .unwrap()
                        .clone()
                } else {
                    PekoType::error_type()
                };

                let qualified_varargs = codegen_context.qualify_value_to_current(varargs_argument);
                codegen_context.scoped_variables.insert(
                    class_method.get_info().varargs_name.value.clone(),
                    CodegenVariable::new(
                        VisibilityData::open_visibility(),
                        PekoType::from_string(
                            format!("Array<{}>", varargs_type.to_string()).as_str(),
                            codegen_context.get_current_file(),
                        ),
                        qualified_varargs,
                        None,
                        codegen_context.module_context.current_module().clone(),
                        None,
                    ),
                );
            }

            let previous_in_constructor = codegen_context.in_constructor;

            // Constructor: handle `super(...)` call before the body runs.
            if let ClassMethod::Constructor(_, super_call) = class_method {
                codegen_context.in_constructor = true;

                if super_call.is_some() && parent_class.is_none() {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            super_call.clone().unwrap().start.clone(),
                            super_call.clone().unwrap().end.clone(),
                            "cannot have super call on non-derived class".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                } else if let Some(super_call_ast) = super_call {
                    let mut arguments = Vec::new();
                    let mut super_call_keywords = HashMap::new();

                    for (argument_name, argument) in &super_call_ast.arguments {
                        let argument_value = argument.build_value(codegen_context);
                        arguments.push(argument_value.clone());

                        if let Some(name) = argument_name {
                            super_call_keywords.insert(name.value.clone(), argument_value);
                        }
                    }

                    arguments.insert(
                        0,
                        codegen_context.get_function_argument(&method_codegen_value, 0),
                    );

                    let super_constructor_overloads =
                        parent_class.clone().unwrap().main_virtual_table.methods
                            [&String::from("constructor")]
                            .clone();

                    let best_super_overload = codegen_context.choose_function(
                        super_constructor_overloads,
                        arguments
                            .iter()
                            .map(|arg| arg.value_type.clone())
                            .collect_vec(),
                        if super_call_keywords.is_empty() {
                            None
                        } else {
                            Some(
                                super_call_keywords
                                    .iter()
                                    .map(|(name, value)| (name.clone(), value.value_type.clone()))
                                    .collect(),
                            )
                        },
                        true,
                    );

                    match best_super_overload {
                        None => {
                            codegen_context.diagnostics.report_diagnostic(
                                diagnostics::PekoDiagnostic::new(
                                    super_call_ast.start.clone(),
                                    super_call_ast.end.clone(),
                                    "arguments to `super(...)` do not match any constructor overload of the parent class. Check the argument types against the parent's declared constructors".to_string(),
                                    diagnostics::DiagnosticType::Error,
                                    codegen_context.get_current_file().to_path_buf(),
                                ),
                            );
                        }
                        Some(super_overload) => {
                            let mut all_keywords = super_overload.arguments.len() > 1;
                            for (_, arg) in super_overload.arguments.iter().skip(1) {
                                if arg.default_value.is_none() {
                                    all_keywords = false;
                                    break;
                                }
                            }

                            let argument_values_boxed =
                                if super_call_keywords.is_empty() && all_keywords {
                                    super_overload
                                        .arguments
                                        .iter()
                                        .skip(1)
                                        .map(|(_, arg)| {
                                            let arg_value = arg
                                                .default_value
                                                .as_ref()
                                                .unwrap()
                                                .build_value(codegen_context);

                                            codegen_context
                                                .box_value_to_type(&arg.argument_type, &arg_value)
                                                .unwrap()
                                        })
                                        .collect_vec()
                                } else {
                                    arguments
                                        .iter()
                                        .zip(super_overload.arguments.iter())
                                        .map(|(arg, (_, expected))| {
                                            codegen_context
                                                .box_value_to_type(&expected.argument_type, arg)
                                                .unwrap()
                                        })
                                        .collect_vec()
                                };

                            let (previous_line, previous_file) =
                                if !super_overload.visibility.notrack {
                                    codegen_context.track_call_position(
                                        super_call_ast.start.file.to_string_lossy().into_owned(),
                                        super_call_ast.start.line,
                                    )
                                } else {
                                    (
                                        codegen_context.create_null_pointer(),
                                        codegen_context.create_null_pointer(),
                                    )
                                };

                            let uuid = codegen_context.get_owning_module_uuid();
                            codegen_context.call_function(
                                &super_overload.get_type(),
                                false,
                                super_overload.function_value[&uuid].llvm_value,
                                argument_values_boxed,
                            );

                            if !super_overload.visibility.notrack {
                                codegen_context.reset_call_position(&previous_line, &previous_file);
                            }
                        }
                    }
                }
            }

            // Walk the method body.
            let mut branch_returns = false;
            for ast in &class_method.get_info().body.value {
                if !branch_returns
                    && ast.build_value(codegen_context).value_type.to_string() == "<<returnexit>>"
                {
                    branch_returns = true;
                } else if branch_returns {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            ast.get_start().clone(),
                            class_method.get_info().body.value.last().unwrap().get_end().clone(),
                            "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    break;
                }
            }

            // Return-coverage check.
            if !branch_returns && class_method.get_return_type().to_string() != "void" {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        class_method.get_info().start.clone(),
                        class_method.get_info().end.clone(),
                        format!(
                            "function `{}` does not return on all paths. The declared return type is `{}`, but at least one execution path can reach the end of the function without returning",
                            class_method.get_info().name.value,
                            class_method.get_return_type().to_string()
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            } else if !branch_returns && class_method.get_return_type().to_string() == "void" {
                codegen_context.build_return(None);
            }

            // Restore outer state.
            codegen_context.scoped_variables = previous_scoped_variables;
            codegen_context.current_llvm_function = previous_function;
            match previous_basic_block {
                Some(block) => codegen_context.goto_block_end(block),
                None => codegen_context.current_basic_block = None,
            }
            codegen_context.current_entry_block = previous_entry_block;
            codegen_context.current_this = previous_current_this;
            codegen_context.local_scope = previous_local_scope;
            codegen_context.current_return_type = previous_return_type;
            codegen_context.in_constructor = previous_in_constructor;
        }

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for ModuleCreationAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Build the new module, anchored at the current module as its
        // parent.
        let mut new_module = CodegenModule::new(
            self.module_name.value.clone(),
            codegen_context.get_current_file(),
        );
        new_module.parent = Some(codegen_context.module_context.current_module().clone());
        new_module.visibility = self.visibility.clone();

        let new_module_ref = Arc::new(RwLock::new(new_module));

        codegen_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .get_modules_mut()
            .insert(self.module_name.value.clone(), Arc::clone(&new_module_ref));

        codegen_context
            .module_context
            .move_to_module(new_module_ref, false, false);

        // Codegen the module body inside the new module's scope.
        for ast in &self.module_body.value {
            ast.build_value(codegen_context);
        }

        codegen_context.module_context.move_out_of_module();
        codegen_context.create_null_pointer()
    }
}
