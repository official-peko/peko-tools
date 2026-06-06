//! Analysis-engine interface.
//!
//! This module defines the compiler-neutral types and the [`AnalysisEngine`]
//! trait that the LSP backend calls into for every language-intelligence
//! feature. The actual implementation lives in [`crate::analyzer`] and is
//! built on top of `peko_core`.

use std::path::Path;

use crate::analyzer::PekoAnalyzer;

// ---------------------------------------------------------------------------
// Positions and ranges
// ---------------------------------------------------------------------------

/// A source range expressed in 0-based line/character offsets (LSP convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A 0-based line/character offset (LSP convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

/// Severity level for a diagnostic surfaced to the editor.
#[derive(Debug, Clone)]
#[allow(unused)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

/// A single diagnostic (error, warning, or informational note) to be shown
/// in the editor at `range`.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: DiagnosticSeverity,
    /// Optional code, surfaced as a clickable error code in some editors.
    pub code: Option<String>,
    pub message: String,
    /// Optional source label shown next to the diagnostic.
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Symbol category, used to pick the appropriate editor icon in outlines and
/// breadcrumbs.
#[derive(Debug, Clone)]
#[allow(unused)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

/// A node in the document-symbol tree used for outline and breadcrumb views.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    /// The range that should be selected when the user navigates to this
    /// symbol (usually just the name token, not the whole declaration).
    pub selection_range: Range,
    pub detail: Option<String>,
    pub children: Vec<Symbol>,
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

/// Content returned from a hover request.
#[derive(Debug, Clone)]
pub struct HoverInfo {
    /// Markdown-formatted hover content.
    pub contents: String,
    /// If `Some`, the editor highlights this range while the hover is shown.
    pub range: Option<Range>,
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

/// Completion-item category. This picks the icon shown in the completion popup.
#[derive(Debug, Clone)]
#[allow(unused)]
pub enum CompletionKind {
    Text,
    Method,
    Function,
    Constructor,
    Field,
    Variable,
    Class,
    Interface,
    Module,
    Property,
    Unit,
    Value,
    Enum,
    Keyword,
    Snippet,
    Color,
    File,
    Reference,
    Folder,
    EnumMember,
    Constant,
    Struct,
    Event,
    Operator,
    TypeParameter,
}

/// Whether `insert_text` should be treated as plain text or as an LSP snippet
/// (tab-stops, placeholders, etc.).
#[derive(Debug, Clone)]
#[allow(unused)]
pub enum InsertTextFormat {
    PlainText,
    Snippet,
}

/// Editor command that the client should run after accepting a completion item.
#[derive(Debug, Clone)]
pub struct Command {
    pub title: String,
    pub command: String,
}

/// A single completion candidate.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    /// Markdown documentation shown in the completion popup.
    pub documentation: Option<String>,
    /// Text inserted when the item is accepted. Falls back to `label` if `None`.
    pub insert_text: Option<String>,
    pub sort_text: Option<String>,
    pub insert_text_format: Option<InsertTextFormat>,
    pub command: Option<Command>,
}

// ---------------------------------------------------------------------------
// Go-to / references
// ---------------------------------------------------------------------------

