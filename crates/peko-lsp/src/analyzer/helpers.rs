//! Helpers shared between the analyzer entry points.
//!
//! Three responsibilities live here:
//!
//! - The `char_is_*` macros used by the cursor-context heuristics in
//!   [`crate::analyzer`].
//! - [`parse_peko_source`], which wraps `peko_core::parser::PekoParser` and
//!   prepends the default preloaded imports.
//! - The scope-tree walkers that turn a `peko_core::simulator` `Scope` into a
//!   flat list of [`Symbol`]s for the document-symbol / outline LSP feature.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, RwLock};

use peko_core::{
    asts::{
        PekoAST,
        data_structures::{PositionData, PositionedValue, UnpackItem},
        statements::ImportStatementAST,
    },
    diagnostics::DiagnosticList,
    lexer::TokenList,
    parser::PekoParser,
    simulator::data_structures::{Scope, ScopeSymbol},
};

use crate::server::analysis::{Position, Range, Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Character-classification macros
// ---------------------------------------------------------------------------

/// Match ASCII whitespace as the analyzer considers it.
#[macro_export]
macro_rules! char_is_whitespace {
    ($ch:expr) => {
        ($ch == ' ' || $ch == '\t' || $ch == '\n' || $ch == '\r')
    };
}

/// Match bytes that are legal inside a Peko identifier.
#[macro_export]
macro_rules! char_is_peko_id_eligible {
    ($ch:expr) => {
        (($ch >= 'a' && $ch <= 'z')
            || ($ch >= 'A' && $ch <= 'Z')
            || ($ch >= '0' && $ch <= '9')
            || $ch == '_'
            || $ch == '$')
    };
}

/// Match bytes that may appear inside a type expression while doing forward
/// or backward cursor-context search. `$fwd` is `true` when scanning forward,
/// which flips the direction in which `<` and `>` are accepted.
#[macro_export]
macro_rules! char_is_peko_type_eligible {
    ($ch:expr, $fwd:expr) => {
        (char_is_peko_id_eligible!($ch)
            || $ch == ' '
            || $ch == '\t'
            || $ch == '\n'
            || $ch == ','
            || ($ch == '>' && !$fwd)
            || ($ch == '<' && $fwd)
            || $ch == '['
            || $ch == ']'
            || $ch == '('
            || $ch == ')'
            || $ch == '&'
            || $ch == '?')
    };
}

// ---------------------------------------------------------------------------
// Default preloaded imports
// ---------------------------------------------------------------------------

/// Build the four `import` AST nodes that the analyzer silently injects at
/// the top of every parsed file: `runtime`, `standard` (with `*`-unpack),
/// `console`, and `pekoui as ui`. The same set is replayed at startup to
/// preload the simulator with these modules.
pub(crate) fn default_preloaded_imports() -> Vec<PekoAST> {
    vec![
        PekoAST::ImportStatement(ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            vec![PositionedValue::create_no_position("runtime".to_string())],
            Some(PositionedValue::create_no_position("Runtime".to_string())),
            Vec::new(),
            Option::None,
        )),
        PekoAST::ImportStatement(ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            vec![PositionedValue::create_no_position("standard".to_string())],
            None,
            vec![UnpackItem::All],
            Option::None,
        )),
        PekoAST::ImportStatement(ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            vec![PositionedValue::create_no_position("console".to_string())],
            None,
            Vec::new(),
            Option::None,
        )),
        PekoAST::ImportStatement(ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            vec![PositionedValue::create_no_position("pekoui".to_string())],
            Some(PositionedValue::create_no_position("ui".to_string())),
            Vec::new(),
            Option::None,
        )),
    ]
}

// ---------------------------------------------------------------------------
// Parsing entry point
// ---------------------------------------------------------------------------

/// Parse a Peko source file, prepending the default preloaded imports so the
/// `runtime`, `standard`, `console`, and `pekoui` modules are in scope. The
/// returned [`DiagnosticList`] contains only parser-side diagnostics; the
/// caller still runs the simulator to collect type-checker diagnostics.
pub fn parse_peko_source(file: &Path, source: String) -> (Vec<PekoAST>, DiagnosticList) {
    let mut parser = PekoParser::new(TokenList::from_source(&source, file), file);

    let mut parsed_asts = default_preloaded_imports();

    while !parser.tokens.finished() {
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }

        parsed_asts.push(parser.parse());

        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
    }

    (parsed_asts, parser.get_diagnostics().clone())
}

// ---------------------------------------------------------------------------
// Position translation
// ---------------------------------------------------------------------------

/// Convert a peko_core `PositionData` (1-based line, 0-based byte column) to
/// the 0-based LSP `Position` the editor expects.
pub fn create_position(peko_position: &PositionData) -> Position {
    Position {
        line: (peko_position.line - 1) as u32,
        character: peko_position.column as u32,
    }
}

// ---------------------------------------------------------------------------
// Scope -> Symbol conversion
// ---------------------------------------------------------------------------

