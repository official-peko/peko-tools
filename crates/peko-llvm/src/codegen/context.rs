//! The `PekoCodegenContext` struct, its constructor, and the
//! `ExecutionContextAlgorithms` impl.
//!
//! All LLVM-building methods that previously lived on the inherent impl
//! of `PekoCodegenContext` have been moved into layered traits under
//! [`crate::codegen::builders`]. See [`crate::codegen::builders`] for
//! the layer table and per-trait documentation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use llvm_sys_180::core;
use llvm_sys_180::prelude::{
    LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMModuleRef, LLVMValueRef,
};
use peko_core::ExternalModuleInfo;
use peko_core::execution::data_structures::ExecutionModule;
use peko_core::execution::{ExecutionContextAlgorithms, ExecutionModuleContext};
use peko_core::types::PekoType;

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::prelude::*;
use crate::codegen::cstr;
use crate::codegen::data_structures::{
    BooleanOperation, CodegenArg, CodegenClass, CodegenClassAttribute, CodegenClassGeneric,
    CodegenFunction, CodegenFunctionGeneric, CodegenModule, CodegenValue, CodegenVariable,
    CodegenVirtualTable, NumericalOperation,
};

#[derive(Clone)]
pub struct PekoCodegenContext {
    pub unnamed_offset: usize,

    pub state: Vec<String>,
    pub generated_args: Vec<CodegenValue>,
    pub generated_kw_args: Option<HashMap<String, CodegenValue>>,

    pub diagnostics: peko_core::diagnostics::DiagnosticList,
    pub errored: bool,

    pub external_modules: HashMap<String, ExternalModuleInfo>,
    pub module_context: ExecutionModuleContext<CodegenModule>,
    pub outside_primary_module: bool,
    pub outside_declarations_only: bool,
    pub target: peko_core::target::PekoTarget,
    pub creating_required: bool,

    pub files_to_link: Vec<PathBuf>,

    pub imported_styles: HashMap<PathBuf, String>,
    pub compiled_styles_folder: Option<PathBuf>,
    pub application_id: Option<String>,
    pub asset_debug_folder: Option<PathBuf>,

    pub previous_scoped_variables: Vec<HashMap<String, CodegenVariable>>,
    pub scoped_variables: HashMap<String, CodegenVariable>,
    pub local_scope: bool,
    pub generic_types: HashMap<String, PekoType>,

    pub return_references: bool,
    pub current_return_type: Option<PekoType>,
    pub current_expected_type_options: Option<Vec<PekoType>>,

    pub current_this: Option<CodegenVariable>,
    pub previous_was_this: bool,
    pub attributes_to_set: Vec<String>,

    pub primary_object: Option<CodegenValue>,
    pub accessed_state: Option<String>,

    pub in_constructor: bool,
    pub windowsgui: bool,

    pub llvm_builder: LLVMBuilderRef,
    pub llvm_context: LLVMContextRef,

    pub current_loop_finish_block: Option<LLVMBasicBlockRef>,
    pub current_loop_block: Option<LLVMBasicBlockRef>,
    pub current_basic_block: Option<LLVMBasicBlockRef>,
    pub current_entry_block: Option<LLVMBasicBlockRef>,
    pub current_llvm_function: Option<LLVMValueRef>,

    pub final_linked_module: Option<LLVMModuleRef>,

    /// The folder that `project::` imports resolve against and that
    /// canonical module ids are rooted at. The import logic reassigns
    /// this while a registry package loads and restores it afterward.
    pub root_folder: PathBuf,
}

impl PekoCodegenContext {
    /// Construct a fresh codegen context for the given target and source file.
    ///
    /// Creates the two default top-level LLVM modules (`main` and `extern`),
    /// initializes an empty diagnostic list, and binds a fresh LLVM IR
    /// builder. The returned context is positioned at the top level of the
    /// `main` module with no active function or basic block.
    pub fn new(
        target: peko_core::target::PekoTarget,
        current_file: PathBuf,
        outside_declarations_only: bool,
        root_folder: PathBuf,
    ) -> PekoCodegenContext {
        let llvm_context = unsafe { core::LLVMGetGlobalContext() };

        let main_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
            "main",
            current_file.clone(),
            None,
            llvm_context,
        )));

        let extern_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
            "extern",
            current_file,
            None,
            llvm_context,
        )));

        PekoCodegenContext {
            unnamed_offset: 0,

            state: Vec::new(),
            generated_args: Vec::new(),
            generated_kw_args: None,
            creating_required: false,

            diagnostics: peko_core::diagnostics::DiagnosticList::new(),
            errored: false,

            external_modules: HashMap::new(),
            outside_declarations_only,
            outside_primary_module: false,
            target,

            files_to_link: Vec::new(),

            imported_styles: HashMap::new(),
            compiled_styles_folder: None,
            asset_debug_folder: None,
            application_id: None,

            module_context: ExecutionModuleContext::new(main_module, extern_module),

            previous_scoped_variables: Vec::new(),
            scoped_variables: HashMap::new(),
            local_scope: false,
            generic_types: HashMap::new(),

            return_references: false,
            current_return_type: None,
            current_expected_type_options: None,

            current_this: None,
            previous_was_this: false,
            attributes_to_set: Vec::new(),

            primary_object: None,
            accessed_state: None,

            in_constructor: false,
            windowsgui: false,

            llvm_builder: unsafe { core::LLVMCreateBuilder() },
            llvm_context,

            current_loop_finish_block: None,
            current_loop_block: None,
            current_basic_block: None,
            current_entry_block: None,
            current_llvm_function: None,
            final_linked_module: None,
            root_folder,
        }
    }
}

