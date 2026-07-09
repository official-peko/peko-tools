//! Per-subcommand entry points and dispatch table for the cli.
//!
//! Every subcommand is a value in [`ALL_COMMANDS`]. Each entry pairs a
//! short name (`"add"`, `"build"`, etc.) with its one-line summary,
//! its full help text (loaded at compile time from
//! `commands/help/<name>.txt`), and the async function that executes
//! it. Main looks up commands via [`lookup`] and invokes the stored
//! function pointer.
//!
//! Adding a new subcommand means three things:
//!
//! 1. Create `commands/<name>.rs` exposing
//!    `pub async fn execute(cli: &CLIInfo, reporter: &Reporter) ->
//!    ExitCode`.
//! 2. Create `commands/help/<name>.txt` with the help text.
//! 3. Add a `<name> => "<one-line summary>"` line to the
//!    [`commands!`] invocation below.
//!
//! `configure` and `install` from earlier cli iterations have been
//! removed.

use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;

use peko_core::target::OperatingSystem;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// One subcommand: identifier, one-line summary, full help text, and
/// the async function that runs it.
pub struct Command {
    pub name: &'static str,
    pub summary: &'static str,
    pub help: &'static str,
    pub execute: ExecuteFn,
}

/// Function-pointer type for a command's async execute. The future is
/// boxed because function-pointer signatures can't directly hold the
/// unnamed `async fn` future type. The runtime cost is one allocation
/// per cli invocation, which doesn't matter for a one-shot process.
pub type ExecuteFn =
    for<'a> fn(&'a CLIInfo, &'a Reporter) -> Pin<Box<dyn Future<Output = ExitCode> + 'a>>;

/// Declare the set of subcommands the cli knows about. Each entry
/// produces a `pub mod`, a constant `Command` value, and an
/// `include_str!` of its help file.
macro_rules! commands {
    ($($name:ident => $summary:literal),* $(,)?) => {
        $(pub mod $name;)*

        /// The full set of subcommands the cli knows about. Order here
        /// is the order shown in `peko help`.
        pub const ALL_COMMANDS: &[Command] = &[
            $(Command {
                name: stringify!($name),
                summary: $summary,
                help: include_str!(concat!("help/", stringify!($name), ".txt")),
                execute: |cli, rep| Box::pin($name::execute(cli, rep)),
            }),*
        ];
    };
}

commands! {
    add        => "add a dependency to peko.toml and install it",
    build      => "build the project for one or more target platforms",
    check      => "verify the Peko toolchain installation is healthy",
    clangflags => "print clang flags peko_core would pass to the C compiler",
    compile    => "compile a single Pekoscript file to an object or binary",
    format     => "normalize the indentation and spacing of Pekoscript files",
    icon       => "generate the per-platform app icon set from the icon source",
    install    => "resolve, download, and lock the project's dependencies",
    keys       => "manage per-project signing keys",
    login      => "authenticate the cli with the Peko platform",
    logout     => "clear the stored platform session",
    pkg        => "scaffold and pack a library package",
    project    => "create or inspect a Pekoscript project",
    publish    => "pack and publish a package to the registry",
    remove     => "remove a dependency from peko.toml and re-resolve",
    run        => "build and run the project, with optional hot reload",
    search     => "search or replace text across the project (used by the IDE)",
    test       => "type-check a Pekoscript file without producing output",
    toolchain  => "inspect and install build toolchains",
    update     => "re-resolve dependencies and refresh peko.lock",
    verify     => "scan a .pkpkg container and verify its structure and keys",
    version    => "print the cli version and exit",
    whoami     => "print the identity behind the stored platform session",
}

/// The devtools window shown by `peko run --devtools`. A helper module rather
/// than a subcommand, driven from `run`.
pub mod devtools;

/// The devtools window's client of the running app's bridge, for the
/// interactive console and view source.
pub mod bridge_client;

/// Look up a command by name. Returns `None` for unknown commands.
pub fn lookup(name: &str) -> Option<&'static Command> {
    ALL_COMMANDS.iter().find(|c| c.name == name)
}

// ---------------------------------------------------------------------------
// Shared helpers used by multiple commands
// ---------------------------------------------------------------------------

/// Human-readable label for an [`OperatingSystem`] used in messages.
/// Returns `None` for [`OperatingSystem::Unknown`] so callers can
/// surface a proper error rather than falling through to a default.
pub fn platform_label(os: &OperatingSystem) -> Option<&'static str> {
    match os {
        OperatingSystem::Android => Some("Android"),
        OperatingSystem::IOS => Some("iOS"),
        OperatingSystem::Linux => Some("Linux"),
        OperatingSystem::MacOS => Some("macOS"),
        OperatingSystem::Windows => Some("Windows"),
        OperatingSystem::Unknown => None,
    }
}
