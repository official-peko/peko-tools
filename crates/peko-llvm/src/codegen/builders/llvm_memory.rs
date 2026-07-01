//! Layer 1: allocation and object-layout access.
//!
//! These methods deal with the memory side of codegen: stack
//! allocation, GC heap allocation, and the LLVM level layout of class
//! objects (the vtable slot and individual vtable methods).
//!
//! Class semantics (method dispatch, attribute resolution by name,
//! inheritance walks) live in declaration_gen.rs and expression_gen.rs.
//! Object construction (allocating an instance and wiring its vtable)
//! lives one layer up in HighLevelCodegen::allocate_class.
//!
//! Allowed callees: LlvmTypeBuilder, LlvmConstantBuilder,
//! LlvmInstructionBuilder.

use llvm_sys_180::core;
use llvm_sys_180::prelude::LLVMTypeRef;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::builders::prelude::LlvmInstructionBuilder;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{CodegenValue, managed_pointer_type};

/// Memory allocation and object-layout access.
pub trait LlvmMemoryBuilder {
    /// Emit `alloca` for `value_type`. Returns a pointer-typed
    /// `CodegenValue`.
    fn build_stack_allocation(&mut self, value_type: &PekoType) -> CodegenValue;

    /// Allocate a managed object through the GC.
    ///
    /// Calls the runtime object allocator with the static type descriptor
    /// and the object size in bytes. The runtime prepends the object
    /// header, writes the descriptor pointer into it, and returns the
    /// object pointer. This helper then casts the raw pointer to the
    /// managed (address space 1) pointer type for `result_type`.
    ///
    /// `descriptor` is the `opaque` pointer to the type's static
    /// descriptor (or a null opaque pointer when the object has no traced
    /// children). `size` is the object size in bytes. `result_type` is
    /// the Peko type the returned value should carry.
    fn allocate_managed_object(
        &mut self,
        descriptor: &CodegenValue,
        size: usize,
        result_type: &PekoType,
    ) -> Option<CodegenValue>;

    /// Allocate a managed object with a runtime-computed size.
    ///
    /// Like `allocate_managed_object`, but `byte_count` is a runtime
    /// `CodegenValue` rather than a compile-time constant. Used for
    /// variable-length allocations such as array buffers, where the
    /// element count is only known at runtime.
    fn allocate_managed_object_sized(
        &mut self,
        descriptor: &CodegenValue,
        byte_count: &CodegenValue,
        result_type: &PekoType,
    ) -> Option<CodegenValue>;

    /// Allocate raw managed bytes through the GC.
    ///
    /// Calls the runtime raw allocator for `byte_count` bytes. The result
    /// is managed memory with no object header and no traced children
    /// (used for backing buffers and boxed values). The returned value is
    /// cast to a managed pointer of `result_type`.
    fn allocate_raw(&mut self, byte_count: usize, result_type: &PekoType) -> Option<CodegenValue>;

    /// GEP the vtable slot of `object` (always the first slot of any
    /// class object). When `load_vtable` is `true`, loads through the
    /// slot; otherwise returns the pointer to the slot.
    fn get_object_vtable(&mut self, object: &CodegenValue, load_vtable: bool) -> CodegenValue;

    /// GEP an individual method slot inside a vtable. `vtable_struct_type`
    /// is the LLVM struct type of the vtable; `method_index` is the slot
    /// index assigned at class-layout time. When `load_method` is `true`,
    /// loads the function pointer out of the slot.
    fn get_vtable_method(
        &mut self,
        vtable: &CodegenValue,
        vtable_struct_type: LLVMTypeRef,
        method_type: &PekoType,
        method_index: usize,
        load_method: bool,
    ) -> CodegenValue;
}

impl LlvmMemoryBuilder for PekoCodegenContext {
    fn build_stack_allocation(&mut self, value_type: &PekoType) -> CodegenValue {
        let mut pointer_type = value_type.clone();
        pointer_type.array_depth += 1;

        let llvm_type = self.get_llvm_type(value_type).unwrap();

        let current_bb = self.current_basic_block.unwrap();

        self.goto_block_start(self.current_entry_block.unwrap());

        let alloca = CodegenValue::new(
            unsafe { core::LLVMBuildAlloca(self.llvm_builder, llvm_type, c"".as_ptr()) },
            pointer_type,
        );

        self.goto_block_end(current_bb);

        alloca
    }

    fn allocate_managed_object(
        &mut self,
        descriptor: &CodegenValue,
        size: usize,
        result_type: &PekoType,
    ) -> Option<CodegenValue> {
        let size_arg = self.create_constant_int(size as i32);

        // The runtime allocates the object, prepends and initializes the
        // header from the descriptor, and returns the object pointer.
        let allocation = self.call_named_function(
            "extern::peko_gc_alloc_object",
            vec![descriptor.clone(), size_arg],
        )?;

        Some(cast_to_managed(self, &allocation, result_type))
    }

