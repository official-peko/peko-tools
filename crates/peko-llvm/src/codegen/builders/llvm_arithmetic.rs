//! Layer 2 -- arithmetic, comparison, and boolean operations.
//!
//! These methods translate Pekoscript binary and unary operators into
//! LLVM instruction sequences: integer arithmetic, float arithmetic,
//! boolean logic (including the short-circuit phi-node variant),
//! pointer comparison, string comparison (via the runtime's `strcmp`),
//! and numeric typecasts between integer / float widths.
//!
//! Allowed callees: layers 0-1 (`LlvmTypeBuilder`,
//! `LlvmConstantBuilder`, `LlvmInstructionBuilder`), plus a single
//! cross-layer call from `short_circuit_boolean_operation` into
//! `HighLevelCodegen::box_value_to_type` -- documented at that method.

use llvm_sys_180::core;
use peko_core::asts::PekoAST;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::llvm_constants::LlvmConstantBuilder;
use crate::codegen::builders::llvm_instructions::LlvmInstructionBuilder;
use crate::codegen::builders::llvm_types::LlvmTypeBuilder;
use crate::codegen::builders::prelude::HighLevelCodegen;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{BooleanOperation, CodegenValue, NumericalOperation};

/// Arithmetic, comparison, and boolean operation emitters.
pub trait LlvmArithmeticBuilder {
    /// Cast a numeric `value` to `target_type`. Handles bool->int (with the
    /// negation step that flips LLVM's `i1` representation to a signed
    /// integer), int<->int with width adjustment, float<->float, float->int
    /// (truncation), and int->float. Passes the value through unchanged
    /// for combinations not covered.
    fn typecast_number_value(
        &mut self,
        value: &CodegenValue,
        target_type: &PekoType,
    ) -> CodegenValue;

    /// Emit an integer arithmetic or comparison op. For Modulus and
    /// Exponentiation this dispatches to the runtime functions
    /// `Runtime::Modulus` / `Runtime::Exponential`.
    fn build_int_operation(
        &mut self,
        operation: NumericalOperation,
        int1: &CodegenValue,
        int2: &CodegenValue,
    ) -> CodegenValue;

    /// Emit a float arithmetic or comparison op. Modulus and
    /// Exponentiation also route through the runtime.
    fn build_float_operation(
        &mut self,
        operation: NumericalOperation,
        float1: &CodegenValue,
        float2: &CodegenValue,
    ) -> CodegenValue;

    /// Emit a short-circuit `&&` / `||` over two AST nodes, building
    /// fresh basic blocks for the RHS evaluation and the join, and using
    /// a phi node to merge. This is the form used at call sites where
    /// only the AST is available; if both operands are already
    /// codegen-evaluated `CodegenValue`s, use `build_boolean_operation`.
    fn short_circuit_boolean_operation(
        &mut self,
        operation: BooleanOperation,
        bool1: &PekoAST,
        bool2: &PekoAST,
    ) -> Option<CodegenValue>;

    /// Emit a non-short-circuit boolean op over two already-evaluated
    /// `bool` values.
    fn build_boolean_operation(
        &mut self,
        operation: BooleanOperation,
        bool1: &CodegenValue,
        bool2: &CodegenValue,
    ) -> CodegenValue;

    /// Emit a string equality or inequality test via the runtime's
    /// `strcmp`. When `equals` is `true`, returns whether the strings are
    /// equal; otherwise returns whether they differ.
    fn build_string_comparison(
        &mut self,
        string1: &CodegenValue,
        string2: &CodegenValue,
        equals: bool,
    ) -> CodegenValue;

    /// Emit a pointer equality or inequality test by integer-comparing
    /// the pointer bits.
    fn build_pointer_comparison(
        &mut self,
        pointer1: &CodegenValue,
        pointer2: &CodegenValue,
        equals: bool,
    ) -> CodegenValue;
}

