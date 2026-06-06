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

use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_arithmetic::LlvmArithmeticBuilder;
use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_instructions::LlvmInstructionBuilder;
use crate::codegen::builders::llvm_memory::LlvmMemoryBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::builders::modules::ModuleManager;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{
    is_managed_pointer, managed_pointer_type, CodegenClass, CodegenValue,
};

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
        if value.value_type.is_error_type {
            return Some(value.clone());
        }

        // `expected_type = Option<T>` and `value: T`: wrap in Option.
        if let Some(inner) = expected_type.optional_get_inner_type() {
            // Only wrap in Option when the value genuinely matches the
            // inner type.
            let neither_is_opaque = value.value_type.type_name != "opaque"
                && inner.type_name != "opaque"
                && !(value.value_type.type_name == "Pointer"
                    && value
                        .value_type
                        .generic_types
                        .first()
                        .map(|t| t.type_name == "void")
                        .unwrap_or(false))
                && !(inner.type_name == "Pointer"
                    && inner
                        .generic_types
                        .first()
                        .map(|t| t.type_name == "void")
                        .unwrap_or(false));
            if neither_is_opaque && self.types_similar(&value.value_type, &inner) {
                let current_file = self.get_current_file();
                let class = self.get_class_by_type(&PekoType::from_string(
                    format!("Option<{}>", inner.to_string()).as_str(),
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
        let expected_is_opaque = expected_type.type_name == "opaque";
        let expected_is_managed_void = expected_type.type_name == "Pointer"
            && expected_type
                .generic_types
                .first()
                .map(|t| t.type_name == "void")
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
        let value_is_opaque = value.value_type.type_name == "opaque";
        let value_is_managed_void = value.value_type.type_name == "Pointer"
            && value
                .value_type
                .generic_types
                .first()
                .map(|t| t.type_name == "void")
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
            && value.value_type.pointer_depth == 0
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
        if expected_type_class.is_some()
            && expected_type.pointer_depth == 0
            && expected_type.reference_depth == 0
            && value.value_type.is_builtin_type()
        {
            let class_to_create = expected_type_class.unwrap();
            let allocate_wrapper_object = self.allocate_class(&class_to_create)?;

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
            && (expected_managed
                || value_managed
                || expected_type.is_pointer()
                || value.value_type.is_pointer())
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
        // Allocate the vtable. The vtable is a managed object too, but it
        // holds only function pointers, which the collector must not
        // trace. So it gets an empty descriptor (zero traced offsets):
        // the collector can move it but will not follow its contents.
        let vtable_size = 8 * class.main_virtual_table.get_method_count();
        let vtable_descriptor = self.emit_type_descriptor("vtable", 0, Vec::new());
        let allocate_class_vtable = self.allocate_managed_object(
            &vtable_descriptor,
            vtable_size,
            &managed_pointer_type(PekoType::simple_type("void")),
        )?;

        // Wire each method into its assigned vtable slot.
        for (_, functions) in &class.main_virtual_table.methods {
            for function in functions {
                let function_element = self.get_vtable_method(
                    &allocate_class_vtable,
                    class.main_virtual_table.llvm_type,
                    &function.get_type(),
                    function.virtual_table_index,
                    false,
                );

                let uuid = &self.get_owning_module_uuid();
                self.build_store(&function_element, &function.function_value[uuid]);
            }
        }

        // Allocate the class instance. The runtime prepends the object
        // header and writes the class descriptor pointer into it, so the
        // collector can trace this object's managed fields. The descriptor
        // is looked up for the current module: the owning module holds the
        // definition, importing modules hold a declaration of the same
        // symbol.
        let object_size = self.get_type_size(&class.class_type, true);
        let descriptor_uuid = self.get_owning_module_uuid();
        let class_descriptor = if let Some(desc) = class.get_descriptor(&descriptor_uuid) {
            desc
        } else {
            let managed_offset_count = class
                .attributes
                .iter()
                .filter(|(_, attr)| self.is_managed(&attr.attribute_type))
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

        // Store the vtable pointer into the class object's vtable slot.
        let virtual_table_class_element = self
            .get_object_vtable(&allocate_class_memory, false)
            .llvm_value;
        self.build_store(
            &CodegenValue::new(
                virtual_table_class_element,
                managed_pointer_type(PekoType::simple_type("void")),
            ),
            &allocate_class_vtable,
        );

        Some(allocate_class_memory)
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

            self.call_named_function(
                "extern::peko_gc_write_barrier".to_string(),
                vec![slot_arg, value_arg],
            );
        }
    }
}
