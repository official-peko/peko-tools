//! Layer 2: global variables and the per-module init function.
//!
//! These methods build LLVM globals and orchestrate the construction of
//! the synthetic `<module>::set_globals` function that runs all of a
//! module's global initializers in declaration order, as well as the
//! top-level `__create_globals` function that calls every module's
//! `set_globals` in turn.
//!
//! Module-to-module wiring (`link_modules`, `output_binary`, `emit_ir`)
//! lives in `ModuleManager`, not here, because that is the final-output
//! step rather than per-module init.
//!
//! Allowed callees: layers 0-1, plus `FunctionBuilder` (for the init
//! functions themselves) and `LlvmConstantBuilder::build_zero_value`
//! (for the initial value of each global). `init_module_globals` also
//! reaches up into `HighLevelCodegen::box_value_to_type` and
//! `ScopeManager` for call-position tracking, documented at that
//! method.

use std::sync::{Arc, RwLock};

use itertools::Itertools;
use llvm_sys_180::core;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::functions::FunctionBuilder;
use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_instructions::LlvmInstructionBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::{CodegenModule, CodegenValue};
use crate::codegen::symbol::SymbolName;

/// Global-variable construction and module-init wiring.
pub trait GlobalBuilder {
    /// Add a named global of `global_type` to the current module.
    /// When `initialize` is `true`, the global is given a zero
    /// initializer of the appropriate type. Otherwise the global is
    /// marked externally initialized.
    fn create_named_global(
        &mut self,
        name: Option<SymbolName>,
        global_type: &PekoType,
        external: bool,
        initialize: bool,
    ) -> CodegenValue;

    /// Add an anonymous global. When `external` is `true`, the global is
    /// externally initialized; otherwise it gets a zero initializer.
    fn create_global(&mut self, global_type: &PekoType, external: bool) -> CodegenValue;

    /// Build a `global_sets` synthetic module containing
    /// `__create_globals`, which calls every per-module `set_globals`
    /// in turn. Returns the new module; the caller is responsible for
    /// linking it into the final binary.
    fn create_global_set_module(&mut self) -> Arc<RwLock<CodegenModule>>;

    /// Like `create_global_set_module` but with an explicit list of
    /// init-function symbol names. Used by `create_global_set_module`
    /// after it discovers the names from the loaded modules.
    fn init_all_globals_specified(
        &mut self,
        global_sets: Vec<String>,
    ) -> Arc<RwLock<CodegenModule>>;

    /// Build the `<module>::set_globals` function for a single module.
    /// Walks `top_level_info.globals_info.globals_to_set`, evaluates
    /// each initializer AST, boxes the value into the global's declared
    /// type, stores it, and for managed globals (class instances,
    /// closures, Pointer<T>) calls the runtime's
    /// `peko_gc_add_global_root` to register the global as a GC root.
    fn init_module_globals(&mut self, module: &Arc<RwLock<CodegenModule>>);
}

impl GlobalBuilder for PekoCodegenContext {
    fn create_named_global(
        &mut self,
        name: Option<SymbolName>,
        global_type: &PekoType,
        external: bool,
        initialize: bool,
    ) -> CodegenValue {
        let mut pointer_type = global_type.clone();
        pointer_type.pointer_depth += 1;

        let llvm_type = self.get_llvm_type(global_type).unwrap();

        let post_stack = self.module_context.step_back_generics();
        let module = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_top_level()
            .unwrap()
            .llvm_module;
        self.module_context.step_forward(post_stack);

        let owned_name = match name {
            Some(symbol) => cstr(symbol.to_string(!external)),
            None => cstr(""),
        };

        let global_allocation =
            unsafe { core::LLVMAddGlobal(module, llvm_type, owned_name.as_ptr()) };

        unsafe {
            core::LLVMSetLinkage(
                global_allocation,
                llvm_sys_180::LLVMLinkage::LLVMCommonLinkage,
            );
        }

        if initialize {
            let zero = self.build_zero_value(global_type);
            unsafe {
                core::LLVMSetInitializer(global_allocation, zero.llvm_value);
            }
        } else {
            unsafe {
                core::LLVMSetExternallyInitialized(global_allocation, 1);
            }
        }

        CodegenValue::new(global_allocation, pointer_type)
    }

    fn create_global(&mut self, global_type: &PekoType, external: bool) -> CodegenValue {
        self.create_named_global(None, global_type, external, external)
    }

    fn create_global_set_module(&mut self) -> Arc<RwLock<CodegenModule>> {
        let mut module_global_sets_names = Vec::new();
        for (modname, module) in &self.module_context.top_level_modules {
            if modname == "extern" {
                continue;
            }
            module_global_sets_names.push(
                module
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .globals_info
                    .globals_set_name
                    .to_string(false),
            );
        }
        self.init_all_globals_specified(module_global_sets_names)
    }