impl LlvmArithmeticBuilder for PekoCodegenContext {
    fn typecast_number_value(
        &mut self,
        value: &CodegenValue,
        target_type: &PekoType,
    ) -> CodegenValue {
        let target_llvm_type = self.get_llvm_type(target_type).unwrap();

        let casted_llvm_value = unsafe {
            if value.value_type.to_string() == "bool" && target_type.is_integer() {
                // LLVMBuildIntCast emits sext i1 to iN, producing an
                // instruction. LLVMConstNeg cannot wrap an instruction
                // result; use LLVMBuildNeg to negate it as an instruction.
                core::LLVMBuildNeg(
                    self.llvm_builder,
                    core::LLVMBuildIntCast(
                        self.llvm_builder,
                        value.llvm_value,
                        target_llvm_type,
                        c"".as_ptr(),
                    ),
                    c"".as_ptr(),
                )
            } else if value.value_type.is_integer() && target_type.is_integer() {
                core::LLVMBuildIntCast(
                    self.llvm_builder,
                    value.llvm_value,
                    target_llvm_type,
                    c"".as_ptr(),
                )
            } else if value.value_type.is_float() && target_type.is_float() {
                core::LLVMBuildFPCast(
                    self.llvm_builder,
                    value.llvm_value,
                    target_llvm_type,
                    c"".as_ptr(),
                )
            } else if value.value_type.is_float() && target_type.is_integer() {
                core::LLVMBuildFPToSI(
                    self.llvm_builder,
                    value.llvm_value,
                    target_llvm_type,
                    c"".as_ptr(),
                )
            } else if value.value_type.is_integer() && target_type.is_float() {
                core::LLVMBuildSIToFP(
                    self.llvm_builder,
                    value.llvm_value,
                    target_llvm_type,
                    c"".as_ptr(),
                )
            } else {
                value.llvm_value
            }
        };

        CodegenValue::new(casted_llvm_value, target_type.clone())
    }

