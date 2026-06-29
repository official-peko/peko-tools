//! `PekoValueBuilder` implementations for the statement-producing AST
//! nodes: control flow (if / while / for / break / return), declarations
//! at statement scope (variable reassignment, platform-gated bodies),
//! and module-level statements (import, link, style, asset).

use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use llvm_sys_180::core;
use llvm_sys_180::prelude::{LLVMBasicBlockRef, LLVMValueRef};
use peko_core::asts::data_structures::{PositionData, PositionedValue, UnpackItem, VisibilityData};
use peko_core::asts::statements::{
    BreakAST, ContinueAST, ForLoopAST, IfStatementAST, ImportStatementAST, LinkStatementAST,
    PlatformStatementAST, ReturnAST, StyleStatementAST, SwitchStatementAST, VariableReassignmentAST,
    WhileLoopAST,
};
use peko_core::asts::{self, PekoAST};
use peko_core::diagnostics;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::execution::data_structures::{ExecutionModule, ExecutionValue};
use peko_core::types::PekoType;

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::prelude::*;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{
    CodegenModule, CodegenValue, CodegenVariable, GlobalVariable,
};
use crate::codegen::symbol::SymbolName;

impl PekoValueBuilder for PlatformStatementAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let targets = self
            .targets
            .iter()
            .map(|target_value| target_value.value.clone())
            .collect::<Vec<String>>();

        // The body is emitted only when the platform / architecture
        // matches; otherwise the gate evaluates to a no-op at codegen
        // time.
        if (self.architecture_test
            && targets.contains(&codegen_context.target.architecture.to_string()))
            || (!self.architecture_test
                && targets.contains(&codegen_context.target.operating_system.to_string()))
            || (!self.architecture_test
                && targets.contains(&"windowsgui".to_owned())
                && codegen_context.target.operating_system.to_string() == "windows"
                && codegen_context.windowsgui)
        {
            for ast in &self.body.value {
                ast.build_value(codegen_context);
            }
        }

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for VariableReassignmentAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // The LHS expression is built in reference mode so we get back
        // a pointer rather than a loaded value.
        let return_references = codegen_context.return_references;
        codegen_context.return_references = true;

        let variable_reference = self
            .variable_reference
            .as_ref()
            .build_value(codegen_context);

        let previous_primary_object = codegen_context.primary_object.clone();
        let previous_accessed_state = codegen_context.accessed_state.clone();

        codegen_context.return_references = return_references;

        let mut expected_type = variable_reference.get_type();
        expected_type.decrease_pointer_depth();

        // If we are assigning to a constructor-tracked attribute, remove
        // it from the still-needs-to-be-set list so we know the
        // constructor's contract is satisfied.
        let variable_name = match self.variable_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                variable_reference.variable_name.value.clone()
            }
            _ => String::new(),
        };

        if codegen_context.previous_was_this
            && codegen_context.attributes_to_set.contains(&variable_name)
        {
            codegen_context.attributes_to_set.remove(
                codegen_context
                    .attributes_to_set
                    .iter()
                    .position(|key| key.as_str() == variable_name)
                    .unwrap(),
            );
        }
        codegen_context.previous_was_this = false;

        // Hint the RHS expression's expected type so that polymorphic
        // values (e.g. null literals) infer to the LHS's declared type.
        let previous_expected_type = codegen_context.current_expected_type_options.clone();
        codegen_context.current_expected_type_options = Some(vec![expected_type.clone()]);

        let variable_value = self.variable_value.as_ref().build_value(codegen_context);

        let variable_value_boxed = if self.assignment_operator.is_none() {
            codegen_context.box_value_to_type(&expected_type, &variable_value)
        } else {
            let variable_reference_loaded = codegen_context.load_value(&variable_reference);
            codegen_context.apply_operator(
                self.assignment_operator.clone().unwrap().as_str(),
                &variable_reference_loaded,
                &variable_value,
            )
        };

        let value = if variable_value_boxed.is_none() && self.assignment_operator.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_value.get_start().clone(),
                    self.variable_value.get_end().clone(),
                    format!(
                        "cannot assign value of type `{}` to variable of type `{}`. The right-hand side type is not compatible with the variable's declared type",
                        variable_value.get_type(),
                        expected_type
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            codegen_context.create_error_value()
        } else if variable_value_boxed.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.variable_reference.get_start().clone(),
                    self.variable_value.get_end().clone(),
                    format!(
                        "cannot apply binary operator `{}` to values of type `{}` and `{}`. There is no operator overload that accepts these two operand types",
                        self.assignment_operator.clone().unwrap(),
                        variable_reference.get_type(),
                        variable_value.get_type()
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            codegen_context.create_error_value()
        } else {
            variable_value_boxed.unwrap()
        };

        codegen_context.build_managed_store(&variable_reference, &value);

        // If this assignment is on an object state attribute, notify
        // its `onStateChanged` (but not inside a constructor; the
        // object is not yet fully initialized).
        if !codegen_context.in_constructor
            && let Some(accessed_state) = &previous_accessed_state
            && let Some(primary_object) = &previous_primary_object
        {
            let attribute_name_value = codegen_context.create_string(accessed_state);
            let _ = codegen_context.call_object_method(
                primary_object,
                "onStateChanged".to_owned(),
                vec![attribute_name_value],
                None,
            );
        }

        codegen_context.accessed_state = None;
        codegen_context.primary_object = None;

        codegen_context.current_expected_type_options = previous_expected_type;
        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for ReturnAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if !codegen_context.is_builder_in_scope() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_value.clone().unwrap().get_start().clone(),
                    self.return_value.clone().unwrap().get_end().clone(),
                    "cannot return outside of function".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_return_exit();
        }

        // Bare `return;`: no return value to check.
        if self.return_value.is_none() {
            codegen_context.build_return(None);
            return codegen_context.create_return_exit();
        }

        let previous_expected_type = codegen_context.current_expected_type_options.clone();

        // Hint the return-value expression with the current return type.
        if codegen_context.current_return_type.is_some() {
            codegen_context.current_expected_type_options =
                Some(vec![codegen_context.current_return_type.clone().unwrap()]);
        } else if codegen_context.current_return_type.is_none()
            || codegen_context
                .current_return_type
                .as_ref()
                .unwrap()
                .to_string()
                == "void"
        {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_value.clone().unwrap().get_start().clone(),
                    self.return_value.clone().unwrap().get_end().clone(),
                    "cannot return a value from a function with declared return type `void`. Void functions must use `return;` with no expression".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_return_exit();
        }

        codegen_context.expecting_value = true;
        let return_value = self
            .return_value
            .clone()
            .unwrap()
            .as_ref()
            .build_value(codegen_context);
        codegen_context.expecting_value = false;

        let return_value_boxed = codegen_context.box_value_to_type(
            &codegen_context.current_return_type.clone().unwrap(),
            &return_value,
        );

        if return_value_boxed.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.return_value.clone().unwrap().get_start().clone(),
                    self.return_value.clone().unwrap().get_end().clone(),
                    format!(
                        "cannot return value of type `{}`. The enclosing function's declared return type is `{}`, and the returned value's type is not compatible",
                        return_value.value_type,
                        codegen_context.current_return_type.as_ref().unwrap()
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        } else {
            codegen_context.build_return(return_value_boxed);
        }

        codegen_context.current_expected_type_options = previous_expected_type;
        codegen_context.create_return_exit()
    }
}

