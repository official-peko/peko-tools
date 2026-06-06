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
use std::path::Path;
use std::ptr::null_mut;
use std::sync::{Arc, RwLock};

use itertools::Itertools;
use llvm_sys_180::core;
use llvm_sys_180::prelude::LLVMModuleRef;
use llvm_sys_180::target_machine::LLVMTargetRef;
use peko_core::asts::data_structures::{PositionedValue, UnpackItem};
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::target::{OperatingSystem, PekoTarget};

use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::{CodegenModule, CodegenValue};

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

        // Import global variables.
        let variables = from.read().unwrap().variables.clone();
        for (variable_name, mut variable) in variables {
            if variable.imported_from.is_some() {
                continue;
            }

            self.module_context
                .move_to_module(from.clone(), false, false);
            let variable_llvm_type = self.get_llvm_type(&variable.variable_type).unwrap();
            self.module_context.move_out_of_module();

            let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();

            if !variable.variable_value.contains_key(&to_uuid) {
                let variable_qualified_name = cstr(
                    variable
                        .qualified_name
                        .as_ref()
                        .unwrap()
                        .to_string(!variable.variable_visibility.external),
                );

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
                    variable.variable_type.clone(),
                );

                variable
                    .variable_value
                    .insert(to_uuid.clone(), new_variable_value);

                from.write()
                    .unwrap()
                    .variables
                    .insert(variable_name.clone(), variable.clone());
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

            variable.imported_from = Some(from.clone());
            to.write()
                .unwrap()
                .variables
                .insert(variable_name, variable);
        }

        // Import global functions.
        let functions = from.read().unwrap().functions.clone();
        for (function_name, mut function_options) in functions {
            for (idx, function) in function_options.iter_mut().enumerate() {
                let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();
                if function.function_value.contains_key(&to_uuid)
                    || (function.imported_from.is_some()
                        && function
                            .imported_from
                            .as_mut()
                            .unwrap()
                            .write()
                            .unwrap()
                            .functions
                            .get_mut(&function_name)
                            .unwrap()[idx]
                            .function_value
                            .contains_key(&to_uuid))
                {
                    continue;
                }

                self.module_context
                    .move_to_module(from.clone(), false, false);
                let function_llvm_type = self
                    .get_llvm_type_full(&function.get_type(), true, function.visibility.variadic)
                    .unwrap();
                self.module_context.move_out_of_module();

                let function_qualified_name = cstr(
                    function
                        .qualified_name
                        .to_string(!function.visibility.external),
                );

                let new_function_value = CodegenValue::new(
                    unsafe {
                        core::LLVMAddFunction(
                            to.read().unwrap().get_top_level().unwrap().llvm_module,
                            function_qualified_name.as_ptr(),
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

                // An imported declaration must carry the same gc-leaf marking
                // its owning module gave it, or RewriteStatepointsForGC wraps
                // calls to it in a statepoint in this module. External
                // functions are leaf unless declared gcsafe; the allocation
                // entrypoints always collect and so are never leaf. This
                // mirrors the marking applied where the function is first
                // declared.
                let unmangled_name = function.qualified_name.to_string(false);
                let is_gc_alloc_entrypoint =
                    unmangled_name == "peko_gc_alloc_object" || unmangled_name == "peko_gc_alloc";
                if function.visibility.external
                    && !function.visibility.gc_safepoint
                    && !is_gc_alloc_entrypoint
                {
                    crate::codegen::builders::functions::set_gc_leaf_attribute(
                        new_function_value.llvm_value,
                    );
                }

                if function.imported_from.is_some() {
                    function
                        .imported_from
                        .as_mut()
                        .unwrap()
                        .write()
                        .unwrap()
                        .functions
                        .get_mut(&function_name)
                        .unwrap()[idx]
                        .function_value
                        .insert(to_uuid, new_function_value);
                } else {
                    function.function_value.insert(to_uuid, new_function_value);
                }
            }

            if matches!(function_options.first(), Some(option) if option.imported_from.is_some()) {
                continue;
            }

            from.write()
                .unwrap()
                .functions
                .insert(function_name.clone(), function_options.clone());

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

            for function in function_options.iter_mut() {
                function.imported_from = Some(from.clone());
            }

            to.write()
                .unwrap()
                .functions
                .insert(function_name.clone(), function_options.clone());
        }

        // Import class methods.
        let classes = from.read().unwrap().classes.clone();
        for (class_name, mut class) in classes {
            for (method_name, method_options) in class.main_virtual_table.methods.iter_mut() {
                for (idx, option) in method_options.iter_mut().enumerate() {
                    let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();
                    if option.function_value.contains_key(&to_uuid)
                        || (class.imported_from.is_some()
                            && class
                                .imported_from
                                .as_mut()
                                .unwrap()
                                .write()
                                .unwrap()
                                .classes
                                .get_mut(&class_name)
                                .unwrap()
                                .main_virtual_table
                                .methods[method_name][idx]
                                .function_value
                                .contains_key(&to_uuid))
                    {
                        continue;
                    }

                    self.module_context
                        .move_to_module(from.clone(), false, false);

                    let function_llvm_type = self
                        .get_llvm_type_full(&option.get_type(), true, option.visibility.variadic)
                        .unwrap();

                    self.module_context.move_out_of_module();

                    // Find the method's parent-class slot (if any) to
                    // reuse the same `LLVMValueRef` across overrides.
                    let mut parent_method: Option<CodegenValue> = None;
                    let mut parent_idx: i32 = -1;
                    if option.parent_class_info.is_some()
                        && option.parent_class_info.as_ref().unwrap().0.to_string()
                            != class.class_type.to_string()
                    {
                        for (parent_option_idx, parent_option) in option
                            .parent_class_info
                            .as_ref()
                            .unwrap()
                            .1
                            .read()
                            .unwrap()
                            .classes[&option
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

                            if parent_option.function_value.contains_key(&to_uuid) {
                                parent_method =
                                    Some(parent_option.function_value[&to_uuid].clone());
                            } else {
                                parent_idx = parent_option_idx as i32;
                            }
                            break;
                        }
                    }

                    let new_function_value = match parent_method {
                        Some(value) => value,
                        None => {
                            let option_qualified_name = cstr(option.qualified_name.to_string(true));
                            CodegenValue::new(
                                unsafe {
                                    core::LLVMAddFunction(
                                        to.read().unwrap().get_top_level().unwrap().llvm_module,
                                        option_qualified_name.as_ptr(),
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
                            .insert(to_uuid.clone(), new_function_value.clone());
                    }

                    if class.imported_from.is_some() {
                        class
                            .imported_from
                            .as_mut()
                            .unwrap()
                            .write()
                            .unwrap()
                            .classes
                            .get_mut(&class_name)
                            .unwrap()
                            .main_virtual_table
                            .methods[method_name][idx]
                            .function_value
                            .insert(to_uuid, new_function_value);
                        continue;
                    } else {
                        option.function_value.insert(to_uuid, new_function_value);
                    }
                }
            }

            let to_uuid = to.read().unwrap().get_uuid().as_ref().unwrap().clone();
            if !class.type_descriptor.contains_key(&to_uuid) {
                let managed_offset_count = {
                    let mut count = 0;
                    for (_, attribute) in class.attributes.iter() {
                        if self.is_managed(&attribute.attribute_type) {
                            count += 1;
                        }
                    }
                    count
                };

                let descriptor_declaration = {
                    self.module_context.move_to_module(to.clone(), false, false);
                    let declaration = self.declare_class_descriptor(
                        &class.class_type.to_mangled_string(),
                        managed_offset_count,
                    );
                    self.module_context.move_out_of_module();
                    declaration
                };

                class
                    .type_descriptor
                    .insert(to_uuid.clone(), descriptor_declaration.clone());

                if let Some(imported_from) = class.imported_from.as_mut() {
                    imported_from
                        .write()
                        .unwrap()
                        .classes
                        .get_mut(&class_name)
                        .unwrap()
                        .type_descriptor
                        .insert(to_uuid, descriptor_declaration.clone());
                }
            }

            if class.imported_from.is_some() {
                continue;
            }

            from.write()
                .unwrap()
                .classes
                .insert(class_name.clone(), class.clone());

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

            class.imported_from = Some(from.clone());
            to.write().unwrap().classes.insert(class_name, class);
        }

        // Import function generic templates.
        for (function_name, mut function) in from.read().unwrap().function_generics.clone() {
            if function.imported_from.is_some()
                || unpacked_symbols.is_empty()
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

            function.imported_from = Some(from.clone());
            to.write()
                .unwrap()
                .function_generics
                .insert(function_name, function);
        }

        // Import class generic templates.
        for (class_name, mut class) in from.read().unwrap().class_generics.clone() {
            if class.imported_from.is_some()
                || unpacked_symbols.is_empty()
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

            class.imported_from = Some(from.clone());
            to.write().unwrap().class_generics.insert(class_name, class);
        }

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
        if self.final_linked_module.is_some() {
            return self.final_linked_module.unwrap();
        }

        let final_module = unsafe { core::LLVMModuleCreateWithName(c"main".as_ptr()) };

        for (_, module) in &self.module_context.top_level_modules {
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
        let mut error_llvm = errors.as_mut_ptr() as *mut i8;

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
            let dump_path =
                format!("/Users/preston/Work/peko/planning/newtests/TestCLI/main.peko.post-gc.ll");
            let dump_c = cstr(dump_path);
            let mut dump_error: *mut std::ffi::c_char = std::ptr::null_mut();
            unsafe {
                core::LLVMPrintModuleToFile(linked_module, dump_c.as_ptr(), &mut dump_error);
            }
        }

        let success = unsafe {
            llvm_sys_180::target_machine::LLVMTargetMachineEmitToFile(
                target_machine,
                linked_module,
                output_path_cstring.as_ptr() as *mut i8,
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
        ) {
            if let Err(e) = crate::codegen::data_structures::patch_stackmaps_section_writable(
                output_file.as_ref(),
            ) {
                eprintln!("warning: failed to patch .llvm_stackmaps: {e}");
            }
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
