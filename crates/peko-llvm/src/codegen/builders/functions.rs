//! Layer 2: function creation and direct calls.
//!
//! These methods build LLVM function definitions and emit direct calls.
//! Indirect calls through closure pointers are also routed here because
//! the closure-call mechanics (loading the function pointer out of the
//! closure struct, prepending the closure context as a hidden first
//! argument) belong with the rest of the call-site logic.
//!
//! Named-function lookup (`call_named_function`) is not here. That is
//! part of the `ExecutionContextAlgorithms` impl on `PekoCodegenContext`,
//! because the simulator-like algorithms in `peko_core` call it through
//! the trait.
//!
//! Allowed callees: layers 0-1.

use itertools::Itertools;
use llvm_sys_180::core;
use llvm_sys_180::prelude::LLVMValueRef;
use peko_core::asts::data_structures::PositionData;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_instructions::LlvmInstructionBuilder;
use crate::codegen::builders::llvm_memory::LlvmMemoryBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::{CodegenValue, managed_pointer_type};
use crate::codegen::symbol::SymbolName;

/// Function-definition and direct-call construction.
pub trait FunctionBuilder {
    /// Add a function to the current module under the supplied raw name.
    /// `external` controls the LLVM linkage (`ExternalLinkage` vs.
    /// `CommonLinkage`). Used by higher-level builders that have already
    /// mangled the name themselves.
    fn create_function_raw(
        &mut self,
        name: String,
        argument_types: Vec<PekoType>,
        return_type: &PekoType,
        has_var_args: bool,
        external: bool,
    ) -> CodegenValue;

    /// Add a function to the current module, mangling its name through
    /// `SymbolName` when `name` is provided. When `name` is `None`, an
    /// auto-generated `___unnamed_N_<file>` name is used.
    /// `unnamed_offset` on the context is incremented to keep the names
    /// unique across the compilation unit.
    fn create_function(
        &mut self,
        name: Option<SymbolName>,
        argument_types: Vec<PekoType>,
        return_type: &PekoType,
        has_var_args: bool,
        external_visibility: bool,
        external: bool,
    ) -> CodegenValue;

    /// Emit `call` on `function_value`. When `function_type.is_closure`
    /// is `true`, the closure context is loaded from slot 0 of the
    /// closure struct, the function pointer is loaded from slot 1, the
    /// context is prepended to `arguments` as the first argument, and
    /// the call proceeds with the unwrapped function type.
    fn call_function(
        &mut self,
        function_type: &PekoType,
        var_args: bool,
        function_value: LLVMValueRef,
        arguments: Vec<CodegenValue>,
    ) -> CodegenValue;

    /// Return the Nth argument of `function` as a `CodegenValue`. The
    /// returned value's type comes from `function.value_type.generic_types[N]`.
    fn get_function_argument(
        &mut self,
        function: &CodegenValue,
        argument_index: usize,
    ) -> CodegenValue;

    /// Like `get_function_argument`, but the value is first stored to a
    /// fresh stack slot and the slot pointer is returned. Used at
    /// function entry when the body needs to mutate the parameter.
    fn get_allocated_function_argument(
        &mut self,
        function: &CodegenValue,
        argument_index: usize,
    ) -> CodegenValue;
}

impl FunctionBuilder for PekoCodegenContext {
    fn create_function_raw(
        &mut self,
        name: String,
        argument_types: Vec<PekoType>,
        return_type: &PekoType,
        has_var_args: bool,
        external: bool,
    ) -> CodegenValue {
        let function_type = PekoType::new(
            Vec::new(),
            String::new(),
            argument_types,
            0,
            0,
            0,
            Some(return_type.clone()),
            false,
            PositionData::default(),
            PositionData::default(),
        );

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

        let function_llvm_type = self
            .get_llvm_type_full(&function_type, true, has_var_args)
            .unwrap();
        let owned_name = cstr(&name);

        let function_value =
            unsafe { core::LLVMAddFunction(module, owned_name.as_ptr(), function_llvm_type) };

        unsafe {
            core::LLVMSetLinkage(
                function_value,
                llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
            );

            if !external {
                // Peko-defined functions can hold managed pointers live
                // across calls, so they participate in GC. The statepoint
                // strategy makes the RewriteStatepointsForGC pass emit a
                // stack map for each safepoint. External C functions are
                // not tagged: they have no managed roots to record.
                let gc_strategy = cstr("statepoint-example");
                core::LLVMSetGC(function_value, gc_strategy.as_ptr());
            }

            // The collector walks each stopped thread by following the
            // saved frame pointer chain. A standard frame record must
            // exist in every managed frame so the walk can read the
            // saved frame pointer and the return address from it. Force
            // the frame pointer on managed functions.
            set_frame_pointer_all(function_value);
        }

        CodegenValue::new(function_value, function_type)
    }

    fn create_function(
        &mut self,
        name: Option<SymbolName>,
        argument_types: Vec<PekoType>,
        return_type: &PekoType,
        has_var_args: bool,
        external_visibility: bool,
        external: bool,
    ) -> CodegenValue {
        let name = match name {
            None => {
                self.unnamed_offset += 1;
                [
                    "___unnamed_",
                    self.unnamed_offset.to_string().as_str(),
                    "_",
                    self.get_current_file().to_str().unwrap(),
                ]
                .concat()
            }
            Some(symbol) => symbol.to_string(!external_visibility),
        };

        self.create_function_raw(name, argument_types, return_type, has_var_args, external)
    }

