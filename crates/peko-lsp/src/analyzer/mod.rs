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
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use document::PekoDocument;
use helpers::create_position;
use peko_core::{
    ExternalModuleInfo,
    asts::data_structures::PositionData,
    config::Lockfile,
    diagnostics::{DiagnosticType, PekoDiagnostic},
    packages::PekoPackageIndex,
    simulator::{
        PekoValueSimulator,
        context::PekoSimulatorContext,
        data_structures::{ScopeSymbol, ScopeMethodSignature, SimulatorModule},
    },
    target::PekoTarget,
};

use crate::{
    char_is_peko_id_eligible, char_is_peko_type_eligible, char_is_whitespace,
    server::analysis::{
        AnalysisEngine, Command, CompletionItem, CompletionKind, CompletionTextEdit, Diagnostic,
        HoverInfo, InsertTextFormat, Location, ParameterInfo, Position, Range, SignatureHelp,
        SignatureInfo, Symbol,
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

    /// Per-file memo of the last simulation, keyed by path and holding the hash
    /// of the source that produced it. A single document is simulated once per
    /// distinct text, so the several requests the editor fires on one keystroke
    /// (completion, hover, signature help, diagnostics) reuse one simulation.
    /// Every `update_file` clears the whole memo, so a change to any file (the
    /// edited one or one it imports) forces a fresh simulation. Guarded by a
    /// mutex because the analyzer is shared across the LSP's request threads.
    simulation_cache: Mutex<HashMap<PathBuf, (u64, SimulationResult)>>,
}

/// Bundle of (diagnostics, simulator context) returned from running one
/// document through parse + simulate.
#[derive(Clone)]
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
    /// Cursor is after `.` or `::`, accessing a known scope. The optional
    /// replacement describes a `module::` access, so a class in that module can
    /// be offered as `new module::Class(...)` over the whole access span.
    Access(Vec<ScopeSymbol>, Option<ModuleAccessReplace>),
    /// Cursor is in a type position: offer types only.
    Types(Vec<ScopeSymbol>),
    /// Cursor is somewhere general: offer every in-scope symbol.
    Symbols(Vec<ScopeSymbol>),
    /// Cursor is inside an `import pkg::` path: offer the package's module
    /// names.
    ImportModules(Vec<String>),
    /// Cursor is after `EnumName::`: offer the enum's variant names.
    EnumVariants(Vec<String>),
    /// Cursor is at a method-declaration position inside a class body that
    /// derives from traits: offer those traits' methods as override stubs.
    OverrideMethods(Vec<ScopeMethodSignature>),
    /// File not tracked, or no useful context.
    None,
}

/// Describes a `module::` access span at the cursor, used to rewrite a class
/// completion as a full `new module::Class(...)` expression.
struct ModuleAccessReplace {
    /// Byte offset where the module path begins, used to insert a `new`
    /// keyword before it.
    start: usize,
    /// Whether a `new` keyword already precedes the module path, so the rewrite
    /// must not prepend a second one.
    after_new: bool,
}

/// Classify a class-header clause from the text before the cursor on the
/// current line. Returns `Some(true)` when the cursor is in an `impl` clause
/// (traits only), `Some(false)` for a `from` clause (superclasses and traits),
/// and `None` when the cursor is not in a class header's inheritance clause.
///
/// The check is line-local and keyword-bounded: the line must contain `class`,
/// the relevant keyword must stand as its own word, and no `{` may follow it
/// (which would mean the class body has already opened).
fn class_clause_kind(line_prefix: &str) -> Option<bool> {
    if !line_prefix.contains("class ") {
        return None;
    }

    let bytes = line_prefix.as_bytes();
    let is_word_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    // Last word-bounded occurrence of `keyword` in the prefix.
    let last_keyword = |keyword: &str| -> Option<usize> {
        let mut found = None;
        let mut search = 0;
        while let Some(relative) = line_prefix[search..].find(keyword) {
            let start = search + relative;
            let end = start + keyword.len();
            let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
            let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
            if before_ok && after_ok {
                found = Some(start);
            }
            search = start + 1;
        }
        found
    };

    let from_position = last_keyword("from");
    let impl_position = last_keyword("impl");

    let (keyword_position, is_impl) = match (from_position, impl_position) {
        (from, Some(implements)) if from.is_none_or(|from| implements > from) => {
            (implements, true)
        }
        (Some(from), _) => (from, false),
        _ => return None,
    };

    // A `{` after the keyword means the body has started; the cursor is no
    // longer in the inheritance clause.
    if line_prefix[keyword_position..].contains('{') {
        return None;
    }

    Some(is_impl)
}

