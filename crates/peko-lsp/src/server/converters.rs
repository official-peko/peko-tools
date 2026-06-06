//! Type converters between the neutral types in [`crate::server::analysis`]
//! and the concrete wire types from `ls-types` (the LSP type library that
//! ships with `tower-lsp-server`).
//!
//! Keeping this layer separate means the analysis engine never has to know
//! about the LSP wire types, and the backend never has to know about Peko's
//! internal representations.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use tower_lsp_server::ls_types::{self as lsp, Uri};

use crate::server::analysis;

// ---------------------------------------------------------------------------
// Positions and ranges
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::Position`] to an LSP `Position`.
pub fn position_to_lsp(p: &analysis::Position) -> lsp::Position {
    lsp::Position {
        line: p.line,
        character: p.character,
    }
}

/// Convert an LSP `Position` to a neutral [`analysis::Position`].
pub fn position_from_lsp(p: lsp::Position) -> analysis::Position {
    analysis::Position {
        line: p.line,
        character: p.character,
    }
}

/// Convert a neutral [`analysis::Range`] to an LSP `Range`.
pub fn range_to_lsp(r: &analysis::Range) -> lsp::Range {
    lsp::Range {
        start: position_to_lsp(&r.start),
        end: position_to_lsp(&r.end),
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::Diagnostic`] to an LSP `Diagnostic`.
pub fn diagnostic_to_lsp(d: &analysis::Diagnostic) -> lsp::Diagnostic {
    lsp::Diagnostic {
        range: range_to_lsp(&d.range),
        severity: Some(severity_to_lsp(&d.severity)),
        code: d
            .code
            .as_ref()
            .map(|c| lsp::NumberOrString::String(c.clone())),
        source: d.source.clone(),
        message: d.message.clone(),
        ..Default::default()
    }
}

