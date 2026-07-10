//! Single-file compilation entry points.
//!
//! This module exposes the three "do one file" passes that sit one level
//! below a top-level subcommand:
//!
//! - [`compile`]: parse + codegen + link a `.peko` source into an object
//!   file at the target output path.
//! - [`test`]: parse + simulate a `.peko` source, returning the simulator
//!   context and accumulated diagnostics without writing anything.
//! - [`load_required_packages`]: parse + codegen *only* the default-import
//!   modules, returning their compiled top-level modules plus any extra
//!   files the linker will need. Used by commands that want the runtime /
//!   standard library available without compiling user code.
//!
//! Multi-file orchestration (the import graph walk, incremental rebuilds,
//! per-file progress) lives in [`incremental`]. Progress reporting is the
//! caller's responsibility: each function here represents a single unit of
//! work, so the caller installs a phase + ticks once per call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use peko_core::ExternalModuleInfo;
use peko_core::asts::PekoAST;
use peko_core::asts::data_structures::{PositionData, PositionedValue, UnpackItem};
use peko_core::asts::statements::ImportStatementAST;
use peko_core::diagnostics::{DiagnosticList, DiagnosticType, PekoDiagnostic};
use peko_core::error::PekoResult;
use peko_core::lexer::TokenList;
use peko_core::parser::PekoParser;
use peko_core::simulator::PekoValueSimulator;
use peko_core::simulator::context::PekoSimulatorContext;
use peko_core::target::PekoTarget;
use peko_llvm::codegen::PekoValueBuilder;
use peko_llvm::codegen::builders::prelude::{GlobalBuilder, ModuleManager};
use peko_llvm::codegen::context::PekoCodegenContext;
use peko_llvm::codegen::data_structures::CodegenModule;

pub mod incremental;
pub mod native;

/// Outcome of a successful (or partial) [`compile`] invocation.
///
/// Diagnostics from both parsing and codegen are merged into
/// [`diagnostics`](Self::diagnostics); the codegen context and globals
/// module are returned so the caller can inspect linkage state, the module
/// graph, or files to link.
pub struct CompileOutcome {
    pub codegen_context: PekoCodegenContext,
    pub diagnostics: DiagnosticList,
    pub globals_set: Arc<RwLock<CodegenModule>>,
}

/// Outcome of a [`test`] invocation. The simulator context is returned so
/// the caller can read out post-simulation state (scoped variables, module
/// tree, etc.) when needed.
pub struct TestOutcome {
    pub simulator_context: PekoSimulatorContext,
    pub diagnostics: DiagnosticList,
}

/// Build the implicit `import` statements that every Peko source file receives
/// at the top.
///
/// `std::core` and `std::collections` are unpacked so their names are used
/// bare; `std::runtime`, `std::xml`, and `std::json` are imported under their
/// prefix. Everything else needs an explicit import.
///
/// Returned as a `Vec<PekoAST>` so callers can prepend the list to a
/// freshly-parsed AST stream.
fn default_imports() -> Vec<PekoAST> {
    fn no_position(value: &str) -> PositionedValue<String> {
        PositionedValue::create_no_position(value.to_owned())
    }

    fn import(path: &str, alias: Option<&str>, unpack: Vec<UnpackItem>) -> PekoAST {
        PekoAST::ImportStatement(ImportStatementAST::new(
            PositionData::default(),
            PositionData::default(),
            path.split("::").map(no_position).collect(),
            alias.map(no_position),
            unpack,
            Option::None,
            false,
        ))
    }

    vec![
        import("std::core", None, vec![UnpackItem::All]),
        import("std::collections", None, vec![UnpackItem::All]),
        import("std::runtime", Some("runtime"), Vec::new()),
        import("std::xml", Some("xml"), Vec::new()),
        import("std::json", Some("json"), Vec::new()),
        import("std::bundle", Some("bundle"), Vec::new()),
    ]
}

