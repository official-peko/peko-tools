//! # Peko Core
//!
//! Core compiler infrastructure for the [Pekoscript](https://pekoui.com) language.
//!
//! This crate implements the front end and static analyzer of the Pekoscript
//! toolchain: lexing, parsing, AST construction, type representation, diagnostic
//! collection, and a simulator (type checker) that walks the AST without
//! generating code. Code generation lives in the separate `peko_llvm` crate;
//! command-line tooling lives in the separate `peko` CLI.
//!
//! ## Pipeline
//!
//! ```text
//! source >> lexer >> parser >> AST >> simulator > diagnostics
//! ```
//!
//! ## Error handling
//!
//! Two error channels run in parallel:
//!
//! * [`PekoError`]: environmental failures from the tooling itself (I/O,
//!   non-UTF-8 paths). Propagated via `Result`.
//! * [`diagnostics::PekoDiagnostic`]: semantic findings about user source
//!   code (syntax errors, type mismatches, unresolved references). Collected
//!   into a [`diagnostics::DiagnosticList`] without halting the compiler.
//!
//! ## Example
//!
//! ```no_run
//! use peko_core::{lexer, parser, PekoResult};
//! use std::path::PathBuf;
//!
//! fn parse_file(path: PathBuf) -> PekoResult<()> {
//!     let source = peko_core::error::read_to_string(&path)?;
//!     // ...feed `source` into the lexer, then the parser...
//!     Ok(())
//! }
//! ```
#![allow(clippy::too_many_arguments)]

pub mod asts;
pub mod config;
pub mod diagnostics;
pub mod error;
pub mod execution;
pub mod ffi;
pub mod lexer;
pub mod packages;
pub mod parser;
pub mod simulator;
pub mod target;
pub mod types;

pub use error::{PekoError, PekoResult};

use derive_new::new;
use std::path::PathBuf;

/// Metadata describing an external (package-managed) Pekoscript module.
///
/// Constructed by the package index when scanning the registry source cache,
/// or directly by the simulator when resolving a module relative to the current
/// source file.
#[derive(Clone, Debug, new)]
pub struct ExternalModuleInfo {
    /// Module identifier as it appears in `import` statements.
    pub module_name: String,
    /// Free-form human-readable description from the module's manifest.
    pub description: String,
    /// One entry per installed version of the module.
    pub versions: Vec<ExternalModuleVersion>,
}

/// One installed version of an external module.
#[derive(Clone, Debug, new)]
pub struct ExternalModuleVersion {
    /// The version string from the module's manifest.
    pub version: String,
    /// The directory that the module's submodules and entry resolve within.
    pub source_root: PathBuf,
    /// The entry file name within `source_root`.
    pub entry_file: String,
}
