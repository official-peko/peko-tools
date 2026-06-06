//! Layer 0: building `LLVMTypeRef`s from `PekoType`.
//!
//! These methods translate Pekoscript's type system into LLVM's type
//! system. They are leaf operations: they do not emit instructions, do
//! not allocate memory, and do not touch the basic-block cursor.
//!
//! Allowed callees: nothing from `builders/`. Methods on the inherent
//! `PekoCodegenContext` and provided helpers from the core
//! `ExecutionContextAlgorithms` trait (`expand_type`, `get_class_by_type`)
//! are the only out-of-layer calls.

use llvm_sys_180::core;
use llvm_sys_180::prelude::LLVMTypeRef;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::context::PekoCodegenContext;
use crate::codegen::cstr;
use crate::codegen::data_structures::managed_pointer_type;

/// Construction of `LLVMTypeRef`s from `PekoType`s and primitives.
pub trait LlvmTypeBuilder {
    /// Returns `LLVMPointerType(llvm_type, 0)`: a raw pointer (address
    /// space 0) to the given LLVM type.
    fn pointer_type_of(&mut self, llvm_type: LLVMTypeRef) -> LLVMTypeRef;

    /// Returns `LLVMPointerType(llvm_type, 1)`: a managed pointer
    /// (address space 1) to the given LLVM type. Used for GC-tracked
    /// pointers such as the vtable slot.
    fn managed_pointer_type_of(&mut self, llvm_type: LLVMTypeRef) -> LLVMTypeRef;

    /// Translate a `PekoType` to an `LLVMTypeRef`.
    ///
    /// When `get_base_type` is `true`, closure and function types come back
    /// as their concrete struct or function type rather than as a pointer
    /// to one. When `has_var_args` is `true`, function types are emitted as
    /// variadic.
    ///
    /// Returns `None` when the type cannot be fully expanded (typically
    /// because a referenced class or generic is not in scope).
    fn get_llvm_type_full(
        &mut self,
        type1: &PekoType,
        get_base_type: bool,
        has_var_args: bool,
    ) -> Option<LLVMTypeRef>;

    /// Shorthand for `get_llvm_type_full(type1, false, false)`.
    fn get_llvm_type(&mut self, type1: &PekoType) -> Option<LLVMTypeRef>;

    /// Whether a type is GC-managed (lives in address space 1 and is traced
    /// by the collector). A type is managed only at pointer/reference depth
    /// zero, and only when it is a `Pointer<T>`, a closure, or a class
    /// instance. A class type behind a raw pointer (`Object*`) is not
    /// managed; neither are primitives, raw `string`, function pointers, or
    /// `opaque`.
    fn is_managed(&mut self, type1: &PekoType) -> bool;

    /// Build an anonymous LLVM struct type from a list of element types.
    fn create_struct_type(&mut self, types: Vec<LLVMTypeRef>) -> LLVMTypeRef;

    /// Build a named (but body-less) opaque struct in the current LLVM context.
    fn create_named_struct(&mut self, name: impl ToString) -> LLVMTypeRef;

    /// Fill in the body of a previously-named opaque struct.
    fn set_struct_body(&mut self, llvm_struct: LLVMTypeRef, body_type: Vec<LLVMTypeRef>);

    /// The shared LLVM type used to represent the codegen "error" sentinel:
    /// a pointer to `i8`. Distinct from `PekoType::error_type` only at the
    /// LLVM level.
    fn llvm_error_type(&mut self) -> LLVMTypeRef;

    fn layout_placeholder_type(&mut self, attr_type: &PekoType) -> LLVMTypeRef;

    /// Compute the byte size of a `PekoType` as laid out by LLVM for the
    /// current module's data layout. When `base_type` is `false`, pointer
    /// and reference depths collapse to the machine pointer size (8).
    fn get_type_size(&mut self, type1: &PekoType, base_type: bool) -> usize;

    /// Print the LLVM type to stderr. Debugging aid only.
    fn dump_type(&mut self, ty: LLVMTypeRef);
}

impl LlvmTypeBuilder for PekoCodegenContext {
    fn pointer_type_of(&mut self, llvm_type: LLVMTypeRef) -> LLVMTypeRef {
        unsafe { core::LLVMPointerType(llvm_type, 0) }
    }