impl PekoValueBuilder for IfStatementAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if !codegen_context.local_scope {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "if statement cannot be outside of function".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // Whether this `if`'s value is consumed. Captured before building the
        // condition and branches resets `expecting_value`.
        let is_expression = codegen_context.expecting_value;
        codegen_context.expecting_value = false;

        let after_block = codegen_context.create_new_block(None);

        // Tracks whether every branch (including the else) exits via
        // `return`. Only true when an else block exists; without an
        // else, the if-statement cannot guarantee return on all paths.
        let mut every_block_returns = self.else_body.is_some();
        let mut every_block_exits = self.else_body.is_some();

        let mut current_if_block = codegen_context.create_new_block(None);

        // Else-check block: where the *next* condition is evaluated
        // when the current condition is false.
        let mut current_elsecheck_block = codegen_context.current_basic_block.unwrap();

        let else_block = if self.else_body.is_some() {
            Some(codegen_context.create_new_block(None))
        } else {
            None
        };

        // The tail type of each branch (used to decide whether this `if` is an
        // expression) and the PHI incomings (value, originating block) for the
        // branches that reach the merge.
        let mut branch_tails: Vec<Option<PekoType>> = Vec::new();
        let mut phi_incomings: Vec<(LLVMValueRef, LLVMBasicBlockRef)> = Vec::new();

