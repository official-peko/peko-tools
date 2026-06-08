//! # Peko LLVM
//!
//! `peko_llvm` extends the Peko Core by adding compilation to LLVM IR and
//! binary outputs.
//!
//! The crate is structured around three modules:
//!
//! - [`codegen`]: translates `peko_core` AST nodes to LLVM IR through the
//!   `PekoCodegenContext` and its layered builder traits.
//! - [`linker`]: wraps the bundled `lld` static archive (built from the
//!   `rust_lld/lldentry.cc` shim) and dispatches link invocations to the
//!   correct driver for the target operating system.
//! - `llvm_sys_180`: re-exported so consumers can construct their own
//!   `LLVMTypeRef` / `LLVMValueRef` values when working with the codegen
//!   context directly.

#![allow(clippy::too_many_arguments)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::arc_with_non_send_sync)]

pub extern crate llvm_sys_180;

pub mod codegen;
pub mod linker;