/// Whether the current line is a method-declaration position: `fn` as a word,
/// followed by the method name being typed and nothing else yet.
fn is_method_decl_position(line_prefix: &str) -> bool {
    let trimmed = line_prefix.trim_start();
    let Some(rest) = trimmed.strip_prefix("fn") else {
        return false;
    };
    // `fn` must be its own word: what follows is whitespace, then only the
    // identifier being typed (no `(`, `<`, or other punctuation yet).
    if !rest.starts_with([' ', '\t']) {
        return false;
    }
    rest.trim_start()
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
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
            simulation_cache: Mutex::new(HashMap::new()),
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

        simulator_context.external_modules = self.external_modules();

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

    /// Build the lock-scoped external-module map for the active project.
    ///
    /// Imports resolve against the versions pinned in the project's
    /// `peko.lock`. A project without a discovered root or lockfile resolves
    /// against nothing.
    fn external_modules(&self) -> HashMap<String, ExternalModuleInfo> {
        let mut modules = HashMap::new();

        // The auto-imported `std` package, resolved from the installed registry
        // cache under the Peko root. Mirrors the compiler's
        // `external_modules_for`, so `std::io` and the other submodules resolve
        // in the editor even when the project has no lockfile. Without this the
        // language server reports a spurious "no module named `std::io`" that
        // the build never sees.
        let installed_std =
            peko_core::packages::registry_source_dir(&self.peko_root, "std", "0.1.0")
                .join("peko.toml");
        if let Ok(loaded) = peko_core::config::Manifest::load(&installed_std) {
            let info = loaded.manifest.to_external_module(&loaded.root);
            modules.insert(info.module_name.clone(), info);
        }

        if let Some(project_root) = self.project_root.as_ref()
            && let Ok(Some(lockfile)) = Lockfile::load_from_root(project_root)
        {
            modules.extend(
                PekoPackageIndex::from_lockfile(&self.peko_root, project_root, &lockfile)
                    .get_external_modules(),
            );
        }

        modules
    }

    /// Names of the modules a package exposes at its root, for completing an
    /// `import package::` path. Each `.peko` source file in the package's
    /// source root is one module; the package entry file (`lib.peko`) is not a
    /// module and is excluded. Returns an empty list for an unknown package.
    fn package_module_names(&self, package: &str) -> Vec<String> {
        let externals = self.external_modules();
        let Some(info) = externals.get(package) else {
            return Vec::new();
        };
        let Some(version) = info.versions.first() else {
            return Vec::new();
        };

        let entry_stem = Path::new(&version.entry_file)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned());

        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&version.source_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("peko") {
                    continue;
                }
                let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if Some(&stem) == entry_stem.as_ref() {
                    continue;
                }
                names.push(stem);
            }
        }

        names.sort();
        names.dedup();
        names
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

        simulator_context.external_modules = self.external_modules();

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
            // Generic templates live in the function and class maps, so they
            // are copied above.
        }

        simulator_context
    }

    /// Parse and simulate the tracked document at `document_path`, returning
    /// the combined parser + simulator diagnostics plus the resulting
    /// simulator context. Returns `None` if the document is not tracked.
    fn simulate_document(&self, document_path: &Path) -> Option<SimulationResult> {
        let doc = self.doc(document_path)?;
        let source = doc.contents();

        // The source hash keys the memo. A cache hit returns a clone of the
        // prior simulation, which is cheap because the simulator's modules are
        // reference-counted and the query methods run below never mutate them.
        let source_hash = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            source.hash(&mut hasher);
            hasher.finish()
        };

        if let Ok(cache) = self.simulation_cache.lock()
            && let Some((cached_hash, cached_result)) = cache.get(document_path)
            && *cached_hash == source_hash
        {
            return Some(cached_result.clone());
        }

        let (parsed_asts, parser_diagnostics) =
            helpers::parse_peko_source(document_path, source);

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

        // Type-check every generic body once with its parameters erased to
        // their bound carriers, so tooling (completion, hover) works inside a
        // generic function or class even when it is never instantiated. This
        // records the defined objects and symbols the object-access completion
        // relies on.
        simulator_context.check_generics_erased();

        // Propagate `[mutates]` to a fixpoint so a method that calls another
        // method inferred `[mutates]` only later in the file is itself marked.
        // Without this the editor would show a method as non-mutating and miss
        // the const-call diagnostics that the compiler applies.
        simulator_context.propagate_mutates_fixpoint();

        let result = SimulationResult {
            diagnostics: [
                parser_diagnostics.get_diagnostics().to_vec(),
                simulator_context.diagnostics.get_diagnostics().to_vec(),
            ]
            .concat(),
            context: simulator_context,
        };

        if let Ok(mut cache) = self.simulation_cache.lock() {
            cache.insert(document_path.to_path_buf(), (source_hash, result.clone()));
        }

        Some(result)
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

        // Back-search for any chain of `name::name::name::`. Track the byte
        // offset where the leftmost module name begins so a `module::` access
        // completion can replace the whole path.
        let mut module_full_name = String::new();
        let mut back_search_offset_module = back_search_offset_access;
        let mut module_path_start = back_search_offset_access;
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
                module_path_start = back_search_offset_module;
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

        // Import-path context: `import package::` offers the package's module
        // names. Detect the `import` keyword just before the module path. Only
        // a single-segment package path is handled; the module names come from
        // the filesystem, so no simulation is needed and this returns early.
        if !module_full_name.is_empty() && !module_full_name.contains("::") {
            let mut probe = module_path_start;
            while probe > 0
                && doc
                    .char_at(probe - 1)
                    .map(|c| char_is_whitespace!(c))
                    .unwrap_or(false)
            {
                probe -= 1;
            }
            let after_import = probe >= 6
                && doc.string_back(probe - 1, 6) == "import"
                && (probe < 7
                    || !doc
                        .char_at(probe - 7)
                        .map(|c| char_is_peko_id_eligible!(c))
                        .unwrap_or(false));
            if after_import {
                return SymbolSearchResult::ImportModules(
                    self.package_module_names(&module_full_name),
                );
            }
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

        // A `<` boundary may open a type's generic arguments before the closing
        // `>` has been typed (`let x: Array<num|`). Treat it as a type position
        // when the `<` follows a type name that is itself in a type context:
        // preceded by `:` (annotation), `<` (nested args), or `,` (arg list).
        let generic_type_context = type_boundary_char == Some('<') && {
            let mut probe = back_search_offset_type.saturating_sub(1);
            while probe > 0
                && doc.char_at(probe).map(|c| char_is_whitespace!(c)).unwrap_or(false)
            {
                probe -= 1;
            }
            while probe > 0
                && doc
                    .char_at(probe)
                    .map(|c| char_is_peko_id_eligible!(c))
                    .unwrap_or(false)
            {
                probe -= 1;
            }
            while probe > 0
                && doc.char_at(probe).map(|c| char_is_whitespace!(c)).unwrap_or(false)
            {
                probe -= 1;
            }
            matches!(doc.char_at(probe), Some(':') | Some('<') | Some(','))
        };

        // A `class X from |` clause offers superclasses and traits; a
        // `class X impl |` clause offers only traits. Detect the clause from the
        // current line prefix and treat it as a type position.
        let cursor_byte = doc.offset_at(position_unprocessed);
        let before_cursor = doc.string_back(cursor_byte.saturating_sub(1), cursor_byte);
        let line_prefix = before_cursor.rsplit('\n').next().unwrap_or("");
        let class_clause_impl = class_clause_kind(line_prefix);
        let restrict_to_traits = class_clause_impl == Some(true);

        // Decide whether this is a "types only" search.
        let only_grab_types = (type_boundary_char == Some(':')
            && doc.string_back(back_search_offset_type, 2) != "::")
            || (type_boundary_char == Some('<') && type_forward_char == Some('>'))
            || generic_type_context
            || class_clause_impl.is_some();

        let object_access = access_boundary_char == Some('.')
            && doc.string_back(back_search_offset_access, 2) != "..";

        // For an object access, anchor the simulator query at the access
        // boundary (the `.`) so it reports members of the object on the left.
        // Otherwise anchor at the cursor. Both are byte offsets; the document
        // converts them to a char-based peko_core position.
        let final_byte = if object_access {
            back_search_offset_access
        } else {
            doc.offset_at(position)
        };

        let peko_position = doc.position_data_at_byte(final_byte, path);

        // Run simulation now so we have somewhere to pull symbols from.
        let mut simulation_result = match self.simulate_document(path) {
            Some(result) => result,
            None => return SymbolSearchResult::None,
        };

        // Override context: typing `fn <name>` inside a class body. Offer the
        // methods of every superclass and trait the enclosing class derives
        // from, walked transitively, as override stubs.
        if is_method_decl_position(line_prefix) {
            let in_scope = simulation_result
                .context
                .get_available_symbols_from_position(peko_position.clone());

            // The enclosing class is the innermost in-scope class whose
            // declaration span contains the cursor. Using the simulator's span
            // is precise where scanning the header text is not.
            let enclosing_class = in_scope
                .iter()
                .filter(|symbol| symbol.get_kind().starts_with("class"))
                .filter(|symbol| {
                    let start = symbol.get_start();
                    let end = symbol.get_end();
                    start.file == peko_position.file
                        && start.positioned_before_inclusive(peko_position.clone())
                        && peko_position.positioned_before_inclusive(end)
                })
                .max_by_key(|symbol| symbol.get_start().index)
                .and_then(|symbol| symbol.as_class());

            if let Some(enclosing_class) = enclosing_class {
                let by_name: HashMap<String, &ScopeSymbol> =
                    in_scope.iter().map(|s| (s.get_name(), s)).collect();

                // Walk the parent chain, collecting each parent's methods. A
                // parent may itself derive from others, so classes enqueue their
                // own parents. Seen-sets guard against inheritance cycles and
                // duplicate method names.
                let mut methods: Vec<ScopeMethodSignature> = Vec::new();
                let mut seen_types: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut seen_methods: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut pending = enclosing_class.parents.clone();

                while let Some(parent_name) = pending.pop() {
                    if !seen_types.insert(parent_name.clone()) {
                        continue;
                    }
                    let Some(symbol) = by_name.get(&parent_name) else {
                        continue;
                    };
                    if let Some(parent_trait) = symbol.as_trait() {
                        for method in parent_trait.methods {
                            if seen_methods.insert(method.name.clone()) {
                                methods.push(method);
                            }
                        }
                    } else if let Some(parent_class) = symbol.as_class() {
                        for method in parent_class.methods {
                            if seen_methods.insert(method.name.clone()) {
                                methods.push(method);
                            }
                        }
                        pending.extend(parent_class.parents);
                    }
                }

                if !methods.is_empty() {
                    return SymbolSearchResult::OverrideMethods(methods);
                }
            }
        }

        let mut available_symbols = if object_access {
            simulation_result
                .context
                .get_symbols_from_object_at_position(peko_position.clone())
        } else if !module_full_name.is_empty() {
            simulation_result
                .context
                .get_available_symbols_from_module(module_full_name.clone(), peko_position.clone())
        } else {
            simulation_result
                .context
                .get_available_symbols_from_position(peko_position.clone())
        };

        // An `EnumName::` access lists the enum's variants. When module
        // resolution found nothing for a single-segment name, check whether
        // that name is an enum in scope and surface its variant names.
        if !module_full_name.is_empty()
            && !module_full_name.contains("::")
            && available_symbols.is_empty()
        {
            let in_scope = simulation_result
                .context
                .get_available_symbols_from_position(peko_position.clone());
            if let Some(variants) = in_scope
                .iter()
                .find(|symbol| {
                    symbol.get_kind() == "enum" && symbol.get_name() == module_full_name
                })
                .and_then(|symbol| symbol.as_enum())
                .map(|scope_enum| scope_enum.variants)
            {
                return SymbolSearchResult::EnumVariants(variants);
            }
        }

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
            // A type position accepts named user types: classes (including
            // generic classes), enums, and traits (usable as trait-object
            // types). Values, functions, and modules are dropped so they do not
            // appear where only a type is valid.
            let mut index = 0;
            while index < available_symbols.len() {
                let kind = available_symbols[index].get_kind();
                // An `impl` clause accepts only traits; every other type
                // position also accepts classes and enums.
                let keep = if restrict_to_traits {
                    kind == "trait"
                } else {
                    kind.starts_with("class") || kind == "enum" || kind == "trait"
                };
                if keep {
                    index += 1;
                } else {
                    available_symbols.remove(index);
                }
            }

            SymbolSearchResult::Types(available_symbols)
        } else if object_access || !module_full_name.is_empty() {
            // For a `module::` access, describe the whole `module::partial`
            // span so a class completion can rewrite it as a full
            // `new module::Class(...)` expression. Object access (`.`) carries
            // no such rewrite.
            let module_replace = (!module_full_name.is_empty()).then(|| {
                // Does a `new` keyword already sit before the module path?
                let mut probe = module_path_start;
                while probe > 0
                    && doc
                        .char_at(probe - 1)
                        .map(|c| char_is_whitespace!(c))
                        .unwrap_or(false)
                {
                    probe -= 1;
                }
                let after_new = probe >= 3
                    && doc.string_back(probe - 1, 3) == "new"
                    && (probe < 4
                        || !doc
                            .char_at(probe - 4)
                            .map(|c| char_is_peko_id_eligible!(c))
                            .unwrap_or(false));

                ModuleAccessReplace {
                    start: module_path_start,
                    after_new,
                }
            });
            SymbolSearchResult::Access(available_symbols, module_replace)
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
        // A change to this file may also change any file that imports it, so
        // drop the whole simulation memo rather than just this path's entry.
        if let Ok(mut cache) = self.simulation_cache.lock() {
            cache.clear();
        }
    }

    fn close_file(&mut self, path: &Path) {
        self.tracked_files.remove(path);
        if let Ok(mut cache) = self.simulation_cache.lock() {
            cache.remove(path);
        }
    }

    fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        let Some(simulation_result) = self.simulate_document(path) else {
            return Vec::new();
        };

        // Only surface diagnostics that belong to the file being analyzed.
        // Simulating a file also simulates its imports (std and other
        // packages); those diagnostics reference other files, and publishing
        // them under this file's URI would misplace them onto unrelated lines.
        let analyzed = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        simulation_result
            .diagnostics
            .iter()
            .filter(|peko_diagnostic| {
                peko_diagnostic
                    .file
                    .canonicalize()
                    .unwrap_or_else(|_| peko_diagnostic.file.clone())
                    == analyzed
            })
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
            SymbolSearchResult::Access(symbols, _)
            | SymbolSearchResult::Symbols(symbols)
            | SymbolSearchResult::Types(symbols) => symbols,
            SymbolSearchResult::Visibilities
            | SymbolSearchResult::ImportModules(_)
            | SymbolSearchResult::EnumVariants(_)
            | SymbolSearchResult::OverrideMethods(_)
            | SymbolSearchResult::None => Vec::new(),
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

        let search = self.get_symbols_at(path, position);

        // An `import package::` path offers the package's module names as plain
        // completions, with none of the expression-context snippets below.
        if let SymbolSearchResult::ImportModules(module_names) = search {
            return module_names
                .into_iter()
                .map(|name| CompletionItem {
                    label: name,
                    kind: CompletionKind::Module,
                    detail: None,
                    documentation: None,
                    insert_text: None,
                    sort_text: Some("0001".to_string()),
                    insert_text_format: None,
                    command: None,
                    additional_text_edits: Vec::new(),
                })
                .collect();
        }

        // A method-declaration position inside a deriving class offers the
        // derived traits' methods as override stubs. The `fn` keyword is
        // already typed, so the insertion begins at the method name.
        if let SymbolSearchResult::OverrideMethods(methods) = search {
            return methods
                .into_iter()
                .map(|method| {
                    let arguments = method
                        .arguments
                        .iter()
                        .map(|(name, argument_type)| format!("{name}: {argument_type}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let return_suffix = if method.return_type == "void" {
                        String::new()
                    } else {
                        format!(" => {}", method.return_type)
                    };
                    CompletionItem {
                        label: method.name.clone(),
                        kind: CompletionKind::Method,
                        detail: Some("override".to_string()),
                        documentation: None,
                        insert_text: Some(format!(
                            "{}({arguments}){return_suffix} {{\n\t${{0}}\n}}",
                            method.name
                        )),
                        sort_text: Some("0000".to_string()),
                        insert_text_format: Some(InsertTextFormat::Snippet),
                        command: None,
                        additional_text_edits: Vec::new(),
                    }
                })
                .collect();
        }

        // An `EnumName::` access offers the enum's variants as plain
        // completions.
        if let SymbolSearchResult::EnumVariants(variants) = search {
            return variants
                .into_iter()
                .map(|variant| CompletionItem {
                    label: variant,
                    kind: CompletionKind::EnumMember,
                    detail: None,
                    documentation: None,
                    insert_text: None,
                    sort_text: Some("0001".to_string()),
                    insert_text_format: None,
                    command: None,
                    additional_text_edits: Vec::new(),
                })
                .collect();
        }

        let (
            available_symbols,
            list_builtin_types,
            list_visibilities,
            accessing_symbols,
            module_replace,
        ) = match search {
            SymbolSearchResult::Access(symbols, replace) => {
                (symbols, false, false, true, replace)
            }
            SymbolSearchResult::Symbols(symbols) => (symbols, false, false, false, None),
            SymbolSearchResult::Types(symbols) => (symbols, true, false, false, None),
            SymbolSearchResult::Visibilities => (Vec::new(), false, true, false, None),
            SymbolSearchResult::ImportModules(_)
            | SymbolSearchResult::EnumVariants(_)
            | SymbolSearchResult::OverrideMethods(_)
            | SymbolSearchResult::None => (Vec::new(), false, false, false, None),
        };

        // Whether the cursor already follows a `new` keyword (`new Foo|`), so a
        // class-instantiation completion does not prepend a second `new`.
        let after_new = {
            let mut probe = doc.offset_at(position);
            while probe > 0
                && doc
                    .char_at(probe.saturating_sub(1))
                    .map(|c| char_is_peko_id_eligible!(c))
                    .unwrap_or(false)
            {
                probe -= 1;
            }
            while probe > 0
                && doc
                    .char_at(probe.saturating_sub(1))
                    .map(|c| char_is_whitespace!(c))
                    .unwrap_or(false)
            {
                probe -= 1;
            }
            // After the whitespace walk `probe` sits on the whitespace before
            // the preceding word, so that word ends at `probe - 1`.
            let boundary_ok = probe < 4
                || !doc
                    .char_at(probe - 4)
                    .map(|c| char_is_peko_id_eligible!(c))
                    .unwrap_or(false);
            probe >= 3 && doc.string_back(probe.saturating_sub(1), 3) == "new" && boundary_ok
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
                            // In an open expression position a class name is an
                            // object instantiation, so prepend `new` (unless the
                            // cursor already follows `new`, or this is a type or
                            // `mod::` access position).
                            let mut insert =
                                if !list_builtin_types && !accessing_symbols && !after_new {
                                    format!("new {}", symbol_class.name)
                                } else {
                                    symbol_class.name.clone()
                                };
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
                    // Traits and enums are named types. In a type position
                    // (annotations, `from` / `impl` clauses) the bare name is
                    // wanted. In an expression position a trailing `::` opens
                    // the type's members (enum variants, static trait methods).
                    "trait" | "enum" => {
                        if list_builtin_types {
                            (Some(symbol.get_name()), None, None)
                        } else {
                            (
                                Some(format!("{}::", symbol.get_name())),
                                None,
                                Some(Command {
                                    title: "resuggest".to_string(),
                                    command: "editor::ShowCompletions".to_string(),
                                }),
                            )
                        }
                    }
                    // Modules: a trailing `::` opens the module's members.
                    _ => (
                        Some(format!("{}::", symbol.get_name())),
                        None,
                        Some(Command {
                            title: "resuggest".to_string(),
                            command: "editor::ShowCompletions".to_string(),
                        }),
                    ),
                };

                // Under a `module::` access, a class instantiation needs a
                // leading `new`. The main insertion still replaces the typed
                // identifier after `::`; a separate edit inserts `new ` before
                // the module path so the result reads `new module::Class(...)`.
                // Skip it when a `new` already precedes the path, and for
                // non-class symbols.
                let additional_text_edits = match (symbol.get_kind(), &module_replace) {
                    ("class" | "class-generic", Some(replace)) if !replace.after_new => {
                        let at = create_position(&doc.position_data_at_byte(replace.start, path));
                        vec![CompletionTextEdit {
                            range: Range {
                                start: at.clone(),
                                end: at,
                            },
                            new_text: "new ".to_string(),
                        }]
                    }
                    _ => Vec::new(),
                };

                CompletionItem {
                    label: symbol.get_name(),
                    kind: match symbol.get_kind() {
                        "variable" => CompletionKind::Variable,
                        "attribute" => CompletionKind::Field,
                        "function" | "function-generic" => CompletionKind::Function,
                        "class" | "class-generic" => CompletionKind::Class,
                        "enum" => CompletionKind::Enum,
                        "trait" => CompletionKind::Interface,
                        _ => CompletionKind::Module,
                    },
                    detail: None,
                    documentation: symbol.get_doc_info().map(|d| d.description.clone()),
                    insert_text,
                    sort_text: Some("0001".to_string()),
                    insert_text_format,
                    command,
                    additional_text_edits,
                }
            })
            .collect();

        let keyword_symbols: &[&str] = if list_builtin_types {
            &[
                "i1", "i8", "i16", "i32", "i64", "i128", "f16", "f32", "f64", "cstr", "opaque",
                "void", "pointer", "number", "string", "bool", "char",
            ]
        } else if list_visibilities {
            &[
                "public",
                "private",
                "mutates",
                "static",
                "serial",
                "external",
                "notrack",
                "gcsafe",
                "constant",
                "state",
                "variadic",
                "blockexit",
                "hide",
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
                additional_text_edits: Vec::new(),
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
                    sort_text: Some("0009".to_string()),
                    insert_text_format: Some(InsertTextFormat::Snippet),
                    command: None,
                    additional_text_edits: Vec::new(),
                });
            }
        }

        // Statement and block snippets belong only in an open code position,
        // not while completing a type, a visibility attribute, or a `mod::`
        // member access.
        if !list_builtin_types && !list_visibilities && !accessing_symbols {
        // Language keywords that are not already offered as a block snippet.
        // Sorted after symbols but before the snippets and XML tags.
        for keyword in LANGUAGE_KEYWORDS {
            completion_items.push(CompletionItem {
                label: (*keyword).to_string(),
                kind: CompletionKind::Keyword,
                detail: None,
                documentation: None,
                insert_text: None,
                sort_text: Some("0008".to_string()),
                insert_text_format: None,
                command: None,
                additional_text_edits: Vec::new(),
            });
        }

        // Block-shaped snippets that all follow the same `keyword name { body }` form.
        for simple_snip in ["if", "while", "class", "module", "trait", "enum"] {
            completion_items.push(CompletionItem {
                label: simple_snip.to_string(),
                kind: CompletionKind::Snippet,
                detail: None,
                documentation: None,
                insert_text: Some(format!("{simple_snip} ${{1}} {{\n\t${{0}}\n}}")),
                sort_text: Some("0009".to_string()),
                insert_text_format: Some(InsertTextFormat::Snippet),
                command: None,
                additional_text_edits: Vec::new(),
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
                sort_text: Some("0009".to_string()),
                insert_text_format: Some(InsertTextFormat::Snippet),
                command: None,
                additional_text_edits: Vec::new(),
            });
        }

        // `for x in y { body }`.
        completion_items.push(CompletionItem {
            label: "for".to_string(),
            kind: CompletionKind::Snippet,
            detail: None,
            documentation: None,
            insert_text: Some("for ${1} in ${2} {\n\t${0}\n}".to_string()),
            sort_text: Some("0009".to_string()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            command: None,
            additional_text_edits: Vec::new(),
        });

        // `fn name(args) { body }`.
        completion_items.push(CompletionItem {
            label: "fn".to_string(),
            kind: CompletionKind::Snippet,
            detail: None,
            documentation: None,
            insert_text: Some("fn ${1}(${2}) {\n\t${0}\n}".to_string()),
            sort_text: Some("0009".to_string()),
            insert_text_format: Some(InsertTextFormat::Snippet),
            command: None,
            additional_text_edits: Vec::new(),
        });
        }

        completion_items
    }

    fn signature_help(&self, path: &Path, position: &Position) -> Option<SignatureHelp> {
        let doc = self.doc(path)?;
        let mut simulation_result = self.simulate_document(path)?;

        let peko_position = doc.position_data_at_byte(doc.offset_at(position), path);

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
            SymbolSearchResult::Access(symbols, _)
            | SymbolSearchResult::Symbols(symbols)
            | SymbolSearchResult::Types(symbols) => symbols,
            SymbolSearchResult::Visibilities
            | SymbolSearchResult::ImportModules(_)
            | SymbolSearchResult::EnumVariants(_)
            | SymbolSearchResult::OverrideMethods(_)
            | SymbolSearchResult::None => Vec::new(),
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

    fn format(
        &self,
        path: &Path,
        text: &str,
        indent_size: usize,
        use_spaces: bool,
    ) -> Option<String> {
        // Honor the editor's indentation preference. A tab-indenting editor gets
        // a single tab per level; a space-indenting editor gets its tab width.
        let indent_unit = if use_spaces {
            " ".repeat(indent_size.max(1))
        } else {
            "\t".to_string()
        };
        let config = peko_core::formatter::data_structures::FormatConfig {
            indent_unit,
            ..peko_core::formatter::data_structures::FormatConfig::default()
        };
        let formatted = peko_core::formatter::format_source(text, path, config);
        if formatted == text {
            None
        } else {
            Some(formatted)
        }
    }
}