/// Parse a Peko source file into ASTs, with the default-import block
/// prepended.
///
/// Returns the parsed ASTs plus any diagnostics the parser produced.
pub(super) fn parse_peko_source(file: PathBuf, source: String) -> (Vec<PekoAST>, DiagnosticList) {
    let mut parser = PekoParser::new(TokenList::from_source(&source, &file), &file);

    let mut parsed_asts = default_imports();

    // Walk the token stream until exhausted, skipping stray `;` / `}` that
    // tail-end a previous statement.
    while !parser.tokens.finished() || parser.has_pending() {
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

/// Build the external-module map a compile/test/codegen context resolves
/// imports against.
///
/// `peko_root` is the global Peko install root that holds the registry source
/// cache. `compilation_root` is the project root when called from a project
/// build, or `None` for an ad-hoc single-file compile. The map is scoped to the
/// project's `peko.lock`, so imports bind the locked versions; a project with
/// no lockfile resolves against nothing.
pub(crate) fn external_modules_for<P: AsRef<Path>>(
    peko_root: &Path,
    compilation_root: Option<P>,
) -> HashMap<String, ExternalModuleInfo> {
    let mut modules = HashMap::new();

    // Resolve the auto-imported `std` package from the installed registry cache
    // under the Peko root, rather than requiring it in every project lockfile.
    // This makes std available on every path, including load_required_packages
    // (which passes no compilation root). Remove once std is published and
    // locked normally.
    let installed_std =
        peko_core::packages::registry_source_dir(peko_root, "std", "0.1.0").join("peko.toml");
    if let Ok(loaded) = peko_core::config::Manifest::load(&installed_std) {
        let info = loaded.manifest.to_external_module(&loaded.root);
        modules.insert(info.module_name.clone(), info);
    }

    // Packages installed with `peko add --global` live in a shared manifest under
    // the Peko root and are importable from every project, ahead of project deps.
    let global_root = peko_root.join("global");
    if let Ok(Some(lockfile)) = peko_core::config::Lockfile::load_from_root(&global_root) {
        modules.extend(
            peko_core::packages::PekoPackageIndex::from_lockfile(
                peko_root,
                &global_root,
                &lockfile,
            )
            .get_external_modules(),
        );
    }

    if let Some(project_root) = compilation_root
        && let Ok(Some(lockfile)) =
            peko_core::config::Lockfile::load_from_root(project_root.as_ref())
    {
        modules.extend(
            peko_core::packages::PekoPackageIndex::from_lockfile(
                peko_root,
                project_root.as_ref(),
                &lockfile,
            )
            .get_external_modules(),
        );
    }

    modules
}

/// Parse a project component file into ASTs.
///
/// An FFI header (`.peko.h`) is parsed as a C interop surface and lowered to
/// external Peko declarations, matching the import path so an incrementally
/// recompiled header is handled the same way as on a full build. An ordinary
/// source file is parsed with the default-import prelude.
pub(super) fn parse_component_source(file: PathBuf) -> (Vec<PekoAST>, DiagnosticList) {
    let raw_source = std::fs::read_to_string(&file).unwrap();

    if !peko_core::ffi::is_ffi_header(&file) {
        return parse_peko_source(file, raw_source);
    }

    let mut diagnostics = DiagnosticList::new();
    let parsed = peko_core::ffi::parse_header(&raw_source);
    for error in &parsed.errors {
        diagnostics.report_diagnostic(PekoDiagnostic::new(
            PositionData::new(1, 0, 1, file.clone()),
            PositionData::new(1, 0, 1, file.clone()),
            format!("FFI header `{}`: {error}", file.display()),
            DiagnosticType::Error,
            file.clone(),
        ));
    }

    // The lowered header is a list of `[external]` declarations, so it parses
    // without the default-import prelude.
    let source = peko_core::ffi::header_to_peko_source(&parsed);
    let mut parser = PekoParser::new(TokenList::from_source(&source, &file), &file);
    let mut asts = Vec::new();
    while !parser.tokens.finished() || parser.has_pending() {
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
        if parser.tokens.finished() {
            break;
        }
        asts.push(parser.parse());
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
    }
    for diagnostic in parser.get_diagnostics().get_diagnostics() {
        diagnostics.report_diagnostic(diagnostic.clone());
    }

    (asts, diagnostics)
}

/// Assign the lock-scoped external-module map onto a context's
/// `external_modules` field.
///
/// A macro rather than a function so each call site can target a different
/// context type without naming the field's type.
macro_rules! load_external_modules {
    ($context:expr, $peko_root:expr, $compilation_root:expr $(,)?) => {{
        $context.external_modules =
            $crate::execution::external_modules_for($peko_root, $compilation_root);
    }};
}
pub(super) use load_external_modules;

/// Parse, codegen, and emit an object file for a single Peko source.
///
/// Does **not** perform incremental work (the entire source is parsed and
/// codegened from scratch, regardless of any prior build state). Callers
/// wanting incremental compilation should drive
/// [`incremental::compile_project`] instead.
///
/// The `windowsgui` flag on the codegen context is derived from
/// `target.console`: console-mode targets compile against the normal
/// entrypoint, GUI-mode targets compile against `WinMain` on Windows.
pub fn compile(
    peko_root: &Path,
    target: PekoTarget,
    main_file: PathBuf,
    compilation_root: PathBuf,
    output: PathBuf,
) -> PekoResult<CompileOutcome> {
    let (asts, mut diagnostics) = parse_peko_source(
        main_file.clone(),
        std::fs::read_to_string(&main_file).unwrap(),
    );

    let mut codegen_context =
        PekoCodegenContext::new(target, main_file.clone(), false, compilation_root.clone());

    load_external_modules!(codegen_context, peko_root, Some(&compilation_root));
    codegen_context.windowsgui = !target.console;

    // Codegen every top-level AST in three passes - shells, class signatures,
    // then bodies - so a class body can dispatch on any other class regardless
    // of source order.
    for ast in &asts {
        PekoValueBuilder::declare(ast, &mut codegen_context);
    }
    for ast in &asts {
        ast.declare_signatures(&mut codegen_context);
    }
    for ast in &asts {
        ast.build_value(&mut codegen_context);
    }
    // Emit the bodies of any generic instantiation laid out during the
    // signature pass, now that every class is laid out.
    codegen_context.drain_pending_generic_class_bodies();

    // Build the module containing all global-initializer functions.
    let globals_set = codegen_context.create_global_set_module();

    // Emit per-module globals initialization for each top-level module.
    let modules = codegen_context.module_context.top_level_modules.clone();
    for (_, module) in &modules {
        codegen_context.init_module_globals(module);
    }

    // Emit the final object if neither pass had errors.
    if !codegen_context.diagnostics.has_errors() && !diagnostics.has_errors() {
        codegen_context.output_binary(target, Arc::clone(&globals_set), output);
    }

    // Merge codegen diagnostics into the returned list.
    for error in codegen_context.diagnostics.get_diagnostics() {
        diagnostics.report_diagnostic(error.clone());
    }

    Ok(CompileOutcome {
        codegen_context,
        diagnostics,
        globals_set,
    })
}

/// Parse, simulate, and codegen a source snippet with no standard library,
/// returning the generated LLVM IR plus all diagnostics.
///
/// The `--astir` harness uses this to inspect a single language construct in
/// isolation. No default imports are prepended and no external modules are
/// loaded, so only primitive and FFI-level constructs codegen. A construct
/// that needs the standard library is out of scope for this path.
pub fn compile_snippet_to_ir(source: &str) -> (String, DiagnosticList) {
    let file = PathBuf::from("<astir>.peko");
    let work_dir = std::env::temp_dir();
    let mut diagnostics = DiagnosticList::new();

    // Parse with no default-import prelude.
    let mut parser = PekoParser::new(TokenList::from_source(source, &file), &file);
    let mut asts = Vec::new();
    while !parser.tokens.finished() || parser.has_pending() {
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
        if parser.tokens.finished() {
            break;
        }
        asts.push(parser.parse());
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
    }
    for diagnostic in parser.get_diagnostics().get_diagnostics() {
        diagnostics.report_diagnostic(diagnostic.clone());
    }

    // Simulate (type-check) without the standard library.
    let end = asts
        .last()
        .map(|ast| ast.get_end().clone())
        .unwrap_or_else(|| PositionData::new(1, 0, 1, file.clone()));
    let mut simulator =
        PekoSimulatorContext::new(PekoTarget::default(), file.clone(), end, work_dir.clone());
    for ast in &asts {
        PekoValueSimulator::declare(ast, &mut simulator);
    }
    for ast in &asts {
        ast.simulate(&mut simulator);
    }
    simulator.propagate_mutates_fixpoint();
    simulator.report_unused_symbols();
    for diagnostic in simulator.diagnostics.get_diagnostics() {
        diagnostics.report_diagnostic(diagnostic.clone());
    }

    // Codegen without the standard library. Some constructs (object
    // allocation, number and string objects) reach for the runtime, which this
    // harness omits, so codegen can panic on them. Run it under catch_unwind
    // and report rather than crashing, so the type-check results above still
    // surface.
    let asts_for_codegen = &asts;
    let codegen_file = file.clone();
    let codegen_dir = work_dir.clone();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let codegen_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut codegen_context = PekoCodegenContext::new(
            PekoTarget::default(),
            codegen_file.clone(),
            false,
            codegen_dir.clone(),
        );
        for ast in asts_for_codegen {
            PekoValueBuilder::declare(ast, &mut codegen_context);
        }
        for ast in asts_for_codegen {
            ast.declare_signatures(&mut codegen_context);
        }
        for ast in asts_for_codegen {
            ast.build_value(&mut codegen_context);
        }
        codegen_context.drain_pending_generic_class_bodies();
        let codegen_diagnostics = codegen_context.diagnostics.get_diagnostics().to_vec();

        // Emit just the main module's IR. The full link step pulls in the
        // runtime and standard library, which this harness deliberately omits.
        let ir_path = codegen_dir.join("peko_astir.ll");
        codegen_context
            .module_context
            .current_module()
            .write()
            .unwrap()
            .top_level_info
            .as_mut()
            .unwrap()
            .emit_ir(&ir_path);
        let ir = std::fs::read_to_string(&ir_path)
            .unwrap_or_else(|error| format!("(could not read IR: {error})"));

        (ir, codegen_diagnostics)
    }));
    std::panic::set_hook(previous_hook);

    let ir = match codegen_result {
        Ok((ir, codegen_diagnostics)) => {
            for diagnostic in codegen_diagnostics {
                diagnostics.report_diagnostic(diagnostic);
            }
            ir
        }
        Err(_) => String::from(
            "(codegen did not complete: this construct needs the runtime or standard library)",
        ),
    };

    (ir, diagnostics)
}