    fn managed_pointer_type_of(&mut self, llvm_type: LLVMTypeRef) -> LLVMTypeRef {
        unsafe { core::LLVMPointerType(llvm_type, 1) }
    }

    fn get_llvm_type_full(
        &mut self,
        type1: &PekoType,
        get_base_type: bool,
        has_var_args: bool,
    ) -> Option<LLVMTypeRef> {
        if type1.is_error_type {
            return Some(self.llvm_error_type());
        }

        let fully_qualified_type = self.expand_type(type1)?;

        // Closure types: lower to a struct containing
        // { closure_context: managed i8*, function: <function_pointer> }.
        if fully_qualified_type.is_closure {
            let mut function_type = fully_qualified_type.clone();
            function_type
                .generic_types
                .insert(0, managed_pointer_type(PekoType::simple_type("void")));
            function_type.is_closure = false;
            if function_type.function_type.is_none() {
                function_type.function_type = Some(Box::new(PekoType::simple_type("void")));
            }

            let closure_function_type = self.get_llvm_type(&function_type)?;

            let closure_type = unsafe {
                core::LLVMStructType(
                    vec![
                        // The context is a managed allocation, so its
                        // pointer lives in address space 1.
                        core::LLVMPointerType(core::LLVMInt8Type(), 1), // closure context type
                        closure_function_type,                          // closure function type
                    ]
                    .as_mut_ptr(),
                    2,
                    0,
                )
            };

            if get_base_type {
                return Some(closure_type);
            }

            // Closures are managed objects, so the pointer to a closure
            // lives in address space 1.
            return Some(unsafe { core::LLVMPointerType(closure_type, 1) });
        }

        // Function types: collect argument LLVM types, then return type,
        // then emit `LLVMFunctionType`.
        if fully_qualified_type.function_type.is_some() {
            let mut argument_types = Vec::new();
            for argument_type in &fully_qualified_type.generic_types {
                let argument_llvm_type = self.get_llvm_type(argument_type)?;
                argument_types.push(argument_llvm_type);
            }

            let return_type = self.get_llvm_type(
                fully_qualified_type
                    .function_type
                    .as_ref()
                    .unwrap()
                    .as_ref(),
            )?;

            let function_type = unsafe {
                core::LLVMFunctionType(
                    return_type,
                    argument_types.as_mut_ptr(),
                    argument_types.len() as u32,
                    has_var_args as i32,
                )
            };

            if get_base_type {
                return Some(function_type);
            }

            return Some(unsafe { core::LLVMPointerType(function_type, 0) });
        }

        // Pointer<T>: a managed pointer (address space 1) to a value of
        // type T. Pointer<void> is the managed opaque pointer and lowers
        // to an i8 pointer in address space 1.
        if fully_qualified_type.type_name == "Pointer"
            && fully_qualified_type.pointer_depth == 0
            && fully_qualified_type.reference_depth == 0
        {
            let inner = fully_qualified_type
                .generic_types
                .first()
                .cloned()
                .unwrap_or_else(|| PekoType::simple_type("void"));

            let inner_llvm_type = if inner.type_name == "void" && inner.pointer_depth == 0 {
                unsafe { core::LLVMInt8Type() }
            } else {
                self.get_llvm_type(&inner)?
            };

            return Some(unsafe { core::LLVMPointerType(inner_llvm_type, 1) });
        }

        let base_type_class = self.get_class_by_type(type1);

        // Resolve the type without pointer or reference depth, then wrap
        // it `pointer_depth + reference_depth` times.
        let mut base_llvm_type = unsafe {
            if fully_qualified_type.declutter().is_builtin_type() {
                match fully_qualified_type.declutter().to_string().as_str() {
                    // `string` is a managed char buffer in address space 1
                    // (GC-allocated, relocatable). `cstr` is the raw,
                    // unmanaged char* in address space 0 (a C string), as is
                    // `opaque`.
                    "string" => core::LLVMPointerType(core::LLVMInt8Type(), 1),
                    "cstr" | "opaque" => core::LLVMPointerType(core::LLVMInt8Type(), 0),
                    "int" => core::LLVMInt32Type(),
                    "int16" => core::LLVMInt16Type(),
                    "int128" => core::LLVMInt128Type(),
                    "int64" => core::LLVMInt64Type(),
                    "float" => core::LLVMFloatType(),
                    "double" => core::LLVMDoubleType(),
                    "char" => core::LLVMInt8Type(),
                    "bool" => core::LLVMInt1Type(),
                    "void" => core::LLVMVoidType(),
                    _ => panic!("error should not be reached"),
                }
            } else if let Some(class) = base_type_class {
                if get_base_type {
                    class.struct_type
                } else {
                    // A class instance reference is a managed pointer
                    // (address space 1). Any extra pointer or reference
                    // depth below adds raw (address space 0) wraps, so
                    // Object* is a plain pointer to a managed reference.
                    core::LLVMPointerType(class.struct_type, 1)
                }
            } else {
                return None;
            }
        };

        // Extra pointer or reference depth is always raw (address space
        // 0). Only the base of a managed type (handled above) is in
        // address space 1.
        for _ in 0..(fully_qualified_type.pointer_depth + fully_qualified_type.reference_depth) {
            base_llvm_type = unsafe { core::LLVMPointerType(base_llvm_type, 0) }
        }

        Some(base_llvm_type)
    }

