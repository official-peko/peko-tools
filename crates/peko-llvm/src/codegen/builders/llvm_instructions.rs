//! Layer 1: basic block management and elementary instruction emission.
//!
//! These methods control the basic-block cursor and emit the simplest
//! family of LLVM instructions: store, load, branch, return, struct GEP,
//! pointer dereference.
//!
//! Allowed callees: `LlvmTypeBuilder`. (Some methods need `get_llvm_type`
//! to know the type to load through.)

use std::ptr::null_mut;

use llvm_sys_180::core;
use llvm_sys_180::prelude::{LLVMBasicBlockRef, LLVMTypeRef};
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::{
    CodegenValue, is_managed_pointer, pointee_type, reference_into,
};

/// Basic-block control and the instructions that depend on it.
pub trait LlvmInstructionBuilder {
    /// Position the LLVM builder at the end of `block`, recording it as the
    /// current basic block on the context.
    fn goto_block_end(&mut self, block: LLVMBasicBlockRef);

    /// Position the LLVM builder before the first instruction in `block`.
    /// Inserts and immediately deletes a no-op instruction to ensure the
    /// position is well-defined for an empty block.
    fn goto_block_start(&mut self, block: LLVMBasicBlockRef);

    /// Returns true when the builder has a current basic block (that is,
    /// instructions can be emitted right now).
    fn is_builder_in_scope(&mut self) -> bool;

    /// Append a fresh basic block to the current function. When
    /// `block_name` is `None`, LLVM picks an anonymous name.
    fn create_new_block(&mut self, block_name: Option<String>) -> LLVMBasicBlockRef;

    /// Delete a basic block from its parent function.
    fn remove_block(&mut self, block: LLVMBasicBlockRef);

    /// Emit `store value, pointer`.
    fn build_store(&mut self, pointer: &CodegenValue, value: &CodegenValue);

    /// Emit a typed load: returns a `CodegenValue` carrying the dereferenced type.
    fn load_value(&mut self, value: &CodegenValue) -> CodegenValue;

    /// Emit `br i1 condition, success, failure`.
    fn build_conditional_branch(
        &mut self,
        condition: &CodegenValue,
        success: LLVMBasicBlockRef,
        failure: LLVMBasicBlockRef,
    );

    /// Emit unconditional `br block`.
    fn build_branch(&mut self, block: LLVMBasicBlockRef);

    /// Emit `ret value` (or `ret void` when `return_value` is `None`).
    fn build_return(&mut self, return_value: Option<CodegenValue>);

    /// Emit a load through a pointer. The pointee type is T for a managed
    /// Pointer<T> and one less pointer depth for a raw T*.
    fn build_pointer_dereference(&mut self, pointer: &CodegenValue) -> CodegenValue;

    /// Emit a struct-element GEP. `item_type` is the logical Pekoscript
    /// type of the element; the returned value's `value_type` will be that
    /// type with `pointer_depth + 1` (since GEP returns a pointer).
    fn get_struct_element(
        &mut self,
        struct_pointer: &CodegenValue,
        item_type: &PekoType,
        index: usize,
    ) -> CodegenValue;

    /// Like `get_struct_element` but for closure context structs, where
    /// the closure's LLVM type is supplied separately rather than derived
    /// from the pointer's `value_type`.
    fn get_closure_context_element(
        &mut self,
        closure_context_pointer: &CodegenValue,
        closure_context_type: LLVMTypeRef,
        item_type: &PekoType,
        index: usize,
    ) -> CodegenValue;

    /// Emit a one-dimensional GEP through `array_pointer` at the given index.
    fn get_array_element(
        &mut self,
        array_pointer: &CodegenValue,
        index: &CodegenValue,
    ) -> CodegenValue;
}

impl LlvmInstructionBuilder for PekoCodegenContext {
    fn goto_block_end(&mut self, block: LLVMBasicBlockRef) {
        unsafe { core::LLVMPositionBuilderAtEnd(self.llvm_builder, block) };
        self.current_basic_block = Some(block);
    }

    fn goto_block_start(&mut self, block: LLVMBasicBlockRef) {
        // unsafe { core::LLVMPositionBuilderAtEnd(self.llvm_builder, block) };

        // // Insert a placeholder instruction so the builder has a valid
        // // anchor even when the block starts out empty.
        // let instr =
        //     unsafe { core::LLVMBuildAlloca(self.llvm_builder, core::LLVMInt8Type(), c"".as_ptr()) };

        if unsafe { core::LLVMGetFirstInstruction(block) } == null_mut() {
            unsafe { core::LLVMPositionBuilderAtEnd(self.llvm_builder, block) };
        } else {
            unsafe {
                core::LLVMPositionBuilderBefore(
                    self.llvm_builder,
                    core::LLVMGetFirstInstruction(block),
                );
            }
        }

        // Remove the placeholder now that the builder is positioned.
        // unsafe {
        //     core::LLVMInstructionEraseFromParent(instr);
        // }

        self.current_basic_block = Some(block);
    }

    fn is_builder_in_scope(&mut self) -> bool {
        self.current_basic_block.is_some()
    }

    fn create_new_block(&mut self, block_name: Option<String>) -> LLVMBasicBlockRef {
        match block_name {
            None => unsafe {
                core::LLVMAppendBasicBlock(self.current_llvm_function.unwrap(), c"".as_ptr())
            },
            Some(name) => {
                let owned = cstr(name);
                unsafe {
                    core::LLVMAppendBasicBlock(self.current_llvm_function.unwrap(), owned.as_ptr())
                }
            }
        }
    }

