//! Layer 3: high-level codegen orchestrators.
//!
//! Two methods live here: box_value_to_type (type-driven coercion) and
//! allocate_class (object construction including vtable allocation and
//! wiring). Both are orchestrators. They sit on top of lower layers and
//! compose them, rather than reaching the LLVM API directly.
//!
//! box_value_to_type calls into nearly every lower layer: type tests
//! through the core ExecutionContextAlgorithms helpers, method dispatch
//! through call_object_method, numeric casts through
//! LlvmArithmeticBuilder, and class allocation through this trait's own
//! allocate_class.
//!
//! allocate_class is here rather than in LlvmMemoryBuilder because it
//! crosses several layers: it needs the module UUID (ModuleManager),
//! constant and descriptor builders (LlvmConstantBuilder), type sizes
//! (LlvmTypeBuilder), and the vtable GEP helpers in LlvmMemoryBuilder.
//! Putting it at Layer 3 keeps the rule that lower layers never reach
//! upward.

use llvm_sys_180::core;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::cstr;
use crate::codegen::builders::llvm_arithmetic::LlvmArithmeticBuilder;
use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_instructions::LlvmInstructionBuilder;
use crate::codegen::builders::llvm_memory::LlvmMemoryBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::builders::modules::ModuleManager;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{CodegenClass, CodegenValue, is_managed_pointer};

/// High-level codegen orchestrators that span lower layers.
pub trait HighLevelCodegen {
    /// Convert `value` to a value of `expected_type`, emitting whatever
    /// IR is necessary. Returns `None` when the types are incompatible
    /// and no coercion path exists; the caller is expected to surface
    /// that as a diagnostic. Returns `Some(value)` unchanged when
    /// `value` already has `expected_type` exactly or when `value` is an
    /// error value.
    ///
    /// Conversion paths attempted, in order:
    ///
    /// 1. Error value: pass-through.
    /// 2. `expected_type` is `Option<T>` and `value` is `T`: allocate an
    ///    `Option<T>` and call its constructor.
    /// 3. Types are not similar (per `types_similar`): no path; `None`.
    /// 4. Types are equal: pass-through.
    /// 5. Both types resolve to class types: pointer reinterpretation
    ///    (opaque pointer layout means no LLVM cast is emitted, only the
    ///    `value_type` is rewritten).
    /// 6. `value` is a class and `expected_type` is a builtin: dispatch
    ///    to the `[operator to_<builtin>]` overload on the class.
    /// 7. `expected_type` is a class and `value` is a builtin: allocate
    ///    the wrapper class and call its constructor on `value`.
    /// 8. Numeric cast via `typecast_number_value`.
    /// 9. String-to-datatype cast via `Runtime::StringTo*` runtime functions.
    /// 10. Datatype-to-string cast via `Runtime::*ToString` runtime functions.
    /// 11. Pointer reinterpretation between two pointer-typed values.
    fn box_value_to_type(
        &mut self,
        expected_type: &PekoType,
        value: &CodegenValue,
    ) -> Option<CodegenValue>;

    /// Allocate a class instance plus its vtable, wire the vtable methods
    /// into the vtable allocation, store the vtable pointer into the
    /// object, and return the object pointer.
    ///
    /// The runtime allocator prepends and initializes the object header
    /// from the class type descriptor, so this method does not build the
    /// header itself.
    ///
    /// This is layout work (vtable population and storage), not
    /// constructor invocation. Callers are responsible for calling the
    /// class's constructor on the returned object themselves.
    fn allocate_class(&mut self, class: &CodegenClass) -> Option<CodegenValue>;

    /// Build a fat-pointer trait object `{ self, witness }` from `value` (an
    /// object of a class that implements `trait_type`). `self` is the object
    /// pointer; `witness` is the static witness table for (value's class,
    /// trait_type). The returned value carries `trait_type`.
    fn build_trait_object(&mut self, value: &CodegenValue, trait_type: &PekoType) -> CodegenValue;