    fn init_all_globals_specified(
        &mut self,
        mut global_sets: Vec<String>,
    ) -> Arc<RwLock<CodegenModule>> {
        // Build the `global_sets` synthetic module that holds the
        // top-level `__create_globals` function. The default-constructed
        // module comes with a placeholder globals function which is
        // discarded since we replace it with our own.
        let global_sets_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
            "global_sets",
            self.get_current_file().to_path_buf(),
            None,
            self.llvm_context,
        )));

        unsafe {
            core::LLVMDeleteFunction(
                global_sets_module
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .globals_info
                    .globals_function
                    .llvm_value,
            );
        }

        self.module_context
            .move_to_module(global_sets_module.clone(), false, false);

        let create_globals_function = self.create_function(
            Some(SymbolName::from(None, None, "__create_globals", None)),
            Vec::new(),
            &PekoType::simple_type("void"),
            false,
            true,
            false,
        );

        self.current_llvm_function = Some(create_globals_function.llvm_value);
        self.local_scope = true;

        let entry_block = self.create_new_block(Some("entry".to_string()));
        self.goto_block_end(entry_block);

        ["Runtime", "standard", "ui"].iter().for_each(|modname| {
            let global_method = global_sets.remove(
                global_sets
                    .iter()
                    .find_position(|global_set| {
                        global_set.as_str() == &format!("{modname}::set_globals")
                    })
                    .unwrap()
                    .0,
            );

            let globals_init_function = self.create_function_raw(
                global_method,
                Vec::new(),
                &PekoType::simple_type("void"),
                false,
                true,
            );

            self.call_function(
                &globals_init_function.value_type,
                false,
                globals_init_function.llvm_value,
                Vec::new(),
            );
        });

        for global_method in global_sets {
            let globals_init_function = self.create_function_raw(
                global_method,
                Vec::new(),
                &PekoType::simple_type("void"),
                false,
                true,
            );

            self.call_function(
                &globals_init_function.value_type,
                false,
                globals_init_function.llvm_value,
                Vec::new(),
            );
        }

        self.build_return(None);
        global_sets_module
    }

    fn init_module_globals(&mut self, module: &Arc<RwLock<CodegenModule>>) {
        // Cross-layer scope: `box_value_to_type` and the call-position
        // helpers are upward layer calls (Layer 3). Bringing them in
        // locally keeps the global-pollution narrow.
        use crate::codegen::builders::high_level::HighLevelCodegen;
        use crate::codegen::builders::scope::ScopeManager;

        self.module_context
            .move_to_module(module.clone(), false, false);

        let top_level_info = module.read().unwrap().get_top_level().unwrap();
        self.current_llvm_function = Some(top_level_info.globals_info.globals_function.llvm_value);

        let globals_entry_block = self.create_new_block(Some("entry".to_string()));
        self.goto_block_end(globals_entry_block);

        for (global, global_ast) in top_level_info.globals_info.globals_to_set {
            self.current_expected_type_options = Some(vec![global.variable_type.clone()]);

            // Special-cased compiler-injected globals for the styles
            // subsystem. When `compiled_styles_folder` is set, these two
            // globals get fixed values rather than going through the AST.
            let value = if self.compiled_styles_folder.is_some()
                && global.variable_name == "ui::debug_styles_dir"
            {
                self.create_string(
                    self.compiled_styles_folder
                        .clone()
                        .unwrap()
                        .to_str()
                        .unwrap(),
                )
            } else if self.compiled_styles_folder.is_some()
                && global.variable_name == "ui::debug_mode"
            {
                self.create_constant_boolean(true)
            } else if self.compiled_styles_folder.is_some()
                && global.variable_name == "assets::asset_debug"
            {
                self.create_constant_boolean(true)
            } else if self.compiled_styles_folder.is_some()
                && global.variable_name == "assets::asset_debug_dir"
            {
                self.create_cstring(self.asset_debug_folder.clone().unwrap().to_str().unwrap())
            } else if self.application_id.is_some()
                && global.variable_name == "storage::application_identifier"
            {
                self.create_cstring(self.application_id.clone().unwrap())
            } else {
                global_ast.build_value(self)
            };

            let (previous_line, previous_file) = self.track_call_position(
                global_ast.get_start().file.to_string_lossy().into_owned(),
                global_ast.get_start().line,
            );

            let value_boxed = self.box_value_to_type(&global.variable_type, &value);

            self.reset_call_position(&previous_line, &previous_file);

            let value_boxed = match value_boxed {
                Some(boxed) => boxed,
                None => {
                    self.diagnostics
                        .report_diagnostic(peko_core::diagnostics::PekoDiagnostic::new(
                            global_ast.get_start().clone(),
                            global_ast.get_end().clone(),
                            format!(
                                "cannot assign value of type `{}` to variable of type `{}`. The right-hand side type is not compatible with the variable's declared type",
                                value.value_type.to_string(),
                                global.variable_type.to_string()
                            ),
                            peko_core::diagnostics::DiagnosticType::Error,
                            global.file.clone(),
                        ));
                    continue;
                }
            };

            self.build_store(&global.value, &value_boxed);

            // Managed globals (class instances, closures, Pointer<T>) hold
            // address space 1 references the collector must trace, so they
            // are registered as GC roots. Raw pointers are not tracked.
            if self.is_managed(&global.variable_type) {
                let mut value_as_opaque = global.value.clone();
                value_as_opaque.value_type = PekoType::simple_type("opaque");

                self.call_named_function("extern::peko_gc_add_global_root", vec![value_as_opaque]);
            }
        }

        self.build_return(None);

        self.module_context.move_out_of_module();
    }
}