    fn allocate_managed_object_sized(
        &mut self,
        descriptor: &CodegenValue,
        byte_count: &CodegenValue,
        result_type: &PekoType,
    ) -> Option<CodegenValue> {
        // Same as allocate_managed_object, but the size is a runtime
        // value rather than a compile-time constant.
        let allocation = self.call_named_function(
            "extern::peko_gc_alloc_object",
            vec![descriptor.clone(), byte_count.clone()],
        )?;

        Some(cast_to_managed(self, &allocation, result_type))
    }

    fn allocate_raw(&mut self, byte_count: usize, result_type: &PekoType) -> Option<CodegenValue> {
        let size_arg = self.create_constant_int(byte_count as i32);

        let allocation = self.call_named_function("extern::peko_gc_alloc", vec![size_arg])?;

        Some(cast_to_managed(self, &allocation, result_type))
    }

    fn get_object_vtable(&mut self, object: &CodegenValue, load_vtable: bool) -> CodegenValue {
        let object_class = self.get_class_by_type(&object.value_type).unwrap();

        let vtable_element = unsafe {
            core::LLVMBuildStructGEP2(
                self.llvm_builder,
                object_class.struct_type,
                object.llvm_value,
                object_class.main_virtual_table.struct_index as u32,
                c"".as_ptr(),
            )
        };

        if load_vtable {
            // Offset-0 holds a raw (address-space-0) pointer to the class's
            // static TypeInfo. Load it, then load the vtable pointer out of the
            // TypeInfo's vtable field (index 3). Both are raw static pointers.
            let raw_pointer_type = unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) };
            let type_info = unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    raw_pointer_type,
                    vtable_element,
                    c"".as_ptr(),
                )
            };
            let type_info_type = self.type_info_struct_type();
            let vtable_field = unsafe {
                core::LLVMBuildStructGEP2(
                    self.llvm_builder,
                    type_info_type,
                    type_info,
                    3,
                    c"".as_ptr(),
                )
            };
            let vtable = unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    raw_pointer_type,
                    vtable_field,
                    c"".as_ptr(),
                )
            };

            // The loaded value is the raw vtable pointer itself.
            return CodegenValue::new(vtable, PekoType::simple_type("opaque"));
        }

        // The GEP result is a managed interior pointer to the offset-0 slot,
        // which holds a raw pointer to the static TypeInfo.
        CodegenValue::new(
            vtable_element,
            managed_pointer_type(PekoType::simple_type("opaque")),
        )
    }

    fn get_vtable_method(
        &mut self,
        vtable: &CodegenValue,
        vtable_struct_type: LLVMTypeRef,
        method_type: &PekoType,
        method_index: usize,
        load_method: bool,
    ) -> CodegenValue {
        let method_llvm_type = self.get_llvm_type(method_type).unwrap();

        let mut vtable_method = unsafe {
            core::LLVMBuildStructGEP2(
                self.llvm_builder,
                vtable_struct_type,
                vtable.llvm_value,
                method_index as u32,
                c"".as_ptr(),
            )
        };

        if load_method {
            vtable_method = unsafe {
                core::LLVMBuildLoad2(
                    self.llvm_builder,
                    method_llvm_type,
                    vtable_method,
                    c"".as_ptr(),
                )
            };

            // The loaded value is the function pointer itself.
            return CodegenValue::new(vtable_method, method_type.clone());
        }

        // The GEP result is a managed interior pointer to the method slot,
        // which holds a function pointer.
        CodegenValue::new(vtable_method, managed_pointer_type(method_type.clone()))
    }
}

/// Cast a raw allocation pointer (address space 0, as returned by the C
/// allocator) to the pointer type for `result_type`.
///
/// Managed object pointers live in address space 1, so when the result
/// type is managed this emits an `addrspacecast` from the raw allocation.
/// When the result type is itself a raw (address space 0) pointer, a
/// plain pointer cast is emitted instead, since `addrspacecast` requires
/// the source and destination address spaces to differ.
fn cast_to_managed(
    context: &mut PekoCodegenContext,
    allocation: &CodegenValue,
    result_type: &PekoType,
) -> CodegenValue {
    let target_llvm_type = context
        .get_llvm_type(result_type)
        .unwrap_or_else(|| unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 1) });

    let target_address_space = unsafe { core::LLVMGetPointerAddressSpace(target_llvm_type) };
    let source_address_space =
        unsafe { core::LLVMGetPointerAddressSpace(core::LLVMTypeOf(allocation.llvm_value)) };

    // When the allocation already lives in the target address space, emit no
    // cast at all.
    if source_address_space == target_address_space {
        return CodegenValue::new(allocation.llvm_value, result_type.clone());
    }

    // The allocation comes back in a different address space. Use
    // addrspacecast when crossing to/from the managed space (address space 1);
    // otherwise a plain pointer cast within address space 0.
    let cast = if target_address_space == 0 {
        unsafe {
            core::LLVMBuildPointerCast(
                context.llvm_builder,
                allocation.llvm_value,
                target_llvm_type,
                c"".as_ptr(),
            )
        }
    } else {
        unsafe {
            core::LLVMBuildAddrSpaceCast(
                context.llvm_builder,
                allocation.llvm_value,
                target_llvm_type,
                c"".as_ptr(),
            )
        }
    };

    CodegenValue::new(cast, result_type.clone())
}