/// Simulate a snippet with the std package loaded and the implicit import
/// prelude prepended, returning the merged parse and analysis diagnostics.
///
/// Loads `std/peko.toml` relative to the working directory, registers it as
/// the `std` external module, and prepends [`default_imports`]. Used to verify
/// that std::core, optionals, and the bare `Object`/`Option` resolve end to
/// end at the analysis level, without the codegen and link steps.
pub fn simulate_snippet_with_std(source: &str) -> DiagnosticList {
    let mut diagnostics = DiagnosticList::new();
    let file = PathBuf::from("<astir-std>.peko");
    let work_dir = std::env::temp_dir();

    // Load the std package and register it as an external module.
    let mut external_modules = HashMap::new();
    match peko_core::config::Manifest::load("std/peko.toml") {
        Ok(loaded) => {
            let info = loaded.manifest.to_external_module(&loaded.root);
            external_modules.insert(info.module_name.clone(), info);
        }
        Err(error) => {
            eprintln!("could not load std/peko.toml: {error}");
        }
    }

    // Parse with the V2 import prelude prepended.
    let mut asts = default_imports();
    let mut parser = PekoParser::new(TokenList::from_source(source, &file), &file);
    while !parser.tokens.finished() || parser.has_pending() {
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
        if parser.tokens.finished() {
            break;
        }
        asts.push(parser.parse());
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
    }
    for diagnostic in parser.get_diagnostics().get_diagnostics() {
        diagnostics.report_diagnostic(diagnostic.clone());
    }

    // Simulate with the std modules available for import resolution.
    let end = asts
        .last()
        .map(|ast| ast.get_end().clone())
        .unwrap_or_else(|| PositionData::new(1, 0, 1, file.clone()));
    let mut simulator =
        PekoSimulatorContext::new(PekoTarget::default(), file.clone(), end, work_dir);
    simulator.external_modules = external_modules;
    // Header pass: register every declaration's name and signature so the body
    // pass below can resolve forward references regardless of order.
    for ast in &asts {
        PekoValueSimulator::declare(ast, &mut simulator);
    }
    for ast in &asts {
        ast.simulate(&mut simulator);
    }
    // Type-check every generic body once with its parameters erased to their
    // bound carriers (the erasure invariant).
    simulator.check_generics_erased();
    for diagnostic in simulator.diagnostics.get_diagnostics() {
        diagnostics.report_diagnostic(diagnostic.clone());
    }

    diagnostics
}

