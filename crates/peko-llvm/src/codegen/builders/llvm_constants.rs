//! Layer 0: building constant `CodegenValue`s.
//!
//! These methods produce LLVM constants for use anywhere a literal value
//! is needed. They are leaf operations: they do not depend on the basic
//! block cursor, do not emit non-constant instructions, and do not touch
//! the type system beyond the primitive widths.
//!
//! Allowed callees: `LlvmTypeBuilder` (for the typed-default variant).
//! `emit_type_descriptor` additionally reads `module_context` to reach the
//! current module for global placement.

use llvm_sys_180::core;
use llvm_sys_180::prelude::{LLVMTypeRef, LLVMValueRef};
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::builders::prelude::{HighLevelCodegen, LlvmMemoryBuilder};
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::{CodegenValue, managed_pointer_type};

/// Builders that produce constant `CodegenValue`s for use in IR.
pub trait LlvmConstantBuilder {
    /// Create a managed `string` object for the given text. The bytes
    /// (including the `\0` terminator) are GC-allocated in address space 1 and
    /// copied into a managed `pointer<i8>` buffer, then wrapped in a freshly
    /// allocated std::core `string` object whose `data` field points at the
    /// buffer and whose `length` field holds the byte count. The returned value
    /// carries `PekoType::simple_type("string")`. Use this for ordinary string
    /// literals. When the `string` class is not in scope (the no-std snippet
    /// harness), the raw buffer is returned typed `string` as a fallback.
    fn create_string(&mut self, string_value: impl ToString) -> CodegenValue;

    /// Create a raw `cstr` for the given text: a static, unmanaged C string
    /// (`i8*` in address space 0) with a `\0` terminator, carrying
    /// `PekoType::simple_type("cstr")`. This is the `cstring(...)` builtin's
    /// backing path and is what crosses the C boundary.
    fn create_cstring(&mut self, string_value: impl ToString) -> CodegenValue;

    /// Build a constant `char` (`i8`).
    fn create_constant_char(&mut self, char_value: char) -> CodegenValue;

    /// Build a constant `int` (`i32`).
    fn create_constant_int(&mut self, int_value: i32) -> CodegenValue;

    /// Build a constant `int64` (`i64`). Currently takes `i32` for parity
    /// with the rest of the surface.
    fn create_constant_int64(&mut self, int_value: i32) -> CodegenValue;

    /// Build a constant `float` (`f32`).
    fn create_constant_float(&mut self, float_value: f32) -> CodegenValue;

    /// Build a constant `double` (`f64`).
    fn create_constant_double(&mut self, double_value: f64) -> CodegenValue;

    /// Build a constant `bool` (`i1`).
    fn create_constant_boolean(&mut self, boolean: bool) -> CodegenValue;

    /// Build a constant `null` of opaque pointer type.
    fn create_null_pointer(&mut self) -> CodegenValue;

    /// Default value representing "this branch exited via `break`".
    /// Codegen looks for this `PekoType` when threading control flow.
    fn create_branch_exit(&mut self) -> CodegenValue;

    /// Default value representing "this branch exited via `return`".
    fn create_return_exit(&mut self) -> CodegenValue;

    /// Default value used in place of a real value when typechecking
    /// has already diagnosed an error. Carries `PekoType::error_type`,
    /// which downstream layers treat as "skip me".
    fn create_error_value(&mut self) -> CodegenValue;

    /// Build the zero value for a `PekoType`. For primitives this is the
    /// LLVM zero constant; for non-primitives it is a null pointer of the
    /// appropriate type. Used for global initialization and other places
    /// where a typed default is required.
    fn build_zero_value(&mut self, default_type: &PekoType) -> CodegenValue;

    /// Emit a static GC type descriptor as a private constant global and
    /// return a pointer to it (typed `opaque`).
    ///
    /// The descriptor has the layout `{ i32 kind, i32 count, [count x i64]
    /// offsets }`, matching the runtime's `PekoFixedDescriptor`:
    ///
    /// * `kind` tags the descriptor flavor (0 = fixed/record layout).
    /// * `count` is the number of traced offsets.
    /// * `offsets` lists the byte offset of each managed-pointer slot.
    fn emit_type_descriptor(
        &mut self,
        mangled_name: &str,
        kind: i32,
        offsets: Vec<usize>,
    ) -> CodegenValue;

