//! Temporary local test harness for the new package resolution.
//!
//! Activated by the global `--testtmp` flag, which short-circuits normal
//! dispatch in `main`. It parses the project config, resolves dependencies
//! offline, builds the lock-scoped package index, and simulates importing each
//! path dependency by name, printing a report at every step. This is a scaffold
//! for local testing of resolution without the standard library, and is removed
//! once resolution is validated.

use std::collections::HashMap;
use std::path::Path;
use std::process::ExitCode;

use peko_core::ExternalModuleInfo;
use peko_core::asts::PekoAST;
use peko_core::asts::data_structures::PositionData;
use peko_core::config::{Dependency, LoadedManifest, Manifest};
use peko_core::diagnostics::{DiagnosticList, DiagnosticType};
use peko_core::lexer::TokenList;
use peko_core::packages::PekoPackageIndex;
use peko_core::parser::PekoParser;
use peko_core::simulator::PekoValueSimulator;
use peko_core::simulator::context::PekoSimulatorContext;
use peko_core::target::PekoTarget;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::PekoProject;
use crate::registry::install;

/// Run the temporary resolution test harness from the current directory.
pub async fn run(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // ----- Config -----------------------------------------------------------
    println!("== testtmp: project config ==");
    let loaded = match Manifest::discover(&cwd) {
        Ok(loaded) => loaded,
        Err(e) => {
            reporter.error(format!("could not load peko.toml: {e}"));
            return ExitCode::FAILURE;
        }
    };
    print_config(&loaded);

    match PekoProject::from_current_directory() {
        Ok(project) => {
            println!("  project model:");
            println!("    root  = {}", project.get_root().display());
            println!("    entry = {}", project.get_entrypoint().display());
            println!("    ui    = {}", project.ui_project_info.is_some());
        }
        Err(e) => println!("  project model: could not build ({e})"),
    }

    // ----- Resolve + lock ---------------------------------------------------
    println!();
    println!("== testtmp: resolve + lock ==");
    let progress = reporter.progress();
    progress.start_phase("Resolving dependencies");
    let lock_result = install::update(cli_info.get_peko_root(), &loaded, progress).await;
    progress.finish_phase();

    let lockfile = match lock_result {
        Ok(lockfile) => lockfile,
        Err(e) => {
            reporter.error(format!("resolution failed: {e}"));
            return ExitCode::FAILURE;
        }
    };
    if lockfile.packages.is_empty() {
        println!("  (no dependencies locked)");
    }
    for package in &lockfile.packages {
        let path = package
            .path
            .as_ref()
            .map(|dir| format!(" path={}", dir.display()))
            .unwrap_or_default();
        println!(
            "  {} {} [{:?}]{path}",
            package.name, package.version, package.source
        );
    }

    // ----- Lock-scoped discovery -------------------------------------------
    println!();
    println!("== testtmp: lock-scoped discovery ==");
    let modules =
        PekoPackageIndex::from_lockfile(cli_info.get_peko_root(), &loaded.root, &lockfile)
            .get_external_modules();
    if modules.is_empty() {
        println!("  (no modules discovered)");
    }
    let mut names: Vec<&String> = modules.keys().collect();
    names.sort();
    for name in &names {
        for version in &modules[*name].versions {
            let entry = version.source_root.join(&version.entry_file);
            let status = if entry.is_file() { "ok" } else { "MISSING" };
            println!("  {name} {} -> {} [{status}]", version.version, entry.display());
        }
    }

    // ----- Import simulation ------------------------------------------------
    println!();
    println!("== testtmp: import simulation ==");

    // A positional argument is a custom import to probe (a full statement, or
    // a shorthand module path / symbol unpack). Without one, every path
    // dependency is imported by its bare name.
    let all_resolved = if let Some(argument) = cli_info.arguments.first() {
        let source = import_source(argument);
        println!("  simulating: {}", source.trim_end());
        simulate_source(&loaded.root, &modules, "custom", &source)
    } else {
        let path_deps: Vec<&String> = loaded
            .manifest
            .dependencies()
            .iter()
            .filter(|(_, dep)| matches!(dep, Dependency::Path { .. }))
            .map(|(name, _)| name)
            .collect();
        if path_deps.is_empty() {
            println!(
                "  (no path dependencies; pass an import to probe, e.g. \
                 peko --testtmp \"import {{ Sym }} from Pkg;\")"
            );
            return ExitCode::SUCCESS;
        }
        let mut resolved = true;
        for name in path_deps {
            resolved &= simulate_source(&loaded.root, &modules, name, &format!("import {name};\n"));
        }
        resolved
    };

    if all_resolved {
        reporter.success("testtmp: all imports resolved");
        ExitCode::SUCCESS
    } else {
        reporter.error("testtmp: some imports failed to resolve");
        ExitCode::FAILURE
    }
}

