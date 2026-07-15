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
        pointer_type.array_depth += 1;

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
            // An FFI module (a `.peko.h` header) is only external C declarations;
            // it has no Peko globals and so no `set_globals` to run. Skip it
            // rather than reference a `set_globals` nothing defines (which links
            // fine from source but breaks against a prebuilt dependency).
            if peko_core::ffi::is_ffi_header(&module.read().unwrap().file) {
                continue;
            }
            // A source file bound under two names appears twice in the registry
            // as the same module. Its globals initializer is one function, so
            // record its name once; calling it twice would reference a name the
            // module never defines.
            let set_name = module
                .read()
                .unwrap()
                .get_top_level()
                .unwrap()
                .globals_info
                .globals_set_name
                .to_string(false);
            if !module_global_sets_names.contains(&set_name) {
                module_global_sets_names.push(set_name);
            }
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
            self.get_current_file(),
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

        // Initialize the foundational modules' globals first, in dependency
        // order (core is the base; runtime and collections build on it), so
        // later modules can rely on them. A module is imported either under an
        // alias (`runtime`) or, when unpacked, under its canonical id
        // (`std__core`), so both spellings of each set_globals name are
        // matched. A module that declares no globals (and so has no
        // `set_globals`) is simply skipped rather than required.
        ["core", "runtime", "collections"]
            .iter()
            .for_each(|modname| {
                let candidates = [
                    format!("{modname}::set_globals"),
                    format!("std__{modname}::set_globals"),
                ];
                let Some((index, _)) = global_sets
                    .iter()
                    .find_position(|global_set| candidates.iter().any(|c| c == *global_set))
                else {
                    return;
                };
                let global_method = global_sets.remove(index);

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

        let bundle_info = self.bundle_info.clone();

        for (global, global_ast) in top_level_info.globals_info.globals_to_set {
            self.current_expected_type_options = Some(vec![global.variable_type.clone()]);

            // Compiler-injected `bundle` module globals. When project
            // metadata is present, these globals take fixed values from the
            // manifest rather than the module's declared defaults. The value
            // is boxed to the global's declared type by the shared step
            // below, so a raw `i1` or `string` reconciles with `bool` /
            // `string` as needed.
            let injected = match &bundle_info {
                Some(bundle) => match global.variable_name.as_str() {
                    "bundle::name" => Some(self.create_string(bundle.name.clone())),
                    "bundle::identifier" => Some(self.create_string(bundle.identifier.clone())),
                    "bundle::app_id" => Some(self.create_string(bundle.app_id.clone())),
                    "bundle::host" => Some(self.create_string(bundle.host.clone())),
                    "bundle::version" => Some(self.create_string(bundle.version.clone())),
                    "bundle::framework" => Some(self.create_string(bundle.framework.clone())),
                    "bundle::scheme" => Some(self.create_string(bundle.scheme.clone())),
                    "bundle::window" => Some(self.create_string(bundle.window.clone())),
                    "bundle::debug" => Some(self.create_constant_boolean(bundle.debug)),
                    _ => None,
                },
                None => None,
            };

            let value = match injected {
                Some(value) => value,
                None => global_ast.build_value(self),
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
                                value.value_type,
                                global.variable_type
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