fn severity_to_lsp(s: &analysis::DiagnosticSeverity) -> lsp::DiagnosticSeverity {
    match s {
        analysis::DiagnosticSeverity::Error => lsp::DiagnosticSeverity::ERROR,
        analysis::DiagnosticSeverity::Warning => lsp::DiagnosticSeverity::WARNING,
        analysis::DiagnosticSeverity::Information => lsp::DiagnosticSeverity::INFORMATION,
        analysis::DiagnosticSeverity::Hint => lsp::DiagnosticSeverity::HINT,
    }
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::SymbolKind`] to an LSP `SymbolKind`.
pub fn symbol_kind_to_lsp(k: &analysis::SymbolKind) -> lsp::SymbolKind {
    match k {
        analysis::SymbolKind::File => lsp::SymbolKind::FILE,
        analysis::SymbolKind::Module => lsp::SymbolKind::MODULE,
        analysis::SymbolKind::Namespace => lsp::SymbolKind::NAMESPACE,
        analysis::SymbolKind::Class => lsp::SymbolKind::CLASS,
        analysis::SymbolKind::Method => lsp::SymbolKind::METHOD,
        analysis::SymbolKind::Property => lsp::SymbolKind::PROPERTY,
        analysis::SymbolKind::Field => lsp::SymbolKind::FIELD,
        analysis::SymbolKind::Constructor => lsp::SymbolKind::CONSTRUCTOR,
        analysis::SymbolKind::Enum => lsp::SymbolKind::ENUM,
        analysis::SymbolKind::Interface => lsp::SymbolKind::INTERFACE,
        analysis::SymbolKind::Function => lsp::SymbolKind::FUNCTION,
        analysis::SymbolKind::Variable => lsp::SymbolKind::VARIABLE,
        analysis::SymbolKind::Constant => lsp::SymbolKind::CONSTANT,
        analysis::SymbolKind::String => lsp::SymbolKind::STRING,
        analysis::SymbolKind::Number => lsp::SymbolKind::NUMBER,
        analysis::SymbolKind::Boolean => lsp::SymbolKind::BOOLEAN,
        analysis::SymbolKind::Array => lsp::SymbolKind::ARRAY,
        analysis::SymbolKind::Object => lsp::SymbolKind::OBJECT,
        analysis::SymbolKind::Key => lsp::SymbolKind::KEY,
        analysis::SymbolKind::Null => lsp::SymbolKind::NULL,
        analysis::SymbolKind::EnumMember => lsp::SymbolKind::ENUM_MEMBER,
        analysis::SymbolKind::Struct => lsp::SymbolKind::STRUCT,
        analysis::SymbolKind::Event => lsp::SymbolKind::EVENT,
        analysis::SymbolKind::Operator => lsp::SymbolKind::OPERATOR,
        analysis::SymbolKind::TypeParameter => lsp::SymbolKind::TYPE_PARAMETER,
    }
}

/// Recursively convert a tree of neutral symbols into LSP `DocumentSymbol`s.
pub fn document_symbol_to_lsp(s: &analysis::Symbol) -> lsp::DocumentSymbol {
    #[allow(deprecated)]
    lsp::DocumentSymbol {
        name: s.name.clone(),
        detail: s.detail.clone(),
        kind: symbol_kind_to_lsp(&s.kind),
        tags: None,
        deprecated: None,
        range: range_to_lsp(&s.range),
        selection_range: range_to_lsp(&s.selection_range),
        children: if s.children.is_empty() {
            None
        } else {
            Some(s.children.iter().map(document_symbol_to_lsp).collect())
        },
    }
}

// ---------------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::HoverInfo`] to an LSP `Hover`.
pub fn hover_to_lsp(h: &analysis::HoverInfo) -> lsp::Hover {
    lsp::Hover {
        contents: lsp::HoverContents::Markup(lsp::MarkupContent {
            kind: lsp::MarkupKind::Markdown,
            value: h.contents.clone(),
        }),
        range: h.range.as_ref().map(range_to_lsp),
    }
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::CompletionKind`] to an LSP `CompletionItemKind`.
pub fn completion_kind_to_lsp(k: &analysis::CompletionKind) -> lsp::CompletionItemKind {
    match k {
        analysis::CompletionKind::Text => lsp::CompletionItemKind::TEXT,
        analysis::CompletionKind::Method => lsp::CompletionItemKind::METHOD,
        analysis::CompletionKind::Function => lsp::CompletionItemKind::FUNCTION,
        analysis::CompletionKind::Constructor => lsp::CompletionItemKind::CONSTRUCTOR,
        analysis::CompletionKind::Field => lsp::CompletionItemKind::FIELD,
        analysis::CompletionKind::Variable => lsp::CompletionItemKind::VARIABLE,
        analysis::CompletionKind::Class => lsp::CompletionItemKind::CLASS,
        analysis::CompletionKind::Interface => lsp::CompletionItemKind::INTERFACE,
        analysis::CompletionKind::Module => lsp::CompletionItemKind::MODULE,
        analysis::CompletionKind::Property => lsp::CompletionItemKind::PROPERTY,
        analysis::CompletionKind::Unit => lsp::CompletionItemKind::UNIT,
        analysis::CompletionKind::Value => lsp::CompletionItemKind::VALUE,
        analysis::CompletionKind::Enum => lsp::CompletionItemKind::ENUM,
        analysis::CompletionKind::Keyword => lsp::CompletionItemKind::KEYWORD,
        analysis::CompletionKind::Snippet => lsp::CompletionItemKind::SNIPPET,
        analysis::CompletionKind::Color => lsp::CompletionItemKind::COLOR,
        analysis::CompletionKind::File => lsp::CompletionItemKind::FILE,
        analysis::CompletionKind::Reference => lsp::CompletionItemKind::REFERENCE,
        analysis::CompletionKind::Folder => lsp::CompletionItemKind::FOLDER,
        analysis::CompletionKind::EnumMember => lsp::CompletionItemKind::ENUM_MEMBER,
        analysis::CompletionKind::Constant => lsp::CompletionItemKind::CONSTANT,
        analysis::CompletionKind::Struct => lsp::CompletionItemKind::STRUCT,
        analysis::CompletionKind::Event => lsp::CompletionItemKind::EVENT,
        analysis::CompletionKind::Operator => lsp::CompletionItemKind::OPERATOR,
        analysis::CompletionKind::TypeParameter => lsp::CompletionItemKind::TYPE_PARAMETER,
    }
}

/// Convert a neutral [`analysis::CompletionItem`] to an LSP `CompletionItem`.
pub fn completion_item_to_lsp(item: &analysis::CompletionItem) -> lsp::CompletionItem {
    lsp::CompletionItem {
        label: item.label.clone(),
        kind: Some(completion_kind_to_lsp(&item.kind)),
        detail: item.detail.clone(),
        documentation: item.documentation.as_ref().map(|d| {
            lsp::Documentation::MarkupContent(lsp::MarkupContent {
                kind: lsp::MarkupKind::Markdown,
                value: d.clone(),
            })
        }),
        insert_text: item.insert_text.clone(),
        insert_text_format: item.insert_text_format.as_ref().map(|f| match f {
            analysis::InsertTextFormat::PlainText => lsp::InsertTextFormat::PLAIN_TEXT,
            analysis::InsertTextFormat::Snippet => lsp::InsertTextFormat::SNIPPET,
        }),
        sort_text: item.sort_text.clone(),
        command: item.command.as_ref().map(|cmd| lsp::Command {
            title: cmd.title.clone(),
            command: cmd.command.clone(),
            arguments: None,
        }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::Location`] to an LSP `Location`.