    /// Emit a class GC type descriptor as an external constant global
    /// (the definition, with initializer) and return a pointer to it.
    ///
    /// Like `emit_type_descriptor` with kind 0, but the global has
    /// external linkage so it is visible across modules. The owning
    /// module emits this definition; importing modules declare the same
    /// symbol via `declare_class_descriptor`, and the linker resolves all
    /// references to this one definition.
    fn emit_class_descriptor(&mut self, mangled_name: &str, offsets: Vec<usize>) -> CodegenValue;

    /// Declare a class GC type descriptor as an external global with no
    /// initializer (the importer's declaration). The symbol name and type
    /// match the owning module's `emit_class_descriptor` definition, so
    /// the linker resolves references here to that definition.
    ///
    /// `offset_count` is the number of traced offsets, needed to build the
    /// `{ i32, i32, [offset_count x i64] }` type that matches the
    /// definition.
    fn declare_class_descriptor(&mut self, mangled_name: &str, offset_count: usize)
    -> CodegenValue;

    /// Emit a static array (kind 1) GC type descriptor as a private
    /// constant global and return a pointer to it (typed `opaque`).
    ///
    /// The descriptor has the layout `{ i32 kind, i64 stride, ptr
    /// element_descriptor }`, matching the runtime's `PekoArrayDescriptor`:
    ///
    /// * `kind` is 1 (array layout).
    /// * `stride` is the size of one element in bytes.
    /// * `element_descriptor` points at the descriptor used to trace one
    ///   element, or is null when elements are atomic (no tracing).
    ///
    /// The element count is derived at runtime from the allocation size
    /// and the stride, so it is not stored here.
    fn emit_array_descriptor(
        &mut self,
        mangled_name: &str,
        stride: usize,
        element_descriptor: &CodegenValue,
    ) -> CodegenValue;

    fn emit_empty_cstr_global(&mut self) -> LLVMValueRef;

    /// Emit a class's virtual table as a private constant global and return a
    /// pointer to it (a raw address-space-0 pointer typed `opaque`).
    ///
    /// The vtable is static, shared by every instance of the class, and never
    /// GC-allocated or traced: it holds only function pointers. `method_pointers`
    /// pairs each method's `virtual_table_index` with the LLVM value of the
    /// function (as resolved in the current module). The functions are placed in
    /// a `{ ptr, ptr, ... }` constant at their slot index, matching the layout
    /// `vtable_struct_type` describes. The global is reused if a vtable with the
    /// same name already exists in this module.
    fn emit_class_vtable(
        &mut self,
        mangled_name: &str,
        vtable_struct_type: LLVMTypeRef,
        method_pointers: Vec<(usize, LLVMValueRef)>,
    ) -> CodegenValue;

    /// Emit a per-(class, trait) witness table as a private constant global and
    /// return a pointer to it (a raw address-space-0 pointer typed `opaque`).
    ///
    /// The witness is an `[N x i32]` array: entry `k` is the class's vtable
    /// index for the trait's slot `k`. Trait dispatch loads `witness[k]` and
    /// indexes the object's runtime vtable with it, so a child override is
    /// always reached. The table is static data, never traced. Reused by name
    /// if it already exists in this module.
    fn emit_witness_table(
        &mut self,
        class_mangled: &str,
        trait_mangled: &str,
        slot_indices: Vec<i32>,
    ) -> CodegenValue;

    /// Emit a class's static `TypeInfo` record as an external constant global
    /// and return a pointer to it. The record is the runtime-reachable type
    /// handle erased generics dispatch through:
    ///
    /// `{ i32 type_id, ptr parent, ptr descriptor, ptr vtable, ptr itable,
    /// i32 itable_len }`
    ///
    /// * `type_id` is a stable hash of the class mangled name.
    /// * `parent` points at the parent class's `TypeInfo`, or null at the root.
    /// * `descriptor` and `vtable` point at the class's existing static
    ///   descriptor and vtable globals (vtable null when the class has none).
    /// * `itable` points at a `[itable_len x { i32 trait_id, ptr witness }]`
    ///   global mapping each implemented trait's id to its witness table, or
    ///   null when the class implements no traits.
    ///
    /// The record and itable are static data, never GC-allocated or traced.
    /// Reused by name if already emitted in this module. This emits the data;
    /// dispatch through it is wired separately.
    fn emit_type_info(
        &mut self,
        mangled_name: &str,
        parent_mangled: Option<&str>,
        descriptor: &CodegenValue,
        vtable: Option<&CodegenValue>,
        itable_entries: Vec<(i32, CodegenValue)>,
    ) -> CodegenValue;