// ---------------------------------------------------------------------------
// Keyword lists for completion suggestions
// ---------------------------------------------------------------------------

/// Language keywords offered as plain completions in open code positions.
/// Keywords that already have a block snippet (`if`, `while`, `for`, `fn`,
/// `class`, `module`, `trait`, `enum`, `platform`, `arch`) are omitted here so
/// they are not offered twice. Visibility keywords and builtin type names have
/// their own context-specific lists.
const LANGUAGE_KEYWORDS: &[&str] = &[
    "let",
    "const",
    "constant",
    "new",
    "from",
    "impl",
    "return",
    "else",
    "switch",
    "break",
    "continue",
    "import",
    "as",
    "in",
    "danger_cast",
    "this",
    "self",
    "Self",
    "super",
    "true",
    "false",
    "null",
    "None",
];

/// The set of well-known HTML tag names recognized in PekoX, offered in the
/// open, type, visibility, and object-access contexts. These get wrapped in
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

#[cfg(test)]
mod tests {
    use super::{class_clause_kind, is_method_decl_position};

    #[test]
    fn class_clause_detection() {
        // `from` clause offers superclasses and traits.
        assert_eq!(class_clause_kind("class Foo from Ba"), Some(false));
        // `impl` clause offers traits only.
        assert_eq!(class_clause_kind("class Foo impl Ba"), Some(true));
        // The last clause on the line wins.
        assert_eq!(class_clause_kind("class Foo from A impl B"), Some(true));
        assert_eq!(class_clause_kind("class Foo from A, "), Some(false));
        // Not a class header.
        assert_eq!(class_clause_kind("let y = transform"), None);
        // Body already opened.
        assert_eq!(class_clause_kind("class Foo from A { fn "), None);
    }

