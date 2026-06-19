//! Peko analyzer: the [`AnalysisEngine`] implementation backed by
//! `peko_core`.
//!
//! Holds the set of currently-tracked source files plus enough simulator
//! state to be able to re-run analysis on any one of them on demand. The
//! analyzer is deliberately stateless beyond the open-file set: every LSP
//! request (`hover`, `completions`, `diagnostics`, ...) reparses and
//! re-simulates the requested file from scratch.

mod document;
mod helpers;

use std::{
    cmp,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use document::PekoDocument;
use helpers::create_position;
use peko_core::{
    asts::data_structures::PositionData,
    diagnostics::{DiagnosticType, PekoDiagnostic},
    packages::PekoPackageIndex,
    simulator::{
        PekoValueSimulator,
        context::PekoSimulatorContext,
        data_structures::{ScopeSymbol, SimulatorModule},
    },
    target::PekoTarget,
};

use crate::{
    char_is_peko_id_eligible, char_is_peko_type_eligible, char_is_whitespace,
    server::analysis::{
        AnalysisEngine, Command, CompletionItem, CompletionKind, Diagnostic, HoverInfo,
        InsertTextFormat, Location, ParameterInfo, Position, Range, SignatureHelp, SignatureInfo,
        Symbol,
    },
};

// ---------------------------------------------------------------------------
// Analyzer state
// ---------------------------------------------------------------------------

/// The Peko `AnalysisEngine` implementation. Holds the path of the user's
/// Peko installation, the optional project root, the preloaded module set,
/// and the open-file table.
pub struct PekoAnalyzer {
    preloaded_modules: HashMap<String, Arc<RwLock<SimulatorModule>>>,
    peko_root: PathBuf,
    project_root: Option<PathBuf>,
    target: PekoTarget,

    tracked_files: HashMap<PathBuf, PekoDocument>,
}

/// Bundle of (diagnostics, simulator context) returned from running one
/// document through parse + simulate.
struct SimulationResult {
    diagnostics: Vec<PekoDiagnostic>,
    context: PekoSimulatorContext,
}

/// Result of the cursor-context search that determines what kind of symbols
/// should be offered at a given position. Each branch describes a different
/// completion-flavored intent the user might have.
enum SymbolSearchResult {
    /// Cursor is inside a `[...]` visibility list: offer visibility keywords.
    Visibilities,
    /// Cursor is after `.` or `::`, accessing a known scope.
    Access(Vec<ScopeSymbol>),
    /// Cursor is in a type position: offer types only.
    Types(Vec<ScopeSymbol>),
    /// Cursor is somewhere general: offer every in-scope symbol.
    Symbols(Vec<ScopeSymbol>),
    /// File not tracked, or no useful context.
    None,
}

impl PekoAnalyzer {
    /// Construct a new analyzer. Returns `None` if the `PEKO_ROOT_PATH`
    /// environment variable is unset or does not point at an existing
    /// directory; the LSP cannot function without the standard library, so
    /// callers should treat this as a fatal startup error.
    pub fn new() -> Option<Self> {
        let peko_root = match std::env::var("PEKO_ROOT_PATH") {
            Ok(path) => {
                let path = PathBuf::from(path);
                if !path.exists() || !path.is_dir() {
                    return None;
                }
                path
            }
            Err(_) => return None,
        };

        let mut analyzer = Self {
            preloaded_modules: HashMap::new(),
            peko_root,
            project_root: None,
            target: PekoTarget::default(),
            tracked_files: HashMap::new(),
        };

        analyzer.load_required_packages(analyzer.target);

        Some(analyzer)
    }

    /// Walk upward from `project_folder` looking for a directory that
    /// contains a `.peko` subdirectory. That directory is the project root,
    /// matching the CLI compilation root, which is the parent of `.peko/`.
    /// The search climbs up to five parents.
    pub fn set_project_folder(&mut self, mut project_folder: PathBuf) {
        let mut search_limit = 5;
        while search_limit > 0
            && !project_folder.join(".peko").is_dir()
            && let Some(parent) = project_folder.parent()
        {
            project_folder = parent.to_path_buf();
            search_limit -= 1;
        }

        self.project_root = if project_folder.join(".peko").is_dir() {
            Some(project_folder)
        } else {
            None
        };
    }

    /// Look up a tracked document by path. Used as the early-exit guard in
    /// every `AnalysisEngine` method that needs source text.
    fn doc(&self, path: &Path) -> Option<&PekoDocument> {
        self.tracked_files.get(path)
    }

    /// Preload the default modules (`runtime`, `standard`, `console`,
    /// `pekoui`) once at startup, into `preloaded_modules`, so every
    /// per-request simulator context can copy them in cheaply.
    fn load_required_packages(&mut self, target: PekoTarget) {
        let asts = helpers::default_preloaded_imports();

        let mut simulator_context = PekoSimulatorContext::new(
            target,
            std::env::current_dir().unwrap_or_default(),
            PositionData::default(),
            PathBuf::new(),
        );

        let package_indexer = PekoPackageIndex::new(&self.peko_root, Option::<&Path>::None);
        simulator_context.external_modules = package_indexer
            .map(|idx| idx.get_external_modules())
            .unwrap_or_default();

        simulator_context.windowsgui = !target.console;

        for ast in asts {
            ast.simulate(&mut simulator_context);
        }

        simulator_context
            .module_context
            .top_level_modules
            .shift_remove("main");

        self.preloaded_modules
            .extend(simulator_context.module_context.top_level_modules);
    }

    /// Build a fresh simulator context for `file`. Copies the preloaded
    /// modules in and re-indexes any project-local packages.
    fn create_simulator_context(
        &self,
        file: PathBuf,
        file_end_position: PositionData,
    ) -> PekoSimulatorContext {
        let mut simulator_context = PekoSimulatorContext::new(
            self.target,
            file.clone(),
            file_end_position,
            self.project_root.clone().unwrap_or(file),
        );

        // The package index appends `Packages` to each input it is given,
        // so the project-local directory passed here is `<project>/.peko`
        // and the index reads `<project>/.peko/Packages`. The argument is
        // passed only when that directory exists, since the index errors on
        // a missing path.
        let local_packages = self
            .project_root
            .as_ref()
            .map(|root| root.join(".peko"))
            .filter(|dot_peko| dot_peko.join("Packages").is_dir());

        let package_indexer = PekoPackageIndex::new(&self.peko_root, local_packages.as_deref());
        simulator_context.external_modules = package_indexer
            .map(|idx| idx.get_external_modules())
            .unwrap_or_default();

        // Copy the preloaded modules in, but keep the extern module
        // request-local. The resolver reads top_level_modules["extern"]
        // while declarations write module_context.extern_module, and the
        // two must stay the same object. Pulling the preloaded "extern"
        // straight into top_level_modules would replace the fresh one and
        // break that pairing, so externs the analyzed file declares would
        // never resolve.
        let mut preloaded_modules = self.preloaded_modules.clone();
        let preloaded_extern = preloaded_modules.remove("extern");

        simulator_context
            .module_context
            .top_level_modules
            .extend(preloaded_modules);

        // Seed the request-local extern module with the preloaded extern
        // symbols. The Arcs are shared, so resolution sees the runtime and
        // standard externs, while externs declared by the analyzed file go
        // into this request-local module only and do not leak across files.
        if let Some(preloaded_extern) = preloaded_extern {
            let request_extern = simulator_context.module_context.extern_module.clone();
            let source = preloaded_extern.read().unwrap();
            let mut destination = request_extern.write().unwrap();

            for (name, overloads) in &source.functions {
                destination
                    .functions
                    .insert(name.clone(), overloads.clone());
            }
            for (name, variable) in &source.variables {
                destination.variables.insert(name.clone(), variable.clone());
            }
            for (name, class) in &source.classes {
                destination.classes.insert(name.clone(), class.clone());
            }
            for (name, generic) in &source.function_generics {
                destination
                    .function_generics
                    .insert(name.clone(), generic.clone());
            }
            for (name, generic) in &source.class_generics {
                destination
                    .class_generics
                    .insert(name.clone(), generic.clone());
            }
        }

        simulator_context
    }

    /// Parse and simulate the tracked document at `document_path`, returning
    /// the combined parser + simulator diagnostics plus the resulting
    /// simulator context. Returns `None` if the document is not tracked.
    fn simulate_document(&self, document_path: &Path) -> Option<SimulationResult> {
        let doc = self.doc(document_path)?;

        let (parsed_asts, parser_diagnostics) =
            helpers::parse_peko_source(document_path, doc.contents());

        let end_position = match parsed_asts.last() {
            Some(last_ast) => last_ast.get_end().clone(),
            None => PositionData {
                file: document_path.to_path_buf(),
                ..PositionData::default()
            },
        };

        let mut simulator_context =
            self.create_simulator_context(document_path.to_path_buf(), end_position);
        simulator_context.minified_import_errors = true;

        for ast in &parsed_asts {
            ast.simulate(&mut simulator_context);
        }

        Some(SimulationResult {
            diagnostics: [
                parser_diagnostics.get_diagnostics().to_vec(),
                simulator_context.diagnostics.get_diagnostics().to_vec(),
            ]
            .concat(),
            context: simulator_context,
        })
    }
}

impl PekoAnalyzer {
    /// Cursor-context search. Walks backward through the source to figure out
    /// whether the cursor is inside a type position, a `::` module path, an
    /// object access (`.`), a visibility list (`[...]`), or just open scope,
    /// and returns the appropriate set of in-scope symbols.
    ///
    /// This is load-bearing for completions, hover, and go-to definition.
    /// Preserved largely verbatim from the original implementation.
    fn get_symbols_at(&self, path: &Path, position_unprocessed: &Position) -> SymbolSearchResult {
        let Some(doc) = self.doc(path) else {
            return SymbolSearchResult::None;
        };

        let position = &{
            let mut new_position = position_unprocessed.clone();
            if new_position.character > 0 {
                new_position.character -= 1;
            }
            new_position
        };

        // Back-search through the (potential) current identifier; the goal is
        // to find `::`, `:`, `.`, or `[`.
        let mut back_search_offset_access = doc.offset_at(position);
        while back_search_offset_access > 0
            && let Some(cur_char) = doc.char_at(back_search_offset_access)
            && (char_is_peko_id_eligible!(cur_char) || char_is_whitespace!(cur_char))
        {
            back_search_offset_access -= 1;
        }

        // Back-search for any chain of `name::name::name::`.
        let mut module_full_name = String::new();
        let mut back_search_offset_module = back_search_offset_access;
        while doc.string_back(back_search_offset_module, 2) == "::" {
            if back_search_offset_module < 2 {
                break;
            }

            back_search_offset_module -= 2;
            let mut current_module_name = String::new();
            while back_search_offset_module > 0
                && let Some(cur_char) = doc.char_at(back_search_offset_module)
                && char_is_peko_id_eligible!(cur_char)
            {
                current_module_name.insert(0, cur_char);
                back_search_offset_module -= 1;
            }
            module_full_name.insert_str(0, "::");
            module_full_name.insert_str(0, &current_module_name);

            while back_search_offset_module > 0
                && let Some(cur_char) = doc.char_at(back_search_offset_module)
                && char_is_whitespace!(cur_char)
            {
                back_search_offset_module -= 1;
            }
        }

        if module_full_name.ends_with("::") {
            module_full_name.pop();
            module_full_name.pop();
        }

        // Back-search through the (potential) current type; goal is to find
        // `<` or `:`.
        let mut back_search_offset_type = back_search_offset_module;
        while back_search_offset_type > 0
            && let Some(cur_char) = doc.char_at(back_search_offset_type)
            && char_is_peko_type_eligible!(cur_char, false)
        {
            back_search_offset_type -= 1;
        }

        // Forward-search through the (potential) current type; goal is to
        // find `>` or a definition keyword (`fn` or `class`).
        let mut forward_search_offset_type = doc.offset_at(position);
        let mut forward_search_ends_with_def = false;
        while forward_search_offset_type < doc.contents().len()
            && let Some(cur_char) = doc.char_at(forward_search_offset_type)
            && char_is_peko_type_eligible!(cur_char, true)
        {
            if doc.string_forward(forward_search_offset_type, 2) == "fn"
                || doc.string_forward(forward_search_offset_type, 5) == "class"
            {
                forward_search_ends_with_def = true;
                break;
            }
            forward_search_offset_type += 1;
        }

        // Clamp forward search so it never points past the last byte. The
        // saturating subtraction yields zero for an empty document.
        forward_search_offset_type = cmp::min(
            doc.contents().len().saturating_sub(1),
            forward_search_offset_type,
        );

        // Read the boundary characters once. A missing character means the
        // cursor sits past the end of the source. A missing character compares
        // unequal to every literal below, so the search falls through to the
        // general symbol case.
        let type_boundary_char = doc.char_at(back_search_offset_type);
        let type_forward_char = doc.char_at(forward_search_offset_type);
        let access_boundary_char = doc.char_at(back_search_offset_access);

        if type_boundary_char == Some('[') && forward_search_ends_with_def {
            return SymbolSearchResult::Visibilities;
        }

        // Decide whether this is a "types only" search.
        let only_grab_types = (type_boundary_char == Some(':')
            && doc.string_back(back_search_offset_type, 2) != "::")
            || (type_boundary_char == Some('<') && type_forward_char == Some('>'));

        let object_access = access_boundary_char == Some('.')
            && doc.string_back(back_search_offset_access, 2) != "..";

        // When we're doing an object access, walk the position back to the
        // dot so simulator queries report members of the object on the left.
        let final_position = if object_access {
            let mut position = position.clone();
            // Walk the cursor backward until its byte offset matches the access
            // boundary. Each iteration moves to an earlier position. The loop
            // stops at the start of the document or when a step fails to reduce
            // the offset, so it terminates even when the boundary offset cannot
            // be matched exactly.
            loop {
                let current_offset = doc.offset_at(&position);
                if current_offset == back_search_offset_access {
                    break;
                }

                if position.character > 0 {
                    position.character -= 1;
                } else if position.line > 1 {
                    position.line -= 1;
                    position.character = doc.max_offset_at(&position) as u32;
                } else {
                    break;
                }

                if doc.offset_at(&position) >= current_offset {
                    break;
                }
            }
            position
        } else {
            position.clone()
        };

        let peko_position = PositionData::new(
            final_position.character as usize,
            doc.offset_at(&final_position),
            (final_position.line + 1) as usize,
            path.to_path_buf(),
        );

        // Run simulation now so we have somewhere to pull symbols from.
        let mut simulation_result = match self.simulate_document(path) {
            Some(result) => result,
            None => return SymbolSearchResult::None,
        };

        let mut available_symbols = if object_access {
            simulation_result
                .context
                .get_symbols_from_object_at_position(peko_position)
        } else if !module_full_name.is_empty() {
            simulation_result
                .context
                .get_available_symbols_from_module(module_full_name.clone(), peko_position)
        } else {
            simulation_result
                .context
                .get_available_symbols_from_position(peko_position)
        };

        // A `module::` access lists another module's public surface, so
        // private symbols are dropped along with hidden ones. A bare
        // in-scope search keeps private symbols, which the declaring module
        // can reach.
        let cross_module_access = !module_full_name.is_empty();
        let mut index = 0;
        while index < available_symbols.len() {
            let visibility = available_symbols[index].get_visibility();
            if visibility.hidden
                || (cross_module_access && visibility.private)
                || available_symbols[index]
                    .get_name()
                    .chars()
                    .any(|c| ['<', '>', '[', ']'].contains(&c))
                || available_symbols[index].get_name() == "constructor"
            {
                available_symbols.remove(index);
            } else {
                index += 1;
            }
        }

        if only_grab_types {
            let mut index = 0;
            while index < available_symbols.len() {
                if !available_symbols[index].get_kind().starts_with("class") {
                    available_symbols.remove(index);
                } else {
                    index += 1;
                }
            }

            SymbolSearchResult::Types(available_symbols)
        } else if object_access || !module_full_name.is_empty() {
            SymbolSearchResult::Access(available_symbols)
        } else {
            SymbolSearchResult::Symbols(available_symbols)
        }
    }
}

// ---------------------------------------------------------------------------
// AnalysisEngine implementation
// ---------------------------------------------------------------------------

impl AnalysisEngine for PekoAnalyzer {
    fn update_project_root(&mut self, path: &Path) {
        self.set_project_folder(path.to_path_buf());
    }

    fn update_file(&mut self, path: &Path, text: &str) {
        self.tracked_files
            .insert(path.to_path_buf(), PekoDocument::from(text));
    }

    fn close_file(&mut self, path: &Path) {
        self.tracked_files.remove(path);
    }

    fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let Some(simulation_result) = self.simulate_document(path) else {
            return Vec::new();
        };

        simulation_result
            .diagnostics
            .iter()
            .map(|peko_diagnostic| Diagnostic {
                range: Range {
                    start: create_position(&peko_diagnostic.start),
                    end: create_position(&peko_diagnostic.end),
                },
                severity: match peko_diagnostic.diagnostic_type {
                    DiagnosticType::Error => crate::server::analysis::DiagnosticSeverity::Error,
                    DiagnosticType::Warning => crate::server::analysis::DiagnosticSeverity::Warning,
                },
                code: None,
                message: peko_diagnostic.message.clone(),
                source: Some("pekoscript".to_string()),
            })
            .collect()
    }

    fn document_symbols(&self, path: &Path) -> Vec<Symbol> {
        let Some(simulation_result) = self.simulate_document(path) else {
            return Vec::new();
        };

        let Some(main_module) = simulation_result
            .context
            .module_context
            .top_level_modules
            .get("main")
        else {
            return Vec::new();
        };

        let scope = main_module
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .scope
            .clone();

        // Keep only symbols declared in this file. Imported modules and
        // symbols pulled in by an unpack import are defined elsewhere, and
        // the implicit default imports carry a zero position that reorders
        // the breadcrumb trail. File paths are canonicalized so that
        // logically-equal paths compare equal, with a raw-path fallback for
        // in-memory documents that do not exist on disk.
        let current_file = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        let mut top_level_scope = scope
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        top_level_scope.symbols.retain(|_, symbol| {
            let symbol_file = symbol.get_start().file;
            let symbol_file = symbol_file
                .canonicalize()
                .unwrap_or_else(|_| symbol_file.clone());
            symbol_file == current_file
        });

        helpers::document_symbols_from_scope(Arc::new(RwLock::new(top_level_scope)), false)
    }

    fn hover(&self, path: &Path, position: &Position) -> Option<HoverInfo> {
        let doc = self.doc(path)?;
        let current_identifier = doc.identifier_at(position);
        if current_identifier.is_empty() {
            return None;
        }

        let available_symbols = match self.get_symbols_at(path, position) {
            SymbolSearchResult::Access(symbols)
            | SymbolSearchResult::Symbols(symbols)
            | SymbolSearchResult::Types(symbols) => symbols,
            SymbolSearchResult::Visibilities | SymbolSearchResult::None => Vec::new(),
        };

        let symbol = available_symbols
            .iter()
            .find(|symbol| symbol.get_name() == current_identifier)?
            .clone();

        let mut symbol_markdown = String::from("```pekoscript\n");
        match symbol.get_kind() {
            "class" | "class-generic" => {
                symbol_markdown.push_str("class ");

                if let Some(symbol_class) = symbol.as_class()
                    && symbol_class.generic
                {
                    symbol_markdown.push('<');
                    symbol_markdown.push_str(
                        &symbol_class
                            .generic_types
                            .iter()
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    symbol_markdown.push('>');
                }

                symbol_markdown.push_str(&symbol.get_name());

                if let Some(symbol_class) = symbol.as_class()
                    && !symbol_class.first_constructor_arguments.is_empty()
                {
                    symbol_markdown.push('(');
                    let args: Vec<String> = symbol_class
                        .first_constructor_arguments
                        .iter()
                        .map(|(argument_name, argument_type)| {
                            format!("{argument_name}: {argument_type}")
                        })
                        .collect();
                    symbol_markdown.push_str(&args.join(", "));
                    symbol_markdown.push(')');
                }
            }

            "function" | "function-generic" => {
                symbol_markdown.push_str("fn ");

                if let Some(symbol_function) = symbol.as_function() {
                    if symbol_function.generic {
                        symbol_markdown.push('<');
                        symbol_markdown.push_str(
                            &symbol_function
                                .generic_type_names
                                .iter()
                                .map(String::as_str)
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                        symbol_markdown.push('>');
                    }

                    symbol_markdown.push_str(&symbol.get_name());
                    symbol_markdown.push('(');

                    let args: Vec<String> = symbol_function
                        .arguments
                        .iter()
                        .map(|(argument_name, (_, argument_type))| {
                            format!("{argument_name}: {argument_type}")
                        })
                        .collect();
                    symbol_markdown.push_str(&args.join(", "));

                    symbol_markdown.push_str(") => ");
                    symbol_markdown.push_str(&symbol_function.return_type.to_string());
                } else {
                    symbol_markdown.push_str(&symbol.get_name());
                }
            }

            "variable" | "attribute" => {
                symbol_markdown.push_str(&symbol.get_name());
                if let Some(symbol_variable) = symbol.as_variable() {
                    symbol_markdown.push_str(": ");
                    symbol_markdown.push_str(&symbol_variable.value_type.to_string());
                }
            }

            _ => {
                symbol_markdown.push_str("module ");
                symbol_markdown.push_str(&symbol.get_name());
            }
        }

        symbol_markdown.push_str("\n```");

        if let Some(docinfo) = symbol.get_doc_info() {
            symbol_markdown.push_str("\n---\n");
            symbol_markdown.push_str(&docinfo.description);

            if !docinfo.parameter_docs.is_empty() {
                symbol_markdown.push_str("\n## parameters");

                for (parameter_name, parameter_docs) in &docinfo.parameter_docs {
                    symbol_markdown.push_str(&format!("\n- {parameter_name}: {parameter_docs}"));
                }
            }

            if !docinfo.examples.is_empty() {
                symbol_markdown.push_str("\n## examples\n");

                for example in &docinfo.examples {
                    symbol_markdown.push_str("```pekoscript\n");
                    symbol_markdown.push_str(example);
                    symbol_markdown.push_str("\n```\n---\n<br>\n\n");
                }

                // Strip the trailing "\n---\n<br>\n\n" left by the last example.
                for _ in 0..11 {
                    symbol_markdown.pop();
                }
            }
        }

        Some(HoverInfo {
            contents: symbol_markdown,
            range: None,
        })
    }

    fn completions(&self, path: &Path, position: &Position) -> Vec<CompletionItem> {
        let Some(doc) = self.doc(path) else {
            return Vec::new();
        };

        let (available_symbols, list_builtin_types, list_visibilities, accessing_symbols) =
            match self.get_symbols_at(path, position) {
                SymbolSearchResult::Access(symbols) => (symbols, false, false, true),
                SymbolSearchResult::Symbols(symbols) => (symbols, false, false, false),
                SymbolSearchResult::Types(symbols) => (symbols, true, false, false),
                SymbolSearchResult::Visibilities => (Vec::new(), false, true, false),
                SymbolSearchResult::None => (Vec::new(), false, false, false),
            };

        let mut completion_items: Vec<CompletionItem> = available_symbols
            .iter()
            .map(|symbol| {
                let (insert_text, insert_text_format, command) = match symbol.get_kind() {
                    "variable" | "attribute" => (None, None, None),
                    "function" | "function-generic" => match symbol.as_function() {
                        None => (None, None, None),
                        Some(symbol_function) => {
                            let mut insert = symbol_function.name.clone();
                            if symbol_function.generic {
                                insert.push('<');
                                for (idx, generic_name) in
                                    symbol_function.generic_type_names.iter().enumerate()
                                {
                                    insert.push_str(&format!("${{{}:{generic_name}}}", idx + 1));
                                    if idx < symbol_function.generic_type_names.len() - 1 {
                                        insert.push_str(", ");
                                    }
                                }
                                insert.push('>');
                            }

                            if let Some(cur_char) = doc.char_at(doc.offset_at(position))
                                && cur_char != '('
                            {
                                insert.push('(');
                                for (idx, (argument_name, _)) in
                                    symbol_function.arguments.iter().enumerate()
                                {
                                    insert.push_str(&format!(
                                        "${{{}:{argument_name}}}",
                                        idx + symbol_function.generic_type_names.len() + 1
                                    ));
                                    if idx < symbol_function.arguments.len() - 1 {
                                        insert.push_str(", ");
                                    }
                                }

                                insert.push(')');

                                (
                                    Some(insert),
                                    Some(InsertTextFormat::Snippet),
                                    Some(Command {
                                        title: "suggestionhelper".to_string(),
                                        command: "editor::ShowSignatureHelp".to_string(),
                                    }),
                                )
                            } else if symbol_function.generic {
                                (
                                    Some(insert),
                                    Some(InsertTextFormat::Snippet),
                                    Some(Command {
                                        title: "suggestionhelper".to_string(),
                                        command: "editor::ShowSignatureHelp".to_string(),
                                    }),
                                )
                            } else {
                                (Some(insert), None, None)
                            }
                        }
                    },
                    "class" | "class-generic" => match symbol.as_class() {
                        None => (None, None, None),
                        Some(symbol_class) => {
                            let mut insert = symbol_class.name.clone();
                            if symbol_class.generic {
                                insert.push('<');
                                for (idx, generic_name) in
                                    symbol_class.generic_types.iter().enumerate()
                                {
                                    insert.push_str(&format!("${{{}:{generic_name}}}", idx + 1));
                                    if idx < symbol_class.generic_types.len() - 1 {
                                        insert.push_str(", ");
                                    }
                                }
                                insert.push('>');
                            }

                            if list_builtin_types {
                                (Some(insert), Some(InsertTextFormat::Snippet), None)
                            } else if doc.char_at(doc.offset_at(position)) != Some('(') {
                                insert.push('(');
                                for (idx, (argument_name, _)) in
                                    symbol_class.first_constructor_arguments.iter().enumerate()
                                {
                                    insert.push_str(&format!(
                                        "${{{}:{argument_name}}}",
                                        idx + symbol_class.generic_types.len() + 1
                                    ));
                                    if idx < symbol_class.first_constructor_arguments.len() - 1 {
                                        insert.push_str(", ");
                                    }
                                }

                                insert.push(')');

                                (
                                    Some(insert),
                                    Some(InsertTextFormat::Snippet),
                                    Some(Command {
                                        title: "suggestionhelper".to_string(),
                                        command: "editor::ShowSignatureHelp".to_string(),
                                    }),
                                )
                            } else if symbol_class.generic {
                                (
                                    Some(insert),
                                    Some(InsertTextFormat::Snippet),
                                    Some(Command {
                                        title: "suggestionhelper".to_string(),
                                        command: "editor::ShowSignatureHelp".to_string(),
                                    }),
                                )
                            } else {
                                (Some(insert), None, None)
                            }
                        }
                    },
                    _ => (
                        Some(format!("{}::", symbol.get_name())),
                        None,
                        Some(Command {
                            title: "resuggest".to_string(),
                            command: "editor::ShowCompletions".to_string(),
                        }),
                    ),
                };

                CompletionItem {
                    label: symbol.get_name(),
                    kind: match symbol.get_kind() {
                        "variable" => CompletionKind::Variable,
                        "attribute" => CompletionKind::Field,
                        "function" | "function-generic" => CompletionKind::Function,
                        "class" | "class-generic" => CompletionKind::Class,
                        _ => CompletionKind::Module,
                    },
                    detail: None,
                    documentation: symbol.get_doc_info().map(|d| d.description.clone()),
                    insert_text,
                    sort_text: Some("0001".to_string()),
                    insert_text_format,
                    command,
                }
            })
            .collect();

        let keyword_symbols: &[&str] = if list_builtin_types {
            &[
                "int", "int16", "int128", "int64", "char", "bool", "string", "opaque", "void",
            ]
        } else if list_visibilities {
            &[
                "private",
                "constant",
                "external",
                "notrack",
                "variadic",
                "blockexit",
                "hidden",
                "mutates",
            ]
        } else {
            &[]
        };

        for keyword in keyword_symbols {
            completion_items.push(CompletionItem {
                label: (*keyword).to_string(),
                kind: CompletionKind::Keyword,
                detail: None,
                documentation: None,
                insert_text: None,
                insert_text_format: None,
                command: None,
                sort_text: Some("0002".to_string()),
            });
        }

        if !list_builtin_types && !list_visibilities && !accessing_symbols {
            for xml_keyword in XML_KEYWORDS {
                completion_items.push(CompletionItem {
                    label: (*xml_keyword).to_string(),
                    kind: CompletionKind::Property,
                    detail: None,
                    documentation: None,
                    insert_text: Some(format!("<{xml_keyword}>${{0}}</{xml_keyword}>")),
                    sort_text: Some("0003".to_string()),
                    insert_text_format: Some(InsertTextFormat::Snippet),
                    command: None,
                });
            }
        }

        // Block-shaped snippets that all follow the same `keyword name { body }` form.
        for simple_snip in ["if", "while", "class", "module"] {
            completion_items.push(CompletionItem {
                label: simple_snip.to_string(),
                kind: CompletionKind::Snippet,
                detail: None,
                documentation: None,
                insert_text: Some(format!("{simple_snip} ${{1}} {{\n\t${{0}}\n}}")),
                sort_text: Some("0002".to_string()),
                insert_text_format: Some(InsertTextFormat::Snippet),
                command: None,
            });
        }

        // Platform-statement snippets: `keyword "name" { body }`.
        for platform_snip in ["platform", "arch"] {
            completion_items.push(CompletionItem {
                label: platform_snip.to_string(),
                kind: CompletionKind::Snippet,
                detail: None,
                documentation: None,
                insert_text: Some(format!("{platform_snip} \"${{1}}\" {{\n\t${{0}}\n}}")),
                sort_text: Some("0002".to_string()),
                insert_text_format: Some(InsertTextFormat::Snippet),
                command: None,
            });
        }

        // `for x in y { body }`.
        completion_items.push(CompletionItem {
            label: "for".to_string(),
            kind: CompletionKind::Snippet,
            detail: None,
            documentation: None,
            insert_text: Some("for ${1} in ${2} {\n\t${0}\n}".to_string()),
            sort_text: Some("0002".to_string()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            command: None,
        });

        // `fn name(args) { body }`.
        completion_items.push(CompletionItem {
            label: "fn".to_string(),
            kind: CompletionKind::Snippet,
            detail: None,
            documentation: None,
            insert_text: Some("fn ${1}(${2}) {\n\t${0}\n}".to_string()),
            sort_text: Some("0002".to_string()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            command: None,
        });

        completion_items
    }

    fn signature_help(&self, path: &Path, position: &Position) -> Option<SignatureHelp> {
        let doc = self.doc(path)?;
        let mut simulation_result = self.simulate_document(path)?;

        let peko_position = PositionData::new(
            position.character as usize,
            doc.offset_at(position),
            (position.line + 1) as usize,
            path.to_path_buf(),
        );

        let function_call = simulation_result
            .context
            .get_function_call_from_position_in_main(peko_position.clone())?;

        Some(SignatureHelp {
            signatures: vec![SignatureInfo {
                label: function_call.get_signature(),
                parameters: function_call
                    .signature_arguments
                    .iter()
                    .map(|(argument_name, argument_type)| ParameterInfo {
                        label: if argument_name.is_empty() {
                            argument_type.to_string()
                        } else {
                            format!("{argument_name}: {argument_type}")
                        },
                        documentation: None,
                    })
                    .collect(),
                documentation: function_call.docinfo.as_ref().map(|docinfo| {
                    let mut info_markup = docinfo.description.clone();
                    let argument_index =
                        function_call.argument_index_at_position(peko_position.clone());

                    // Clamp the index to the last valid argument. An index past
                    // the end maps to the final argument, and an empty argument
                    // list yields no name.
                    let last_index = function_call.signature_arguments.len().saturating_sub(1);
                    let clamped_index = argument_index.min(last_index);
                    let argument_name = function_call
                        .signature_arguments
                        .get_index(clamped_index)
                        .map(|(name, _)| name);

                    if let Some(argument_name) = argument_name
                        && docinfo.parameter_docs.contains_key(argument_name)
                    {
                        info_markup.push_str(&format!(
                            "\n- {argument_name}: {}",
                            docinfo.parameter_docs[argument_name]
                        ));
                    }

                    info_markup
                }),
            }],
            active_signature: Some(0),
            active_parameter: Some(function_call.argument_index_at_position(peko_position) as u32),
        })
    }

    fn goto_definition(&self, path: &Path, position: &Position) -> Vec<Location> {
        let Some(doc) = self.doc(path) else {
            return Vec::new();
        };
        let current_identifier = doc.identifier_at(position);
        if current_identifier.is_empty() {
            return Vec::new();
        }

        let available_symbols = match self.get_symbols_at(path, position) {
            SymbolSearchResult::Access(symbols)
            | SymbolSearchResult::Symbols(symbols)
            | SymbolSearchResult::Types(symbols) => symbols,
            SymbolSearchResult::Visibilities | SymbolSearchResult::None => Vec::new(),
        };

        let symbol = match available_symbols
            .iter()
            .find(|symbol| symbol.get_name() == current_identifier)
        {
            Some(symbol) => symbol.clone(),
            None => return Vec::new(),
        };

        vec![Location {
            file: symbol.get_start().file.clone(),
            range: Range {
                start: create_position(&symbol.get_start()),
                end: create_position(&symbol.get_end()),
            },
        }]
    }

    fn format(&self, _path: &Path, _text: &str) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// XML keyword list for completion suggestions
// ---------------------------------------------------------------------------

/// HTML/XML tag names suggested by the completion popup outside of type,
/// visibility, and object-access contexts. These get wrapped in
/// `<name>$0</name>` snippet templates by [`PekoAnalyzer::completions`].
const XML_KEYWORDS: &[&str] = &[
    "abbr",
    "acronym",
    "address",
    "a",
    "applet",
    "area",
    "article",
    "aside",
    "audio",
    "base",
    "basefont",
    "bdi",
    "bdo",
    "bgsound",
    "big",
    "blockquote",
    "body",
    "b",
    "br",
    "button",
    "caption",
    "canvas",
    "center",
    "cite",
    "code",
    "colgroup",
    "col",
    "data",
    "datalist",
    "dd",
    "dfn",
    "del",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "font",
    "footer",
    "form",
    "frame",
    "frameset",
    "head",
    "header",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "ins",
    "isindex",
    "i",
    "kbd",
    "keygen",
    "label",
    "legend",
    "li",
    "main",
    "mark",
    "marquee",
    "menuitem",
    "meta",
    "meter",
    "nav",
    "nobr",
    "noembed",
    "noscript",
    "object",
    "optgroup",
    "option",
    "output",
    "p",
    "param",
    "em",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "samp",
    "script",
    "section",
    "small",
    "source",
    "spacer",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "summary",
    "svg",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "time",
    "title",
    "tr",
    "track",
    "tt",
    "u",
    "var",
    "video",
    "wbr",
    "xmp",
];