    /// Get a pointer to a class's `TypeInfo` global, declaring it as an external
    /// reference when it is not already present in this module. Used at object
    /// allocation to store the handle at offset-0, and wherever dispatch needs
    /// the runtime type. The owning module emits the definition via
    /// `emit_type_info`; the linker resolves references here to it.
    fn reference_type_info(&mut self, mangled_name: &str) -> CodegenValue;

    /// The LLVM struct type of a `TypeInfo` record: `{ i32, ptr, ptr, ptr, ptr,
    /// i32 }`. The fields are type_id, parent, descriptor, vtable, itable, and
    /// itable_len. Used to GEP into a `TypeInfo` at a dispatch site.
    fn type_info_struct_type(&mut self) -> LLVMTypeRef;
}

/// A stable 32-bit id for a mangled type or trait name (FNV-1a). Used as the
/// `type_id` / `trait_id` so an itable entry emitted in one module matches the
/// id a dispatch site computes in another, with no global counter.
pub fn stable_type_id(mangled_name: &str) -> i32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in mangled_name.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash as i32
}

/// The dispatch id of a trait, identifying it by module path and name with its
/// generic arguments dropped. A class that implements `Equals<string>` and a
/// bound written `impl Equals` name the same trait, so both must hash to the
/// same itable id; the type argument does not take part in the identity.
pub fn trait_dispatch_id(trait_type: &peko_core::types::PekoType) -> i32 {
    let mut identity = trait_type.clone();
    if !identity.is_generic_param() && !identity.is_function() {
        identity.generics_mut().clear();
    }
    stable_type_id(&identity.to_mangled_string())
}