impl
    ExecutionContextAlgorithms<
        CodegenModule,
        CodegenValue,
        CodegenVariable,
        CodegenFunction,
        CodegenArg,
        CodegenFunctionGeneric,
        CodegenClass,
        CodegenVirtualTable,
        CodegenClassAttribute,
        CodegenClassGeneric,
    > for PekoCodegenContext
{
    fn get_module_context(&self) -> &ExecutionModuleContext<CodegenModule> {
        &self.module_context
    }

    fn get_module_context_mut(&mut self) -> &mut ExecutionModuleContext<CodegenModule> {
        &mut self.module_context
    }

    fn get_generic_types(&self) -> &HashMap<String, PekoType> {
        &self.generic_types
    }

    fn get_current_this(&self) -> &Option<CodegenVariable> {
        &self.current_this
    }

    fn get_generic_types_mut(&mut self) -> &mut HashMap<String, PekoType> {
        &mut self.generic_types
    }

    fn get_current_this_mut(&mut self) -> &mut Option<CodegenVariable> {
        &mut self.current_this
    }

    fn get_external_modules(&self) -> &HashMap<String, ExternalModuleInfo> {
        &self.external_modules
    }

    fn get_root_folder(&self) -> &PathBuf {
        &self.root_folder
    }

    fn get_root_folder_mut(&mut self) -> &mut PathBuf {
        &mut self.root_folder
    }

    /// Generates a generic function with the provided type parameters.
    fn create_generic_function(
        &mut self,
        generic: &CodegenFunctionGeneric,
        type_parameters: Vec<PekoType>,
    ) -> Option<CodegenFunction> {
        let mut type_parameters_expanded = Vec::new();

        // Build the specialized function name with the type parameters
        // appended in angle brackets.
        let mut generic_function_name = generic.function.function_name.clone();
        generic_function_name.value.push('<');

        let post_stack = self.module_context.step_back();
        for parameter in type_parameters {
            let type_expanded = self.expand_type(&parameter)?;

            type_parameters_expanded.push(type_expanded.clone());

            generic_function_name
                .value
                .push_str(type_expanded.to_string().as_str());
            generic_function_name.value.push_str(", ");
        }
        self.module_context.step_forward(post_stack);

        // Strip trailing ", " and close the bracket.
        generic_function_name.value.pop();
        generic_function_name.value.pop();
        generic_function_name.value.push('>');

        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        // Map the generic typenames to their concrete values.
        // ex: class Generic<T> {...} | Generic<String> ~ T -> String
        let mut new_generic_types = HashMap::new();
        for (type_name, generic_type) in generic
            .generic_typenames
            .iter()
            .zip(type_parameters_expanded.iter())
        {
            new_generic_types.insert(type_name.value.clone(), generic_type.clone());
        }

        let previous_context_generic_types = self.get_generic_types().clone();
        self.get_generic_types_mut().clear();
        self.get_generic_types_mut().extend(new_generic_types);

        let mut generic_function = generic.function.clone();
        generic_function.function_name = generic_function_name.clone();

        let context = self.snapshot_context();

        self.module_context
            .move_to_module(generic.module.clone(), false, true);
        let module = self.module_context.current_module().clone();

        generic_function.generic_types.clear();
        generic_function.build_value(self);

        self.module_context.move_out_of_module();

        let mut function_reference =
            module.read().unwrap().get_functions()[&generic_function_name.value].clone();
        let first_function = function_reference[0].clone();

        let imported_by = module
            .read()
            .unwrap()
            .get_top_level()
            .as_ref()
            .unwrap()
            .imported_by
            .clone();
        for imported_to in imported_by {
            let imported_to_uuid = imported_to.read().unwrap().get_uuid().unwrap();
            for (idx, function) in function_reference.iter_mut().enumerate() {
                if function.function_value.contains_key(&imported_to_uuid)
                    || (function.imported_from.is_some()
                        && function
                            .imported_from
                            .as_mut()
                            .unwrap()
                            .write()
                            .unwrap()
                            .functions
                            .get_mut(&generic_function_name.value)
                            .unwrap()[idx]
                            .function_value
                            .contains_key(&imported_to_uuid))
                {
                    continue;
                }

                self.module_context
                    .move_to_module(module.clone(), false, false);
                let function_llvm_type = self
                    .get_llvm_type_full(&function.get_type(), true, function.visibility.variadic)
                    .unwrap();
                self.module_context.move_out_of_module();

                let owned_name = cstr(
                    function
                        .qualified_name
                        .to_string(!function.visibility.external),
                );

                let new_function_value = CodegenValue::new(
                    unsafe {
                        core::LLVMAddFunction(
                            imported_to
                                .read()
                                .unwrap()
                                .get_top_level()
                                .unwrap()
                                .llvm_module,
                            owned_name.as_ptr(),
                            function_llvm_type,
                        )
                    },
                    function.get_type(),
                );

                unsafe {
                    core::LLVMSetLinkage(
                        new_function_value.llvm_value,
                        llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                    );
                }

                if let Some(imported_from) = function.imported_from.as_mut() {
                    imported_from
                        .write()
                        .unwrap()
                        .functions
                        .get_mut(&generic_function_name.value)
                        .unwrap()[idx]
                        .function_value
                        .insert(imported_to_uuid.clone(), new_function_value);
                } else {
                    function
                        .function_value
                        .insert(imported_to_uuid.clone(), new_function_value);
                }
            }
        }

        module
            .write()
            .unwrap()
            .get_functions_mut()
            .insert(generic_function_name.value.clone(), function_reference);

        // After compiling the generic function body, declare the GC type
        // descriptors of every class the body allocated into each importing
        // module. The function body may allocate classes such as Option<T>
        // or Array<T> whose descriptors are defined in the generic's module.
        // Importing modules that call this function need external declarations
        // of those descriptors so the verifier does not see cross-module
        // references.
        {
            let module_uuid = module.read().unwrap().get_uuid().unwrap();
            let all_classes: Vec<_> = module
                .read()
                .unwrap()
                .get_classes()
                .iter()
                .map(|(name, class)| (name.clone(), class.clone()))
                .collect();

            let imported_by2 = module
                .read()
                .unwrap()
                .get_top_level()
                .as_ref()
                .unwrap()
                .imported_by
                .clone();

            for imported_to in imported_by2 {
                let imported_to_uuid = imported_to.read().unwrap().get_uuid().unwrap();
                for (_class_name, class) in &all_classes {
                    // Only act on classes whose descriptor is defined in
                    // the generic's module (has the module's own UUID) but
                    // not yet declared in the importing module.
                    if !class.type_descriptor.contains_key(&module_uuid) {
                        continue;
                    }
                    if class.type_descriptor.contains_key(&imported_to_uuid) {
                        continue;
                    }
                    let managed_offset_count = class
                        .attributes
                        .iter()
                        .filter(|(_, attr)| self.is_managed(&attr.attribute_type))
                        .count();
                    self.module_context
                        .move_to_module(imported_to.clone(), false, false);
                    self.declare_class_descriptor(
                        &class.class_type.to_mangled_string(),
                        managed_offset_count,
                    );
                    self.module_context.move_out_of_module();
                }
            }
        }

        self.reset_context(context);
        self.generic_types.clear();
        self.generic_types.extend(previous_context_generic_types);

        Some(first_function)
    }

    /// Generates a generic class with the provided type parameters.
    fn create_generic_class(
        &mut self,
        generic: &CodegenClassGeneric,
        type_parameters: Vec<PekoType>,
    ) -> Option<CodegenClass> {
        let mut type_parameters_expanded = Vec::new();

        let mut generic_class_name = generic.class.class_name.clone();
        generic_class_name.value.push('<');

        let post_stack = self.module_context.step_back();
        for parameter in type_parameters {
            let type_expanded = self.expand_type(&parameter)?;

            type_parameters_expanded.push(type_expanded.clone());

            generic_class_name
                .value
                .push_str(type_expanded.to_string().as_str());
            generic_class_name.value.push_str(", ");
        }
        self.module_context.step_forward(post_stack);

        generic_class_name.value.pop();
        generic_class_name.value.pop();
        generic_class_name.value.push('>');

        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        let mut new_generic_types = HashMap::new();
        for (type_name, generic_type) in generic
            .generic_typenames
            .iter()
            .zip(type_parameters_expanded.iter())
        {
            new_generic_types.insert(
                type_name.value.clone(),
                self.expand_type(generic_type).unwrap(),
            );
        }

        let previous_context_generic_types = self.generic_types.clone();
        self.generic_types.clear();
        self.generic_types.extend(new_generic_types);

        let mut generic_class = generic.class.clone();
        generic_class.class_name = generic_class_name.clone();

        let context = self.snapshot_context();

        self.module_context
            .move_to_module(generic.module.clone(), false, true);
        let module = self.module_context.current_module().clone();

        generic_class.build_value(self);

        self.module_context.move_out_of_module();

        let mut class_reference =
            module.read().unwrap().get_classes()[&generic_class_name.value].clone();

        let imported_by = module
            .read()
            .unwrap()
            .get_top_level()
            .as_ref()
            .unwrap()
            .imported_by
            .clone();
        for imported_to in imported_by {
            let imported_to_uuid = imported_to.read().unwrap().get_uuid().unwrap();

            // Register a descriptor DECLARATION for this importing module.
            // The instantiated generic class's descriptor is DEFINED only in
            // the generic's defining module (via build_value above); importing
            // modules that allocate this class need an external declaration of
            // the same descriptor symbol, keyed by their own UUID. Without it,
            // allocate_class fails its get_descriptor lookup for the importing
            // module. This mirrors the non-generic import path in
            // import_modules. The declaration's managed-offset count must match
            // the definition.
            if !class_reference
                .type_descriptor
                .contains_key(&imported_to_uuid)
            {
                let managed_offset_count = {
                    let mut count = 0;
                    for (_, attribute) in class_reference.attributes.iter() {
                        if self.is_managed(&attribute.attribute_type) {
                            count += 1;
                        }
                    }
                    count
                };

                let descriptor_declaration = {
                    self.module_context
                        .move_to_module(imported_to.clone(), false, false);
                    let declaration = self.declare_class_descriptor(
                        &class_reference.class_type.to_mangled_string(),
                        managed_offset_count,
                    );
                    self.module_context.move_out_of_module();
                    declaration
                };

                class_reference
                    .type_descriptor
                    .insert(imported_to_uuid.clone(), descriptor_declaration);
            }

            for (method_name, method_options) in
                class_reference.main_virtual_table.methods.iter_mut()
            {
                for (idx, option) in method_options.iter_mut().enumerate() {
                    if option.function_value.contains_key(&imported_to_uuid)
                        || (class_reference.imported_from.is_some()
                            && class_reference
                                .imported_from
                                .as_ref()
                                .unwrap()
                                .read()
                                .unwrap()
                                .classes
                                .get(&generic_class_name.value)
                                .unwrap()
                                .main_virtual_table
                                .methods[method_name][idx]
                                .function_value
                                .contains_key(&imported_to_uuid))
                    {
                        continue;
                    }

                    self.module_context
                        .move_to_module(module.clone(), false, false);

                    let function_llvm_type = self
                        .get_llvm_type_full(&option.get_type(), true, option.visibility.variadic)
                        .unwrap();

                    self.module_context.move_out_of_module();

                    let mut parent_method: Option<CodegenValue> = None;
                    let mut parent_idx: i32 = -1;
                    if let Some(parent_class_info) = &&option.parent_class_info
                        && parent_class_info.0.to_string() != class_reference.class_type.to_string()
                    {
                        for (parent_option_idx, parent_option) in
                            parent_class_info.1.read().unwrap().classes[&option
                                .parent_class_info
                                .as_ref()
                                .unwrap()
                                .0
                                .declutter()
                                .to_string()]
                                .main_virtual_table
                                .methods[method_name]
                                .iter()
                                .enumerate()
                        {
                            if !self.types_equal(&parent_option.return_type, &option.return_type)
                                || parent_option.arguments.len() != option.arguments.len()
                            {
                                continue;
                            }

                            let mut breakout = false;
                            for ((_, argument1), (_, argument2)) in parent_option
                                .arguments
                                .iter()
                                .zip(option.arguments.iter())
                                .skip(1)
                            {
                                if !self
                                    .types_equal(&argument1.argument_type, &argument2.argument_type)
                                {
                                    breakout = true;
                                    break;
                                }
                            }

                            if breakout {
                                continue;
                            }

                            if parent_option.function_value.contains_key(&imported_to_uuid) {
                                parent_method =
                                    Some(parent_option.function_value[&imported_to_uuid].clone());
                            } else {
                                parent_idx = parent_option_idx as i32;
                            }
                            break;
                        }
                    }

                    let new_function_value = match parent_method {
                        Some(value) => value,
                        None => {
                            let owned_name = cstr(option.qualified_name.to_string(true));
                            CodegenValue::new(
                                unsafe {
                                    core::LLVMAddFunction(
                                        imported_to
                                            .read()
                                            .unwrap()
                                            .get_top_level()
                                            .unwrap()
                                            .llvm_module,
                                        owned_name.as_ptr(),
                                        function_llvm_type,
                                    )
                                },
                                option.get_type(),
                            )
                        }
                    };

                    unsafe {
                        core::LLVMSetLinkage(
                            new_function_value.llvm_value,
                            llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                        );
                    }

                    if parent_idx >= 0 {
                        option
                            .parent_class_info
                            .as_ref()
                            .unwrap()
                            .1
                            .write()
                            .unwrap()
                            .classes
                            .get_mut(
                                &option
                                    .parent_class_info
                                    .as_ref()
                                    .unwrap()
                                    .0
                                    .declutter()
                                    .to_string(),
                            )
                            .unwrap()
                            .main_virtual_table
                            .methods
                            .get_mut(method_name)
                            .unwrap()[parent_idx as usize]
                            .function_value
                            .insert(imported_to_uuid.clone(), new_function_value.clone());
                    }

                    if class_reference.imported_from.is_some() {
                        module
                            .write()
                            .unwrap()
                            .get_classes_mut()
                            .get_mut(&generic_class_name.value)
                            .unwrap()
                            .imported_from
                            .as_mut()
                            .unwrap()
                            .write()
                            .unwrap()
                            .classes
                            .get_mut(&generic_class_name.value)
                            .unwrap()
                            .main_virtual_table
                            .methods[method_name][idx]
                            .function_value
                            .insert(imported_to_uuid.clone(), new_function_value);
                        continue;
                    } else {
                        option
                            .function_value
                            .insert(imported_to_uuid.clone(), new_function_value);
                    }
                }
            }
        }

        module
            .write()
            .unwrap()
            .get_classes_mut()
            .insert(generic_class_name.value.clone(), class_reference.clone());

        self.reset_context(context);
        self.generic_types.clear();
        self.generic_types.extend(previous_context_generic_types);

        Some(class_reference)
    }

    fn apply_operator(
        &mut self,
        operator: impl ToString,
        lhs: &CodegenValue,
        rhs: &CodegenValue,
    ) -> Option<CodegenValue> {
        let lhs_type = self.expand_type(&lhs.value_type)?;
        let rhs_type = self.expand_type(&rhs.value_type)?;

        let mut lhs = lhs.clone();
        let mut rhs = rhs.clone();

        lhs.value_type = lhs_type;
        rhs.value_type = rhs_type;

        // For closures, any operation returns `true`. This is so closures
        // can be used in generic classes without creating type errors.
        if lhs.value_type.is_closure
            && rhs.value_type.is_closure
            && self.types_equal(&lhs.value_type, &rhs.value_type)
        {
            return Some(self.create_constant_boolean(true));
        }

        let operator_str = operator.to_string();

        // If the LHS is a class, try the user-defined operator overload.
        if self.get_class_by_type(&lhs.value_type).is_some() {
            let call_overload = self.call_object_method(
                &lhs,
                format!("[operator {}]", operator_str),
                vec![rhs.clone()],
                None,
            );

            if let Ok(value) = call_overload {
                return Some(value);
            }

            // Overload failed. If the RHS is a datatype, try to cast the
            // LHS to that datatype and continue as if the LHS were itself
            // a datatype.
            if rhs.value_type.is_datatype() {
                let cast_to_datatype = self.call_object_method(
                    &lhs,
                    format!("[operator to_{}]", rhs.value_type),
                    Vec::new(),
                    None,
                );

                match cast_to_datatype {
                    Ok(value) => lhs = value,
                    Err(_) => return None,
                }
            }
        }

        // Numeric / boolean operations.
        if lhs.value_type.is_float() || lhs.value_type.is_integer() || lhs.value_type.is_datatype()
        {
            let rhs = self.box_value_to_type(&lhs.value_type, &rhs)?;

            if lhs.value_type.to_string() == "bool"
                && (operator_str == "&&" || operator_str == "||")
            {
                return Some(self.build_boolean_operation(
                    BooleanOperation::from(&operator_str).unwrap(),
                    &lhs,
                    &rhs,
                ));
            }

            if lhs.value_type.is_integer() {
                return Some(self.build_int_operation(
                    NumericalOperation::from(&operator_str).unwrap(),
                    &lhs,
                    &rhs,
                ));
            } else {
                return Some(self.build_float_operation(
                    NumericalOperation::from(&operator_str).unwrap(),
                    &lhs,
                    &rhs,
                ));
            }
        }

        // String equality / inequality. Covers managed `string` and the raw
        // string forms (cstr / char[] / &char).
        let lhs_stringish =
            lhs.value_type.is_string_type() || lhs.value_type.to_string() == "string";
        let rhs_stringish = (rhs.value_type.is_string_type()
            || rhs.value_type.to_string() == "string")
            || self
                .get_class_by_type(&rhs.value_type)
                .is_some_and(|class| {
                    class
                        .main_virtual_table
                        .methods
                        .contains_key("[operator to_string]")
                });
        if lhs_stringish && rhs_stringish && (operator_str == "==" || operator_str == "!=") {
            return Some(self.build_string_comparison(&lhs, &rhs, operator_str == "=="));
        }

        // Pointer equality / inequality.
        if lhs.value_type.is_pointer()
            && rhs.value_type.is_pointer()
            && (operator_str == "==" || operator_str == "!=")
        {
            return Some(self.build_pointer_comparison(&lhs, &rhs, operator_str == "=="));
        }

        // Managed-reference identity (== / !=). Class instances, Pointer<T>,
        // and closures are pointers at the LLVM level, so reference equality
        // and null checks are pointer-identity comparisons. This also covers
        // comparing a managed reference against a Default / null value, which
        // lowers to an opaque (address-space-0) null pointer; the comparison
        // helper casts both sides through a pointer-sized integer, so the
        // differing address spaces do not matter. At least one operand must be
        // a managed reference, and the other must be managed or an opaque /
        // null pointer.
        if operator_str == "==" || operator_str == "!=" {
            let lhs_managed = self.is_managed(&lhs.value_type);
            let rhs_managed = self.is_managed(&rhs.value_type);
            let lhs_opaque = lhs.value_type.to_string() == "opaque";
            let rhs_opaque = rhs.value_type.to_string() == "opaque";

            if (lhs_managed && rhs_opaque) || (lhs_opaque && rhs_managed) {
                return Some(self.build_pointer_comparison(&lhs, &rhs, operator_str == "=="));
            }
        }

        None
    }

    /// Calls a named function. The function name should include the
    /// owning module path when it cannot be located by type expansion
    /// alone.
    fn call_named_function(
        &mut self,
        function_name: impl ToString,
        function_arguments: Vec<CodegenValue>,
    ) -> Option<CodegenValue> {
        let mut function_name_type = PekoType::from_string(&function_name.to_string(), "");
        for generic_type in function_name_type.generic_types.iter_mut() {
            *generic_type = self.expand_type(generic_type)?;
        }

        // Walk to the module that owns the function.
        let mut next_module = if !function_name_type.module_names.is_empty() {
            self.module_context.top_level_modules[&function_name_type.module_names[0]].clone()
        } else {
            CodegenModule::get_top_parent(self.module_context.current_module())
        };

        for i in 1..function_name_type.module_names.len() {
            let child = next_module.read().unwrap().get_modules()
                [&function_name_type.module_names[i]]
                .clone();
            next_module = child;
        }

        let mut argument_types = Vec::new();
        for argument in &function_arguments {
            argument_types.push(argument.value_type.clone());
        }

        // Pick the best-matching overload from the function's option set.
        let function_to_call = self.choose_function(
            next_module.read().unwrap().get_functions()[&function_name_type.type_name].clone(),
            argument_types,
            None,
            false,
        )?;

        // Box arguments to the chosen overload's parameter types.
        let mut arguments_boxed = Vec::new();
        for (argument, (_, arg)) in
            itertools::izip!(&function_arguments, &function_to_call.arguments)
        {
            let boxed_argument = self.box_value_to_type(&arg.argument_type, argument)?;
            arguments_boxed.push(boxed_argument);
        }

        // Pass through any extra arguments to a variadic.
        if function_arguments.len() > function_to_call.arguments.len()
            && function_to_call.visibility.variadic
        {
            for argument in function_arguments
                .iter()
                .skip(function_to_call.arguments.len())
            {
                arguments_boxed.push(argument.clone());
            }
        }

        let post_stack = self.module_context.step_back_generics();
        let uuid = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_uuid()
            .unwrap();
        self.module_context.step_forward(post_stack);

        if !function_to_call.function_value.contains_key(&uuid) {
            println!("{} in {uuid}", function_name.to_string());
        }

        Some(self.call_function(
            &function_to_call.get_type(),
            function_to_call.visibility.variadic,
            function_to_call.function_value[&uuid].llvm_value,
            arguments_boxed,
        ))
    }

    fn call_object_method(
        &mut self,
        object: &CodegenValue,
        method_name: impl ToString,
        mut arguments: Vec<CodegenValue>,
        provided_arguments: Option<HashMap<String, CodegenValue>>,
    ) -> Result<CodegenValue, String> {
        // Objects are the first argument to a method.
        arguments.insert(0, object.clone());

        let object_class = match self.get_class_by_type(&object.value_type) {
            Some(class) => class,
            None => {
                return Err(format!(
                    "could not find object type '{}'",
                    object.value_type
                ));
            }
        };

        let method_name_str = method_name.to_string();

        let method_options = if object_class
            .main_virtual_table
            .methods
            .contains_key(&method_name_str)
        {
            object_class.main_virtual_table.methods[&method_name_str].clone()
        } else {
            return Err(format!(
                "could not find method type '{}' on object of type '{}'",
                method_name_str, object.value_type
            ));
        };

        let mut argument_types = Vec::new();
        for argument in &arguments {
            argument_types.push(argument.value_type.clone());
        }

        let provided_function_argument_types = provided_arguments.as_ref().map(|provided| {
            let mut arguments = HashMap::new();
            for (argument_name, argument_value) in provided {
                arguments.insert(argument_name.clone(), argument_value.value_type.clone());
            }
            arguments
        });

        let method = match self.choose_function(
            method_options,
            argument_types.clone(),
            provided_function_argument_types,
            true,
        ) {
            Some(method) => method,
            None => {
                return Err(format!(
                    "incorrect argument types for method '{}'",
                    method_name_str
                ));
            }
        };

        // Constructors are never virtual: the exact class being constructed
        // is always statically known, so there is no subtype whose
        // constructor could be reached through a base reference. Dispatch
        // them directly through the method's own symbol rather than loading
        // a function pointer out of the vtable. This avoids an indirect
        // statepoint (a gc.statepoint whose call target is a loaded pointer)
        // carrying multiple managed-pointer arguments.
        let direct_constructor_value = if method_name_str == "constructor" {
            method
                .function_value
                .get(&self.get_owning_module_uuid())
                .cloned()
        } else {
            None
        };

        let object_vtable_method = match direct_constructor_value {
            Some(direct_value) => direct_value,
            None => {
                let object_vtable = self.get_object_vtable(object, true);
                self.get_vtable_method(
                    &object_vtable,
                    object_class.main_virtual_table.llvm_type,
                    &method.get_type(),
                    method.virtual_table_index,
                    true,
                )
            }
        };

        // Private methods can only be called from inside the owning class.
        if method.visibility.private && self.current_this.is_none() {
            return Err(format!(
                "cannot access private method '{}' on object of type '{}'",
                method_name_str, object.value_type
            ));
        }

        // Determine whether every non-self argument has a default value;
        // when true the method can be invoked with all-keyword arguments.
        let mut all_args_keywords = method.arguments.len() > 1;
        for (_, arg) in method.arguments.iter().skip(1) {
            if arg.default_value.is_none() {
                all_args_keywords = false;
                break;
            }
        }

        let mut boxed_arguments = Vec::new();

        if !all_args_keywords
            || (provided_arguments.is_none()
                && (arguments.len() == method.arguments.len() || argument_types.len() != 1))
        {
            // Positional call.
            for (argument, (_, arg)) in itertools::izip!(&arguments, &method.arguments) {
                let boxed_argument_value = match self
                    .box_value_to_type(&arg.argument_type, argument)
                {
                    Some(value) => value,
                    None => {
                        return Err(format!(
                            "incorrect argument types for method '{}' (expected '{}' but got '{}')",
                            method_name_str, arg.argument_type, argument.value_type
                        ));
                    }
                };
                boxed_arguments.push(boxed_argument_value);
            }

            // Variadic tail: collect remaining positional args into the
            // var-args array.
            if let Some(var_args_type) = &method.var_args_type {
                let mut var_arguments = Vec::new();
                for arg in arguments.iter().skip(method.arguments.len()) {
                    var_arguments.push(arg.clone());
                }

                let create_array = self.create_standard_array(var_args_type, var_arguments);

                let create_array = match create_array {
                    Some(value) => value,
                    None => {
                        return Err(format!(
                            "could not create variable arguments of type '{}'",
                            var_args_type
                        ));
                    }
                };

                boxed_arguments.push(create_array);
            }
        } else {
            // Keyword call: for each declared arg, use the provided value
            // if any, otherwise its default.
            let provided_arguments = provided_arguments.unwrap_or_default();

            for (argument_name, arg) in method.arguments.iter().skip(1) {
                let argument_value = if provided_arguments.contains_key(argument_name) {
                    provided_arguments[argument_name].clone()
                } else {
                    arg.default_value.as_ref().unwrap().build_value(self)
                };

                let boxed_argument =
                    match self.box_value_to_type(&arg.argument_type, &argument_value) {
                        Some(value) => value,
                        None => {
                            return Err(format!(
                                "incorrect argument types for method '{}'",
                                method_name_str
                            ));
                        }
                    };

                boxed_arguments.push(boxed_argument);
            }

            boxed_arguments.insert(0, arguments[0].clone());
        }

        let function_call = self.call_function(
            &method.get_type(),
            false,
            object_vtable_method.llvm_value,
            boxed_arguments,
        );

        // State-change notification: if this method mutates state on a
        // primary object that has an accessed state, notify the primary
        // object's `onStateChanged`. Skip when we are inside a
        // constructor since the object is not fully initialized.
        if !self.in_constructor
            && method.visibility.mutates
            && self.primary_object.is_some()
            && self.accessed_state.is_some()
        {
            let method_name_value =
                self.create_string(self.accessed_state.as_ref().unwrap().clone());
            let _ = self.call_object_method(
                &self.primary_object.clone().unwrap(),
                "onStateChanged",
                vec![method_name_value],
                None,
            );
        }

        self.primary_object = None;
        self.accessed_state = None;

        Ok(function_call)
    }

    fn set_object_attribute(
        &mut self,
        object: &CodegenValue,
        attribute_name: impl ToString,
        value: &CodegenValue,
    ) -> bool {
        let attribute_name_str = attribute_name.to_string();
        let get_attribute_pointer =
            self.get_object_attribute(object, attribute_name_str.as_str(), false);

        // Saving these here because get_object_attribute sets them as a
        // side effect of the state-tracking machinery; we need to restore
        // them after we've used the result.
        let previous_accessed_state = self.accessed_state.clone();
        let previous_primary_object = self.primary_object.clone();
        self.accessed_state = None;
        self.primary_object = None;

        let get_attribute_pointer = match get_attribute_pointer {
            Ok(value) => value,
            Err(_) => return false,
        };

        let mut attribute_type = get_attribute_pointer.value_type.clone();
        attribute_type.decrease_pointer_depth();

        let box_value = match self.box_value_to_type(&attribute_type, value) {
            Some(value) => value,
            None => return false,
        };

        self.build_managed_store(&get_attribute_pointer, &box_value);

        if !self.in_constructor
            && let (Some(accessed_state), Some(primary_object)) =
                (&previous_accessed_state, previous_primary_object)
        {
            let attribute_name_value = self.create_string(accessed_state);
            let _ = self.call_object_method(
                &primary_object,
                "onStateChanged".to_owned(),
                vec![attribute_name_value],
                None,
            );
        }

        true
    }

    fn get_object_attribute(
        &mut self,
        object: &CodegenValue,
        attribute_name: impl ToString,
        load_value: bool,
    ) -> Result<CodegenValue, String> {
        let class = match self.get_class_by_type(&object.value_type) {
            Some(class) => class,
            None => {
                return Err(format!(
                    "could not find object type '{}'",
                    object.value_type
                ));
            }
        };

        let attribute_name_str = attribute_name.to_string();

        if !class.attributes.contains_key(&attribute_name_str) {
            return Err(format!(
                "object of type '{}' does not have attribute '{}'",
                attribute_name_str, object.value_type
            ));
        }

        if class.attributes[&attribute_name_str].visibility.private
            && self.get_current_this().is_none()
        {
            return Err(format!(
                "cannot access private attribute '{}' of object of type '{}'",
                attribute_name_str, object.value_type
            ));
        }

        self.accessed_state = if class.attributes[&attribute_name_str].visibility.state {
            Some(attribute_name_str.clone())
        } else {
            None
        };
        self.primary_object = Some(object.clone());

        let element_access = self.get_struct_element(
            object,
            &class.attributes[&attribute_name_str].attribute_type,
            class.attributes[&attribute_name_str].struct_index,
        );

        if load_value {
            Ok(self.load_value(&element_access))
        } else {
            Ok(element_access)
        }
    }

    fn create_standard_map(
        &mut self,
        key_type: &PekoType,
        value_type: &PekoType,
        key_value_pairs: Vec<(CodegenValue, CodegenValue)>,
    ) -> Option<CodegenValue> {
        let mut map_object_type = PekoType::from_string("Map", "");
        map_object_type.generic_types.push(key_type.clone());
        map_object_type.generic_types.push(value_type.clone());

        let map_object = self.create_object(&map_object_type, Vec::new())?;

        for (key, value) in &key_value_pairs {
            if self
                .call_object_method(
                    &map_object,
                    "insert",
                    vec![key.clone(), value.clone()],
                    None,
                )
                .is_err()
            {
                return None;
            }
        }

        Some(map_object)
    }

    fn create_standard_array(
        &mut self,
        array_type: &PekoType,
        values: Vec<CodegenValue>,
    ) -> Option<CodegenValue> {
        let mut array_object_type = PekoType::from_string("Array", "");
        array_object_type.generic_types.push(array_type.clone());

        let array_object = self.create_object(&array_object_type, Vec::new())?;

        for value in &values {
            if self
                .call_object_method(&array_object, "push", vec![value.clone()], None)
                .is_err()
            {
                return None;
            }
        }

        Some(array_object)
    }

    fn create_object(
        &mut self,
        class_type: &PekoType,
        constructor_arguments: Vec<CodegenValue>,
    ) -> Option<CodegenValue> {
        let object_class = self.get_class_by_type(class_type)?;
        let allocate_object = self.allocate_class(&object_class)?;

        // Initialize a normal (vtable-bearing) class.
        if object_class.main_virtual_table.get_method_count() != 0
            && self
                .call_object_method(
                    &allocate_object,
                    "constructor",
                    constructor_arguments.clone(),
                    None,
                )
                .is_err()
        {
            return None;
        }

        // Initialize a struct class (no methods, no constructor).
        if object_class.main_virtual_table.get_method_count() == 0 {
            if constructor_arguments.len() != object_class.attributes.len() {
                return None;
            }

            for ((attribute_name, _), attribute_value) in
                object_class.attributes.iter().zip(&constructor_arguments)
            {
                if !self.set_object_attribute(&allocate_object, attribute_name, attribute_value) {
                    return None;
                }
            }
        }

        Some(allocate_object)
    }
}