    /// Dispatch a method call on a trait-typed value (a fat pointer). Loads the
    /// vtable index from the carried witness table for the method's slot, loads
    /// the object's runtime vtable, indexes it, and calls the function pointer
    /// with `self` and `arguments`. Resolution bottoms out at the object's own
    /// vtable, so a child override is always reached.
    fn call_trait_method(
        &mut self,
        object: &CodegenValue,
        method_name: &str,
        arguments: Vec<CodegenValue>,
    ) -> CodegenValue;

    /// Store `value` into `slot`, emitting a GC write barrier when the
    /// store creates a managed-to-managed reference edge.
    ///
    /// The barrier fires only when the slot lives in the managed heap (it
    /// is a managed pointer, address space 1) and the stored value is
    /// itself a managed reference (a class instance, closure, or
    /// Pointer<T>). It notifies the collector of the new edge so a moving
    /// or generational collector can update or remember the slot.
    ///
    /// Initializing stores into freshly-allocated, not-yet-reachable
    /// objects (constructor field init, closure box and context setup)
    /// should use `build_store` directly and skip this, since there is no
    /// existing edge to record.
    fn build_managed_store(&mut self, slot: &CodegenValue, value: &CodegenValue);
}

impl HighLevelCodegen for PekoCodegenContext {
    fn box_value_to_type(
        &mut self,
        expected_type: &PekoType,
        value: &CodegenValue,
    ) -> Option<CodegenValue> {
        // Error values pass through unchanged.
        if value.value_type.is_error_type() {
            return Some(value.clone());
        }

        // `expected_type = Option<T>` and `value: T`: wrap in Option.
        if let Some(inner) = expected_type.optional_get_inner_type() {
            // Only wrap in Option when the value genuinely matches the
            // inner type.
            let neither_is_opaque = value.value_type.name() != "opaque"
                && inner.name() != "opaque"
                && !(value.value_type.name() == "Pointer"
                    && value
                        .value_type
                        .generics()
                        .first()
                        .map(|t| t.name() == "void")
                        .unwrap_or(false))
                && !(inner.name() == "Pointer"
                    && inner
                        .generics()
                        .first()
                        .map(|t| t.name() == "void")
                        .unwrap_or(false));
            if neither_is_opaque && self.types_similar(&value.value_type, &inner) {
                let current_file = self.get_current_file();
                let class = self.get_class_by_type(&PekoType::from_string(
                    format!("Option<{}>", inner).as_str(),
                    current_file,
                ))?;

                let allocated_object = self.allocate_class(&class)?;

                let arguments = vec![self.box_value_to_type(&inner, value).unwrap()];
                let constructor_call = self.call_object_method(
                    &allocated_object,
                    String::from("constructor"),
                    arguments,
                    None,
                );

                if constructor_call.is_err() {
                    return None;
                }

                return Some(allocated_object);
            }
        }

        // Auto-box: a raw FFI scalar value coerces up into its value-type
        // wrapper. Allocate the wrapper, convert the scalar to the wrapper's
        // raw field type, and store it. One-directional: only ffi-value to
        // wrapper.
        if self.get_class_by_type(&value.value_type).is_none()
            && let Some(raw_type) = self.value_wrapper_raw(expected_type)
            && self.types_similar(&value.value_type, &raw_type)
        {
            let class = self.get_class_by_type(expected_type)?;
            let boxed = self.allocate_class(&class)?;
            let raw_value = self.typecast_number_value(value, &raw_type);
            self.set_object_attribute(&boxed, "raw", &raw_value);
            return Some(boxed);
        }

        // Types must be related; otherwise no path exists.
        if !self.types_similar(&value.value_type, expected_type) {
            return None;
        }

        // Exact match: pass through.
        if self.types_equal(expected_type, &value.value_type) {
            return Some(value.clone());
        }

        let expected_type_class = self.get_class_by_type(expected_type);
        let value_type_class = self.get_class_by_type(&value.value_type);

        // Class-to-class via opaque pointer reinterpretation. `types_similar`
        // having already returned true guarantees the classes connect through
        // inheritance, so no further check is needed.
        if expected_type_class.is_some() && value_type_class.is_some() {
            return Some(CodegenValue::new(value.llvm_value, expected_type.clone()));
        }

        // Class -> opaque: as1 -> as0 via addrspacecast.
        // Class -> Pointer<void>: both as1, relabel only.
        let expected_is_opaque = expected_type.name() == "opaque";
        let expected_is_managed_void = expected_type.name() == "Pointer"
            && expected_type
                .generics()
                .first()
                .map(|t| t.name() == "void")
                .unwrap_or(false);
        if value_type_class.is_some() && (expected_is_opaque || expected_is_managed_void) {
            if expected_is_opaque {
                let raw_ptr = unsafe {
                    llvm_sys_180::core::LLVMPointerType(llvm_sys_180::core::LLVMInt8Type(), 0)
                };
                let cast = unsafe {
                    llvm_sys_180::core::LLVMBuildAddrSpaceCast(
                        self.llvm_builder,
                        value.llvm_value,
                        raw_ptr,
                        c"".as_ptr(),
                    )
                };
                return Some(CodegenValue::new(cast, expected_type.clone()));
            } else {
                return Some(CodegenValue::new(value.llvm_value, expected_type.clone()));
            }
        }

        // opaque -> class: as0 -> as1 via addrspacecast.
        // Pointer<void> -> class: both as1, relabel only.
        let value_is_opaque = value.value_type.name() == "opaque";
        let value_is_managed_void = value.value_type.name() == "Pointer"
            && value
                .value_type
                .generics()
                .first()
                .map(|t| t.name() == "void")
                .unwrap_or(false);
        if expected_type_class.is_some() && (value_is_opaque || value_is_managed_void) {
            if value_is_opaque {
                let managed_ptr = unsafe {
                    llvm_sys_180::core::LLVMPointerType(llvm_sys_180::core::LLVMInt8Type(), 1)
                };
                let cast = unsafe {
                    llvm_sys_180::core::LLVMBuildAddrSpaceCast(
                        self.llvm_builder,
                        value.llvm_value,
                        managed_ptr,
                        c"".as_ptr(),
                    )
                };
                return Some(CodegenValue::new(cast, expected_type.clone()));
            } else {
                return Some(CodegenValue::new(value.llvm_value, expected_type.clone()));
            }
        }

        // Object to builtin via the `[operator to_<builtin>]` overload.
        if value_type_class.is_some()
            && value.value_type.array_depth == 0
            && value.value_type.reference_depth == 0
            && expected_type.is_builtin_type()
        {
            let operator_function_name =
                ["[operator to_", expected_type.to_string().as_str(), "]"].concat();

            let overload_cast_call =
                self.call_object_method(value, operator_function_name, Vec::new(), None);

            return overload_cast_call.ok();
        }

        // Builtin to wrapper class via the wrapper's constructor.
        if let Some(class_to_create) = &expected_type_class
            && expected_type.array_depth == 0
            && expected_type.reference_depth == 0
            && value.value_type.is_builtin_type()
        {
            let allocate_wrapper_object = self.allocate_class(class_to_create)?;

            // Then call its constructor.
            let constructor_call = self.call_object_method(
                &allocate_wrapper_object,
                String::from("constructor"),
                vec![value.clone()],
                None,
            );

            if constructor_call.is_err() {
                return None;
            }

            return Some(allocate_wrapper_object);
        }

        // Numeric cast between integer/float types.
        if (value.value_type.is_float() || value.value_type.is_integer())
            && (expected_type.is_float() || expected_type.is_integer())
        {
            return Some(self.typecast_number_value(value, expected_type));
        }

        // String to datatype via the runtime's parsing functions.
        if value.value_type.to_string() == "string"
            && (expected_type.is_datatype() || expected_type.is_integer())
        {
            let runtime_casting_function_name = match expected_type.to_string().as_str() {
                "bool" => "Runtime::StringToBool",
                "float" | "double" => "Runtime::StringToFloat",
                "int" | "int16" | "int128" | "int64" => "Runtime::StringToInt",
                "char" => "Runtime::StringToChar",
                _ => panic!("error should not be reached"),
            };

            let casted_value = self.call_named_function(
                String::from(runtime_casting_function_name),
                vec![value.clone()],
            )?;

            // The runtime parsers return the narrow types (`int`, `float`);
            // widen to the requested width when needed.
            if expected_type.to_string() == "int64" || expected_type.to_string() == "double" {
                return Some(self.typecast_number_value(&casted_value, expected_type));
            }

            return Some(casted_value);
        }

        // Raw-string => string: a managed string cannot be manufactured by
        // relabeling or addrspacecasting a raw (address space 0) string source
        // (it points at header-less memory the collector does not own, so
        // treating it as a managed object would corrupt the heap).
        if value.value_type.is_string_type() && expected_type.to_string() == "string" {
            return self
                .call_named_function("Runtime::CreateManaged".to_string(), vec![value.clone()]);
        }

        // Datatype to string via the runtime's formatting functions.
        if (value.value_type.is_datatype() || value.value_type.is_integer())
            && expected_type.to_string() == "string"
        {
            let runtime_casting_function_name = match value.value_type.to_string().as_str() {
                "bool" => "Runtime::BoolToString",
                "float" | "double" => "Runtime::FloatToString",
                "int" | "int16" | "int128" | "int64" => "Runtime::IntToString",
                "char" => "Runtime::CharToString",
                _ => panic!("error should not be reached"),
            };

            return self.call_named_function(
                String::from(runtime_casting_function_name),
                vec![value.clone()],
            );
        }

        // Pointer<T> autocast. When one side is a managed pointer
        // (address space 1) and the other is a raw pointer (address space
        // 0), emit an addrspacecast to the expected type. When both are
        // managed, only the value_type label changes.
        let expected_managed = is_managed_pointer(expected_type);
        let value_managed = is_managed_pointer(&value.value_type);
        if (expected_managed || value_managed)
            && (expected_type.is_pointer() || value.value_type.is_pointer())
        {
            if expected_managed == value_managed {
                // Same address space (both managed): relabel only.
                return Some(CodegenValue::new(value.llvm_value, expected_type.clone()));
            }

            // Address spaces differ: emit an addrspacecast to the
            // expected pointer type.
            let target_llvm_type = self.get_llvm_type(expected_type)?;
            let cast = unsafe {
                llvm_sys_180::core::LLVMBuildAddrSpaceCast(
                    self.llvm_builder,
                    value.llvm_value,
                    target_llvm_type,
                    c"".as_ptr(),
                )
            };
            return Some(CodegenValue::new(cast, expected_type.clone()));
        }

        // Opaque pointer reinterpretation: no LLVM cast emitted, only the
        // `value_type` is rewritten.
        if expected_type.is_pointer() && value.value_type.is_pointer() {
            return Some(CodegenValue::new(value.llvm_value, expected_type.clone()));
        }

        // Failsafe; should be unreachable given the earlier `types_similar`
        // gate, but kept defensively in case a path is missed.
        None
    }