impl LlvmConstantBuilder for PekoCodegenContext {
    fn create_string(&mut self, string_value: impl ToString) -> CodegenValue {
        // A `string` is the std::core object `{ data: pointer<i8>, length: i64 }`.
        // Emit the literal bytes as a static raw template, GC-allocate a managed
        // buffer sized to hold them (including the trailing NUL), copy the bytes
        // in, then wrap the buffer in a fresh `string` object. The buffer and
        // the object are real managed allocations the collector can relocate.
        let text = string_value.to_string();
        let char_len = text.len(); // logical byte count, excluding the NUL
        let byte_len = char_len + 1; // include the trailing '\0'

        // 1. Static raw template: an address-space-0 i8* to the literal bytes.
        //    An empty literal uses an explicit NUL global so the template
        //    pointer is always valid and the byte copy in step 3 reads a real
        //    terminated byte rather than a degenerate zero-length global.
        let template_ptr = if text.is_empty() {
            self.emit_empty_cstr_global()
        } else {
            let owned = cstr(text);
            unsafe {
                core::LLVMBuildGlobalStringPtr(self.llvm_builder, owned.as_ptr(), c"".as_ptr())
            }
        };

        // 2. Managed char buffer. `char` is unmanaged, so the array's element
        //    descriptor is the atomic (null) descriptor: no element tracing,
        //    but the buffer itself is a managed, relocatable object.
        let post_stack = self.module_context.step_back_generics();
        let null_element_descriptor = CodegenValue::new(
            unsafe { core::LLVMConstPointerNull(core::LLVMPointerType(core::LLVMInt8Type(), 0)) },
            PekoType::simple_type("opaque"),
        );
        let array_descriptor =
            self.emit_array_descriptor("array_char", 1, &null_element_descriptor);
        let module = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_top_level()
            .unwrap()
            .llvm_module;
        self.module_context.step_forward(post_stack);
        let byte_count = self.create_constant_int(byte_len as i32);
        let buffer_type = managed_pointer_type(PekoType::simple_type("i8"));
        let buffer = self
            .allocate_managed_object_sized(&array_descriptor, &byte_count, &buffer_type)
            .unwrap_or_else(|| self.create_error_value());

        // 3. memcpy the literal bytes from the raw template (as0) into the
        //    managed buffer (as1). The intrinsic declaration was already added
        //    to the owning module in step 2 above.
        let memcpy_name = c"llvm.memcpy.p1.p0.i64";
        let mut memcpy_fn = unsafe { core::LLVMGetNamedFunction(module, memcpy_name.as_ptr()) };
        if memcpy_fn.is_null() {
            // void @llvm.memcpy.p1.p0.i64(ptr addrspace(1) dest, ptr src,
            //                             i64 len, i1 isvolatile)
            let mut param_types = [
                unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 1) },
                unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) },
                unsafe { core::LLVMInt64Type() },
                unsafe { core::LLVMInt1Type() },
            ];
            let memcpy_type = unsafe {
                core::LLVMFunctionType(
                    core::LLVMVoidType(),
                    param_types.as_mut_ptr(),
                    param_types.len() as u32,
                    0,
                )
            };
            memcpy_fn = unsafe { core::LLVMAddFunction(module, memcpy_name.as_ptr(), memcpy_type) };
        }

        let memcpy_param_types = [
            unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 1) },
            unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) },
            unsafe { core::LLVMInt64Type() },
            unsafe { core::LLVMInt1Type() },
        ];
        let memcpy_type = unsafe {
            core::LLVMFunctionType(
                core::LLVMVoidType(),
                memcpy_param_types.as_ptr() as *mut _,
                memcpy_param_types.len() as u32,
                0,
            )
        };

        let mut memcpy_args = [
            buffer.llvm_value,
            template_ptr,
            unsafe { core::LLVMConstInt(core::LLVMInt64Type(), byte_len as u64, 0) },
            unsafe { core::LLVMConstInt(core::LLVMInt1Type(), 0, 0) },
        ];
        unsafe {
            core::LLVMBuildCall2(
                self.llvm_builder,
                memcpy_type,
                memcpy_fn,
                memcpy_args.as_mut_ptr(),
                memcpy_args.len() as u32,
                c"".as_ptr(),
            );
        }

        // 4. Wrap the buffer in a `string` object. Allocate the std::core
        //    `string` class, store the managed buffer into `data` and the byte
        //    count into `length`. These initializing stores go into a fresh,
        //    not-yet-reachable object. When the class is out of scope (no-std
        //    snippet harness) fall back to the raw buffer typed `string`.
        let string_type = PekoType::simple_type("string");
        if let Some(string_class) = self.get_class_by_type(&string_type)
            && let Some(string_object) = self.allocate_class(&string_class)
        {
            let buffer_value = CodegenValue::new(buffer.llvm_value, buffer_type);
            let length_value = self.create_constant_int64(char_len as i32);
            self.set_object_attribute(&string_object, "data", &buffer_value);
            self.set_object_attribute(&string_object, "length", &length_value);
            return string_object;
        }

        CodegenValue::new(buffer.llvm_value, PekoType::simple_type("string"))
    }

    fn create_cstring(&mut self, string_value: impl ToString) -> CodegenValue {
        // A `cstr` is a raw, unmanaged (address space 0) static C string.
        // An empty literal is emitted as an explicit one-byte NUL global so
        // the pointer always refers to a real, valid, terminated byte.
        // Passing an empty string to LLVMBuildGlobalStringPtr produces a
        // degenerate zero-length global whose pointer can alias other memory.
        let text = string_value.to_string();
        let ptr = if text.is_empty() {
            self.emit_empty_cstr_global()
        } else {
            let owned = cstr(text);
            unsafe {
                core::LLVMBuildGlobalStringPtr(self.llvm_builder, owned.as_ptr(), c"".as_ptr())
            }
        };
        CodegenValue::new(ptr, PekoType::simple_type("cstr"))
    }

    /// Emits a stable, address-space-0 pointer to a single NUL byte. Used for
    /// empty string and cstr literals so they never route through
    /// LLVMBuildGlobalStringPtr with a zero-length value. The owning module is
    /// resolved with the same step-back-generics dance the descriptor emitters
    /// use, so the global is created in the module that the referencing
    /// instruction belongs to rather than a generic-instantiation context.
    fn emit_empty_cstr_global(&mut self) -> LLVMValueRef {
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

        let name = c"__peko_empty_cstr";
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, name.as_ptr());
            if !existing.is_null() {
                existing
            } else {
                let i8_type = core::LLVMInt8Type();
                let arr_type = core::LLVMArrayType2(i8_type, 1);
                let g = core::LLVMAddGlobal(module, arr_type, name.as_ptr());
                let zero = core::LLVMConstInt(i8_type, 0, 0);
                let mut elems = [zero];
                let init = core::LLVMConstArray2(i8_type, elems.as_mut_ptr(), 1);
                core::LLVMSetInitializer(g, init);
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMPrivateLinkage);
                g
            }
        };

        // Return an i8* (address space 0) to the first byte.
        let i8_type = unsafe { core::LLVMInt8Type() };
        let arr_type = unsafe { core::LLVMArrayType2(i8_type, 1) };
        let zero = unsafe { core::LLVMConstInt(core::LLVMInt64Type(), 0, 0) };
        let mut indices = [zero, zero];
        unsafe {
            core::LLVMBuildInBoundsGEP2(
                self.llvm_builder,
                arr_type,
                global,
                indices.as_mut_ptr(),
                indices.len() as u32,
                c"".as_ptr(),
            )
        }
    }

    fn create_constant_char(&mut self, char_value: char) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstInt(core::LLVMInt8Type(), char_value as u64, 1) },
            PekoType::simple_type("i8"),
        )
    }

    fn create_constant_int(&mut self, int_value: i32) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstInt(core::LLVMInt32Type(), int_value as u64, 1) },
            PekoType::simple_type("i32"),
        )
    }

    fn create_constant_int64(&mut self, int_value: i32) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstInt(core::LLVMInt64Type(), int_value as u64, 1) },
            PekoType::simple_type("i64"),
        )
    }

    fn create_constant_float(&mut self, float_value: f32) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstReal(core::LLVMFloatType(), float_value as f64) },
            PekoType::simple_type("f32"),
        )
    }

    fn create_constant_double(&mut self, double_value: f64) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstReal(core::LLVMDoubleType(), double_value) },
            PekoType::simple_type("f64"),
        )
    }

    fn create_constant_boolean(&mut self, boolean: bool) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstInt(core::LLVMInt1Type(), boolean as u64, 0) },
            PekoType::simple_type("i1"),
        )
    }

    fn create_null_pointer(&mut self) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstNull(core::LLVMPointerType(core::LLVMInt8Type(), 0)) },
            PekoType::simple_type("opaque"),
        )
    }

    fn create_branch_exit(&mut self) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstNull(core::LLVMPointerType(core::LLVMInt8Type(), 0)) },
            PekoType::simple_type("<<branchexit>>"),
        )
    }

    fn create_return_exit(&mut self) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstNull(core::LLVMPointerType(core::LLVMInt8Type(), 0)) },
            PekoType::simple_type("<<returnexit>>"),
        )
    }

    fn create_error_value(&mut self) -> CodegenValue {
        CodegenValue::new(
            unsafe { core::LLVMConstNull(self.llvm_error_type()) },
            PekoType::error_type(),
        )
    }

    fn build_zero_value(&mut self, default_type: &PekoType) -> CodegenValue {
        if default_type.is_builtin_type() {
            CodegenValue::new(
                match default_type.to_string().as_str() {
                    "i32" => unsafe { core::LLVMConstNull(core::LLVMInt32Type()) },
                    "i64" => unsafe { core::LLVMConstNull(core::LLVMInt64Type()) },
                    "i128" => unsafe { core::LLVMConstNull(core::LLVMInt128Type()) },
                    "i16" => unsafe { core::LLVMConstNull(core::LLVMInt16Type()) },
                    "f32" => unsafe { core::LLVMConstNull(core::LLVMFloatType()) },
                    "f64" => unsafe { core::LLVMConstNull(core::LLVMDoubleType()) },
                    "char" => unsafe { core::LLVMConstNull(core::LLVMInt8Type()) },
                    "string" | "opaque" => unsafe {
                        core::LLVMConstPointerNull(core::LLVMPointerType(core::LLVMInt8Type(), 0))
                    },
                    _ => unsafe { core::LLVMConstNull(core::LLVMInt1Type()) },
                },
                default_type.clone(),
            )
        } else {
            let llvm_type = self
                .get_llvm_type(default_type)
                .unwrap_or_else(|| unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) });
            CodegenValue::new(
                unsafe { core::LLVMConstNull(llvm_type) },
                default_type.clone(),
            )
        }
    }

    fn emit_type_descriptor(
        &mut self,
        mangled_name: &str,
        kind: i32,
        offsets: Vec<usize>,
    ) -> CodegenValue {
        let i32_type = unsafe { core::LLVMInt32Type() };
        let i64_type = unsafe { core::LLVMInt64Type() };

        // Build the `[count x i64]` offset array constant.
        let mut offset_consts = offsets
            .iter()
            .map(|offset| unsafe { core::LLVMConstInt(i64_type, *offset as u64, 0) })
            .collect::<Vec<_>>();
        let offsets_array = unsafe {
            core::LLVMConstArray2(
                i64_type,
                offset_consts.as_mut_ptr(),
                offset_consts.len() as u64,
            )
        };

        // Build the descriptor struct constant `{ i32 kind, i32 count,
        // [count x i64] offsets }`.
        let mut descriptor_fields = vec![
            unsafe { core::LLVMConstInt(i32_type, kind as u64, 0) },
            unsafe { core::LLVMConstInt(i32_type, offsets.len() as u64, 0) },
            offsets_array,
        ];
        let descriptor_const = unsafe {
            core::LLVMConstStruct(
                descriptor_fields.as_mut_ptr(),
                descriptor_fields.len() as u32,
                0,
            )
        };

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

        let global_name = cstr(format!("peko_desc_{mangled_name}"));
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if !existing.is_null() {
                existing
            } else {
                let g = core::LLVMAddGlobal(
                    module,
                    core::LLVMTypeOf(descriptor_const),
                    global_name.as_ptr(),
                );
                core::LLVMSetInitializer(g, descriptor_const);
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMPrivateLinkage);
                g
            }
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn emit_class_descriptor(&mut self, mangled_name: &str, offsets: Vec<usize>) -> CodegenValue {
        let i32_type = unsafe { core::LLVMInt32Type() };
        let i64_type = unsafe { core::LLVMInt64Type() };

        // Build the descriptor struct constant { i32 kind=0, i32 count,
        // [count x i64] offsets }.
        let mut offset_consts = offsets
            .iter()
            .map(|offset| unsafe { core::LLVMConstInt(i64_type, *offset as u64, 0) })
            .collect::<Vec<_>>();
        let offsets_array = unsafe {
            core::LLVMConstArray2(
                i64_type,
                offset_consts.as_mut_ptr(),
                offset_consts.len() as u64,
            )
        };
        let mut descriptor_fields = vec![
            unsafe { core::LLVMConstInt(i32_type, 0, 0) },
            unsafe { core::LLVMConstInt(i32_type, offsets.len() as u64, 0) },
            offsets_array,
        ];
        let descriptor_const = unsafe {
            core::LLVMConstStruct(
                descriptor_fields.as_mut_ptr(),
                descriptor_fields.len() as u32,
                0,
            )
        };

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

        // External linkage so importing modules can reference this same
        // symbol; the owner provides the initializer (the definition).
        // If a global with this name already exists (e.g. a prior external
        // declaration was emitted before the definition), reuse it and set
        // the initializer on the existing global rather than creating a
        // duplicate that LLVM would auto-rename to "name.N".
        let global_name = cstr(format!("peko_desc_{mangled_name}"));
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if !existing.is_null() {
                // Upgrade the existing declaration to a definition.
                core::LLVMSetInitializer(existing, descriptor_const);
                core::LLVMSetGlobalConstant(existing, 1);
                core::LLVMSetLinkage(existing, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                existing
            } else {
                let g = core::LLVMAddGlobal(
                    module,
                    core::LLVMTypeOf(descriptor_const),
                    global_name.as_ptr(),
                );
                core::LLVMSetInitializer(g, descriptor_const);
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                g
            }
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn declare_class_descriptor(
        &mut self,
        mangled_name: &str,
        offset_count: usize,
    ) -> CodegenValue {
        let i32_type = unsafe { core::LLVMInt32Type() };
        let i64_type = unsafe { core::LLVMInt64Type() };

        // Reconstruct the descriptor's type { i32, i32, [offset_count x
        // i64] } so the declaration matches the owning module's
        // definition. No initializer is set, leaving this an external
        // reference the linker resolves to the definition.
        let offsets_array_type = unsafe { core::LLVMArrayType2(i64_type, offset_count as u64) };
        let mut field_types = vec![i32_type, i32_type, offsets_array_type];
        let descriptor_type =
            unsafe { core::LLVMStructType(field_types.as_mut_ptr(), field_types.len() as u32, 0) };

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

        let global_name = cstr(format!("peko_desc_{mangled_name}"));
        // If a global with this name already exists in the module (e.g.
        // the definition was emitted here, or a prior declaration was
        // already added), reuse it rather than calling LLVMAddGlobal,
        // which would auto-rename the new global to "name.N" and produce
        // an unresolvable private symbol at link time.
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if !existing.is_null() {
                existing
            } else {
                let g = core::LLVMAddGlobal(module, descriptor_type, global_name.as_ptr());
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                g
            }
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn emit_class_vtable(
        &mut self,
        mangled_name: &str,
        vtable_struct_type: LLVMTypeRef,
        method_pointers: Vec<(usize, LLVMValueRef)>,
    ) -> CodegenValue {
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

        let global_name = cstr(format!("peko_vtable_{mangled_name}"));

        // Reuse an existing vtable global in this module rather than emitting a
        // duplicate that LLVM would auto-rename.
        let existing = unsafe { core::LLVMGetNamedGlobal(module, global_name.as_ptr()) };
        if !existing.is_null() {
            return CodegenValue::new(existing, PekoType::simple_type("opaque"));
        }

        // Place each function pointer at its virtual-table slot index. A
        // function pointer is a constant, so the whole struct is a constant.
        let slot_count = method_pointers
            .iter()
            .map(|(index, _)| index + 1)
            .max()
            .unwrap_or(0);
        let null_pointer = unsafe { core::LLVMConstPointerNull(core::LLVMPointerType(core::LLVMInt8Type(), 0)) };
        let mut slots = vec![null_pointer; slot_count];
        for (index, function_value) in method_pointers {
            slots[index] = function_value;
        }

        let vtable_const = unsafe {
            core::LLVMConstNamedStruct(vtable_struct_type, slots.as_mut_ptr(), slots.len() as u32)
        };

        let global = unsafe {
            let g = core::LLVMAddGlobal(module, vtable_struct_type, global_name.as_ptr());
            core::LLVMSetInitializer(g, vtable_const);
            core::LLVMSetGlobalConstant(g, 1);
            core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMPrivateLinkage);
            g
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn emit_witness_table(
        &mut self,
        class_mangled: &str,
        trait_mangled: &str,
        slot_indices: Vec<i32>,
    ) -> CodegenValue {
        let i32_type = unsafe { core::LLVMInt32Type() };

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

        let global_name = cstr(format!("peko_witness_{class_mangled}__{trait_mangled}"));
        let existing = unsafe { core::LLVMGetNamedGlobal(module, global_name.as_ptr()) };
        if !existing.is_null() {
            return CodegenValue::new(existing, PekoType::simple_type("opaque"));
        }

        let mut index_consts = slot_indices
            .iter()
            .map(|index| unsafe { core::LLVMConstInt(i32_type, *index as u64, 1) })
            .collect::<Vec<_>>();
        let witness_array = unsafe {
            core::LLVMConstArray2(i32_type, index_consts.as_mut_ptr(), index_consts.len() as u64)
        };
        let array_type = unsafe { core::LLVMArrayType2(i32_type, slot_indices.len() as u64) };

        let global = unsafe {
            let g = core::LLVMAddGlobal(module, array_type, global_name.as_ptr());
            core::LLVMSetInitializer(g, witness_array);
            core::LLVMSetGlobalConstant(g, 1);
            // External linkage so a trait cast (`x as Trait`) in another module
            // can reference this witness. The name is unique per (class, trait)
            // and emitted once in the class's own module, so there is one
            // definition to resolve against.
            core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
            g
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn emit_type_info(
        &mut self,
        mangled_name: &str,
        parent_mangled: Option<&str>,
        descriptor: &CodegenValue,
        vtable: Option<&CodegenValue>,
        itable_entries: Vec<(i32, CodegenValue)>,
    ) -> CodegenValue {
        let i32_type = unsafe { core::LLVMInt32Type() };
        let ptr_type = unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) };
        let typeinfo_type = self.type_info_struct_type();
        // ITableEntry = { i32 trait_id, ptr witness }.
        let mut entry_field_types = [i32_type, ptr_type];
        let entry_type = unsafe {
            core::LLVMStructType(entry_field_types.as_mut_ptr(), entry_field_types.len() as u32, 0)
        };

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

        let global_name = cstr(format!("peko_typeinfo_{mangled_name}"));

        // The itable global, or a null pointer when the class implements no
        // traits.
        let itable_count = itable_entries.len();
        let itable_pointer = if itable_entries.is_empty() {
            unsafe { core::LLVMConstPointerNull(ptr_type) }
        } else {
            let mut entry_consts = itable_entries
                .iter()
                .map(|(trait_id, witness)| {
                    let mut fields =
                        [unsafe { core::LLVMConstInt(i32_type, *trait_id as u64, 1) }, witness.llvm_value];
                    unsafe {
                        core::LLVMConstNamedStruct(entry_type, fields.as_mut_ptr(), fields.len() as u32)
                    }
                })
                .collect::<Vec<_>>();
            let itable_array = unsafe {
                core::LLVMConstArray2(entry_type, entry_consts.as_mut_ptr(), entry_consts.len() as u64)
            };
            let itable_array_type = unsafe { core::LLVMArrayType2(entry_type, itable_count as u64) };
            let itable_name = cstr(format!("peko_itable_{mangled_name}"));
            unsafe {
                let g = core::LLVMAddGlobal(module, itable_array_type, itable_name.as_ptr());
                core::LLVMSetInitializer(g, itable_array);
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMPrivateLinkage);
                g
            }
        };

        // The parent's TypeInfo pointer (declared external when not already in
        // this module, so the linker resolves it to the owner's definition), or
        // null at the root.
        let parent_pointer = match parent_mangled {
            Some(parent) => {
                let parent_name = cstr(format!("peko_typeinfo_{parent}"));
                unsafe {
                    let existing = core::LLVMGetNamedGlobal(module, parent_name.as_ptr());
                    if existing.is_null() {
                        let g = core::LLVMAddGlobal(module, typeinfo_type, parent_name.as_ptr());
                        core::LLVMSetGlobalConstant(g, 1);
                        core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                        g
                    } else {
                        existing
                    }
                }
            }
            None => unsafe { core::LLVMConstPointerNull(ptr_type) },
        };

        let vtable_pointer = vtable
            .map(|value| value.llvm_value)
            .unwrap_or_else(|| unsafe { core::LLVMConstPointerNull(ptr_type) });

        let mut fields = [
            unsafe { core::LLVMConstInt(i32_type, stable_type_id(mangled_name) as u64, 1) },
            parent_pointer,
            descriptor.llvm_value,
            vtable_pointer,
            itable_pointer,
            unsafe { core::LLVMConstInt(i32_type, itable_count as u64, 1) },
        ];
        let typeinfo_const = unsafe {
            core::LLVMConstNamedStruct(typeinfo_type, fields.as_mut_ptr(), fields.len() as u32)
        };

        // External linkage so a dispatch site in another module can reference
        // this same handle; the owning module supplies the initializer. Reuse
        // and upgrade an existing declaration rather than emitting a duplicate.
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if !existing.is_null() {
                core::LLVMSetInitializer(existing, typeinfo_const);
                core::LLVMSetGlobalConstant(existing, 1);
                core::LLVMSetLinkage(existing, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                existing
            } else {
                let g = core::LLVMAddGlobal(module, typeinfo_type, global_name.as_ptr());
                core::LLVMSetInitializer(g, typeinfo_const);
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                g
            }
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn type_info_struct_type(&mut self) -> LLVMTypeRef {
        let i32_type = unsafe { core::LLVMInt32Type() };
        let ptr_type = unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) };
        let mut field_types = [i32_type, ptr_type, ptr_type, ptr_type, ptr_type, i32_type];
        unsafe { core::LLVMStructType(field_types.as_mut_ptr(), field_types.len() as u32, 0) }
    }

    fn reference_type_info(&mut self, mangled_name: &str) -> CodegenValue {
        let typeinfo_type = self.type_info_struct_type();

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

        let global_name = cstr(format!("peko_typeinfo_{mangled_name}"));
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if existing.is_null() {
                let g = core::LLVMAddGlobal(module, typeinfo_type, global_name.as_ptr());
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
                g
            } else {
                existing
            }
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }

    fn emit_array_descriptor(
        &mut self,
        mangled_name: &str,
        stride: usize,
        element_descriptor: &CodegenValue,
    ) -> CodegenValue {
        let i32_type = unsafe { core::LLVMInt32Type() };
        let i64_type = unsafe { core::LLVMInt64Type() };

        // Build the descriptor struct constant { i32 kind=1, i64 stride,
        // ptr element_descriptor }.
        let mut descriptor_fields = vec![
            unsafe { core::LLVMConstInt(i32_type, 1, 0) },
            unsafe { core::LLVMConstInt(i64_type, stride as u64, 0) },
            element_descriptor.llvm_value,
        ];
        let descriptor_const = unsafe {
            core::LLVMConstStruct(
                descriptor_fields.as_mut_ptr(),
                descriptor_fields.len() as u32,
                0,
            )
        };

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

        let global_name = cstr(format!("peko_desc_{mangled_name}"));
        let global = unsafe {
            let existing = core::LLVMGetNamedGlobal(module, global_name.as_ptr());
            if !existing.is_null() {
                existing
            } else {
                let g = core::LLVMAddGlobal(
                    module,
                    core::LLVMTypeOf(descriptor_const),
                    global_name.as_ptr(),
                );
                core::LLVMSetInitializer(g, descriptor_const);
                core::LLVMSetGlobalConstant(g, 1);
                core::LLVMSetLinkage(g, llvm_sys_180::LLVMLinkage::LLVMPrivateLinkage);
                g
            }
        };

        CodegenValue::new(global, PekoType::simple_type("opaque"))
    }
}