    fn call_function(
        &mut self,
        function_type: &PekoType,
        var_args: bool,
        mut function_value: LLVMValueRef,
        mut arguments: Vec<CodegenValue>,
    ) -> CodegenValue {
        let mut final_function_type = function_type.clone();

        // Closures are { context: managed i8*, function: fn-pointer }
        // structs. Unwrap them: prepend the context to the argument list
        // and replace `function_value` with the loaded function pointer.
        if function_type.is_closure {
            final_function_type.is_closure = false;
            final_function_type
                .generic_types
                .insert(0, managed_pointer_type(PekoType::simple_type("void")));

            if function_type.function_type.is_none() {
                final_function_type.function_type = Some(Box::new(PekoType::simple_type("void")));
            }

            let function_llvm_type = self.get_llvm_type(&final_function_type).unwrap();
            let closure_llvm_type = self.get_llvm_type_full(function_type, true, false).unwrap();

            let new_function_value = unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    function_llvm_type,
                    core::LLVMBuildStructGEP2(
                        self.llvm_builder,
                        closure_llvm_type,
                        function_value,
                        1,
                        c"".as_ptr(),
                    ),
                    c"".as_ptr(),
                )
            };

            let closure_context = unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    // The context is a managed pointer (address space 1),
                    // matching the closure function's first parameter.
                    core::LLVMPointerType(core::LLVMInt8Type(), 1),
                    core::LLVMBuildStructGEP2(
                        self.llvm_builder,
                        closure_llvm_type,
                        function_value,
                        0,
                        c"".as_ptr(),
                    ),
                    c"".as_ptr(),
                )
            };

            function_value = new_function_value;

            arguments.insert(
                0,
                CodegenValue::new(
                    closure_context,
                    managed_pointer_type(PekoType::simple_type("void")),
                ),
            );
        }

        let final_function_llvm_type = self
            .get_llvm_type_full(&final_function_type, true, var_args)
            .unwrap();
        let return_type = final_function_type
            .function_type
            .as_ref()
            .unwrap()
            .as_ref()
            .clone();

        let mut argument_values = arguments.iter().map(|value| value.llvm_value).collect_vec();

        CodegenValue::new(
            unsafe {
                core::LLVMBuildCall2(
                    self.llvm_builder,
                    final_function_llvm_type,
                    function_value,
                    argument_values.as_mut_ptr(),
                    argument_values.len() as u32,
                    c"".as_ptr(),
                )
            },
            return_type,
        )
    }

    fn get_function_argument(
        &mut self,
        function: &CodegenValue,
        argument_index: usize,
    ) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMGetParam(function.llvm_value, argument_index as u32) },
            function.value_type.generic_types[argument_index].clone(),
        )
    }

    fn get_allocated_function_argument(
        &mut self,
        function: &CodegenValue,
        argument_index: usize,
    ) -> CodegenValue {
        let argument_variable_alloc =
            self.build_stack_allocation(&function.value_type.generic_types[argument_index]);
        let argument_value = CodegenValue::new(
            unsafe { core::LLVMGetParam(function.llvm_value, argument_index as u32) },
            function.value_type.generic_types[argument_index].clone(),
        );

        self.build_store(&argument_variable_alloc, &argument_value);

        argument_variable_alloc
    }
}

/// Mark an LLVM function with the `"gc-leaf-function"` attribute so that
/// RewriteStatepointsForGC does NOT wrap calls to it in a gc.statepoint. A
/// leaf function is one across which no managed pointer in the caller needs to
/// be relocated: the call cannot trigger a garbage collection that moves the
/// caller's live objects. Pure runtime/FFI calls (printf-style sinks, GC
/// lifecycle init/shutdown, leaf C helpers) are leaf; anything that can
/// allocate, collect, or block while another thread collects is NOT and must
/// remain a real safepoint.
pub fn set_gc_leaf_attribute(function_value: LLVMValueRef) {
    let key = c"gc-leaf-function";
    let value = c"true";
    unsafe {
        let attr = core::LLVMCreateStringAttribute(
            core::LLVMGetGlobalContext(),
            key.as_ptr(),
            (key.to_bytes().len()) as u32,
            value.as_ptr(),
            (value.to_bytes().len()) as u32,
        );
        // LLVMAttributeFunctionIndex (~0u32) targets the function itself
        // rather than a parameter or the return value.
        core::LLVMAddAttributeAtIndex(
            function_value,
            llvm_sys_180::LLVMAttributeFunctionIndex,
            attr,
        );
    }
}

/// Add the `"frame-pointer"="all"` attribute to a function so its prologue
/// keeps a standard frame record. The collector walks a stopped thread by
/// following the saved frame pointer chain, and the walk reads the saved
/// frame pointer and the return address out of each frame record. A managed
/// frame that omits the frame pointer has no such record, so the walk cannot
/// step through it or locate its root slots.
pub fn set_frame_pointer_all(function_value: LLVMValueRef) {
    let key = c"frame-pointer";
    let value = c"all";
    unsafe {
        let attr = core::LLVMCreateStringAttribute(
            core::LLVMGetGlobalContext(),
            key.as_ptr(),
            key.to_bytes().len() as u32,
            value.as_ptr(),
            value.to_bytes().len() as u32,
        );
        core::LLVMAddAttributeAtIndex(
            function_value,
            llvm_sys_180::LLVMAttributeFunctionIndex,
            attr,
        );
    }
}
