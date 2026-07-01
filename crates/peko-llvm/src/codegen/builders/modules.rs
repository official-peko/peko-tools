//! Layer 4: cross-module orchestration and final binary output.
//!
//! `import_modules` / `import_module` re-publish symbols from one
//! `CodegenModule` into another, re-creating the LLVM declarations in
//! the importing module's `LLVMModuleRef`. `link_modules` combines all
//! top-level LLVM modules into a single output module. `output_binary`
//! and `emit_ir` finalize the linked module to disk (object file or
//! `.ll` respectively).
//!
//! The output methods live in this trait rather than a separate
//! `BackendOutput` trait so that the "everything that operates on a
//! whole module" surface stays in one place.
//!
//! Public because `peko_cli` drives compilation pipelines and needs
//! these methods directly.

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::{Arc, RwLock};

use itertools::Itertools;
use llvm_sys_180::core;
use llvm_sys_180::prelude::LLVMModuleRef;
use llvm_sys_180::target_machine::LLVMTargetRef;
use peko_core::asts::data_structures::{PositionedValue, UnpackItem};
use peko_core::execution::data_structures::ExecutionModule;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::target::{OperatingSystem, PekoTarget};
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::{CodegenFunction, CodegenModule, CodegenValue};

/// Cross-module wiring and binary / IR output.
pub trait ModuleManager {
    /// Return the UUID of the top-level module that owns the currently
    /// executing scope. Convenience method around the
    /// `module_context.step_back_generics()` pattern used to traverse
    /// past any active generic instantiation frame.
    fn get_owning_module_uuid(&mut self) -> String;

    /// Re-publish symbols from `from` into `to` according to the
    /// supplied `unpacked_symbols`. Globals, classes, functions, generic
    /// templates, and nested modules are all re-created in `to`'s
    /// top-level LLVM module with `ExternalLinkage` so the link step
    /// can resolve them to their definitions in `from`.
    fn import_modules(
        &mut self,
        from: Arc<RwLock<CodegenModule>>,
        to: Arc<RwLock<CodegenModule>>,
        unpacked_symbols: Vec<UnpackItem>,
    );

    /// Import `imported_module` into the current module, also pulling
    /// in everything from the extern module. Convenience wrapper for
    /// the common "I'm about to use this import" call site.
    fn import_module(
        &mut self,
        imported_module: Arc<RwLock<CodegenModule>>,
        unpacked_symbols: Vec<UnpackItem>,
    );

    /// Link every loaded top-level LLVM module into a single output
    /// `LLVMModuleRef`. Also folds in the synthetic `globals_set`
    /// module created by `GlobalBuilder::create_global_set_module`. The
    /// returned module is the final IR; pass it to `output_binary` or
    /// `emit_ir` to write it out.
    fn link_modules(&mut self, globals_set: Arc<RwLock<CodegenModule>>) -> LLVMModuleRef;

    /// Initialize the LLVM target machinery for `target`, link the
    /// modules, and emit a relocatable object file to `output_file`.
    /// Returns `true` on success. On failure, error text is printed
    /// to stdout. Helpful for codegen diagnostics.
    fn output_binary(
        &mut self,
        target: PekoTarget,
        globals_set: Arc<RwLock<CodegenModule>>,
        output_file: impl AsRef<Path>,
    ) -> bool;

    /// Link the modules and emit textual LLVM IR to `path`.
    fn emit_ir(
        &mut self,
        globals_set: Arc<RwLock<CodegenModule>>,
        path: impl Into<std::path::PathBuf>,
    );
}

impl ModuleManager for PekoCodegenContext {
    fn get_owning_module_uuid(&mut self) -> String {
        let post_stack = self.module_context.step_back_generics();
        let uuid = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_uuid()
            .unwrap();
        self.module_context.step_forward(post_stack);
        uuid
    }

