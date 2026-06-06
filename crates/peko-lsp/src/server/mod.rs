//! LSP server glue.
//!
//! The crate is split into two halves:
//!
//! - [`analysis`] defines the compiler-neutral `AnalysisEngine` trait and the
//!   neutral types (`Position`, `Range`, `Diagnostic`, `Symbol`, etc.) that the
//!   trait exposes. The analyzer implementation in `crate::analyzer` knows
//!   about Peko; this module does not.
//! - [`backend`] implements `tower_lsp::LanguageServer`, wiring incoming JSON-RPC
//!   requests to the analysis engine via [`converters`] and tracking open
//!   buffers in [`documents`].

pub mod analysis;
pub mod backend;
pub mod converters;
pub mod documents;