    fn build_int_operation(
        &mut self,
        operation: NumericalOperation,
        int1: &CodegenValue,
        int2: &CodegenValue,
    ) -> CodegenValue {
        match operation {
            NumericalOperation::Addition => CodegenValue::new(
                unsafe {
                    core::LLVMBuildAdd(
                        self.llvm_builder,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                int2.value_type.clone(),
            ),
            NumericalOperation::Subtraction => CodegenValue::new(
                unsafe {
                    core::LLVMBuildSub(
                        self.llvm_builder,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                int2.value_type.clone(),
            ),
            NumericalOperation::Multiplication => CodegenValue::new(
                unsafe {
                    core::LLVMBuildMul(
                        self.llvm_builder,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                int2.value_type.clone(),
            ),
            NumericalOperation::Division => CodegenValue::new(
                unsafe {
                    core::LLVMBuildUDiv(
                        self.llvm_builder,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                int2.value_type.clone(),
            ),
            NumericalOperation::Equals => CodegenValue::new(
                unsafe {
                    core::LLVMBuildICmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMIntPredicate::LLVMIntEQ,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::NotEquals => CodegenValue::new(
                unsafe {
                    core::LLVMBuildICmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMIntPredicate::LLVMIntNE,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::GreaterThan => CodegenValue::new(
                unsafe {
                    core::LLVMBuildICmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMIntPredicate::LLVMIntSGT,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::GreaterThanEqual => CodegenValue::new(
                unsafe {
                    core::LLVMBuildICmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMIntPredicate::LLVMIntSGE,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::LessThan => CodegenValue::new(
                unsafe {
                    core::LLVMBuildICmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMIntPredicate::LLVMIntSLT,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::LessThanEqual => CodegenValue::new(
                unsafe {
                    core::LLVMBuildICmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMIntPredicate::LLVMIntSLE,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::Modulus => CodegenValue::new(
                unsafe {
                    core::LLVMBuildSRem(
                        self.llvm_builder,
                        int1.llvm_value,
                        int2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                int2.value_type.clone(),
            ),
            NumericalOperation::Exponentiation => self
                .call_named_function("runtime::pow", vec![int1.clone(), int2.clone()])
                .unwrap(),
        }
    }

    fn build_float_operation(
        &mut self,
        operation: NumericalOperation,
        float1: &CodegenValue,
        float2: &CodegenValue,
    ) -> CodegenValue {
        match operation {
            NumericalOperation::Addition => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFAdd(
                        self.llvm_builder,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                float2.value_type.clone(),
            ),
            NumericalOperation::Subtraction => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFSub(
                        self.llvm_builder,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                float2.value_type.clone(),
            ),
            NumericalOperation::Multiplication => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFMul(
                        self.llvm_builder,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                float2.value_type.clone(),
            ),
            NumericalOperation::Division => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFDiv(
                        self.llvm_builder,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                float2.value_type.clone(),
            ),
            NumericalOperation::Equals => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFCmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMRealPredicate::LLVMRealOEQ,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::NotEquals => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFCmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMRealPredicate::LLVMRealONE,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::GreaterThan => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFCmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMRealPredicate::LLVMRealOGT,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::GreaterThanEqual => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFCmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMRealPredicate::LLVMRealOGE,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::LessThan => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFCmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMRealPredicate::LLVMRealOLT,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::LessThanEqual => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFCmp(
                        self.llvm_builder,
                        llvm_sys_180::LLVMRealPredicate::LLVMRealOLE,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                PekoType::simple_type("i1"),
            ),
            NumericalOperation::Modulus => CodegenValue::new(
                unsafe {
                    core::LLVMBuildFRem(
                        self.llvm_builder,
                        float1.llvm_value,
                        float2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                float2.value_type.clone(),
            ),
            NumericalOperation::Exponentiation => self
                .call_named_function("runtime::pow", vec![float1.clone(), float2.clone()])
                .unwrap(),
        }
    }

    fn short_circuit_boolean_operation(
        &mut self,
        operation: BooleanOperation,
        bool1: &PekoAST,
        bool2: &PekoAST,
    ) -> Option<CodegenValue> {
        // Bring `HighLevelCodegen` into scope here only -- it is the single
        // upward layer call from this trait, used to dispatch the And/Or trait
        // and to unbox a bool object to a raw i1. Documented as intentional.
        use crate::codegen::builders::high_level::HighLevelCodegen;

        let lhs = bool1.build_value(self);

        // bool and i1 always short-circuit on the raw i1, bypassing the And/Or
        // trait. Only a non-bool object operand routes through the trait (both
        // operands evaluated; no short-circuit).
        let lhs_is_bool_like =
            lhs.value_type.name() == "bool" || lhs.value_type.name() == "i1";
        if !lhs_is_bool_like && self.get_class_by_type(&lhs.value_type).is_some() {
            let rhs = bool2.build_value(self);
            let operator = match operation {
                BooleanOperation::And => "&&",
                BooleanOperation::Or => "||",
            };
            return self.apply_operator(operator, &lhs, &rhs);
        }

        let lhs_raw = self.to_raw_bool(&lhs);
        let lhs_block = self.current_basic_block.unwrap();
        let mut rhs_block = self.create_new_block(Some("rhs".to_string()));
        let end_block = self.create_new_block(None);

        match operation {
            BooleanOperation::And => self.build_conditional_branch(&lhs_raw, rhs_block, end_block),
            BooleanOperation::Or => self.build_conditional_branch(&lhs_raw, end_block, rhs_block),
        };

        self.goto_block_end(rhs_block);
        let rhs = bool2.build_value(self);
        let rhs_raw = self.to_raw_bool(&rhs);

        rhs_block = self.current_basic_block.unwrap();

        self.build_branch(end_block);
        self.goto_block_end(end_block);

        let phi_node =
            unsafe { core::LLVMBuildPhi(self.llvm_builder, core::LLVMInt1Type(), c"".as_ptr()) };

        unsafe {
            core::LLVMAddIncoming(
                phi_node,
                vec![lhs_raw.llvm_value, rhs_raw.llvm_value].as_mut_ptr(),
                vec![lhs_block, rhs_block].as_mut_ptr(),
                2,
            );
        }

        Some(CodegenValue::new(phi_node, PekoType::simple_type("i1")))
    }

    fn build_boolean_operation(
        &mut self,
        operation: BooleanOperation,
        bool1: &CodegenValue,
        bool2: &CodegenValue,
    ) -> CodegenValue {
        match operation {
            BooleanOperation::And => CodegenValue::new(
                unsafe {
                    core::LLVMBuildAnd(
                        self.llvm_builder,
                        bool1.llvm_value,
                        bool2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                bool2.value_type.clone(),
            ),
            BooleanOperation::Or => CodegenValue::new(
                unsafe {
                    core::LLVMBuildOr(
                        self.llvm_builder,
                        bool1.llvm_value,
                        bool2.llvm_value,
                        c"".as_ptr(),
                    )
                },
                bool2.value_type.clone(),
            ),
        }
    }

    fn build_string_comparison(
        &mut self,
        string1: &CodegenValue,
        string2: &CodegenValue,
        equals: bool,
    ) -> CodegenValue {
        let string2 =
            if !string2.value_type.is_string_type() && string2.value_type.to_string() != "string" {
                self.box_value_to_type(&PekoType::simple_type("string"), string2)
                    .unwrap()
            } else {
                string2.clone()
            };

        // `strcmp` takes raw `cstr` operands. A `string` object holds its bytes
        // in the managed `data` buffer, so reach through to that buffer for the
        // comparison.
        let left = string_data_as_cstr(self, string1);
        let right = string_data_as_cstr(self, &string2);

        let strcmp_result = self
            .call_named_function("extern::strcmp", vec![left, right])
            .unwrap();
        let zero = self.create_constant_int(0);

        let predicate = if equals {
            llvm_sys_180::LLVMIntPredicate::LLVMIntEQ
        } else {
            llvm_sys_180::LLVMIntPredicate::LLVMIntNE
        };

        CodegenValue::new(
            unsafe {
                core::LLVMBuildICmp(
                    self.llvm_builder,
                    predicate,
                    strcmp_result.llvm_value,
                    zero.llvm_value,
                    c"".as_ptr(),
                )
            },
            PekoType::simple_type("i1"),
        )
    }

    fn build_pointer_comparison(
        &mut self,
        pointer1: &CodegenValue,
        pointer2: &CodegenValue,
        equals: bool,
    ) -> CodegenValue {
        let predicate = if equals {
            llvm_sys_180::LLVMIntPredicate::LLVMIntEQ
        } else {
            llvm_sys_180::LLVMIntPredicate::LLVMIntNE
        };

        // Compare by pointer identity. The operands may live in different
        // address spaces (a managed reference is address space 1, an opaque or
        // null reference is address space 0), and LLVMBuildICmp rejects a
        // direct comparison of pointers in different address spaces.
        //
        // Cast only when the two operands actually differ in address space.
        // When they already match (opaque vs opaque or null, managed vs
        // managed) no cast is emitted and the comparison runs directly. This
        // is the common case, including an opaque handle compared against null,
        // where both operands are already address space 0.
        //
        // When they differ, bring the operand toward the unmanaged address
        // space (0) rather than the managed one. Casting an opaque or null up
        // to address space 1 would manufacture an address-space-1 value out of
        // a non-GC pointer; RewriteStatepointsForGC would then treat that value
        // as a GC pointer that must be relocated, fail to infer a heap base for
        // it, and leave llvm.experimental.gc.statepoint un-lowered and
        // undefined at link time. Casting the managed operand down to address
        // space 0 for a throwaway identity comparison avoids creating any such
        // value; the casted result feeds only the icmp and is never a live root
        // across a safepoint.
        let lhs_space =
            unsafe { core::LLVMGetPointerAddressSpace(core::LLVMTypeOf(pointer1.llvm_value)) };
        let rhs_space =
            unsafe { core::LLVMGetPointerAddressSpace(core::LLVMTypeOf(pointer2.llvm_value)) };

        let (lhs_ptr, rhs_ptr) = if lhs_space == rhs_space {
            (pointer1.llvm_value, pointer2.llvm_value)
        } else {
            let unmanaged_ptr_type = unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) };
            let to_unmanaged = |value: llvm_sys_180::prelude::LLVMValueRef, space: u32| unsafe {
                if space == 0 {
                    value
                } else {
                    core::LLVMBuildAddrSpaceCast(
                        self.llvm_builder,
                        value,
                        unmanaged_ptr_type,
                        c"".as_ptr(),
                    )
                }
            };
            (
                to_unmanaged(pointer1.llvm_value, lhs_space),
                to_unmanaged(pointer2.llvm_value, rhs_space),
            )
        };

        CodegenValue::new(
            unsafe {
                core::LLVMBuildICmp(self.llvm_builder, predicate, lhs_ptr, rhs_ptr, c"".as_ptr())
            },
            PekoType::simple_type("i1"),
        )
    }
}

/// Reduce a string-like value to a raw `cstr` pointer for a libc call.
///
/// A `string` object stores its bytes in the managed `data` buffer, so the
/// buffer pointer is read out of that field. A value that is already a raw
/// char pointer (`cstr`, `char[]`, `&char`) is used directly. The buffer
/// pointer is cast down to address space 0; the comparison that consumes it is
/// synchronous and allocates nothing, so no collection can move the buffer
/// across the call.
fn string_data_as_cstr(
    context: &mut PekoCodegenContext,
    value: &CodegenValue,
) -> CodegenValue {
    let raw_pointer = if context.get_class_by_type(&value.value_type).is_some() {
        context
            .get_object_attribute(value, "data", true)
            .unwrap_or_else(|_| value.clone())
    } else {
        value.clone()
    };

    let cstr_type = unsafe { core::LLVMPointerType(core::LLVMInt8Type(), 0) };
    let address_space =
        unsafe { core::LLVMGetPointerAddressSpace(core::LLVMTypeOf(raw_pointer.llvm_value)) };
    let lowered = if address_space == 0 {
        raw_pointer.llvm_value
    } else {
        unsafe {
            core::LLVMBuildAddrSpaceCast(
                context.llvm_builder,
                raw_pointer.llvm_value,
                cstr_type,
                c"".as_ptr(),
            )
        }
    };

    CodegenValue::new(lowered, PekoType::simple_type("cstr"))
}