    fn get_llvm_type(&mut self, type1: &PekoType) -> Option<LLVMTypeRef> {
        self.get_llvm_type_full(type1, false, false)
    }

    fn is_managed(&mut self, type1: &PekoType) -> bool {
        // A raw pointer or reference to a managed type is itself unmanaged
        // (e.g. `Object*` is a plain address-space-0 pointer).
        if type1.pointer_depth > 0 || type1.reference_depth > 0 {
            return false;
        }

        // `Pointer<T>` and the builtin `string` are managed address-space-1
        // buffers; closures are managed {context, fn} values; class instances
        // are managed. `string` MUST be included here (it lowers to
        // addrspace(1)) so that string-typed fields are traced and relocated by
        // the collector (matching is_managed_pointer, which already lists it).
        type1.type_name == "Pointer"
            || type1.type_name == "string"
            || type1.is_closure
            || self.get_class_by_type(type1).is_some()
    }

    fn create_struct_type(&mut self, mut types: Vec<LLVMTypeRef>) -> LLVMTypeRef {
        unsafe { core::LLVMStructType(types.as_mut_ptr(), types.len() as u32, 0) }
    }

    fn create_named_struct(&mut self, name: impl ToString) -> LLVMTypeRef {
        let owned = cstr(name.to_string());
        unsafe { core::LLVMStructCreateNamed(self.llvm_context, owned.as_ptr()) }
    }

    fn set_struct_body(&mut self, llvm_struct: LLVMTypeRef, mut body_type: Vec<LLVMTypeRef>) {
        unsafe {
            core::LLVMStructSetBody(
                llvm_struct,
                body_type.as_mut_ptr(),
                body_type.len() as u32,
                0,
            );
        }
    }