    fn remove_block(&mut self, block: LLVMBasicBlockRef) {
        unsafe {
            core::LLVMDeleteBasicBlock(block);
        }
    }

    fn build_store(&mut self, pointer: &CodegenValue, value: &CodegenValue) {
        unsafe { core::LLVMBuildStore(self.llvm_builder, value.llvm_value, pointer.llvm_value) };
    }

    fn load_value(&mut self, value: &CodegenValue) -> CodegenValue {
        // The loaded type is the pointee: T for Pointer<T>, or one less
        // pointer depth for a raw T*.
        let load_type = pointee_type(&value.value_type);
        let load_llvm_type = self.get_llvm_type(&load_type).unwrap();

        CodegenValue::new(
            unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    load_llvm_type,
                    value.llvm_value,
                    c"".as_ptr(),
                )
            },
            load_type,
        )
    }

    fn build_conditional_branch(
        &mut self,
        condition: &CodegenValue,
        success: LLVMBasicBlockRef,
        failure: LLVMBasicBlockRef,
    ) {
        unsafe { core::LLVMBuildCondBr(self.llvm_builder, condition.llvm_value, success, failure) };
    }

    fn build_branch(&mut self, block: LLVMBasicBlockRef) {
        unsafe { core::LLVMBuildBr(self.llvm_builder, block) };
    }

    fn build_return(&mut self, return_value: Option<CodegenValue>) {
        match return_value {
            Some(value) => unsafe {
                // When the value is a null pointer in address space 0 but
                // the function return type is a managed pointer (address
                // space 1), cast the null to the right address space first.
                // This happens on None return paths for Option<T> functions
                // where T is a managed class.
                let ret_llvm_value = {
                    let val_type = core::LLVMTypeOf(value.llvm_value);
                    let val_as = core::LLVMGetPointerAddressSpace(val_type);
                    let fn_ref = self.current_llvm_function.unwrap_or(std::ptr::null_mut());
                    if !fn_ref.is_null() && val_as == 0 {
                        let fn_type = core::LLVMGlobalGetValueType(fn_ref);
                        let ret_type = core::LLVMGetReturnType(fn_type);
                        let ret_as = core::LLVMGetPointerAddressSpace(ret_type);
                        if ret_as == 1 {
                            core::LLVMBuildAddrSpaceCast(
                                self.llvm_builder,
                                value.llvm_value,
                                ret_type,
                                c"".as_ptr(),
                            )
                        } else {
                            value.llvm_value
                        }
                    } else {
                        value.llvm_value
                    }
                };
                core::LLVMBuildRet(self.llvm_builder, ret_llvm_value);
            },
            None => unsafe {
                core::LLVMBuildRetVoid(self.llvm_builder);
            },
        }
    }

    fn build_pointer_dereference(&mut self, pointer: &CodegenValue) -> CodegenValue {
        let element_type = pointee_type(&pointer.value_type);

        CodegenValue::new(
            unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    self.get_llvm_type(&element_type).unwrap(),
                    pointer.llvm_value,
                    c"".as_ptr(),
                )
            },
            element_type,
        )
    }

    fn get_struct_element(
        &mut self,
        struct_pointer: &CodegenValue,
        item_type: &PekoType,
        index: usize,
    ) -> CodegenValue {
        let struct_llvm_type = self
            .get_llvm_type_full(&struct_pointer.value_type, true, false)
            .unwrap();

        // The interior pointer keeps the base's address space: a managed
        // base yields a managed Pointer<item_type>, a raw base a raw
        // item_type*.
        let base_managed = is_managed_pointer(&struct_pointer.value_type)
            || self.is_managed(&struct_pointer.value_type);
        let reference_type = reference_into(item_type, base_managed);

        CodegenValue::new(
            unsafe {
                core::LLVMBuildStructGEP2(
                    self.llvm_builder,
                    struct_llvm_type,
                    struct_pointer.llvm_value,
                    index as u32,
                    c"".as_ptr(),
                )
            },
            reference_type,
        )
    }

    fn get_closure_context_element(
        &mut self,
        closure_context_pointer: &CodegenValue,
        closure_context_type: LLVMTypeRef,
        item_type: &PekoType,
        index: usize,
    ) -> CodegenValue {
        // The context is always a managed allocation, so element pointers
        // into it are managed (address space 1).
        let reference_type = reference_into(item_type, true);

        CodegenValue::new(
            unsafe {
                core::LLVMBuildStructGEP2(
                    self.llvm_builder,
                    closure_context_type,
                    closure_context_pointer.llvm_value,
                    index as u32,
                    c"".as_ptr(),
                )
            },
            reference_type,
        )
    }

    fn get_array_element(
        &mut self,
        array_pointer: &CodegenValue,
        index: &CodegenValue,
    ) -> CodegenValue {
        // The element type is the pointee: T for a managed Pointer<T>
        // buffer, or one less pointer depth for a raw T[] / T*.
        let item_type = pointee_type(&array_pointer.value_type);

        let element_llvm_type = self.get_llvm_type(&item_type).unwrap();

        // The element pointer keeps the base's address space: managed for
        // a Pointer<T> buffer, raw otherwise.
        let base_managed = is_managed_pointer(&array_pointer.value_type);
        let result_type = reference_into(&item_type, base_managed);

        CodegenValue::new(
            unsafe {
                core::LLVMBuildGEP2(
                    self.llvm_builder,
                    element_llvm_type,
                    array_pointer.llvm_value,
                    vec![index.llvm_value].as_mut_ptr(),
                    1,
                    c"".as_ptr(),
                )
            },
            result_type,
        )
    }
}