        // Generate each conditional body (the `if` and any `else if`s).
        for (idx, condition_body) in self.conditional_bodies.iter().enumerate() {
            codegen_context.goto_block_end(current_elsecheck_block);

            let condition = condition_body.condition.build_value(codegen_context);
            // Branch on a raw i1: a bool object unboxes through to_raw(), a raw
            // i1 passes through. The analysis pass already validated the type.
            let condition_boxed = codegen_context.to_raw_bool(&condition);

            // Branching logic: not-last goes to next else-check;
            // last with else goes to else; last without else goes after.
            if idx < self.conditional_bodies.len() - 1 {
                current_elsecheck_block = codegen_context.create_new_block(None);
                codegen_context.build_conditional_branch(
                    &condition_boxed,
                    current_if_block,
                    current_elsecheck_block,
                );
            } else if let Some(else_b) = else_block {
                codegen_context.build_conditional_branch(
                    &condition_boxed,
                    current_if_block,
                    else_b,
                );
            } else {
                codegen_context.build_conditional_branch(
                    &condition_boxed,
                    current_if_block,
                    after_block,
                );
            }

            // Generate the body of this conditional branch.
            codegen_context.goto_block_end(current_if_block);

            codegen_context
                .previous_scoped_variables
                .push(codegen_context.scoped_variables.clone());

            let mut block_exits = false;
            let mut block_returns = false;
            let mut last_value: Option<CodegenValue> = None;

            let body_length = condition_body.body.value.len();
            for (index, peko_ast) in condition_body.body.value.iter().enumerate() {
                codegen_context.expecting_value = is_expression && index + 1 == body_length;

                let value = peko_ast.build_value(codegen_context);
                let value_type = value.get_type().to_string();
                last_value = Some(value);

                if !block_returns
                    && !block_exits
                    && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
                {
                    if value_type == "<<branchexit>>" {
                        block_exits = true;
                    } else {
                        block_returns = true;
                    }
                } else if block_exits || block_returns {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            peko_ast.get_start().clone(),
                            condition_body.body.value.last().unwrap().get_end().clone(),
                            "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    break;
                }
            }

            if !block_exits && !block_returns {
                let incoming_block = codegen_context.current_basic_block.unwrap();
                branch_tails.push(last_value.as_ref().map(|value| value.value_type.clone()));
                if let Some(value) = &last_value {
                    phi_incomings.push((value.llvm_value, incoming_block));
                }
                codegen_context.build_branch(after_block);
            } else {
                branch_tails.push(None);
            }

            if idx < self.conditional_bodies.len() - 1 {
                current_if_block = codegen_context.create_new_block(None);
            }

            if !block_returns {
                every_block_returns = false;
            }
            if !block_exits {
                every_block_exits = false;
            }

            codegen_context.scoped_variables.clear();
            codegen_context
                .scoped_variables
                .extend(codegen_context.previous_scoped_variables.pop().unwrap());
        }

        // Generate the else block if present.
        if self.else_body.is_some() {
            codegen_context.goto_block_end(else_block.unwrap());

            codegen_context
                .previous_scoped_variables
                .push(codegen_context.scoped_variables.clone());

            let mut block_exits = false;
            let mut block_returns = false;
            let mut last_value: Option<CodegenValue> = None;

            let else_value = self.else_body.clone().unwrap().value;
            let body_length = else_value.len();
            for (index, peko_ast) in else_value.iter().enumerate() {
                codegen_context.expecting_value = is_expression && index + 1 == body_length;

                let value = peko_ast.build_value(codegen_context);
                let value_type = value.value_type.to_string();
                last_value = Some(value);

                if !block_exits
                    && !block_returns
                    && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
                {
                    if value_type == "<<branchexit>>" {
                        block_exits = true;
                    } else {
                        block_returns = true;
                    }
                } else if block_exits || block_returns {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            peko_ast.get_start().clone(),
                            self.else_body.clone().unwrap().value.last().unwrap().get_end().clone(),
                            "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                    break;
                }
            }

            if !block_returns {
                every_block_returns = false;
            }
            if !block_exits {
                every_block_exits = false;
            }
            if !block_returns && !block_exits {
                let incoming_block = codegen_context.current_basic_block.unwrap();
                branch_tails.push(last_value.as_ref().map(|value| value.value_type.clone()));
                if let Some(value) = &last_value {
                    phi_incomings.push((value.llvm_value, incoming_block));
                }
                codegen_context.build_branch(after_block);
            } else {
                branch_tails.push(None);
            }

            codegen_context.scoped_variables.clear();
            codegen_context
                .scoped_variables
                .extend(codegen_context.previous_scoped_variables.pop().unwrap());
        }

        codegen_context.goto_block_end(after_block);

        // An `if` used as an expression merges its branch tails into a PHI at
        // the merge block and yields that value.
        if is_expression
            && let Some(value_type) =
                codegen_context.if_expression_value_type(self.else_body.is_some(), &branch_tails)
            && let Some(llvm_type) = codegen_context.get_llvm_type(&value_type)
        {
            let phi =
                unsafe { core::LLVMBuildPhi(codegen_context.llvm_builder, llvm_type, c"".as_ptr()) };
            let mut values: Vec<LLVMValueRef> =
                phi_incomings.iter().map(|(value, _)| *value).collect();
            let mut blocks: Vec<LLVMBasicBlockRef> =
                phi_incomings.iter().map(|(_, block)| *block).collect();
            unsafe {
                core::LLVMAddIncoming(
                    phi,
                    values.as_mut_ptr(),
                    blocks.as_mut_ptr(),
                    values.len() as u32,
                );
            }
            return CodegenValue::new(phi, value_type);
        }

        if every_block_exits || every_block_returns {
            codegen_context.remove_block(after_block);
        }

        if every_block_returns {
            codegen_context.create_return_exit()
        } else if every_block_exits {
            codegen_context.create_branch_exit()
        } else {
            codegen_context.create_null_pointer()
        }
    }
}