/// Turn a `--testtmp` argument into an import statement.
///
/// A value starting with `import` is taken as a full statement; anything else
/// is wrapped as `import <value>`, so `TestPKG::sub` and `{ Foo } from TestPKG`
/// both work. A trailing `;` is added when missing.
fn import_source(argument: &str) -> String {
    let trimmed = argument.trim();
    let statement = if trimmed.starts_with("import") {
        trimmed.to_owned()
    } else {
        format!("import {trimmed}")
    };
    if statement.ends_with(';') {
        format!("{statement}\n")
    } else {
        format!("{statement};\n")
    }
}

/// Print the parsed manifest fields.
fn print_config(loaded: &LoadedManifest) {
    let manifest = &loaded.manifest;
    println!("  kind    = {:?}", manifest.kind());
    println!("  name    = {}", manifest.name());
    println!("  version = {}", manifest.version());
    println!("  root    = {}", loaded.root.display());

    let dependencies = manifest.dependencies();
    if dependencies.is_empty() {
        println!("  deps    = (none)");
    }
    for (name, dep) in dependencies {
        match dep {
            Dependency::Registry { version, .. } => {
                println!("  dep     = {name} = \"{version}\"");
            }
            Dependency::Path { path, .. } => {
                println!("  dep     = {name} = {{ path = \"{}\" }}", path.display());
            }
        }
    }
}

/// Simulate a source snippet against the lock-scoped module set, printing any
/// diagnostics and returning whether it resolved without errors.
fn simulate_source(
    project_root: &Path,
    modules: &HashMap<String, ExternalModuleInfo>,
    label: &str,
    source: &str,
) -> bool {
    let source_dir = project_root.join("source");
    let probe_file = source_dir.join("__testtmp_probe.peko");

    let (asts, parse_diagnostics) = parse_without_defaults(&probe_file, source);
    let end = asts
        .last()
        .map(|ast| ast.get_end().clone())
        .unwrap_or_else(|| PositionData::new(1, 0, 1, probe_file.clone()));

    let mut context = PekoSimulatorContext::new(PekoTarget::default(), probe_file, end, source_dir);
    context.external_modules = modules.clone();

    for ast in asts {
        ast.simulate(&mut context);
    }

    let mut errors = 0;
    for diagnostic in parse_diagnostics
        .get_diagnostics()
        .iter()
        .chain(context.diagnostics.get_diagnostics().iter())
    {
        if matches!(diagnostic.diagnostic_type, DiagnosticType::Error) {
            errors += 1;
            println!("  [{label}] error: {}", diagnostic.message);
        }
    }

    if errors == 0 {
        println!("  [{label}] import resolved");
        true
    } else {
        false
    }
}

/// Parse source into ASTs without the default-import prelude, so the harness
/// exercises resolution without pulling in the standard library.
fn parse_without_defaults(file: &Path, source: &str) -> (Vec<PekoAST>, DiagnosticList) {
    let file = file.to_path_buf();
    let mut parser = PekoParser::new(TokenList::from_source(source, &file), &file);

    let mut asts = Vec::new();
    while !parser.tokens.finished() {
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
        asts.push(parser.parse());
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
    }

    (asts, parser.get_diagnostics().clone())
}