    fn import_modules(
        &mut self,
        from: Arc<RwLock<CodegenModule>>,
        to: Arc<RwLock<CodegenModule>>,
        unpacked_symbols: Vec<UnpackItem>,
    ) {
        let mut current_symbols = Vec::new();
        let mut current_module_unpacks = HashMap::new();
        let mut unpack_all = false;
        for unpacked_symbol in &unpacked_symbols {
            match unpacked_symbol {
                UnpackItem::Symbol(symbol) => {
                    current_symbols.push(symbol.clone());
                }
                UnpackItem::ModuleSymbols(module_unpack) => {
                    current_module_unpacks.insert(
                        module_unpack.module_name.clone(),
                        module_unpack.unpacked_items.clone(),
                    );
                }
                UnpackItem::All => {
                    unpack_all = true;
                }
            };
        }

        // Top-level UUID of the module being imported. A symbol is owned
        // by `from` when its declaring module shares this UUID; symbols
        // that `from` itself imported have a different one and are skipped.
        let from_top_uuid = from.read().unwrap().get_uuid();

        // The extern module holds external declarations whose parent is the
        // module that declared them, not the extern module itself. Every
        // symbol the extern module holds is owned by it for import purposes.
        let is_extern_import = Arc::ptr_eq(&from, &self.module_context.extern_module);

        // Import global variables.
        let variables = from.read().unwrap().variables.clone();
        for (variable_name, variable) in variables {
            let variable_parent = variable.read().unwrap().parent.clone();
            if !is_extern_import && variable_parent.read().unwrap().get_uuid() != from_top_uuid {
                continue;
            }

            let variable_type = variable.read().unwrap().variable_type.clone();
            self.module_context
                .move_to_module(from.clone(), false, false);
            let variable_llvm_type = self.get_llvm_type(&variable_type).unwrap();
            self.module_context.move_out_of_module();

            let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();

            if !variable
                .read()
                .unwrap()
                .variable_value
                .contains_key(&to_uuid)
            {
                let (qualified_name, external, variable_type_inner) = {
                    let variable = variable.read().unwrap();
                    (
                        variable.qualified_name.clone(),
                        variable.variable_visibility.external,
                        variable.variable_type.clone(),
                    )
                };
                let variable_qualified_name =
                    cstr(qualified_name.as_ref().unwrap().to_string(!external));

                let new_variable_value = CodegenValue::new(
                    unsafe {
                        let llvm_value = core::LLVMAddGlobal(
                            to.read().unwrap().get_top_level().unwrap().llvm_module,
                            variable_llvm_type,
                            variable_qualified_name.as_ptr(),
                        );
                        core::LLVMSetLinkage(
                            llvm_value,
                            llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                        );
                        core::LLVMSetExternallyInitialized(llvm_value, 1);
                        llvm_value
                    },
                    variable_type_inner,
                );

                variable
                    .write()
                    .unwrap()
                    .variable_value
                    .insert(to_uuid.clone(), new_variable_value);
            }

            if unpacked_symbols.is_empty()
                || (!unpack_all
                    && !current_symbols
                        .contains(&PositionedValue::create_no_position(variable_name.clone())))
            {
                continue;
            }

            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name
                                == &&PositionedValue::create_no_position(variable_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .variables
                .insert(variable_name, Arc::clone(&variable));
        }

        // Import traits. A trait is stored by value and resolved by name
        // through the module's trait map, so copy each entry. Without this a
        // bound-driven dispatch in this module (an `impl Trait` operator or
        // method on an erased generic parameter) cannot find a trait that an
        // imported module declares.
        let imported_traits = from.read().unwrap().traits.clone();
        for (trait_name, trait_definition) in imported_traits {
            if to.read().unwrap().traits.contains_key(&trait_name) {
                continue;
            }
            to.write().unwrap().traits.insert(trait_name, trait_definition);
        }

        // Import enums. Like traits, an enum is stored by value and resolved by
        // name; copy each entry so an enum type an imported module declares (a
        // token kind, a json value tag) resolves and compares in this module.
        let imported_enums = from.read().unwrap().enums.clone();
        for (enum_name, variants) in imported_enums {
            if to.read().unwrap().enums.contains_key(&enum_name) {
                continue;
            }
            to.write().unwrap().enums.insert(enum_name, variants);
        }

        // Import global functions.
        let functions = from.read().unwrap().functions.clone();
        for (function_name, function_options) in functions {
            for function in function_options.iter() {
                let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();
                if function
                    .read()
                    .unwrap()
                    .function_value
                    .contains_key(&to_uuid)
                {
                    continue;
                }

                let (function_type, variadic, qualified_name, external, gc_safepoint) = {
                    let function = function.read().unwrap();
                    (
                        function.get_type(),
                        function.visibility.variadic,
                        function.qualified_name.clone(),
                        function.visibility.external,
                        function.visibility.gc_safepoint,
                    )
                };

                self.module_context
                    .move_to_module(from.clone(), false, false);
                let function_llvm_type = self
                    .get_llvm_type_full(&function_type, true, variadic)
                    .unwrap();
                self.module_context.move_out_of_module();

                let function_qualified_name = cstr(qualified_name.to_string(!external));

                let new_function_value = CodegenValue::new(
                    unsafe {
                        core::LLVMAddFunction(
                            to.read().unwrap().get_top_level().unwrap().llvm_module,
                            function_qualified_name.as_ptr(),
                            function_llvm_type,
                        )
                    },
                    function_type,
                );

                unsafe {
                    core::LLVMSetLinkage(
                        new_function_value.llvm_value,
                        llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                    );
                }

                // An imported declaration must carry the same gc-leaf marking
                // its owning module gave it, or RewriteStatepointsForGC wraps
                // calls to it in a statepoint in this module. External
                // functions are leaf unless declared gcsafe; the allocation
                // entrypoints always collect and so are never leaf. This
                // mirrors the marking applied where the function is first
                // declared.
                let unmangled_name = qualified_name.to_string(false);
                let is_gc_alloc_entrypoint =
                    unmangled_name == "peko_gc_alloc_object" || unmangled_name == "peko_gc_alloc";
                if external && !gc_safepoint && !is_gc_alloc_entrypoint {
                    crate::codegen::builders::functions::set_gc_leaf_attribute(
                        new_function_value.llvm_value,
                    );
                }

                function
                    .write()
                    .unwrap()
                    .function_value
                    .insert(to_uuid, new_function_value);
            }

            // Publish only symbols `from` owns; skip re-exports.
            let owned = match function_options.first() {
                Some(option) => {
                    let parent = option.read().unwrap().parent.clone();
                    is_extern_import || parent.read().unwrap().get_uuid() == from_top_uuid
                }
                None => is_extern_import,
            };
            if !owned {
                continue;
            }

            if unpacked_symbols.is_empty()
                || (!unpack_all
                    && !current_symbols
                        .contains(&PositionedValue::create_no_position(function_name.clone())))
            {
                continue;
            }

            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name
                                == &&PositionedValue::create_no_position(function_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .functions
                .insert(function_name.clone(), function_options.clone());
        }

        // Import class methods.
        let classes = from.read().unwrap().classes.clone();
        for (class_name, class) in classes {
            // Snapshot the overloads to declare into `to`. Each overload is
            // shared, so declaring records the importing module's UUID in
            // the overload's value map.
            let method_entries: Vec<(String, Arc<RwLock<CodegenFunction>>)> = {
                let class_read = class.read().unwrap();
                let mut entries = Vec::new();
                for (method_name, method_options) in class_read.main_virtual_table.methods.iter() {
                    for option in method_options.iter() {
                        entries.push((method_name.clone(), Arc::clone(option)));
                    }
                }
                entries
            };

            let class_type = class.read().unwrap().class_type.clone();
            let class_type_string = class_type.to_string();
            let class_typenames = class.read().unwrap().generic_typenames.clone();

            for (method_name, option) in method_entries {
                let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();
                if option.read().unwrap().function_value.contains_key(&to_uuid) {
                    continue;
                }

                let (
                    option_type,
                    option_variadic,
                    option_return_type,
                    option_arguments,
                    option_qualified,
                    option_parent_class_info,
                ) = {
                    let option = option.read().unwrap();
                    (
                        option.get_type(),
                        option.visibility.variadic,
                        option.return_type.clone(),
                        option.arguments.clone(),
                        option.qualified_name.clone(),
                        option.parent_class_info.clone(),
                    )
                };

                self.module_context
                    .move_to_module(from.clone(), false, false);

                // An erased generic class's methods are typed in its generic
                // parameters; substitute them to bare carriers so the type
                // lowers (each parameter to a thin managed pointer) outside the
                // class's own generic context.
                let lowering_type = if class_typenames.is_empty() {
                    option_type.clone()
                } else {
                    crate::codegen::context::substitute_generic_params(
                        &option_type,
                        &crate::codegen::context::class_carrier_substitution(&class_typenames),
                    )
                };
                let function_llvm_type = self
                    .get_llvm_type_full(&lowering_type, true, option_variadic)
                    .unwrap();

                self.module_context.move_out_of_module();

                // Find the method's parent-class slot (if any) to
                // reuse the same `LLVMValueRef` across overrides.
                let mut parent_method: Option<CodegenValue> = None;
                let mut parent_slot: Option<Arc<RwLock<CodegenFunction>>> = None;
                if let Some((parent_type, parent_module)) = &option_parent_class_info
                    && parent_type.to_string() != class_type_string
                {
                    let parent_class_name = parent_type.declutter().to_string();
                    let parent_options: Vec<Arc<RwLock<CodegenFunction>>> = {
                        let parent_module_read = parent_module.read().unwrap();
                        parent_module_read.classes[&parent_class_name]
                            .read()
                            .unwrap()
                            .main_virtual_table
                            .methods[&method_name]
                            .iter()
                            .map(Arc::clone)
                            .collect()
                    };

                    for parent_option in parent_options {
                        let (parent_return, parent_arguments, parent_value) = {
                            let parent_option = parent_option.read().unwrap();
                            (
                                parent_option.return_type.clone(),
                                parent_option.arguments.clone(),
                                parent_option.function_value.get(&to_uuid).cloned(),
                            )
                        };

                        if !self.types_equal(&parent_return, &option_return_type)
                            || parent_arguments.len() != option_arguments.len()
                        {
                            continue;
                        }

                        let mut breakout = false;
                        for ((_, argument1), (_, argument2)) in
                            parent_arguments.iter().zip(option_arguments.iter()).skip(1)
                        {
                            if !self.types_equal(&argument1.argument_type, &argument2.argument_type)
                            {
                                breakout = true;
                                break;
                            }
                        }

                        if breakout {
                            continue;
                        }

                        if let Some(value) = parent_value {
                            parent_method = Some(value);
                        } else {
                            parent_slot = Some(parent_option);
                        }
                        break;
                    }
                }

                let new_function_value = match parent_method {
                    Some(value) => value,
                    None => {
                        let option_qualified_name = cstr(option_qualified.to_string(true));
                        CodegenValue::new(
                            unsafe {
                                core::LLVMAddFunction(
                                    to.read().unwrap().get_top_level().unwrap().llvm_module,
                                    option_qualified_name.as_ptr(),
                                    function_llvm_type,
                                )
                            },
                            option_type,
                        )
                    }
                };

                unsafe {
                    core::LLVMSetLinkage(
                        new_function_value.llvm_value,
                        llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                    );
                }

                if let Some(parent_slot) = parent_slot {
                    parent_slot
                        .write()
                        .unwrap()
                        .function_value
                        .insert(to_uuid.clone(), new_function_value.clone());
                }

                option
                    .write()
                    .unwrap()
                    .function_value
                    .insert(to_uuid, new_function_value);
            }

            let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();
            if !class.read().unwrap().type_descriptor.contains_key(&to_uuid) {
                let attribute_types: Vec<PekoType> = class
                    .read()
                    .unwrap()
                    .attributes
                    .values()
                    .map(|attribute| attribute.attribute_type.clone())
                    .collect();
                let mut managed_offset_count = 0;
                for attribute_type in &attribute_types {
                    if self.is_managed(attribute_type) {
                        managed_offset_count += 1;
                    }
                }

                let descriptor_declaration = {
                    self.module_context.move_to_module(to.clone(), false, false);
                    let declaration = self.declare_class_descriptor(
                        &class_type.to_mangled_string(),
                        managed_offset_count,
                    );
                    self.module_context.move_out_of_module();
                    declaration
                };

                class
                    .write()
                    .unwrap()
                    .type_descriptor
                    .insert(to_uuid, descriptor_declaration);
            }

            // Publish only classes `from` owns; skip re-exports.
            let class_parent = class.read().unwrap().parent.clone();
            if !is_extern_import && class_parent.read().unwrap().get_uuid() != from_top_uuid {
                continue;
            }

            if unpacked_symbols.is_empty()
                || (!unpack_all
                    && !current_symbols
                        .contains(&PositionedValue::create_no_position(class_name.clone())))
            {
                continue;
            }

            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name == &&PositionedValue::create_no_position(class_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .classes
                .insert(class_name, Arc::clone(&class));
        }

        // Generic templates are stored in the normal function and class maps
        // (under their bare names) and so are imported by the blocks above.

        // Recurse into submodules.
        for (module_name, submodule) in from.read().unwrap().modules.clone() {
            let import_module = unpacked_symbols.is_empty();
            let unpack_module = current_module_unpacks
                .contains_key(&PositionedValue::create_no_position(module_name.clone()))
                || unpack_all;
            let unpack_module_symbol =
                current_symbols.contains(&PositionedValue::create_no_position(module_name.clone()));

            if !unpack_module && !import_module && !unpack_module_symbol {
                continue;
            }

            if unpack_module_symbol {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name
                                == &&PositionedValue::create_no_position(module_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            self.import_modules(
                submodule.clone(),
                to.clone(),
                if unpack_module {
                    current_module_unpacks
                        .remove(&PositionedValue::create_no_position(module_name.clone()))
                        .unwrap()
                } else {
                    Vec::new()
                },
            );

            if unpack_module_symbol {
                to.write().unwrap().modules.insert(module_name, submodule);
            }
        }

        // Report any symbols / modules that the import statement asked
        // for but were never found in `from`.
        let current_file = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .file
            .clone();

        for unfound_symbol in current_symbols {
            self.diagnostics
                .report_diagnostic(peko_core::diagnostics::PekoDiagnostic::new(
                    unfound_symbol.start.clone(),
                    unfound_symbol.end.clone(),
                    format!(
                        "cannot find symbol {} in module {}",
                        unfound_symbol.value,
                        from.read().unwrap().name.clone()
                    ),
                    peko_core::diagnostics::DiagnosticType::Error,
                    current_file.clone(),
                ));
        }

        for (unfound_module, _) in current_module_unpacks {
            self.diagnostics
                .report_diagnostic(peko_core::diagnostics::PekoDiagnostic::new(
                    unfound_module.start.clone(),
                    unfound_module.end.clone(),
                    format!(
                        "cannot find module {} in module {}",
                        unfound_module.value,
                        from.read().unwrap().name.clone()
                    ),
                    peko_core::diagnostics::DiagnosticType::Error,
                    current_file.clone(),
                ));
        }
    }

    fn import_module(
        &mut self,
        imported_module: Arc<RwLock<CodegenModule>>,
        unpacked_symbols: Vec<UnpackItem>,
    ) {
        // First pull everything from the extern module so its symbols
        // are visible to whatever is being imported.
        self.import_modules(
            self.module_context.extern_module.clone(),
            self.module_context.current_module().clone(),
            Vec::new(),
        );

        self.import_modules(
            Arc::clone(&imported_module),
            self.module_context.current_module().clone(),
            unpacked_symbols,
        );

        self.module_context
            .extern_module
            .write()
            .unwrap()
            .add_imported_by(self.module_context.current_module().clone());

        imported_module
            .write()
            .unwrap()
            .add_imported_by(self.module_context.current_module().clone());
    }

    fn link_modules(&mut self, globals_set: Arc<RwLock<CodegenModule>>) -> LLVMModuleRef {
        if let Some(final_linked) = self.final_linked_module {
            return final_linked;
        }

        let final_module = unsafe { core::LLVMModuleCreateWithName(c"main".as_ptr()) };

        // A source file bound under two names appears twice in the registry as
        // the same module. Link each underlying source once so its symbols are
        // not defined twice in the final module.
        let mut linked_files = std::collections::HashSet::new();
        for (_, module) in &self.module_context.top_level_modules {
            let module_file = module.read().unwrap().get_file().to_path_buf();
            let module_file = module_file.canonicalize().unwrap_or(module_file);
            if !linked_files.insert(module_file) {
                continue;
            }
            unsafe {
                llvm_sys_180::linker::LLVMLinkModules2(
                    final_module,
                    core::LLVMCloneModule(
                        module.read().unwrap().get_top_level().unwrap().llvm_module,
                    ),
                );
            }
        }

        unsafe {
            llvm_sys_180::linker::LLVMLinkModules2(
                final_module,
                globals_set
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .llvm_module,
            );
        }

        self.final_linked_module = Some(final_module);
        final_module
    }

    fn output_binary(
        &mut self,
        target: PekoTarget,
        globals_set: Arc<RwLock<CodegenModule>>,
        output_file: impl AsRef<Path>,
    ) -> bool {
        unsafe {
            llvm_sys_180::target::LLVM_InitializeAllTargetInfos();
            llvm_sys_180::target::LLVM_InitializeAllTargets();
            llvm_sys_180::target::LLVM_InitializeAllTargetMCs();
            llvm_sys_180::target::LLVM_InitializeAllAsmParsers();
            llvm_sys_180::target::LLVM_InitializeAllAsmPrinters();
            llvm_sys_180::target::LLVM_InitializeNativeTarget();
        }

        let mut llvm_target: LLVMTargetRef =
            unsafe { llvm_sys_180::target_machine::LLVMGetFirstTarget() };

        let mut errors = String::new();
        let mut error_llvm = errors.as_mut_ptr() as *mut c_char;

        // Convert the triple string into a C string.
        let mut triple_string = target.to_triple();
        triple_string.push('\0');
        let triple_name = CStr::from_bytes_with_nul(triple_string.as_bytes()).unwrap();
        let target_triple = triple_name.as_ptr();

        if unsafe {
            llvm_sys_180::target_machine::LLVMGetTargetFromTriple(
                target_triple,
                &mut llvm_target,
                &mut error_llvm,
            )
        } == 1
        {
            return false;
        }

        let triple_cstring = cstr(target.to_triple());
        let cpu_generic = c"generic";
        let features_empty = c"";

        let target_machine = unsafe {
            llvm_sys_180::target_machine::LLVMCreateTargetMachine(
                llvm_target,
                triple_cstring.as_ptr(),
                cpu_generic.as_ptr(),
                features_empty.as_ptr(),
                llvm_sys_180::target_machine::LLVMCodeGenOptLevel::LLVMCodeGenLevelDefault,
                match target.operating_system {
                    OperatingSystem::Android | OperatingSystem::Linux => {
                        llvm_sys_180::target_machine::LLVMRelocMode::LLVMRelocPIC
                    }
                    _ => llvm_sys_180::target_machine::LLVMRelocMode::LLVMRelocDefault,
                },
                llvm_sys_180::target_machine::LLVMCodeModel::LLVMCodeModelDefault,
            )
        };

        let output_path_cstring = cstr(output_file.as_ref().to_str().unwrap());
        let linked_module = self.link_modules(globals_set);

        // Lower GC safepoints and statepoints on the linked module before code
        // generation. This is the path the CLI uses to emit objects; without
        // it the statepoint intrinsics that RewriteStatepointsForGC depends on
        // are never inserted or lowered, so the emitted objects reference an
        // unlowered llvm.experimental.gc.statepoint and no .llvm_stackmaps
        // section is produced, causing undefined-symbol errors at link time
        // (llvm.experimental.gc.statepoint and __LLVM_StackMaps). The poll
        // function must be synthesized into the linked module first so
        // place-safepoints can find and inline it.
        crate::codegen::data_structures::synthesize_safepoint_poll_into(linked_module);
        if let Err(text) =
            crate::codegen::data_structures::run_gc_statepoint_passes(linked_module, target_machine)
        {
            println!("GC statepoint pass error: {text}");
            return false;
        }

        // Optional debug dump of the module after the GC statepoint passes
        // have run, so the rewritten IR (gc.statepoint / gc.relocate, the
        // unwrapped intrinsics) can be inspected. Gated on the
        // PEKO_DUMP_GC_IR environment variable so normal builds pay no
        // cost. Writes to "<output>.post-gc.ll".
        if std::env::var_os("PEKO_DUMP_GC_IR").is_some() {
            let dump_c = cstr(self.root_folder.join("final.post-gc.ll").to_str().unwrap());
            let mut dump_error: *mut std::ffi::c_char = std::ptr::null_mut();
            unsafe {
                core::LLVMPrintModuleToFile(linked_module, dump_c.as_ptr(), &mut dump_error);
            }
        }

        let success = unsafe {
            llvm_sys_180::target_machine::LLVMTargetMachineEmitToFile(
                target_machine,
                linked_module,
                output_path_cstring.as_ptr() as *mut c_char,
                llvm_sys_180::target_machine::LLVMCodeGenFileType::LLVMObjectFile,
                &mut error_llvm,
            )
        } == 0;

        if !errors.is_empty() {
            println!("{errors}");
        }

        // On aarch64 Android and x86_64 Linux, ld.lld rejects ABS64
        // relocations in .llvm_stackmaps under PIC. Patch the section
        // header to SHF_WRITE so the dynamic linker applies them instead.
        if matches!(
            target.operating_system,
            OperatingSystem::Android | OperatingSystem::Linux
        ) && let Err(e) =
            crate::codegen::data_structures::patch_stackmaps_section_writable(output_file.as_ref())
        {
            eprintln!("warning: failed to patch .llvm_stackmaps: {e}");
        }

        success
    }

    fn emit_ir(
        &mut self,
        globals_set: Arc<RwLock<CodegenModule>>,
        path: impl Into<std::path::PathBuf>,
    ) {
        let path_buf = path.into();
        let path_cstring = cstr(path_buf.to_str().unwrap());
        let linked_module = self.link_modules(globals_set);

        unsafe {
            core::LLVMPrintModuleToFile(linked_module, path_cstring.as_ptr(), null_mut());
        }
    }
}