/// Extracts the variant name from a switch arm pattern `Enum::Variant`.
/// Returns `None` for any other pattern shape.
fn switch_arm_variant(pattern: &PekoAST) -> Option<String> {
    if let PekoAST::ModuleAccess(module_access) = pattern
        && let PekoAST::VariableReference(variant_reference) = module_access.accessor.as_ref()
    {
        return Some(variant_reference.variable_name.value.clone());
    }

    None
}

/// Lowers a `switch` over an enum to an LLVM `switch` instruction. The
/// subject is the integer variant index; each arm body lives in its own
/// block and branches to a shared merge block.
impl PekoValueBuilder for SwitchStatementAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        codegen_context.expecting_value = true;
        let subject = self.subject.build_value(codegen_context);
        codegen_context.expecting_value = false;

        let enum_name = subject.value_type.name().to_string();
        let variants = codegen_context
            .get_enum_variants(&enum_name)
            .unwrap_or_default();

        let after_block = codegen_context.create_new_block(None);

        // One body block per arm. The `_` arm's block is the switch default;
        // with no default arm, control falls through to the merge block.
        let mut arm_blocks: Vec<LLVMBasicBlockRef> = Vec::new();
        let mut default_block = after_block;
        for arm in &self.arms {
            let block = codegen_context.create_new_block(None);
            arm_blocks.push(block);
            if arm.pattern.is_none() {
                default_block = block;
            }
        }

        let case_count = self.arms.iter().filter(|arm| arm.pattern.is_some()).count();

        // Emit the switch in the current block.
        let switch = unsafe {
            core::LLVMBuildSwitch(
                codegen_context.llvm_builder,
                subject.llvm_value,
                default_block,
                case_count as u32,
            )
        };

        // Add a case mapping each variant index to its arm block.
        for (arm, block) in self.arms.iter().zip(arm_blocks.iter()) {
            if let Some(pattern) = &arm.pattern
                && let Some(variant) = switch_arm_variant(pattern)
            {
                let index = variants
                    .iter()
                    .position(|candidate| candidate == &variant)
                    .unwrap_or(0);
                let case_value = codegen_context.create_constant_int(index as i32);
                unsafe {
                    core::LLVMAddCase(switch, case_value.llvm_value, *block);
                }
            }
        }

        // Generate each arm body, branching to the merge block unless the
        // body already exits via `break` or `return`.
        for (arm, block) in self.arms.iter().zip(arm_blocks.iter()) {
            codegen_context.goto_block_end(*block);

            codegen_context
                .previous_scoped_variables
                .push(codegen_context.scoped_variables.clone());

            let mut block_exits = false;
            let mut block_returns = false;

            for peko_ast in &arm.body.value {
                codegen_context.expecting_value = false;
                let value_type = peko_ast.build_value(codegen_context).value_type.to_string();

                if !block_exits
                    && !block_returns
                    && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
                {
                    if value_type == "<<branchexit>>" {
                        block_exits = true;
                    } else {
                        block_returns = true;
                    }
                }
            }

            if !block_exits && !block_returns {
                codegen_context.build_branch(after_block);
            }

            codegen_context.scoped_variables.clear();
            codegen_context
                .scoped_variables
                .extend(codegen_context.previous_scoped_variables.pop().unwrap());
        }

        codegen_context.goto_block_end(after_block);

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for WhileLoopAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if !codegen_context.local_scope {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "while loop cannot be outside of function".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        let loop_block = codegen_context.create_new_block(None);
        let after_block = codegen_context.create_new_block(None);
        let loop_condition_check = codegen_context.create_new_block(None);

        codegen_context.build_branch(loop_condition_check);
        codegen_context.goto_block_end(loop_condition_check);

        let condition = self.conditional_body.condition.build_value(codegen_context);
        // Branch on a raw i1: a bool object unboxes through to_raw().
        let condition_boxed = codegen_context.to_raw_bool(&condition);

        codegen_context.build_conditional_branch(&condition_boxed, loop_block, after_block);

        // Save the previous loop-finish target so `break` inside this
        // loop branches to `after_block`.
        let previous_loop_finish = codegen_context.current_loop_finish_block;
        let previous_loop = codegen_context.current_loop_block;
        codegen_context.current_loop_finish_block = Some(after_block);
        codegen_context.current_loop_block = Some(loop_condition_check);
        codegen_context.goto_block_end(loop_block);

        codegen_context
            .previous_scoped_variables
            .push(codegen_context.scoped_variables.clone());

        let mut branch_exited = false;
        let mut branch_returned = false;

        for peko_ast in &self.conditional_body.body.value {
            let body_ast = peko_ast.build_value(codegen_context).get_type().to_string();
            if !branch_exited
                && !branch_returned
                && (body_ast == "<<branchexit>>" || body_ast == "<<returnexit>>")
            {
                if body_ast == "<<branchexit>>" {
                    branch_exited = true;
                } else {
                    branch_returned = true;
                }
            } else if branch_exited || branch_returned {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        peko_ast.get_start().clone(),
                        self.conditional_body.body.value.last().unwrap().get_end().clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                break;
            }
        }

        // Re-evaluate the condition at the end of the body for the
        // back-edge branch.
        if !branch_exited && !branch_returned {
            codegen_context.build_branch(loop_condition_check);
        }

        codegen_context.goto_block_end(after_block);

        codegen_context.scoped_variables.clear();
        codegen_context
            .scoped_variables
            .extend(codegen_context.previous_scoped_variables.pop().unwrap());

        codegen_context.current_loop_finish_block = previous_loop_finish;
        codegen_context.current_loop_block = previous_loop;

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for ForLoopAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if !codegen_context.local_scope {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "for loop cannot be outside of function".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // Resolve the iterable to its iterator via the `[operator iterator]` overload.
        let iterable = self.iterator.build_value(codegen_context);
        let get_iterator = codegen_context.call_object_method(
            &iterable,
            String::from("[operator iterator]"),
            Vec::new(),
            None,
        );

        let iterator = match get_iterator {
            Ok(value) => value,
            Err(_) => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.iterator.get_start().clone(),
                        self.iterator.get_end().clone(),
                        format!("value of type `{}` is not iterable", iterable.get_type()),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
        };

        let after_block = codegen_context.create_new_block(None);
        let loop_block = codegen_context.create_new_block(None);
        let loop_condition_check = codegen_context.create_new_block(None);

        // check range first
        codegen_context.build_branch(loop_condition_check);
        codegen_context.goto_block_end(loop_condition_check);
        let inrange_call = codegen_context.call_object_method(
            &iterator,
            String::from("inrange"),
            Vec::new(),
            None,
        );

        match inrange_call {
            Err(_) => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.iterator.get_start().clone(),
                        self.iterator.get_end().clone(),
                        format!(
                            "iterator of type `{}` does not have a valid `inrange` method",
                            iterable.get_type()
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }
            Ok(value) => {
                codegen_context.build_conditional_branch(&value, loop_block, after_block);
            }
        }

        let previous_loop_finish = codegen_context.current_loop_finish_block;
        let previous_loop = codegen_context.current_loop_block;
        codegen_context.current_loop_finish_block = Some(after_block);
        codegen_context.current_loop_block = Some(loop_condition_check);
        codegen_context.goto_block_end(loop_block);

        codegen_context
            .previous_scoped_variables
            .push(codegen_context.scoped_variables.clone());

        // Pull the next value for this iteration via the iterator's
        // `next` method.
        let get_next =
            codegen_context.call_object_method(&iterator, String::from("next"), Vec::new(), None);

        let get_next = match get_next {
            Ok(value) => value,
            Err(_) => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.iterator.get_start().clone(),
                        self.iterator.get_end().clone(),
                        format!(
                            "iterator of type `{}` does not have a valid `next` method",
                            iterable.get_type()
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                codegen_context.create_error_value()
            }
        };

        let allocate_next_value = codegen_context.build_stack_allocation(&get_next.value_type);
        codegen_context.build_store(&allocate_next_value, &get_next);

        let qualified_next_value = codegen_context.qualify_value_to_current(allocate_next_value);
        codegen_context.scoped_variables.insert(
            self.item_id.value.clone(),
            CodegenVariable::new(
                VisibilityData::open_visibility(),
                get_next.value_type,
                qualified_next_value,
                None,
                codegen_context.module_context.current_module().clone(),
                None,
            ),
        );

        let mut branch_exits = false;
        let mut branch_returns = false;

        for peko_ast in &self.body.value {
            let value_type = peko_ast.build_value(codegen_context).get_type().to_string();
            if !branch_exits
                && !branch_returns
                && (value_type == "<<branchexit>>" || value_type == "<<returnexit>>")
            {
                if value_type == "<<branchexit>>" {
                    branch_exits = true;
                } else {
                    branch_returns = true;
                }
            } else if branch_exits || branch_returns {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        peko_ast.get_start().clone(),
                        self.body.value.last().unwrap().get_end().clone(),
                        "unreachable code: this statement (and everything after it) cannot run because the function or branch has already exited via `break` or `return`".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                break;
            }
        }

        if !branch_exits && !branch_returns {
            codegen_context.build_branch(loop_condition_check);
        }

        codegen_context.scoped_variables.clear();
        codegen_context
            .scoped_variables
            .extend(codegen_context.previous_scoped_variables.pop().unwrap());

        codegen_context.goto_block_end(after_block);
        codegen_context.current_loop_finish_block = previous_loop_finish;
        codegen_context.current_loop_block = previous_loop;

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for BreakAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if codegen_context.current_loop_finish_block.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "cannot break out of non-loop".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        } else {
            codegen_context.build_branch(codegen_context.current_loop_finish_block.unwrap());
        }

        codegen_context.create_branch_exit()
    }
}