/// Codegen the default-import modules without compiling any user source.
///
/// Used by commands that need the runtime / standard library available as
/// compiled LLVM modules (for example, the linker pass needs to know what
/// the runtime defines so it can resolve cross-module symbols). The
/// `main` module is removed from the returned map because the caller will
/// be supplying its own.
#[allow(clippy::type_complexity)]
pub fn load_required_packages(
    peko_root: &Path,
    target: PekoTarget,
) -> PekoResult<(IndexMap<String, Arc<RwLock<CodegenModule>>>, Vec<PathBuf>)> {
    let asts = default_imports();

    let mut codegen_context = PekoCodegenContext::new(
        target,
        std::env::current_dir().unwrap(),
        false,
        PathBuf::new(),
    );
    codegen_context.creating_required = true;

    load_external_modules!(codegen_context, peko_root, Option::<&Path>::None);
    codegen_context.windowsgui = !target.console;

    for ast in asts {
        ast.build_value(&mut codegen_context);
    }

    let modules = codegen_context.module_context.top_level_modules.clone();
    for (_, module) in &modules {
        codegen_context.init_module_globals(module);
    }

    // Drop `main`, the caller will supply their own.
    codegen_context
        .module_context
        .top_level_modules
        .shift_remove("main");

    Ok((
        codegen_context.module_context.top_level_modules.clone(),
        codegen_context.files_to_link.clone(),
    ))
}

