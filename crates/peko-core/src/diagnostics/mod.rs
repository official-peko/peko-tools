//! Diagnostic (error and warning) collection for `peko_core`.
//!
//! Diagnostics represent *semantic* findings about user source code (syntax
//! errors, type mismatches, unresolved references, and so on). They are
//! accumulated into a [`DiagnosticList`] during lexing, parsing, and
//! simulation rather than propagated via `Result`, so that an entire source
//! file can be checked end-to-end and every problem reported in one pass.
//!
//! Environmental failures of the tooling itself (failed file reads, malformed
//! JSON, non-UTF-8 paths) flow through [`crate::PekoError`] instead.

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::PathBuf;

use derive_new::new;

use crate::asts::data_structures::PositionData;

/// Severity of a [`PekoDiagnostic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticType {
    /// A non-fatal finding. Compilation can proceed.
    Warning,
    /// A fatal finding. Compilation should not proceed past this stage.
    Error,
}

impl fmt::Display for DiagnosticType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DiagnosticType::Warning => "warning",
            DiagnosticType::Error => "error",
        })
    }
}

/// A single semantic finding about a span of user source code.
///
/// A diagnostic carries the source positions of the offending span, a
/// human-readable message, a severity, and the path of the file in which
/// it occurred.
#[derive(Clone, Debug, new)]
pub struct PekoDiagnostic {
    /// Position (inclusive) where the offending span begins.
    pub start: PositionData,
    /// Position (inclusive) where the offending span ends.
    pub end: PositionData,
    /// Human-readable description of the problem.
    pub message: String,
    /// Severity of the finding.
    pub diagnostic_type: DiagnosticType,
    /// Path of the source file containing the span.
    pub file: PathBuf,
}

impl fmt::Display for PekoDiagnostic {
    /// Renders the diagnostic as `<file>:<line>:<column>: <severity>: <message>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}: {}",
            self.file.display(),
            self.start.line,
            self.start.column,
            self.diagnostic_type,
            self.message,
        )
    }
}

/// An ordered collection of diagnostics with running error and warning counts.
///
/// `DiagnosticList` is the primary output channel for the parser and simulator.
/// Diagnostics are appended in the order they are discovered; counts are
/// updated incrementally so callers can cheaply ask whether anything has gone
/// wrong without scanning the full list.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticList {
    diagnostics: Vec<PekoDiagnostic>,
    error_count: usize,
    warning_count: usize,
}

impl DiagnosticList {
    /// Creates an empty `DiagnosticList`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the list of diagnostics in the order they were reported.
    #[must_use]
    pub fn get_diagnostics(&self) -> &[PekoDiagnostic] {
        &self.diagnostics
    }

    /// Returns the running count of diagnostics with severity
    /// [`DiagnosticType::Error`].
    #[must_use]
    pub fn get_error_count(&self) -> usize {
        self.error_count
    }

    /// Returns the running count of diagnostics with severity
    /// [`DiagnosticType::Warning`].
    #[must_use]
    pub fn get_warning_count(&self) -> usize {
        self.warning_count
    }

    /// Returns `true` if no diagnostics have been reported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Returns the total number of diagnostics, regardless of severity.
    #[must_use]
    pub fn len(&self) -> usize {
        self.diagnostics.len()
    }

    /// Returns `true` if at least one [`DiagnosticType::Error`] has been
    /// reported.
    ///
    /// Useful for callers that want to halt the pipeline between stages once
    /// any error has been seen.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.error_count > 0
    }

    /// Borrows an iterator over the contained diagnostics.
    pub fn iter(&self) -> std::slice::Iter<'_, PekoDiagnostic> {
        self.diagnostics.iter()
    }

    /// Appends all diagnostics from `other` to this list, merging the counts.
    ///
    /// Consumes `other` to avoid cloning the contained diagnostics; callers
    /// that need to keep a copy should clone before merging.
    pub fn extend(&mut self, other: DiagnosticList) {
        self.error_count += other.error_count;
        self.warning_count += other.warning_count;
        self.diagnostics.extend(other.diagnostics);
    }

    /// Reports a single diagnostic, updating the appropriate running count.
    pub fn report_diagnostic(&mut self, diagnostic: PekoDiagnostic) {
        match diagnostic.diagnostic_type {
            DiagnosticType::Error => self.error_count += 1,
            DiagnosticType::Warning => self.warning_count += 1,
        }
        self.diagnostics.push(diagnostic);
    }
}

impl<'a> IntoIterator for &'a DiagnosticList {
    type Item = &'a PekoDiagnostic;
    type IntoIter = std::slice::Iter<'a, PekoDiagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
