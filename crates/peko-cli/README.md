# peko_cli

Console interface for the Pekoscript toolchain. Wraps `peko_core` and
`peko_llvm` into a single `peko` binary that drives every common
workflow: building projects for one or more platforms, compiling and
running individual files, managing packages, scaffolding new projects,
and signing release builds.

`peko_cli` is one of three crates in the Pekoscript compiler workspace:

```
compiler/
├── peko_core/   compiler frontend: parser, AST, type checker, simulator
├── peko_llvm/   LLVM-backed codegen + linker
└── peko_cli/    this crate
```

## Building

The cli is built as part of the workspace:

```sh
cargo build --release -p peko_cli
```

The resulting binary lives at `target/release/peko`. The cli expects a
populated Peko toolchain installation at the path resolved by
`CLIInfo::get_peko_root()` (typically a sibling `Compiler/` directory
next to the binary). `peko check` will verify the installation is
healthy.

## Commands at a glance

```
peko add        install a package from the registry
peko addkey     add a code-signing key to the project
peko build      build the project for one or more target platforms
peko check      verify the Peko toolchain installation is healthy
peko clangflags print clang flags peko_core would pass to the C compiler
peko compile    compile a single Pekoscript file to an object or binary
peko pkg        package a host package for distribution
peko project    create or inspect a Pekoscript project
peko remove     uninstall a package from the project
peko run        build and run the project, with optional hot reload
peko test       type-check a Pekoscript file without producing output
peko update     update an installed package to a newer version
peko version    print the cli version and exit
```

Run `peko help <command>` for the full options block of any command.

## Global options

Every command honors these flags, parsed before subcommand dispatch:

```
--verbose       enable extra-noisy output
--quiet         suppress informational output; errors and warnings still print
--no-color      disable ANSI color in output
```

`NO_COLOR=1` in the environment also disables color.

## Common workflows

Build a UI project for every declared platform:

```sh
peko build --release
```

Run a UI project with hot reload (SCSS and Pekoscript changes are picked
up automatically while the app is running):

```sh
peko run
```

Compile a single source file to a binary for a specific target:

```sh
peko compile main.peko --os=linux --arch=arm
```

Scaffold a new project, then build it:

```sh
peko project new MyApp
cd MyApp
peko build
```

Install a package, then build:

```sh
peko add my_pkg
peko build
```

Set up release signing for Android:

```sh
peko addkey --android ./my-release.keystore
echo '.peko/project/keystores/' >> .gitignore
```

## Source layout

```
src/
├── main.rs              argv parsing, global flags, dispatch
├── cli/                 CLIInfo, Flags, Reporter (terminal output)
├── commands/            one file per subcommand, plus help text
│   ├── mod.rs           dispatcher table + shared helpers
│   ├── add.rs           ...
│   ├── help/<cmd>.txt   per-command help, included at compile time
├── execution/           orchestrates peko_core: compile / test / incremental
├── packager/            installer + .pkpkg binary builder
├── bundler/             per-platform app bundling (apk, ipa, .app, .exe, AppImage)
└── project/             PekoProject struct + binary config format
```

## Adding a new subcommand

Three steps:

1. Create `src/commands/<name>.rs` exposing:

   ```rust
   pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode
   ```

2. Create `src/commands/help/<name>.txt` with the help text. The format
   convention is a synopsis line, a blank line, a one-paragraph
   description, an `OPTIONS` block if any, and an `EXAMPLES` block if
   any. Keep it plain prose, no em dashes.

3. Add a `<name> => "<one-line summary>"` line to the `commands!`
   macro invocation in `src/commands/mod.rs`. The macro will generate
   the `pub mod` declaration, wire up the help text via `include_str!`,
   and add the command to the dispatch table.

The Reporter passed to `execute` is the canonical output channel: use
`reporter.error(...)`, `reporter.warning(...)`, `reporter.help(...)`,
`reporter.info(...)`, and `reporter.success(...)` for user-facing
output, and `reporter.progress()` for the progress sink. Don't
`println!` directly except when the command's product is structured
data (e.g. `peko clangflags` writes its flags to stdout).

## License

Copyright 2026 Peko UI Technologies LLC. All rights reserved.