/// Parse and simulate a Peko source file, returning the simulator context
/// and accumulated diagnostics.
///
/// No object code is emitted. Use this for commands like `check` and
/// `test` that want type/semantic errors surfaced without doing codegen
/// or link work.
pub fn test(
    peko_root: &Path,
    target: PekoTarget,
    main_file: PathBuf,
    compilation_root: PathBuf,
) -> PekoResult<TestOutcome> {
    let (asts, mut diagnostics) = parse_peko_source(
        main_file.clone(),
        std::fs::read_to_string(&main_file).unwrap(),
    );

    // PositionData::new takes (column, index, line, file). When the source
    // parsed to zero ASTs (empty file), seed the context's "end" position
    // at line 1, column 1 of the source file so diagnostics still resolve
    // to a real location.
    let end_position = if asts.is_empty() {
        PositionData::new(1, 0, 1, main_file.clone())
    } else {
        asts.last().unwrap().get_end().clone()
    };

    let mut simulator_context = PekoSimulatorContext::new(
        target,
        main_file.clone(),
        end_position,
        main_file.parent().unwrap().to_path_buf(),
    );

    load_external_modules!(simulator_context, peko_root, Some(&compilation_root));

    // Header pass: register every top-level signature (functions, classes,
    // traits, enums) before checking any body, so a declaration can reference
    // another regardless of source order.
    for ast in &asts {
        PekoValueSimulator::declare(ast, &mut simulator_context);
    }

    for ast in asts {
        ast.simulate(&mut simulator_context);
    }

    // Propagate `[mutates]` across recorded call edges to a fixpoint, so a
    // method that calls another method inferred `[mutates]` only later in the
    // file is itself marked. The single simulation pass cannot see that
    // forward reference.
    simulator_context.propagate_mutates_fixpoint();

    // Report the user's diagnostics, but not those originating inside an
    // installed package under the Peko root (std and any resolved
    // dependency). A `peko test` of the user's file should surface the user's
    // errors, not a trusted dependency's internal analysis noise, which the
    // code generator does not surface either.
    let peko_root_canon = peko_root
        .canonicalize()
        .unwrap_or_else(|_| peko_root.to_path_buf());
    for error in simulator_context.diagnostics.get_diagnostics() {
        let error_file = error
            .file
            .canonicalize()
            .unwrap_or_else(|_| error.file.clone());
        if error_file.starts_with(&peko_root_canon) {
            continue;
        }
        diagnostics.report_diagnostic(error.clone());
    }

    Ok(TestOutcome {
        simulator_context,
        diagnostics,
    })
}