/// A pointer to a span of source in a specific file. Returned from go-to
/// definition and find-references.
#[derive(Debug, Clone)]
pub struct Location {
    pub file: std::path::PathBuf,
    pub range: Range,
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

/// One parameter inside a [`SignatureInfo`].
#[derive(Debug, Clone)]
pub struct ParameterInfo {
    /// Display text for this parameter, e.g. `"count: int"`. Must appear as a
    /// substring of the containing `SignatureInfo::label` so the editor can
    /// highlight it.
    pub label: String,
    /// Optional markdown documentation for this specific parameter.
    pub documentation: Option<String>,
}

/// A single callable signature shown in the signature-help popup.
#[derive(Debug, Clone)]
pub struct SignatureInfo {
    /// The full human-readable signature, e.g. `"fn push(value: T)"`.
    pub label: String,
    /// Optional markdown documentation for the whole signature.
    pub documentation: Option<String>,
    /// Ordered list of parameters. May be empty if the callable takes none.
    pub parameters: Vec<ParameterInfo>,
}

/// Result of a signature-help request.
#[derive(Debug, Clone)]
pub struct SignatureHelp {
    /// All overloads of the callable at the cursor position.
    pub signatures: Vec<SignatureInfo>,
    /// Index into `signatures` of the overload that best matches the current
    /// argument list. `None` if the engine cannot determine which overload is
    /// active.
    pub active_signature: Option<u32>,
    /// Index into the active signature's `parameters` of the parameter the
    /// cursor is currently inside. `None` if the cursor is not inside any
    /// parameter position.
    pub active_parameter: Option<u32>,
}

// ---------------------------------------------------------------------------
// Analysis engine trait
// ---------------------------------------------------------------------------

/// The interface the LSP backend calls into for all language-intelligence
/// features.
///
/// Implementations are stored as `Box<dyn AnalysisEngine>` inside
/// [`AnalysisHost`] and are accessed from async LSP handlers under an
/// `Arc<tokio::sync::RwLock<...>>`. Heavy work (parsing, simulation) is
/// expected to be wrapped in `tokio::task::spawn_blocking` by the backend.
pub trait AnalysisEngine: Send + Sync + 'static {
    // ------------------------------------------------------------------
    // File lifecycle
    // ------------------------------------------------------------------

    /// Tell the engine which directory is the root of the current project.
    /// Called once during LSP initialization.
    fn update_project_root(&mut self, path: &Path);

    /// Called when a file is opened or its content changes. The engine should
    /// store the text, invalidate caches, and kick off whatever background
    /// indexing it needs.
    fn update_file(&mut self, path: &Path, text: &str);

    /// Called when a file is closed and no longer tracked by the editor.
    fn close_file(&mut self, path: &Path);

    // ------------------------------------------------------------------
    // Diagnostics
    // ------------------------------------------------------------------

    /// Return all diagnostics for the given file. Called after every
    /// [`AnalysisEngine::update_file`].
    fn diagnostics(&self, path: &Path) -> Vec<Diagnostic>;

    // ------------------------------------------------------------------
    // Document symbols
    // ------------------------------------------------------------------

    /// Return a tree of symbols defined in the file, used for the outline and
    /// breadcrumb views.
    fn document_symbols(&self, path: &Path) -> Vec<Symbol>;

    // ------------------------------------------------------------------
    // Hover
    // ------------------------------------------------------------------

    /// Return hover info for the token at `position`, or `None` if there is
    /// nothing interesting at that location.
    fn hover(&self, path: &Path, position: &Position) -> Option<HoverInfo>;

    // ------------------------------------------------------------------
    // Completions
    // ------------------------------------------------------------------

    /// Return completion candidates at `position`.
    fn completions(&self, path: &Path, position: &Position) -> Vec<CompletionItem>;

    // ------------------------------------------------------------------
    // Go-to definition
    // ------------------------------------------------------------------

    /// Return one or more locations where the symbol at `position` is defined.
    fn goto_definition(&self, path: &Path, position: &Position) -> Vec<Location>;

    // ------------------------------------------------------------------
    // Signature help
    // ------------------------------------------------------------------

    /// Return signature help for a function call surrounding `position`, or
    /// `None` if the cursor is not inside a call expression.
    fn signature_help(&self, path: &Path, position: &Position) -> Option<SignatureHelp>;

    // ------------------------------------------------------------------
    // Formatting (optional)
    // ------------------------------------------------------------------

    /// Return the fully-formatted source text for the file, or `None` if the
    /// engine does not support formatting.
    fn format(&self, path: &Path, text: &str) -> Option<String>;
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// Owns the analysis engine. The backend holds an
/// `Arc<tokio::sync::RwLock<AnalysisHost>>` to share it across async handlers.
pub struct AnalysisHost {
    pub engine: Box<dyn AnalysisEngine>,
}

impl AnalysisHost {
    /// Construct the default host. Returns `None` if the analyzer cannot
    /// initialize (typically because the `PEKO_ROOT_PATH` environment
    /// variable is unset or points at a non-existent directory).
    pub fn new() -> Option<Self> {
        Some(Self {
            engine: Box::new(PekoAnalyzer::new()?),
        })
    }
}
