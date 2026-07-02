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
use llvm_sys_180::prelude::LLVMValueRef;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::execution::data_structures::{ExecutionClass, TraitMethodSlot};
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_arithmetic::LlvmArithmeticBuilder;
use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_instructions::LlvmInstructionBuilder;
use crate::codegen::builders::llvm_memory::LlvmMemoryBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::builders::modules::ModuleManager;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
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
    /// 9. String-to-datatype cast via `runtime::string_to_*` runtime functions.
    /// 10. Datatype-to-string cast via `runtime::*_to_string` runtime functions.
    /// 11. Pointer reinterpretation between two pointer-typed values.
    fn box_value_to_type(
        &mut self,
        expected_type: &PekoType,
        value: &CodegenValue,
    ) -> Option<CodegenValue>;

    /// Wrap a raw enum value in a `Box<i32>` cell so it can occupy an erased
    /// generic slot. Typed as `target_type`.
    fn box_enum_value(
        &mut self,
        enum_value: &CodegenValue,
        target_type: &PekoType,
    ) -> Option<CodegenValue>;

    /// Recover the `i32` from a `Box<i32>` cell at a generic extraction site,
    /// typed as `enum_type`.
    fn unbox_enum_value(
        &mut self,
        boxed: &CodegenValue,
        enum_type: &PekoType,
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

    /// Reduce a boolean value to a raw i1 for branching. A bool object unboxes
    /// through its `to_raw()` method; a raw i1 passes through unchanged.
    fn to_raw_bool(&mut self, value: &CodegenValue) -> CodegenValue;

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

    /// Dispatch a trait method on a thin `Object*` of unknown concrete type.
    /// Finds the witness table at runtime by scanning the object's TypeInfo
    /// itable for `trait_type` (`peko_itable_lookup`), then dispatches through
    /// it. Erased generics use this: an `impl Trait` parameter is a bare
    /// managed pointer, not a fat trait pointer, so the witness is not carried.
    fn call_trait_method_erased(
        &mut self,
        object: &CodegenValue,
        trait_type: &PekoType,
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
    /// Wraps a raw enum value (an `i32`) in a monomorphized `Box<i32>` managed
    /// cell so it can occupy an erased generic slot, which is a traced managed
    /// pointer. The result is typed as `target_type` (the carrier). Reverses
    /// with `unbox_enum_value`.
    fn box_enum_value(
        &mut self,
        enum_value: &CodegenValue,
        target_type: &PekoType,
    ) -> Option<CodegenValue> {
        let box_type = PekoType::from_string("Box<i32>", self.get_current_file());
        let box_class = self.get_class_by_type(&box_type)?;
        let boxed = self.allocate_class(&box_class)?;
        let raw = CodegenValue::new(enum_value.llvm_value, PekoType::simple_type("i32"));
        self.call_object_method(&boxed, String::from("constructor"), vec![raw], None)
            .ok()?;
        Some(CodegenValue::new(boxed.llvm_value, target_type.clone()))
    }

    /// Loads the `i32` back out of a `Box<i32>` cell and types it as
    /// `enum_type`. Reverses `box_enum_value` at a generic extraction site.
    fn unbox_enum_value(
        &mut self,
        boxed: &CodegenValue,
        enum_type: &PekoType,
    ) -> Option<CodegenValue> {
        let box_type = PekoType::from_string("Box<i32>", self.get_current_file());
        let cell = CodegenValue::new(boxed.llvm_value, box_type);
        let inner = self
            .call_object_method(&cell, String::from("get"), Vec::new(), None)
            .ok()?;
        Some(CodegenValue::new(inner.llvm_value, enum_type.clone()))
    }

    fn box_value_to_type(
        &mut self,
        expected_type: &PekoType,
        value: &CodegenValue,
    ) -> Option<CodegenValue> {
        // Error values pass through unchanged.
        if value.value_type.is_error_type() {
            return Some(value.clone());
        }

        // An enum crossing into an erased generic slot (a carrier) is boxed
        // into a Box<i32> managed cell, since a raw i32 enum cannot occupy a
        // traced managed-pointer slot. The matching extraction unboxes it.
        if expected_type.is_generic_param()
            && expected_type.array_depth == 0
            && expected_type.reference_depth == 0
            && self.get_enum_variants(value.value_type.name()).is_some()
        {
            return self.box_enum_value(value, expected_type);
        }

        // An exact type match needs no coercion. Checked before the Option-wrap
        // below so a value already of the expected optional type is not wrapped
        // a second time (an erased generic parameter compares equal to any type,
        // so the wrap would otherwise recurse without end).
        //
        // The exception is a non-optional value targeting an optional: it is the
        // held value and must wrap, even though the generic-parameter wildcard
        // in types_equal makes it compare equal to the optional. This covers a
        // bare generic parameter and a `danger_cast<T>(...)` whose result type
        // is the named parameter `T` rather than a generic-kind type.
        let optional_of_inner_value = expected_type.optional_get_inner_type().is_some()
            && value.value_type.optional_get_inner_type().is_none();
        if !optional_of_inner_value && self.types_equal(expected_type, &value.value_type) {
            return Some(value.clone());
        }

        // `expected_type = Option<T>` and `value: T`: wrap in Option.
        if let Some(inner) = expected_type.optional_get_inner_type() {
            // Only wrap in Option when the value genuinely matches the
            // inner type.
            let neither_is_opaque = value.value_type.name() != "opaque"
                && inner.name() != "opaque"
                && !(value.value_type.name() == "pointer"
                    && value
                        .value_type
                        .generics()
                        .first()
                        .map(|t| t.name() == "void")
                        .unwrap_or(false))
                && !(inner.name() == "pointer"
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

        // Implicit trait-object coercion: a concrete value whose class
        // implements a trait coerces to that trait (a fat pointer) when a
        // trait-typed parameter is expected. This lets `f(x)` stand in for
        // `f(x as Trait)`. Analysis validates the implementation.
        if self.get_trait(expected_type.name()).is_some()
            && value.value_type.name() != expected_type.name()
            && let Some(class) = self.get_class_by_type(&value.value_type)
            && class
                .get_implemented_trait_names()
                .iter()
                .any(|name| name == expected_type.name())
        {
            return Some(self.build_trait_object(value, expected_type));
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
        let expected_is_managed_void = expected_type.name() == "pointer"
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
        let value_is_managed_void = value.value_type.name() == "pointer"
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
                "bool" => "runtime::string_to_bool",
                "f32" | "f64" => "runtime::string_to_float",
                "i32" | "i16" | "i128" | "i64" => "runtime::string_to_int",
                "char" => "runtime::string_to_char",
                _ => panic!("error should not be reached"),
            };

            let casted_value = self.call_named_function(
                String::from(runtime_casting_function_name),
                vec![value.clone()],
            )?;

            // The runtime parsers return the narrow types (`int`, `float`);
            // widen to the requested width when needed.
            if expected_type.to_string() == "i64" || expected_type.to_string() == "f64" {
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
                .call_named_function("runtime::create_managed".to_string(), vec![value.clone()]);
        }

        // Datatype to string via the runtime's formatting functions.
        if (value.value_type.is_datatype() || value.value_type.is_integer())
            && expected_type.to_string() == "string"
        {
            let runtime_casting_function_name = match value.value_type.to_string().as_str() {
                "bool" => "runtime::bool_to_string",
                "f32" | "f64" => "runtime::float_to_string",
                "i32" | "i16" | "i128" | "i64" => "runtime::int_to_string",
                "char" => "runtime::char_to_string",
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
        // An erased generic class is typed in its own generic parameters
        // (`Option<T>`, `Map<KT, VT>`). Substitute them to bare carriers so the
        // type resolves and sizes outside the class's generic context; every
        // instantiation shares this one erased layout regardless.
        let size_type = if class.generic_typenames.is_empty() {
            class.class_type.clone()
        } else {
            crate::codegen::context::substitute_generic_params(
                &class.class_type,
                &crate::codegen::context::class_carrier_substitution(&class.generic_typenames),
            )
        };
        let object_size = self.get_type_size(&size_type, true);
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

        // Store a pointer to the class's static TypeInfo into the object's
        // offset-0 slot. The TypeInfo is static data shared by every instance
        // (it points at the class vtable, descriptor, and itable); the slot is a
        // raw (address-space-0) pointer the collector does not trace. Dispatch
        // reaches the vtable through this TypeInfo. The TypeInfo, vtable, and
        // itable globals are emitted when the class is built; here they are only
        // referenced (declared external when the class is from another module).
        let type_info = self.reference_type_info(&class.class_type.to_mangled_string());

        let type_info_slot = self
            .get_object_vtable(&allocate_class_memory, false)
            .llvm_value;
        self.build_store(
            &CodegenValue::new(type_info_slot, PekoType::simple_type("opaque")),
            &type_info,
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
        let (self_value, witness_ptr) = unsafe {
            // Extract self (word 0) and the witness pointer (word 1) from the
            // fat pointer.
            (
                core::LLVMBuildExtractValue(builder, object.llvm_value, 0, c"".as_ptr()),
                core::LLVMBuildExtractValue(builder, object.llvm_value, 1, c"".as_ptr()),
            )
        };

        self.dispatch_through_witness(
            self_value,
            witness_ptr,
            &slot,
            slot_index,
            slot_count,
            &arguments,
        )
    }

    fn call_trait_method_erased(
        &mut self,
        object: &CodegenValue,
        trait_type: &PekoType,
        method_name: &str,
        arguments: Vec<CodegenValue>,
    ) -> CodegenValue {
        let trait_definition = self.get_trait(trait_type.name()).unwrap();
        let slot_count = trait_definition.methods.len();
        let (slot_index, slot) = trait_definition
            .methods
            .iter()
            .enumerate()
            .find(|(_, method)| method.name == method_name)
            .map(|(index, method)| (index, method.clone()))
            .unwrap();

        // The value is a thin Object*; the witness is not carried, so find it at
        // runtime by scanning the object's itable for this trait's id.
        let self_value = object.llvm_value;
        let trait_id = crate::codegen::builders::llvm_constants::trait_dispatch_id(trait_type);
        let witness_ptr = self.runtime_itable_lookup(self_value, trait_id);

        self.dispatch_through_witness(
            self_value,
            witness_ptr,
            &slot,
            slot_index,
            slot_count,
            &arguments,
        )
    }

    fn to_raw_bool(&mut self, value: &CodegenValue) -> CodegenValue {
        // An object (the bool wrapper, or a user type) unboxes through to_raw();
        // a raw i1 is used directly.
        if self.get_class_by_type(&value.value_type).is_some()
            && let Ok(raw) = self.call_object_method(value, "to_raw", Vec::new(), None)
        {
            return raw;
        }
        value.clone()
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

impl PekoCodegenContext {
    /// Dispatch through an already-located witness table. This is the shared
    /// back half of trait dispatch: `witness[slot_index]` gives the object's
    /// vtable index for the method, the object's TypeInfo (word 0) gives the
    /// runtime vtable, and the indexed function pointer is called with `self`
    /// and `arguments`. Used by both fat-pointer trait calls (witness carried)
    /// and erased thin-Object* calls (witness found by an itable scan).
    fn dispatch_through_witness(
        &mut self,
        self_value: LLVMValueRef,
        witness_ptr: LLVMValueRef,
        slot: &TraitMethodSlot,
        slot_index: usize,
        slot_count: usize,
        arguments: &[CodegenValue],
    ) -> CodegenValue {
        let builder = self.llvm_builder;
        unsafe {
            let i32_type = core::LLVMInt32Type();
            let raw_ptr_type = core::LLVMPointerType(core::LLVMInt8Type(), 0);
            let managed_ptr_type = core::LLVMPointerType(core::LLVMInt8Type(), 1);

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
            let vtable_index = core::LLVMBuildLoad2(builder, i32_type, witness_slot, c"".as_ptr());

            // Offset-0 holds a pointer to the object's static TypeInfo. Load it,
            // then load the runtime vtable out of the TypeInfo's vtable field
            // (index 3).
            let mut header_fields = [raw_ptr_type];
            let header_type = core::LLVMStructType(header_fields.as_mut_ptr(), 1, 0);
            let mut header_indices = [
                core::LLVMConstInt(i32_type, 0, 0),
                core::LLVMConstInt(i32_type, 0, 0),
            ];
            let type_info_field = core::LLVMBuildGEP2(
                builder,
                header_type,
                self_value,
                header_indices.as_mut_ptr(),
                2,
                c"".as_ptr(),
            );
            let type_info =
                core::LLVMBuildLoad2(builder, raw_ptr_type, type_info_field, c"".as_ptr());
            let type_info_type = self.type_info_struct_type();
            let vtable_field =
                core::LLVMBuildStructGEP2(builder, type_info_type, type_info, 3, c"".as_ptr());
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
            for argument in arguments {
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

    /// Emit a call to the runtime `peko_itable_lookup(object, trait_id)`, which
    /// returns the witness table for the trait in the object's runtime type (or
    /// null). The function is declared into the current module on first use. It
    /// only reads static data, so it is gc-leaf.
    fn runtime_itable_lookup(&mut self, self_value: LLVMValueRef, trait_id: i32) -> LLVMValueRef {
        let module_arc = {
            let post = self.module_context.step_back_generics();
            let module = self.module_context.current_module();
            self.module_context.step_forward(post);
            module
        };
        let llvm_module = module_arc
            .read()
            .unwrap()
            .get_top_level()
            .unwrap()
            .llvm_module;

        let builder = self.llvm_builder;
        unsafe {
            let i32_type = core::LLVMInt32Type();
            let raw_ptr_type = core::LLVMPointerType(core::LLVMInt8Type(), 0);
            let managed_ptr_type = core::LLVMPointerType(core::LLVMInt8Type(), 1);
            let mut param_types = [managed_ptr_type, i32_type];
            let function_type =
                core::LLVMFunctionType(raw_ptr_type, param_types.as_mut_ptr(), 2, 0);

            let name = cstr("peko_itable_lookup");
            let mut function = core::LLVMGetNamedFunction(llvm_module, name.as_ptr());
            if function.is_null() {
                function = core::LLVMAddFunction(llvm_module, name.as_ptr(), function_type);
                core::LLVMSetLinkage(function, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                crate::codegen::builders::functions::set_gc_leaf_attribute(function);
            }

            let trait_id_const = core::LLVMConstInt(i32_type, trait_id as u64, 1);
            let mut call_arguments = [self_value, trait_id_const];
            core::LLVMBuildCall2(
                builder,
                function_type,
                function,
                call_arguments.as_mut_ptr(),
                2,
                c"".as_ptr(),
            )
        }
    }
}