    fn allocate_class(&mut self, class: &CodegenClass) -> Option<CodegenValue> {
        // Allocate the class instance. The runtime prepends the object
        // header and writes the class descriptor pointer into it, so the
        // collector can trace this object's managed fields. The descriptor
        // is looked up for the current module: the owning module holds the
        // definition, importing modules hold a declaration of the same
        // symbol. The vtable slot is excluded from the traced offsets: it
        // points at a static vtable, not at GC-managed memory.
        let object_size = self.get_type_size(&class.class_type, true);
        let descriptor_uuid = self.get_owning_module_uuid();
        let class_descriptor = if let Some(desc) = class.get_descriptor(&descriptor_uuid) {
            desc
        } else {
            let managed_offset_count = class
                .attributes
                .iter()
                .filter(|(name, attr)| {
                    name.as_str() != "<main_virtual_table>"
                        && (self.is_managed(&attr.attribute_type)
                            || self.get_trait(attr.attribute_type.name()).is_some())
                })
                .count();
            self.declare_class_descriptor(
                &class.class_type.to_mangled_string(),
                managed_offset_count,
            )
        };
        let allocate_class_memory =
            self.allocate_managed_object(&class_descriptor, object_size, &class.class_type)?;

        // Classes with no methods have no vtable slot to populate.
        if class.main_virtual_table.methods.is_empty() {
            return Some(allocate_class_memory);
        }

        // Emit (or reuse) the class's static vtable and store a pointer to it
        // into the object's vtable slot. The vtable is static data shared by
        // every instance; the slot is a raw (address-space-0) pointer the
        // collector does not trace.
        let uuid = self.get_owning_module_uuid();
        let mut method_pointers = Vec::new();
        for (_, functions) in &class.main_virtual_table.methods {
            for function in functions {
                let function = function.read().unwrap();
                method_pointers.push((
                    function.virtual_table_index,
                    function.function_value[&uuid].llvm_value,
                ));
            }
        }
        let static_vtable = self.emit_class_vtable(
            &class.class_type.to_mangled_string(),
            class.main_virtual_table.llvm_type,
            method_pointers,
        );

        let virtual_table_class_element = self
            .get_object_vtable(&allocate_class_memory, false)
            .llvm_value;
        self.build_store(
            &CodegenValue::new(
                virtual_table_class_element,
                PekoType::simple_type("opaque"),
            ),
            &static_vtable,
        );

        Some(allocate_class_memory)
    }