pub fn location_to_lsp(loc: &analysis::Location) -> lsp::Location {
    lsp::Location {
        uri: path_to_uri(&loc.file),
        range: range_to_lsp(&loc.range),
    }
}

// ---------------------------------------------------------------------------
// Signature help
// ---------------------------------------------------------------------------

/// Convert a neutral [`analysis::SignatureHelp`] to an LSP `SignatureHelp`.
///
/// Parameter labels are resolved by searching for each `ParameterInfo::label`
/// as a substring of the parent `SignatureInfo::label` and encoding the result
/// as a byte-offset pair `[start, end]`, which is what `ls-types` expects for
/// `SignatureInformation::parameters`.
pub fn signature_help_to_lsp(sh: &analysis::SignatureHelp) -> lsp::SignatureHelp {
    let signatures: Vec<lsp::SignatureInformation> =
        sh.signatures.iter().map(signature_info_to_lsp).collect();

    lsp::SignatureHelp {
        signatures,
        active_signature: sh.active_signature,
        active_parameter: sh.active_parameter,
    }
}

fn signature_info_to_lsp(sig: &analysis::SignatureInfo) -> lsp::SignatureInformation {
    let parameters: Vec<lsp::ParameterInformation> = sig
        .parameters
        .iter()
        .map(|p| parameter_info_to_lsp(p, &sig.label))
        .collect();

    lsp::SignatureInformation {
        label: sig.label.clone(),
        documentation: sig.documentation.as_deref().map(markdown_string),
        parameters: if parameters.is_empty() {
            None
        } else {
            Some(parameters)
        },
        // Per-signature override; the top-level `active_parameter` on
        // `SignatureHelp` is what we actually populate.
        active_parameter: None,
    }
}

fn parameter_info_to_lsp(
    param: &analysis::ParameterInfo,
    sig_label: &str,
) -> lsp::ParameterInformation {
    // Locate the parameter label inside the full signature string and encode
    // it as `[start_byte, end_byte]` so the editor can highlight it. Falls
    // back to a plain string label if the substring is not found.
    let label = match sig_label.find(param.label.as_str()) {
        Some(start) => {
            let end = start + param.label.len();
            lsp::ParameterLabel::LabelOffsets([start as u32, end as u32])
        }
        None => lsp::ParameterLabel::Simple(param.label.clone()),
    };

    lsp::ParameterInformation {
        label,
        documentation: param.documentation.as_deref().map(markdown_string),
    }
}

/// Wrap a string as LSP `MarkupContent` (Markdown).
fn markdown_string(s: &str) -> lsp::Documentation {
    lsp::Documentation::MarkupContent(lsp::MarkupContent {
        kind: lsp::MarkupKind::Markdown,
        value: s.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Path <-> Uri helpers
// ---------------------------------------------------------------------------

/// Convert a filesystem path to an LSP `Uri`.
///
/// Uses [`Uri::from_file_path`], which handles both Unix and Windows paths,
/// including percent-encoding of special characters. If the path is not
/// absolute, this function canonicalizes it first because `from_file_path`
/// rejects relative paths and may also return `None` for non-existent
/// paths. If all of that fails, falls back to a `file:///unknown` sentinel
/// URI so the caller never has to deal with `Option` / `Result` plumbing.
pub fn path_to_uri(path: &Path) -> Uri {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Uri::from_file_path(&abs)
        .unwrap_or_else(|| Uri::from_str("file:///unknown").expect("fallback URI is valid"))
}

/// Convert an LSP `Uri` to a filesystem `PathBuf`. If the URI is not a valid
/// `file:` URL, falls back to interpreting the URI's path component as a
/// filesystem path.
pub fn uri_to_path(uri: &Uri) -> PathBuf {
    uri.to_file_path()
        .map(|p| p.into_owned())
        .unwrap_or_else(|| PathBuf::from(uri.path().as_str()))
}