    #[test]
    fn method_decl_position_detection() {
        assert!(is_method_decl_position("    fn "));
        assert!(is_method_decl_position("\tfn to_str"));
        assert!(!is_method_decl_position("    fn foo("));
        assert!(!is_method_decl_position("    let x"));
        // `fn` must stand as its own word.
        assert!(!is_method_decl_position("    fnfoo"));
    }

}

#[cfg(test)]
mod mutates_tests {
    use super::PekoAnalyzer;
    use crate::server::analysis::AnalysisEngine;
    use std::path::PathBuf;

    /// Returns whether `Class::method` is marked `[mutates]` after simulating
    /// `source` through the analyzer. `None` if the analyzer cannot start (no
    /// `PEKO_ROOT_PATH`) so the test skips cleanly.
    fn method_mutates(source: &str, class: &str, method: &str) -> Option<bool> {
        let mut analyzer = PekoAnalyzer::new()?;
        let path = PathBuf::from("/tmp/mutates_test.peko");
        analyzer.update_file(&path, source);
        let result = analyzer.simulate_document(&path)?;
        let module = result.context.module_context.top_level_modules["main"]
            .read()
            .unwrap();
        let class_ref = module.classes.get(class)?.read().unwrap();
        let overloads = class_ref.main_virtual_table.methods.get(method)?;
        Some(
            overloads
                .iter()
                .any(|overload| overload.read().unwrap().visibility.mutates),
        )
    }

