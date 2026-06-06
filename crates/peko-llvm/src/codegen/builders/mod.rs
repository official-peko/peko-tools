//! Layered trait surface for `PekoCodegenContext`.
//!
//! Each submodule in `builders/` declares one trait that `PekoCodegenContext`
//! implements.
//!
//! # Layer table
//!
//! Higher layers may call into lower layers. Same-layer calls are allowed
//! when convenient. Lower-to-higher calls indicate a misplaced method.
//!
//! | Layer | Trait                    | What lives here                                   |
//! |-------|--------------------------|---------------------------------------------------|
//! | 0     | `LlvmTypeBuilder`        | Building `LLVMTypeRef`s from `PekoType`           |
//! | 0     | `LlvmConstantBuilder`    | Building constant `CodegenValue`s                 |
//! | 1     | `LlvmInstructionBuilder` | Basic blocks, store/load, branches, structs       |
//! | 1     | `LlvmMemoryBuilder`      | Stack alloca, GEP, vtable access, allocation       |
//! | 2     | `LlvmArithmeticBuilder`  | Int / float / bool / string / pointer comparisons |
//! | 2     | `FunctionBuilder`        | Function creation and direct calls                |
//! | 2     | `GlobalBuilder`          | Global variables and the per-module init function |
//! | 3     | `HighLevelCodegen`       | `box_value_to_type` and `allocate_class`          |
//! | 3     | `ScopeManager`           | Context snapshot, restore, call-position tracking |
//! | 4     | `ModuleManager`          | Module import / linking / binary + IR output      |
//!
//! The layering rule is internal discipline, not access control: higher layers
//! may call into lower layers, same-layer calls are fine, and lower-to-higher calls
//! typically indicate a misplaced method.
//!
//! A handful of methods cross layers downward when they need a single
//! out-of-layer call but otherwise topically belong to a lower trait:
//! short_circuit_boolean_operation (in LlvmArithmeticBuilder) calls
//! HighLevelCodegen::box_value_to_type; and init_module_globals (in
//! GlobalBuilder) calls HighLevelCodegen::box_value_to_type and
//! ScopeManager call-tracking for example. Each is documented at its
//! definition site.
//!
//! # Usage
//!
//! Each codegen submodule pulls the whole surface into scope through the
//! prelude:
//!
//! ```ignore
//! use crate::codegen::builders::prelude::*;
//! ```

pub mod llvm_arithmetic;
pub mod llvm_constants;
pub mod llvm_instructions;
pub mod llvm_memory;
pub mod llvm_types;

pub mod functions;
pub mod globals;

pub mod high_level;
pub mod modules;
pub mod scope;

/// Glob-import this at the top of any file that calls builder methods
/// on a `PekoCodegenContext`:
///
/// ```ignore
/// use peko_llvm::codegen::builders::prelude::*;
/// ```
pub mod prelude {
    pub use super::functions::FunctionBuilder;
    pub use super::globals::GlobalBuilder;
    pub use super::high_level::HighLevelCodegen;
    pub use super::llvm_arithmetic::LlvmArithmeticBuilder;
    pub use super::llvm_constants::LlvmConstantBuilder;
    pub use super::llvm_instructions::LlvmInstructionBuilder;
    pub use super::llvm_memory::LlvmMemoryBuilder;
    pub use super::llvm_types::LlvmTypeBuilder;
    pub use super::modules::ModuleManager;
    pub use super::scope::ScopeManager;
}
