//! `peko compile`: compile a single Pekoscript file to an object or
//! linked binary.
//!
//! Unlike `peko build`, this command operates on a single source file
//! rather than a project. It's primarily useful as a debugging entry
//! point and as a backend for IDE integrations.

use std::path::PathBuf;
use std::process::ExitCode;

use peko_core::target::{Architecture, OperatingSystem, PekoTarget};
use peko_llvm::codegen::builders::modules::ModuleManager;

use crate::cli::reporting::Reporter;
use crate::cli::CLIInfo;
use crate::commands::toolchain_sysroot;
use crate::execution;
use crate::project::PekoProject;

/// Where the intermediate object file lives, either at an explicit
/// path the user gave (`--object --output=PATH`), or in a tempfile we
/// clean up when the command exits.
enum ObjectPathChoice {
    /// User asked for `--object` mode with an explicit `--output=...`.
    /// We write the object directly there and skip linking.
    ExplicitObject(PathBuf),
    /// User asked for `--object` mode without specifying an output;
    /// we default to `<cwd>/<source-basename>.o`.
    DefaultObject(PathBuf),
    /// Normal mode - we need an intermediate object that goes away
    /// once linking is done. Held as a `NamedTempFile` so it's removed
    /// on drop.
    Temporary(tempfile::NamedTempFile),
}

impl ObjectPathChoice {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::ExplicitObject(p) | Self::DefaultObject(p) => p,
            Self::Temporary(temp) => temp.path(),
        }
    }
}

/// Execute the `compile` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    // Source file is the first positional argument.
    let Some(source_arg) = cli_info.arguments.get(1) else {
        reporter.error("`compile` requires a source file");
        reporter.help(format!(
            "run '{} help compile' to see how this command works",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };

    let main_file = PathBuf::from(source_arg);
    if !main_file.exists() {
        reporter.error(format!(
            "source file '{}' does not exist",
            main_file.display()
        ));
        return ExitCode::FAILURE;
    }

    // Parse target os/arch flags. Default to the host target if not
    // supplied.
    let target_operating_system = match resolve_os(cli_info, reporter) {
        Some(os) => os,
        None => return ExitCode::FAILURE,
    };
    let target_architecture = match resolve_arch(cli_info, reporter) {
        Some(arch) => arch,
        None => return ExitCode::FAILURE,
    };

    // The `console` flag on PekoTarget controls Windows entrypoint
    // selection (WinMain vs. main). For `compile`, default to console
    // mode - UI apps go through `build`, not `compile`.
    let output_target = PekoTarget::new(
        target_operating_system.clone(),
        target_architecture.clone(),
        true,
    );

    // Resolve the linker output path (binary location). Only meaningful
    // when we're producing a binary, not an object.
    let output_path = match resolve_output_path(cli_info, reporter) {
        OutputPathResult::Path(p) => Some(p),
        OutputPathResult::None => None,
        OutputPathResult::Error => return ExitCode::FAILURE,
    };

    // Pick where the intermediate object will live.
    let object_choice = match decide_object_path(cli_info, &main_file, &output_path, reporter) {
        Some(choice) => choice,
        None => return ExitCode::FAILURE,
    };

    // Figure out the compilation root: the project root if the source
    // is inside one, otherwise the source's directory.
    let compilation_root = match resolve_compilation_root(&main_file, reporter) {
        Some(root) => root,
        None => return ExitCode::FAILURE,
    };

    let progress = reporter.progress();
    progress.start_phase(&format!("Compiling {}", main_file.display()));

    // Codegen.
    let compile_outcome = execution::compile(
        cli_info.get_peko_root(),
        output_target.clone(),
        main_file.clone(),
        compilation_root,
        object_choice.path().to_path_buf(),
    );

    progress.finish_phase();

    let mut compile_outcome = match compile_outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            reporter.error(format!("compilation failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // Emit IR if requested.
    if cli_info.flags.has_flag("emit-ir") {
        compile_outcome.codegen_context.emit_ir(
            compile_outcome.globals_set.clone(),
            format!("{}.ir", main_file.display()),
        );
    }

    // Render any diagnostics.
    reporter.report_diagnostics(&compile_outcome.diagnostics);

    if compile_outcome.diagnostics.get_error_count() > 0 {
        let count = compile_outcome.diagnostics.get_error_count();
        let plural = if count == 1 { "" } else { "s" };
        reporter.error(format!(
            "compilation of {} failed with {count} error{plural}",
            main_file.display()
        ));
        return ExitCode::FAILURE;
    }

    if cli_info.flags.has_flag("print-linked") {
        // The linked-files list goes to stdout (it's the data the user
        // is asking for; cli messages go to stderr via the reporter).
        for file in &compile_outcome.codegen_context.files_to_link {
            print!("{} ", file.display());
        }
        println!();
    }

    // Object-only mode: stop here. The object is at `object_choice`,
    // which is either the user's explicit path or the default
    // `<cwd>/<basename>.o`. Don't drop the temp variant in this path
    // since `--object` mode shouldn't have selected `Temporary`.
    if cli_info.flags.has_flag("object") {
        reporter.success(format!(
            "wrote object to {}",
            object_choice.path().display()
        ));
        return ExitCode::SUCCESS;
    }

    // Otherwise, link the object into a binary.
    let Some(sysroot) = toolchain_sysroot(
        cli_info.get_peko_root(),
        &target_operating_system,
        &target_architecture,
    ) else {
        reporter.error("unsupported target operating system or architecture");
        return ExitCode::FAILURE;
    };

    peko_llvm::linker::lld_link(
        output_target,
        object_choice.path().to_path_buf(),
        compile_outcome.codegen_context.files_to_link,
        sysroot,
        output_path,
        cli_info.flags.has_flag("shared"),
        None,
    );

    reporter.success(format!("compiled {}", main_file.display()));
    // `object_choice` dropping here removes the tempfile if it was the
    // `Temporary` variant.
    ExitCode::SUCCESS
}

/// Three-way outcome for the `--output` flag.
enum OutputPathResult {
    Path(PathBuf),
    None,
    Error,
}

/// Resolve the `--output=<path>` flag for the linker step. The flag is
/// optional; missing means "let the linker decide".
fn resolve_output_path(cli_info: &CLIInfo, reporter: &Reporter) -> OutputPathResult {
    if !cli_info.flags.has_flag("output") {
        return OutputPathResult::None;
    }
    let Some(value) = cli_info.flags.get_flag("output") else {
        reporter.error("'output' flag requires a value");
        reporter.help(format!(
            "run '{} help compile' to see how this command works",
            cli_info.executable
        ));
        return OutputPathResult::Error;
    };

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            reporter.error(format!("cannot read current directory: {e}"));
            return OutputPathResult::Error;
        }
    };
    OutputPathResult::Path(cwd.join(value))
}

/// Decide where the intermediate object file will live.
///
/// `--object` mode: in `<cwd>/<basename>.o` by default, or in the user's
/// `--output` path if both flags are set.
///
/// Otherwise: a `NamedTempFile` in the system temp directory that goes
/// away when the command exits.
fn decide_object_path(
    cli_info: &CLIInfo,
    main_file: &std::path::Path,
    output_path: &Option<PathBuf>,
    reporter: &Reporter,
) -> Option<ObjectPathChoice> {
    if cli_info.flags.has_flag("object") {
        if let Some(p) = output_path {
            return Some(ObjectPathChoice::ExplicitObject(p.clone()));
        }
        // Default object name: `<cwd>/<source-basename>.o`. The original
        // used `<source-full-path>.o` which produced weird paths for
        // sources outside cwd; using the basename is friendlier.
        let cwd = match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                reporter.error(format!("cannot read current directory: {e}"));
                return None;
            }
        };
        let stem = main_file
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "out".to_owned());
        return Some(ObjectPathChoice::DefaultObject(
            cwd.join(format!("{stem}.o")),
        ));
    }

    // Non-object mode: temp file. NamedTempFile creates it eagerly and
    // removes it on drop.
    match tempfile::Builder::new()
        .prefix("pekoobject")
        .suffix(".o")
        .tempfile()
    {
        Ok(temp) => Some(ObjectPathChoice::Temporary(temp)),
        Err(e) => {
            reporter.error(format!("could not create temporary object file: {e}"));
            None
        }
    }
}