    fn llvm_error_type(&mut self) -> LLVMTypeRef {
        unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) }
    }

    fn layout_placeholder_type(&mut self, attr_type: &PekoType) -> LLVMTypeRef {
        // Resolve generic type parameters (T, AT, K, V, ...) to their bound
        // concrete types using ONLY the current generic context map (has to be
        // a plain lookup, never expand_type / get_llvm_type). Loop to follow a
        // parameter that binds to another parameter (T -> AT -> String). Bounded
        // guard prevents an accidental self-binding from spinning.
        let mut resolved = attr_type.clone();
        let mut guard = 0;
        while guard < 16 {
            match self.get_generic_types().get(&resolved.type_name).cloned() {
                Some(bound) => {
                    // Substitute, preserving any pointer/reference depth the
                    // parameter itself carried (a `T&` field stays a reference).
                    let mut next = bound;
                    next.pointer_depth += resolved.pointer_depth;
                    next.reference_depth += resolved.reference_depth;
                    if next.type_name == resolved.type_name
                        && next.pointer_depth == resolved.pointer_depth
                        && next.reference_depth == resolved.reference_depth
                    {
                        break; // bound to itself
                    }
                    resolved = next;
                    guard += 1;
                }
                None => break, // not a generic parameter
            }
        }

        unsafe {
            // Any explicit pointer/reference depth -> raw (as0) pointer, 8 bytes.
            if resolved.pointer_depth > 0 || resolved.reference_depth > 0 {
                return core::LLVMPointerType(core::LLVMInt8Type(), 0);
            }
            // By-value closure field -> 16-byte { context-ptr, fn-ptr } struct,
            // NOT a single pointer. Match it or the size is off by 8.
            if resolved.is_closure {
                let mut fields = [
                    core::LLVMPointerType(core::LLVMInt8Type(), 1), // managed context
                    core::LLVMPointerType(core::LLVMInt8Type(), 0), // function pointer
                ];
                return core::LLVMStructType(fields.as_mut_ptr(), 2, 0);
            }
            // Pointer-shaped builtins, handled explicitly so they never route
            // through get_llvm_type: `string` is a managed (as1) char-buffer
            // pointer; `cstr`/`opaque` are raw (as0) pointers. All 8 bytes.
            // `void` has no size; it must never reach LLVMABISizeOfType (that is
            // the "Invalid size request" error). A bare void field cannot be
            // stored anyway, so fall back to an 8-byte pointer placeholder.
            match resolved.declutter().to_string().as_str() {
                "string" => return core::LLVMPointerType(core::LLVMInt8Type(), 1),
                "cstr" | "opaque" => return core::LLVMPointerType(core::LLVMInt8Type(), 0),
                "void" => return core::LLVMPointerType(core::LLVMInt8Type(), 0),
                _ => {}
            }
            // Builtin non-pointer scalars (int/char/bool/float/double/int64/
            // int128/...): real scalar type -- size intrinsic to the builtin,
            // touches no class body.
            if resolved.is_builtin_type() {
                return self.get_llvm_type(&resolved).unwrap();
            }
            // Everything else (Pointer<T>, a class instance, a function type)
            // lowers to a single managed (as1) pointer. Placeholder pointer; do
            // NOT expand T or any class body.
            core::LLVMPointerType(core::LLVMInt8Type(), 1)
        }
    }

    fn get_type_size(&mut self, type1: &PekoType, base_type: bool) -> usize {
        let type1 = &self.expand_type(type1).unwrap();
        let datalayout = unsafe {
            llvm_sys_180::target::LLVMGetModuleDataLayout(
                self.module_context
                    .current_module()
                    .read()
                    .unwrap()
                    .get_top_level()
                    .unwrap()
                    .llvm_module,
            )
        };

        if type1.is_builtin_type() || type1.is_error_type {
            return unsafe {
                llvm_sys_180::target::LLVMABISizeOfType(
                    datalayout,
                    self.get_llvm_type(type1).unwrap(),
                )
            } as usize;
        }

        if !base_type
            || type1.function_type.is_some()
            || type1.pointer_depth > 0
            || type1.reference_depth > 0
        {
            return 8;
        }

        if type1.is_closure {
            return self.get_type_size(&PekoType::simple_type("opaque"), false) * 2;
        }

        // Safety net: if the class cannot be resolved, fall back to the
        // machine pointer size so layout decisions can still proceed.
        let class = match self.get_class_by_type(&type1.no_depth()) {
            Some(class) => class,
            None => return 8,
        };

        let struct_type = class.struct_type;
        if unsafe { core::LLVMIsOpaqueStruct(struct_type) } == 0 {
            return unsafe {
                llvm_sys_180::target::LLVMABISizeOfType(datalayout, struct_type) as usize
            };
        }

        // Fallback (struct body not yet materialized): conservative aligned sum.
        let mut type_size: usize = 8;
        for (attribute_name, attribute) in &class.attributes {
            if attribute_name == "<main_virtual_table>" {
                type_size += 8;
            } else if attribute.attribute_type.is_builtin_type() {
                type_size += self.get_type_size(&attribute.attribute_type, false);
            } else {
                type_size += 8;
            }
        }
        (type_size + 15) & !15usize
    }

    fn dump_type(&mut self, ty: LLVMTypeRef) {
        unsafe {
            core::LLVMDumpType(ty);
        }
    }
}
