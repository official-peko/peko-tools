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
    /// The flags this command reads a value for (its `get_flag(...)` flags).
    /// The parser consumes the following argv token as the value when one of
    /// these is written space-separated (`--flag value`) with no `=`.
    pub value_flags: &'static [&'static str],
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
    ($($name:ident => $summary:literal $([ $($vf:literal),* $(,)? ])?),* $(,)?) => {
        $(pub mod $name;)*

        /// The full set of subcommands the cli knows about. Order here
        /// is the order shown in `peko help`.
        pub const ALL_COMMANDS: &[Command] = &[
            $(Command {
                name: stringify!($name),
                summary: $summary,
                help: include_str!(concat!("help/", stringify!($name), ".txt")),
                execute: |cli, rep| Box::pin($name::execute(cli, rep)),
                value_flags: &[ $( $( $vf ),* )? ],
            }),*
        ];
    };
}

commands! {
    add        => "add a dependency to peko.toml and install it" [ "version", "path" ],
    bridge     => "mint a native-bridge token for the current app" [ "app-id", "device-id", "base" ],
    build      => "build the project for one or more target platforms" [ "platform", "target", "arch", "output" ],
    check      => "verify the Peko toolchain installation is healthy",
    clangflags => "print clang flags peko_core would pass to the C compiler" [ "os", "arch" ],
    compile    => "compile a single Pekoscript file to an object or binary" [ "os", "arch", "output" ],
    demo       => "run the app's demo shots to verify the automation flow" [ "from", "delay", "shot" ],
    deploy     => "publish a package or deploy the app to Peko server hosting" [ "app-id", "health-path", "base" ],
    format     => "normalize the indentation and spacing of Pekoscript files",
    icon       => "generate the per-platform app icon set from the icon source" [ "platform", "out" ],
    install    => "resolve, download, and lock the project's dependencies",
    keys       => "manage per-project signing keys" [ "platform", "password", "password-file", "keystore", "alias", "store-password", "key-password", "cert", "profile", "entitlements", "pfx", "notary-issuer", "notary-key-id", "notary-p8", "role", "file", "filename" ],
    link       => "link the project to a platform app id for deploys",
    login      => "authenticate the cli with the Peko platform" [ "base" ],
    logout     => "clear the stored platform session",
    project    => "create or inspect a Pekoscript project" [ "name", "type", "bundle", "version", "framework", "dir" ],
    remove     => "remove a dependency from peko.toml and re-resolve",
    run        => "build and run the project, with optional hot reload",
    search     => "search or replace text across the project (used by the IDE)" [ "query", "include", "exclude", "replace", "root" ],
    setup      => "install or update the Peko development environment" [ "peko-version", "sdk-version" ],
    test       => "type-check a Pekoscript file without producing output" [ "os", "arch" ],
    toolchain  => "inspect and install build toolchains",
    update     => "re-resolve dependencies and refresh peko.lock",
    verify     => "scan a .pkpkg container and verify its structure and keys",
    version    => "print the cli version and exit",
    whoami     => "print the identity behind the stored platform session" [ "base" ],
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