impl PekoValueBuilder for ContinueAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        if codegen_context.current_loop_block.is_none() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "cannot continue out of non-loop".to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
        } else {
            codegen_context.build_branch(codegen_context.current_loop_block.unwrap());
        }

        codegen_context.create_branch_exit()
    }
}

impl PekoValueBuilder for ImportStatementAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let importing_file = codegen_context.get_current_file().to_path_buf();

        // Collect the import path segments and the optional version pin.
        let path_ids: Vec<String> = self
            .module_path
            .iter()
            .map(|segment| segment.value.clone())
            .collect();
        let version = self.module_version.as_ref().map(|v| v.value.as_str());

        // The display form of the path for diagnostics.
        let import_display = path_ids.join("::");

        // The bare module name is the last path segment.
        let module_name = path_ids
            .last()
            .cloned()
            .unwrap_or_else(|| String::from("module"));

        // Imports are only valid at the top level of a top-level module.
        if codegen_context
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_parent()
            .is_some()
        {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    "module import must be made at the global scope level, cannot be in submodule"
                        .to_string(),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // Resolve the import through the shared resolver. Local files take
        // precedence over external packages, and the resolver builds the
        // entry path, the canonical module id, and the root folder to use
        // while the module loads.
        let resolved = match codegen_context.resolve_module(&path_ids, version, &importing_file) {
            Some(resolved) => resolved,
            None => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "cannot find module `{import_display}` in the current scope. Check the module name, that the module is declared, and that it is imported",
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }
        };

        let module_entry_file_path = resolved.entry_file.clone();
        let is_ffi = peko_core::ffi::is_ffi_header(&module_entry_file_path);

        // Whether this import has an unpack list (`import { ... } from x`).
        let has_unpack_list = !self.symbols_to_unpack.is_empty();

        // The name the module takes locally. A plain import uses the user
        // alias or the bare module name. An unpack import uses the
        // canonical module id so two unpacks of different files never
        // share an identity.
        let import_as_module_name = if has_unpack_list {
            resolved.module_id.clone()
        } else if self.import_as.is_some() {
            self.import_as.clone().unwrap().value
        } else {
            module_name.clone()
        };

        // A plain import that binds a name already bound to a different
        // module file is a conflict. Unpack imports cannot conflict
        // because their identity is the unique module id.
        let conflicting_import = if has_unpack_list {
            false
        } else {
            codegen_context
                .module_context
                .top_level_modules
                .get(&import_as_module_name)
                .map(|existing| existing.read().unwrap().get_file().to_path_buf())
                .map(|existing_file| existing_file != module_entry_file_path)
                .unwrap_or(false)
        };

        if conflicting_import {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "module name `{import_as_module_name}` is already imported from a different module. Use `as <alias>` to bind one of them under a different name",
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        // Move the root folder to the resolved module's root for the
        // duration of this import. A registry import points the root at
        // the package directory so its internal paths and ids stay
        // self-consistent. The previous root is restored once the module
        // finishes loading.
        let previous_root_folder = codegen_context.root_folder.clone();
        codegen_context.root_folder = resolved.new_root_folder.clone();

        let previous_outside_primary = codegen_context.outside_primary_module;
        codegen_context.outside_primary_module = true;

        let module_to_import = if codegen_context
            .module_context
            .top_level_modules
            .contains_key(&import_as_module_name)
        {
            codegen_context.module_context.top_level_modules[&import_as_module_name].clone()
        } else {
            // Parse the imported module's source into an AST list. An FFI
            // header is parsed as a C interop surface and lowered to external
            // Peko declarations first, so its functions reach codegen with
            // their raw C symbol names through the external linkage path.
            let raw_source = std::fs::read_to_string(&module_entry_file_path).unwrap();
            let source = if is_ffi {
                let parsed = peko_core::ffi::parse_header(&raw_source);
                for error in &parsed.errors {
                    codegen_context
                        .diagnostics
                        .report_diagnostic(diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            format!("FFI header `{}`: {error}", module_entry_file_path.display()),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ));
                }
                peko_core::ffi::header_to_peko_source(&parsed)
            } else {
                raw_source
            };
            let mut parser = peko_core::parser::PekoParser::new(
                peko_core::lexer::TokenList::from_source(&source, &module_entry_file_path),
                codegen_context.get_current_file(),
            );

            let mut asts = Vec::new();

            while parser.tokens.length() != 0
                && parser.tokens.get_index() != parser.tokens.length() - 1
            {
                loop {
                    if parser.tokens.finished() {
                        break;
                    }

                    match parser.tokens.current_token().get_type() {
                        peko_core::lexer::TokenType::Comment => {
                            parser.skip_comment();
                        }
                        _ => {
                            if parser.tokens.get_index() < parser.tokens.length()
                                && parser.tokens.current_token().equals(";")
                            {
                                parser.tokens.increase_index();
                            } else {
                                break;
                            }
                        }
                    }
                }

                if parser.tokens.finished() {
                    break;
                }

                asts.push(parser.parse());
            }

            // Forward the parser's diagnostics into our context.
            for error in parser.diagnostics.get_diagnostics() {
                codegen_context.diagnostics.report_diagnostic(error.clone());
            }

            let new_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
                import_as_module_name.clone(),
                module_entry_file_path,
                None,
                codegen_context.llvm_context,
            )));

            let importing_module = codegen_context
                .module_context
                .current_module()
                .read()
                .unwrap()
                .name
                .clone();
            codegen_context
                .module_context
                .move_to_module(new_module.clone(), false, false);

            // The runtime, standard library, console, and UI modules
            // are implicitly visible in every module; pre-import them
            // here, with care to skip self-imports and circular cases.
            let default_imports = ["Runtime", "standard", "console", "ui"];
            for import in default_imports {
                if import_as_module_name == "Runtime" {
                    break;
                }

                if import_as_module_name == import
                    || (import_as_module_name == "standard" && import == "console")
                    || (import_as_module_name == "ui" && importing_module == "ui")
                    || !codegen_context
                        .module_context
                        .top_level_modules
                        .contains_key(import)
                {
                    continue;
                }

                let module = Arc::clone(&codegen_context.module_context.top_level_modules[import]);
                codegen_context.import_module(
                    module,
                    if import == "standard" {
                        vec![UnpackItem::All]
                    } else {
                        Vec::new()
                    },
                );
            }

            codegen_context
                .module_context
                .top_level_modules
                .insert_before(1, import_as_module_name, Arc::clone(&new_module));

            // Declarations lowered from an FFI header stay external for their
            // raw name and gc-leaf marking, but are scoped to this module so
            // they resolve through it rather than the global extern module.
            if is_ffi {
                for ast in asts.iter_mut() {
                    match ast {
                        PekoAST::FunctionDeclaration(declaration) => {
                            declaration.visibility.scoped = true;
                        }
                        PekoAST::NewVariable(declaration) => {
                            declaration.visibility.scoped = true;
                        }
                        _ => {}
                    }
                }
            }

            for ast in &asts {
                ast.build_value(codegen_context);
            }

            codegen_context.module_context.move_out_of_module();
            new_module
        };

        codegen_context.outside_primary_module = previous_outside_primary;

        // Restore the root folder now that the imported module is loaded.
        codegen_context.root_folder = previous_root_folder;

        if !codegen_context.creating_required || codegen_context.outside_primary_module {
            codegen_context.import_module(module_to_import, self.symbols_to_unpack.clone());
        }

        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for LinkStatementAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let current_file = codegen_context.get_current_file();
        let current_directory = current_file.parent().unwrap();

        let extension = match self.link_as.value.as_str() {
            "object" => ".o",
            "lib" => ".lib",
            "archive" => ".a",
            _ => {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "{} is not a valid linker file type. Valid linker file types are object, lib, and archive.",
                            self.link_as.value,
                        ),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                return codegen_context.create_error_value();
            }
        };

        let file_path = current_directory.join([self.object.value.as_str(), extension].concat());

        if !file_path.exists() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!(
                        "cannot link file because it does not exist at {}",
                        file_path.display()
                    ),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));
            return codegen_context.create_error_value();
        }

        codegen_context.files_to_link.push(file_path);
        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for StyleStatementAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        let stylesheet_name = if self.stylesheet.value.contains('/') {
            self.stylesheet
                .value
                .split('/')
                .collect::<Vec<&str>>()
                .pop()
                .unwrap()
        } else {
            self.stylesheet.value.as_str()
        };

        let file_path = codegen_context
            .get_current_file()
            .parent()
            .unwrap()
            .join([self.stylesheet.value.as_str(), ".scss"].concat());

        let alternate_css_file_path = codegen_context
            .get_current_file()
            .parent()
            .unwrap()
            .join([self.stylesheet.value.as_str(), ".css"].concat());

        let alternate_sass_file_path = codegen_context
            .get_current_file()
            .parent()
            .unwrap()
            .join([self.stylesheet.value.as_str(), ".sass"].concat());

        if !file_path.exists() {
            codegen_context
                .diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    self.start.clone(),
                    self.end.clone(),
                    format!("cannot find stylesheet at {}", file_path.display()),
                    diagnostics::DiagnosticType::Error,
                    codegen_context.get_current_file().to_path_buf(),
                ));

            // If the user accidentally created the stylesheet with a
            // sibling extension (.css / .sass), emit a hint warning.
            if alternate_css_file_path.exists() || alternate_sass_file_path.exists() {
                let path = if alternate_css_file_path.exists() {
                    alternate_css_file_path
                } else {
                    alternate_sass_file_path
                };

                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        self.start.clone(),
                        self.end.clone(),
                        format!(
                            "found stylesheet at {}, but Peko only uses scss stylesheets. To use this stylesheet, change its extension to .scss",
                            path.display()
                        ),
                        diagnostics::DiagnosticType::Warning,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            }

            return codegen_context.create_error_value();
        }

        let style_name = format!("{}.css", file_path.file_stem().unwrap().to_str().unwrap());
        codegen_context
            .imported_styles
            .insert(file_path.clone(), style_name.clone());

        // When `compiled_styles_folder` is set, the style is served by a
        // local dev server: the global resolves at runtime to a fetch of
        // the compiled stylesheet from `127.0.0.1:<debug_style_port>/<style_name>`.
        let (variable_value, variable_type) = if codegen_context.compiled_styles_folder.is_some() {
            let debug_style_port_access =
                PekoAST::ModuleAccess(asts::expressions::ModuleAccessAST::new(
                    PositionData::default(),
                    PositionData::default(),
                    vec![PositionedValue::create_no_position(String::from("ui"))],
                    Box::new(PekoAST::VariableReference(
                        asts::expressions::VariableReferenceAST::new(
                            PositionedValue::create_no_position(String::from("debug_style_port")),
                        ),
                    )),
                ));

            // The `true` is the `interpolated` flag: this URL splices
            // in the runtime value of `ui::debug_style_port`.
            let string_value = PekoAST::String(asts::values::StringAST::new(
                PositionData::default(),
                PositionData::default(),
                true,
                vec![
                    asts::data_structures::StringChunk::new_text(
                        PositionData::default(),
                        PositionData::default(),
                        String::from("<link>http://127.0.0.1:"),
                    ),
                    asts::data_structures::StringChunk::new_interpolation(
                        PositionData::default(),
                        PositionData::default(),
                        vec![debug_style_port_access],
                    ),
                    asts::data_structures::StringChunk::new_text(
                        PositionData::default(),
                        PositionData::default(),
                        format!("/?style={style_name}"),
                    ),
                ],
            ));

            let return_string_value = PekoAST::Return(ReturnAST::new(
                PositionData::default(),
                PositionData::default(),
                Some(Box::new(string_value)),
            ));

            (
                PekoAST::Closure(asts::declarations::ClosureAST::new(
                    PositionData::default(),
                    PositionData::default(),
                    IndexMap::new(),
                    Vec::new(),
                    Some(PekoType::from_string("string", "")),
                    PositionedValue::create_no_position(vec![return_string_value]),
                )),
                PekoType::from_string("closure()=>string", ""),
            )
        } else {
            // Otherwise the stylesheet is compiled into the binary as a
            // string constant: parse the SCSS at codegen time and emit
            // its compiled CSS as a `String` global.
            let scss_stylesheet_contents = std::fs::read_to_string(&file_path).unwrap();
            let parsed_scss =
                grass::from_string(scss_stylesheet_contents, &grass::Options::default());

            let mut parsed_scss = match parsed_scss {
                Ok(css) => css,
                Err(_) => {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.start.clone(),
                            self.end.clone(),
                            "there are diagnostics in this stylesheet".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    String::new()
                }
            };

            if parsed_scss.is_empty() {
                parsed_scss = String::from(" ");
            }

            (
                codegen_context.create_standard_string_ast(parsed_scss),
                PekoType::simple_type("String"),
            )
        };

        // Mangle the global name with the module path.
        let global_name = {
            let mut final_name = self.stylesheet.value.clone();
            let mut next_module = codegen_context.module_context.current_module().clone();
            loop {
                final_name.insert_str(
                    0,
                    [next_module.read().unwrap().get_name(), "::"]
                        .concat()
                        .as_str(),
                );
                let parent = next_module.read().unwrap().get_parent().cloned();
                match parent {
                    Some(p) => next_module = p,
                    None => break,
                }
            }
            final_name
        };

        let global_symbol_name = SymbolName::from(None, None, &global_name, None);

        let mut global_variable = codegen_context.create_named_global(
            Some(global_symbol_name.clone()),
            &variable_type,
            false,
            true,
        );

        let variable = CodegenVariable::new(
            VisibilityData::open_visibility(),
            variable_type.clone(),
            codegen_context.qualify_value_to_current(global_variable.clone()),
            Some(global_symbol_name),
            codegen_context.module_context.current_module().clone(),
            None,
        );
        codegen_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .get_variables_mut()
            .insert(
                String::from(stylesheet_name),
                Arc::new(RwLock::new(variable)),
            );

        global_variable.value_type.decrease_pointer_depth();

        let current_file = codegen_context.get_current_file().to_path_buf();
        CodegenModule::get_top_parent(codegen_context.module_context.current_module())
            .write()
            .unwrap()
            .top_level_info
            .as_mut()
            .unwrap()
            .globals_info
            .globals_to_set
            .push((
                GlobalVariable::new(
                    global_variable,
                    variable_type,
                    stylesheet_name.to_string(),
                    current_file,
                ),
                variable_value,
            ));

        CodegenModule::get_top_parent(codegen_context.module_context.current_module())
            .write()
            .unwrap()
            .top_level_info
            .as_mut()
            .unwrap()
            .imported_styles
            .push(file_path);

        codegen_context.create_null_pointer()
    }
}