    const SOURCE: &str = r#"
[public] trait Bumper {
    [mutates] fn bump();
}

[public] class Counter impl Bumper {
    count: i64;

    constructor() {
        count = danger_cast<i64>(0);
    }

    [mutates] fn bump() {
        this.count = this.count + danger_cast<i64>(1);
    }

    fn calls_this_bump() {
        this.bump();
    }

    fn bare_call() {
        bump();
    }

    fn explicit_assign() {
        this.count = danger_cast<i64>(5);
    }

    fn calls_forward() {
        this.forward_inferred();
    }

    fn forward_inferred() {
        this.count = danger_cast<i64>(9);
    }

    fn read_only() {
        let x: i64 = this.count;
    }
}
"#;

    #[test]
    fn mutates_inferred_through_this_method_call() {
        let Some(mutates) = method_mutates(SOURCE, "Counter", "calls_this_bump") else {
            return;
        };
        assert!(mutates, "`this.bump()` should propagate [mutates]");
    }

    #[test]
    fn mutates_inferred_through_bare_call() {
        let Some(mutates) = method_mutates(SOURCE, "Counter", "bare_call") else {
            return;
        };
        assert!(mutates, "a bare `bump()` call should propagate [mutates]");
    }

    #[test]
    fn mutates_inferred_from_explicit_this_assignment() {
        let Some(mutates) = method_mutates(SOURCE, "Counter", "explicit_assign") else {
            return;
        };
        assert!(mutates, "`this.count = ...` should infer [mutates]");
    }