/// Resolve the compilation root, which is the project root if the source file
/// is inside a project, otherwise it is the source's parent directory.
fn resolve_compilation_root(main_file: &std::path::Path, reporter: &Reporter) -> Option<PathBuf> {
    let canonical = match main_file.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            reporter.error(format!(
                "cannot canonicalize source path {}: {e}",
                main_file.display()
            ));
            return None;
        }
    };
    let parent = canonical
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    match PekoProject::from_directory(parent.clone()) {
        Ok(project) => Some(project.get_root().to_path_buf()),
        Err(_) => Some(parent),
    }
}

/// Parse `--os=<value>`, defaulting to the host target's operating
/// system. Returns `None` if the flag was set but invalid.
fn resolve_os(cli_info: &CLIInfo, reporter: &Reporter) -> Option<OperatingSystem> {
    if !cli_info.flags.has_flag("os") {
        return Some(PekoTarget::default().operating_system);
    }
    let Some(value) = cli_info.flags.get_flag("os") else {
        reporter.error("'os' flag requires a value");
        reporter.help(format!(
            "run '{} help compile' to see how this command works",
            cli_info.executable
        ));
        return None;
    };
    match OperatingSystem::from_name(&value) {
        OperatingSystem::Unknown => {
            reporter.error(format!("'{value}' is not a valid Operating System target"));
            reporter.help(format!(
                "run '{} help compile' to see how this command works",
                cli_info.executable
            ));
            None
        }
        os => Some(os),
    }
}

/// Parse `--arch=<value>`, defaulting to the host target's architecture.
/// Returns `None` if the flag was set but invalid.
fn resolve_arch(cli_info: &CLIInfo, reporter: &Reporter) -> Option<Architecture> {
    if !cli_info.flags.has_flag("arch") {
        return Some(PekoTarget::default().architecture);
    }
    let Some(value) = cli_info.flags.get_flag("arch") else {
        reporter.error("'arch' flag requires a value");
        reporter.help(format!(
            "run '{} help compile' to see how this command works",
            cli_info.executable
        ));
        return None;
    };
    match Architecture::from_name(&value) {
        Architecture::Unknown => {
            reporter.error(format!("'{value}' is not a valid CPU Architecture target"));
            reporter.help(format!(
                "run '{} help compile' to see how this command works",
                cli_info.executable
            ));
            None
        }
        arch => Some(arch),
    }
}
