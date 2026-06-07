//! Layer 3: context snapshot, restore, and call-site position tracking.
//!
//! `snapshot_context` / `reset_context` capture and restore the bits of
//! the codegen context that change when control crosses a function
//! boundary or when generic instantiation temporarily redirects module
//! lookup. They are the codegen counterpart to
//! `SimulatorContextSnapshot` in `peko_core::simulator::context`.
//!
//! `track_call_position` / `reset_call_position` write the runtime's
//! `current_line` / `current_file` globals around a call so that
//! Pekoscript backtraces point at the right source location.

use std::collections::HashMap;

use llvm_sys_180::prelude::LLVMBasicBlockRef;
use peko_core::execution::ExecutionModuleContext;
use peko_core::execution::data_structures::ExecutionModule;

use crate::codegen::builders::modules::ModuleManager;
use crate::codegen::builders::prelude::{LlvmConstantBuilder, LlvmInstructionBuilder};
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{CodegenModule, CodegenValue, CodegenVariable};

/// Concrete shape of a captured codegen context.
pub struct CodegenContextSnapshot {
    pub current_basic_block: Option<LLVMBasicBlockRef>,
    pub previous_scoped_variables: Vec<HashMap<String, CodegenVariable>>,
    pub scoped_variables: HashMap<String, CodegenVariable>,
    pub local_scope: bool,
    pub attributes_to_set: Vec<String>,
    pub module_context: ExecutionModuleContext<CodegenModule>,
    pub current_this: Option<CodegenVariable>,
}

/// Context capture/restore and call-position tracking.
pub trait ScopeManager {
    /// Capture the parts of the context that change when control enters
    /// a new function body or generic instantiation. Also clears
    /// `attributes_to_set` on `self` so the new scope starts fresh; the
    /// caller is expected to restore the previous list via
    /// `reset_context`.
    fn snapshot_context(&mut self) -> CodegenContextSnapshot;

    /// Restore a previously captured snapshot. If
    /// `snapshot.current_basic_block` is `Some`, the LLVM builder is
    /// repositioned at the end of that block.
    fn reset_context(&mut self, snapshot: CodegenContextSnapshot);

    /// Write `line` and `file` to the runtime's `current_line` /
    /// `current_file` globals so that any panic raised during the
    /// upcoming call reports the right source location. Returns the
    /// previous values of those globals so they can be restored after
    /// the call via `reset_call_position`.
    ///
    /// When the `current_line` global is not in scope (e.g. when
    /// compiling the runtime itself), returns null pointers and emits
    /// no IR.
    fn track_call_position(
        &mut self,
        file: impl ToString,
        line: usize,
    ) -> (CodegenValue, CodegenValue);

    /// Wrap a `CodegenValue` in a single-entry map keyed by the current
    /// module's UUID. Used by the per-module-uuid value tables on
    /// `CodegenFunction` / `CodegenVariable`.
    fn qualify_value_to_current(&mut self, value: CodegenValue) -> HashMap<String, CodegenValue>;

    /// Counterpart to `track_call_position`: write the previous line
    /// and file back to the runtime globals.
    fn reset_call_position(&mut self, previous_line: &CodegenValue, previous_file: &CodegenValue);
}

impl ScopeManager for PekoCodegenContext {
    fn snapshot_context(&mut self) -> CodegenContextSnapshot {
        // Take, don't clone, the `attributes_to_set` so the new scope
        // starts empty. Everything else is captured by clone.
        let attributes_to_set = std::mem::take(&mut self.attributes_to_set);

        CodegenContextSnapshot {
            current_basic_block: self.current_basic_block,
            previous_scoped_variables: self.previous_scoped_variables.clone(),
            scoped_variables: self.scoped_variables.clone(),
            local_scope: self.local_scope,
            attributes_to_set,
            module_context: self.module_context.clone(),
            current_this: self.current_this.clone(),
        }
    }

    fn reset_context(&mut self, snapshot: CodegenContextSnapshot) {
        self.module_context = snapshot.module_context;
        self.previous_scoped_variables.clear();
        self.previous_scoped_variables
            .extend(snapshot.previous_scoped_variables);
        self.scoped_variables.clear();
        self.scoped_variables.extend(snapshot.scoped_variables);
        self.local_scope = snapshot.local_scope;
        self.attributes_to_set = snapshot.attributes_to_set;
        self.current_this = snapshot.current_this;

        if let Some(block) = snapshot.current_basic_block {
            self.goto_block_end(block);
        }
    }

    fn track_call_position(
        &mut self,
        file: impl ToString,
        line: usize,
    ) -> (CodegenValue, CodegenValue) {
        if !self
            .module_context
            .extern_module
            .read()
            .unwrap()
            .get_variables()
            .contains_key("current_line")
        {
            return (self.create_null_pointer(), self.create_null_pointer());
        }

        let line_global_variable = self
            .module_context
            .extern_module
            .read()
            .unwrap()
            .get_variables()["current_line"]
            .clone();
        let file_global_variable = self
            .module_context
            .extern_module
            .read()
            .unwrap()
            .get_variables()["current_file"]
            .clone();

        // Capture the previous values so the caller can restore them.
        let uuid = self.get_owning_module_uuid();
        let previous_line_global_value =
            self.load_value(&line_global_variable.variable_value[&uuid]);
        // current_file holds a ptr addrspace(1) value. load_value resolves
        // the pointee type as char (i8), emitting a 1-byte load that reads
        // only the first byte of the 8-byte managed pointer and corrupts
        // @current_file on every restore. Load the full pointer explicitly.
        let previous_file_global_value = CodegenValue::new(
            unsafe {
                llvm_sys_180::core::LLVMBuildLoad2(
                    self.llvm_builder,
                    llvm_sys_180::core::LLVMPointerType(llvm_sys_180::core::LLVMInt8Type(), 0),
                    file_global_variable.variable_value[&uuid].llvm_value,
                    c"".as_ptr(),
                )
            },
            peko_core::types::PekoType::simple_type("cstr"),
        );

        // Write the new values.
        let new_line_value = self.create_constant_int(line as i32);
        self.build_store(&line_global_variable.variable_value[&uuid], &new_line_value);

        let new_file_value = self.create_cstring(file);
        let uuid = self.get_owning_module_uuid();
        self.build_store(&file_global_variable.variable_value[&uuid], &new_file_value);

        (previous_line_global_value, previous_file_global_value)
    }

    fn qualify_value_to_current(&mut self, value: CodegenValue) -> HashMap<String, CodegenValue> {
        HashMap::from([(self.get_owning_module_uuid(), value)])
    }

    fn reset_call_position(&mut self, previous_line: &CodegenValue, previous_file: &CodegenValue) {
        if !self
            .module_context
            .extern_module
            .read()
            .unwrap()
            .get_variables()
            .contains_key("current_line")
        {
            return;
        }

        let line_global_variable = self
            .module_context
            .extern_module
            .read()
            .unwrap()
            .get_variables()["current_line"]
            .clone();
        let file_global_variable = self
            .module_context
            .extern_module
            .read()
            .unwrap()
            .get_variables()["current_file"]
            .clone();

        let uuid = self.get_owning_module_uuid();
        self.build_store(&line_global_variable.variable_value[&uuid], previous_line);
        self.build_store(&file_global_variable.variable_value[&uuid], previous_file);
    }
}