    #[test]
    fn mutates_inferred_forward_via_fixpoint() {
        let Some(mutates) = method_mutates(SOURCE, "Counter", "calls_forward") else {
            return;
        };
        assert!(
            mutates,
            "calling a method whose [mutates] is inferred later must propagate via the fixpoint"
        );
    }

    #[test]
    fn non_mutating_method_stays_pure() {
        let Some(mutates) = method_mutates(SOURCE, "Counter", "read_only") else {
            return;
        };
        assert!(!mutates, "a read-only method must not be marked [mutates]");
    }
}

#[cfg(test)]
mod generic_bound_tests {
    use super::PekoAnalyzer;
    use crate::server::analysis::{AnalysisEngine, Position};
    use std::path::PathBuf;

    /// Completion labels at the `|` marker in `source`. `None` if the analyzer
    /// cannot start (no PEKO_ROOT_PATH).
    fn completions_at(source: &str) -> Option<Vec<String>> {
        let offset = source.find('|').expect("marker");
        let before = &source[..offset];
        let line = before.matches('\n').count() as u32;
        let character = (before.len() - before.rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
        let clean = source.replace('|', "");

        let mut analyzer = PekoAnalyzer::new()?;
        let path = PathBuf::from("/tmp/generic_bound_test.peko");
        analyzer.update_file(&path, &clean);
        let items = analyzer.completions(&path, &Position { line, character });
        Some(items.into_iter().map(|item| item.label).collect())
    }

    #[test]
    fn member_completion_through_impl_bound() {
        let source = r#"
[public] trait Drawable {
    fn draw() => i64;
    fn area() => i64;
}

[public] fn render<T: impl Drawable>(item: T) => i64 {
    return item.|;
}
"#;
        let Some(labels) = completions_at(source) else { return };
        assert!(
            labels.iter().any(|l| l == "draw"),
            "expected `draw` from the impl Drawable bound; got {labels:?}"
        );
        assert!(labels.iter().any(|l| l == "area"), "expected `area`; got {labels:?}");
    }

    #[test]
    fn member_completion_through_multiple_impl_bounds() {
        let source = r#"
[public] trait Drawable {
    fn draw() => i64;
}

[public] trait Named {
    fn label() => i64;
}

[public] fn render<T: impl Drawable, impl Named>(item: T) => i64 {
    return item.|;
}
"#;
        let Some(labels) = completions_at(source) else { return };
        assert!(
            labels.iter().any(|l| l == "draw"),
            "expected `draw` from the first bound; got {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "label"),
            "expected `label` from the second bound; got {labels:?}"
        );
    }

    #[test]
    fn member_completion_on_this_attribute_of_generic_type() {
        let source = r#"
[public] trait Increment {
    fn increment() => i64;
}

[public] class Wrapper<T: impl Increment> {
    dat: T

    constructor(value: T) {
        this.dat = value
    }

    [public] fn bump() => i64 {
        return this.dat.|;
    }
}
"#;
        let Some(labels) = completions_at(source) else { return };
        assert!(
            labels.iter().any(|l| l == "increment"),
            "expected `increment` on `this.dat` (attribute of generic type T); got {labels:?}"
        );
    }

    #[test]
    fn member_completion_through_from_bound() {
        let source = r#"
[public] class Shape {
    [public] size: i64

    constructor() {
        let zero: i64 = constant<i64>(0)
        this.size = zero
    }

    [public] fn describe() => i64 {
        return this.size
    }
}

[public] fn render<T: from Shape>(item: T) => i64 {
    return item.|;
}
"#;
        let Some(labels) = completions_at(source) else { return };
        assert!(
            labels.iter().any(|l| l == "describe"),
            "expected method `describe` from the from Shape bound; got {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l == "size"),
            "expected attribute `size` from the from Shape bound; got {labels:?}"
        );
    }

    /// Diagnostic messages the analyzer reports for `source`. `None` if the
    /// analyzer cannot start (no PEKO_ROOT_PATH).
    fn diagnostics_for(source: &str) -> Option<Vec<String>> {
        let mut analyzer = PekoAnalyzer::new()?;
        let path = PathBuf::from("/tmp/generic_bound_diag_test.peko");
        analyzer.update_file(&path, source);
        Some(
            analyzer
                .diagnostics(&path)
                .into_iter()
                .map(|diagnostic| diagnostic.message)
                .collect(),
        )
    }

    #[test]
    fn bound_enforced_rejects_non_conforming_type() {
        let source = r#"
[public] trait Drawable {
    fn draw() => i64;
}

[public] class Widget {
    constructor() {}

    [public] fn spin() => i64 {
        let one: i64 = constant<i64>(1)
        return one
    }
}

[public] fn render<T: impl Drawable>(item: T) => i64 {
    let zero: i64 = constant<i64>(0)
    return zero
}

[public] fn main() {
    let w: Widget = new Widget()
    render<Widget>(w)
}
"#;
        let Some(messages) = diagnostics_for(source) else { return };
        assert!(
            messages.iter().any(|m| m.contains("does not satisfy the bound")),
            "a type that does not implement the trait must fail the bound; got {messages:?}"
        );
    }

    #[test]
    fn bound_satisfied_accepts_conforming_type() {
        let source = r#"
[public] trait Drawable {
    fn draw() => i64;
}

[public] class Widget impl Drawable {
    constructor() {}

    [public] fn draw() => i64 {
        let one: i64 = constant<i64>(1)
        return one
    }
}

[public] fn render<T: impl Drawable>(item: T) => i64 {
    let zero: i64 = constant<i64>(0)
    return zero
}

[public] fn main() {
    let w: Widget = new Widget()
    render<Widget>(w)
}
"#;
        let Some(messages) = diagnostics_for(source) else { return };
        assert!(
            !messages.iter().any(|m| m.contains("does not satisfy the bound")),
            "a type that implements the trait must satisfy the bound; got {messages:?}"
        );
    }

    #[test]
    fn class_bound_enforced_rejects_non_conforming_type() {
        let source = r#"
[public] trait Speaker {
    fn sound() => i64;
}

[public] class Rock {
    constructor() {}
}

[public] class Holder<T: impl Speaker> {
    item: T

    constructor(value: T) {
        this.item = value
    }
}

[public] fn main() {
    let r: Rock = new Rock()
    let h: Holder<Rock> = new Holder<Rock>(r)
}
"#;
        let Some(messages) = diagnostics_for(source) else { return };
        assert!(
            messages.iter().any(|m| m.contains("does not satisfy the bound")),
            "a generic class argument that does not implement the trait must fail the bound; got {messages:?}"
        );
    }

    #[test]
    fn class_bound_satisfied_accepts_conforming_type() {
        let source = r#"
[public] trait Speaker {
    fn sound() => i64;
}

[public] class Dog impl Speaker {
    constructor() {}

    [public] fn sound() => i64 {
        let one: i64 = constant<i64>(1)
        return one
    }
}

[public] class Holder<T: impl Speaker> {
    item: T

    constructor(value: T) {
        this.item = value
    }
}

[public] fn main() {
    let d: Dog = new Dog()
    let h: Holder<Dog> = new Holder<Dog>(d)
}
"#;
        let Some(messages) = diagnostics_for(source) else { return };
        assert!(
            !messages.iter().any(|m| m.contains("does not satisfy the bound")),
            "a generic class argument that implements the trait must satisfy the bound; got {messages:?}"
        );
    }
}

#[cfg(test)]
mod format_tests {
    use super::PekoAnalyzer;
    use crate::server::analysis::AnalysisEngine;
    use std::path::PathBuf;