    fn build_trait_object(&mut self, value: &CodegenValue, trait_type: &PekoType) -> CodegenValue {
        let fat_type = self.get_llvm_type(trait_type).unwrap();

        // Reference the static witness table for (value's class, trait). It was
        // emitted when the class was codegen'd. If it is not present in this
        // module (the class was declared elsewhere), declare it as an external
        // i32 array; the linker resolves it to the owner's definition.
        let class_mangled = value.value_type.to_mangled_string();
        let trait_mangled = trait_type.to_mangled_string();
        let global_name = cstr(format!("peko_witness_{class_mangled}__{trait_mangled}"));

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

        let witness_global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if existing.is_null() {
                let array_type = core::LLVMArrayType2(core::LLVMInt32Type(), 0);
                let g = core::LLVMAddGlobal(module, array_type, global_name.as_ptr());
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                g
            } else {
                existing
            }
        };

        // Build the fat pointer value: insert self (word 0) and the witness
        // pointer (word 1) into an undef struct of the trait's fat type.
        let fat_value = unsafe {
            let undef = core::LLVMGetUndef(fat_type);
            let with_self = core::LLVMBuildInsertValue(
                self.llvm_builder,
                undef,
                value.llvm_value,
                0,
                c"".as_ptr(),
            );
            core::LLVMBuildInsertValue(
                self.llvm_builder,
                with_self,
                witness_global,
                1,
                c"".as_ptr(),
            )
        };