/// Convert a leaf [`ScopeSymbol`] into a [`Symbol`] suitable for the
/// document-symbol tree. Returns `None` for symbols that should not appear in
/// the outline.
pub fn document_symbol_from_scope_symbol(scope_symbol: &ScopeSymbol) -> Option<Symbol> {
    if scope_symbol.get_kind() != "variable"
        || scope_symbol.get_kind() != "attribute"
        || scope_symbol.get_name() == "this"
    {
        return None;
    }

    let kind = match scope_symbol.get_kind() {
        "variable" => SymbolKind::Variable,
        "attribute" => SymbolKind::Field,
        _ => return None,
    };

    Some(Symbol {
        name: scope_symbol.get_name(),
        kind,
        range: Range {
            start: create_position(&scope_symbol.get_start()),
            end: create_position(&scope_symbol.get_end()),
        },
        selection_range: Range {
            start: create_position(&scope_symbol.get_start()),
            end: create_position(&scope_symbol.get_end()),
        },
        detail: Some(scope_symbol.as_variable().unwrap().value_type.to_string()),
        children: Vec::new(),
    })
}

/// Walk a `Scope` and produce a flat list of `Symbol`s describing every
/// declaration inside the scope that originates from the same file. Recurses
/// into nested function, class, and module scopes.
pub fn document_symbols_from_scope(scope: Arc<RwLock<Scope>>, from_class: bool) -> Vec<Symbol> {
    let is_function_scope = scope.read().unwrap().scope_name.starts_with("function-");
    let is_class_scope = scope.read().unwrap().scope_name.starts_with("class-");

    let scope_file = scope.read().unwrap().start.file.clone();
    let mut scope_children = Vec::new();

    for (_, child_symbol) in &scope.read().unwrap().symbols {
        if child_symbol.get_start().file != scope_file {
            continue;
        }

        if let Some(symbol) = document_symbol_from_scope_symbol(child_symbol) {
            scope_children.push(symbol);
        }
    }

    if is_function_scope {
        let function_name = scope.read().unwrap().scope_name[9..].to_string();

        let symbol_details = scope
            .read()
            .unwrap()
            .symbols
            .iter()
            .find(|(symbol_name, _)| symbol_name == &&function_name)
            .map(|(_, symbol)| {
                let mut signature = String::from("fn ");

                let symbol_function = symbol.as_function().unwrap();
                if symbol_function.generic {
                    signature.push('<');
                    let generics: Vec<&String> =
                        symbol_function.generic_type_names.iter().collect();
                    signature.push_str(
                        &generics
                            .iter()
                            .map(|g| g.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    signature.push('>');
                }

                signature.push('(');
                let args: Vec<String> = symbol_function
                    .arguments
                    .iter()
                    .map(|(_, (_, argument_type))| argument_type.to_string())
                    .collect();
                signature.push_str(&args.join(", "));
                signature.push_str(") => ");
                signature.push_str(&symbol_function.return_type.to_string());

                signature
            });

        return vec![Symbol {
            name: function_name,
            kind: if from_class {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            },
            range: Range {
                start: create_position(&scope.read().unwrap().start),
                end: create_position(&scope.read().unwrap().end),
            },
            selection_range: Range {
                start: create_position(&scope.read().unwrap().start),
                end: create_position(&scope.read().unwrap().end),
            },
            detail: symbol_details,
            children: scope_children,
        }];
    }

    for subscope in &scope.read().unwrap().scopes {
        if subscope.read().unwrap().scope_name.is_empty()
            || subscope.read().unwrap().start.file != scope_file
        {
            continue;
        }

        let scope_name = subscope.read().unwrap().scope_name.clone();
        let (name, kind) = match scope_name {
            scope_name if scope_name.starts_with("function-") => {
                scope_children
                    .push(document_symbols_from_scope(subscope.clone(), is_class_scope)[0].clone());
                continue;
            }
            scope_name if scope_name.starts_with("class-") => {
                (scope_name[6..].to_string(), SymbolKind::Class)
            }
            _ => (scope_name, SymbolKind::Module),
        };

        scope_children.push(Symbol {
            name,
            kind,
            range: Range {
                start: create_position(&subscope.read().unwrap().start),
                end: create_position(&subscope.read().unwrap().end),
            },
            selection_range: Range {
                start: create_position(&subscope.read().unwrap().start),
                end: create_position(&subscope.read().unwrap().end),
            },
            detail: None,
            children: document_symbols_from_scope(subscope.clone(), is_class_scope),
        });
    }

    scope_children
}

// ---------------------------------------------------------------------------
// Ad-hoc debug logging
// ---------------------------------------------------------------------------

/// Append a line to a log file. Kept around for ad-hoc debugging when
/// `tracing` is not enough
#[allow(dead_code)]
pub(crate) fn print_to_log(message: impl ToString, logfile: impl AsRef<Path>) {
    let mut file = OpenOptions::new()
        .append(true)
        .open(logfile.as_ref())
        .unwrap();

    writeln!(file, "{}", message.to_string()).unwrap();
}