    /// The LSP format path (the exact method the editor calls) must honor the
    /// requested indentation and never add a stray space. Gated on
    /// PEKO_ROOT_PATH like the other engine-driven tests.
    #[test]
    fn lsp_format_honors_requested_indent() {
        let Some(analyzer) = PekoAnalyzer::new() else {
            return;
        };
        let path = PathBuf::from("/tmp/format_indent_test.peko");
        let messy = "class W {\ncount: i64\nfn bump() {\nthis.count = this.count\n}\n}\n";

        // Four-space indentation: members at four spaces, the body at eight.
        let four = analyzer.format(&path, messy, 4, true).expect("formats");
        assert!(four.contains("\n    count: i64;"), "want 4-space member indent: {four:?}");
        assert!(four.contains("\n        this.count"), "want 8-space body indent: {four:?}");
        // No indent may carry a stray extra space (the reported bug).
        assert!(!four.contains("\n     count"), "unexpected 5-space indent: {four:?}");
        assert!(!four.contains("\n         this.count"), "unexpected 9-space indent: {four:?}");

        // Two-space indentation, honoring the editor preference.
        let two = analyzer.format(&path, messy, 2, true).expect("formats");
        assert!(two.contains("\n  count: i64;"), "want 2-space member indent: {two:?}");

        // Tab indentation when the editor does not insert spaces.
        let tabbed = analyzer.format(&path, messy, 4, false).expect("formats");
        assert!(tabbed.contains("\n\tcount: i64;"), "want tab member indent: {tabbed:?}");
    }
}