        CodegenValue::new(fat_value, trait_type.clone())
    }

    fn call_trait_method(
        &mut self,
        object: &CodegenValue,
        method_name: &str,
        arguments: Vec<CodegenValue>,
    ) -> CodegenValue {
        let trait_definition = self.get_trait(object.value_type.name()).unwrap();
        let slot_count = trait_definition.methods.len();
        let (slot_index, slot) = trait_definition
            .methods
            .iter()
            .enumerate()
            .find(|(_, method)| method.name == method_name)
            .map(|(index, method)| (index, method.clone()))
            .unwrap();

        let builder = self.llvm_builder;
        unsafe {
            let i32_type = core::LLVMInt32Type();
            let raw_ptr_type = core::LLVMPointerType(core::LLVMInt8Type(), 0);
            let managed_ptr_type = core::LLVMPointerType(core::LLVMInt8Type(), 1);

            // Extract self (word 0) and the witness pointer (word 1) from the
            // fat pointer.
            let self_value =
                core::LLVMBuildExtractValue(builder, object.llvm_value, 0, c"".as_ptr());
            let witness_ptr =
                core::LLVMBuildExtractValue(builder, object.llvm_value, 1, c"".as_ptr());

            // witness[slot_index] -> the object's vtable index for this method.
            let witness_array_type = core::LLVMArrayType2(i32_type, slot_count as u64);
            let mut witness_indices = [
                core::LLVMConstInt(i32_type, 0, 0),
                core::LLVMConstInt(i32_type, slot_index as u64, 0),
            ];
            let witness_slot = core::LLVMBuildGEP2(
                builder,
                witness_array_type,
                witness_ptr,
                witness_indices.as_mut_ptr(),
                2,
                c"".as_ptr(),
            );
            let vtable_index =
                core::LLVMBuildLoad2(builder, i32_type, witness_slot, c"".as_ptr());

            // Load the object's runtime vtable pointer from its offset-0 slot.
            let mut header_fields = [raw_ptr_type];
            let header_type = core::LLVMStructType(header_fields.as_mut_ptr(), 1, 0);
            let mut header_indices = [
                core::LLVMConstInt(i32_type, 0, 0),
                core::LLVMConstInt(i32_type, 0, 0),
            ];
            let vtable_field = core::LLVMBuildGEP2(
                builder,
                header_type,
                self_value,
                header_indices.as_mut_ptr(),
                2,
                c"".as_ptr(),
            );
            let vtable_ptr =
                core::LLVMBuildLoad2(builder, raw_ptr_type, vtable_field, c"".as_ptr());

            // vtable[vtable_index] -> the function pointer.
            let mut method_indices = [vtable_index];
            let method_slot = core::LLVMBuildGEP2(
                builder,
                raw_ptr_type,
                vtable_ptr,
                method_indices.as_mut_ptr(),
                1,
                c"".as_ptr(),
            );
            let function_pointer =
                core::LLVMBuildLoad2(builder, raw_ptr_type, method_slot, c"".as_ptr());

            // Build the function type (self plus the slot's argument types) and
            // call through the loaded pointer.
            let return_llvm = self
                .get_llvm_type(&slot.return_type)
                .unwrap_or_else(|| core::LLVMVoidType());
            let mut param_types = vec![managed_ptr_type];
            for argument_type in &slot.argument_types {
                if let Some(llvm) = self.get_llvm_type(argument_type) {
                    param_types.push(llvm);
                }
            }
            let function_type = core::LLVMFunctionType(
                return_llvm,
                param_types.as_mut_ptr(),
                param_types.len() as u32,
                0,
            );

            let mut call_arguments = vec![self_value];
            for argument in &arguments {
                call_arguments.push(argument.llvm_value);
            }
            let result = core::LLVMBuildCall2(
                builder,
                function_type,
                function_pointer,
                call_arguments.as_mut_ptr(),
                call_arguments.len() as u32,
                c"".as_ptr(),
            );

            CodegenValue::new(result, slot.return_type.clone())
        }
    }

    fn build_managed_store(&mut self, slot: &CodegenValue, value: &CodegenValue) {
        self.build_store(slot, value);

        // A barrier is needed only when a managed reference is stored into
        // a slot that lives in the managed heap. The slot is in the heap
        // when it is a managed pointer (address space 1); a stack slot is
        // a raw pointer and is scanned directly as a root instead. The
        // value is a managed reference when it is a class instance,
        // closure, or Pointer<T>.
        let slot_in_managed_heap = is_managed_pointer(&slot.value_type);
        let value_is_managed =
            is_managed_pointer(&value.value_type) || self.is_managed(&value.value_type);

        // Storing null creates no edge, so there is nothing to record or
        // relocate; skip the barrier in that case.
        let value_is_null = unsafe { llvm_sys_180::core::LLVMIsNull(value.llvm_value) != 0 };

        if slot_in_managed_heap && value_is_managed && !value_is_null {
            // The barrier takes plain (address space 0) pointers, but the
            // slot and value are managed (address space 1). Cast both down
            // to a raw i8* so the call matches peko_gc_write_barrier's
            // signature.
            let opaque_ptr_type = unsafe {
                llvm_sys_180::core::LLVMPointerType(llvm_sys_180::core::LLVMInt8Type(), 0)
            };

            let slot_raw = unsafe {
                llvm_sys_180::core::LLVMBuildAddrSpaceCast(
                    self.llvm_builder,
                    slot.llvm_value,
                    opaque_ptr_type,
                    c"".as_ptr(),
                )
            };
            let value_raw = unsafe {
                llvm_sys_180::core::LLVMBuildAddrSpaceCast(
                    self.llvm_builder,
                    value.llvm_value,
                    opaque_ptr_type,
                    c"".as_ptr(),
                )
            };

            let slot_arg = CodegenValue::new(slot_raw, PekoType::simple_type("opaque"));
            let value_arg = CodegenValue::new(value_raw, PekoType::simple_type("opaque"));

            self.call_named_function("extern::peko_gc_write_barrier", vec![slot_arg, value_arg]);
        }
    }
}
