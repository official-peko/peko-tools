# peko-cli

Console interface for the Pekoscript toolchain. Wraps `peko-core`, `peko-llvm`,
and `peko-lsp` into a single `peko` binary that drives every common workflow:
installing the toolchain, building projects for one or more platforms, compiling
and running files, managing packages, scaffolding projects, signing releases, and
deploying to the Peko platform.

`peko-cli` is one of four crates in the Pekoscript compiler workspace:

```
crates/
  peko-core/   compiler front end: lexer, parser, types, analysis, formatter
  peko-llvm/   LLVM-backed codegen and linker
  peko-lsp/    language server, run as `peko lsp`
  peko-cli/    this crate
```

## Building

The cli is built as part of the workspace:

```sh
cargo build --release -p peko-cli
```

The resulting binary lives at `target/release/peko`. The cli expects a
populated Peko toolchain installation at the path resolved by
`CLIInfo::get_peko_root()` (typically a sibling `Compiler/` directory
next to the binary). `peko check` will verify the installation is
healthy.

## Commands at a glance

```
peko setup      install or update the Peko development environment
peko check      verify the Peko toolchain installation is healthy
peko toolchain  inspect and install build toolchains

peko project    create or inspect a Pekoscript project
peko build      build the project for one or more target platforms
peko run        build and run the project
peko test       type-check a Pekoscript file without producing output
peko compile    compile a single Pekoscript file to an object or binary
peko format     normalize the indentation and spacing of Pekoscript files
peko clean      remove the project's build cache and output
peko clangflags print clang flags peko-core would pass to the C compiler
peko search     search or replace text across the project
peko lsp        run the language server over stdio

peko add        add a dependency to peko.toml and install it
peko remove     remove a dependency and re-resolve
peko install    resolve, download, and lock the project's dependencies
peko update     re-resolve dependencies and refresh peko.lock
peko verify     scan a .pkpkg container and verify its structure and keys

peko login      authenticate the cli with the Peko platform
peko logout     clear the stored platform session
peko whoami     print the identity behind the stored session
peko apps       list the platform apps this account owns
peko link       link the project to a platform app id
peko keys       manage per-project signing keys
peko deploy     publish a package, or deploy the app or its server
peko bridge     mint a native-bridge token for the current app
peko icon       generate the per-platform app icon set
peko demo       run the app's demo shots to verify the automation flow
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

Set up release signing for Android. `keys generate` builds an upload keystore
with the bundled JDK and registers it, so nothing has to be installed first:

```sh
peko keys generate --platform android --password-file pw.txt
peko keys list
```

## Source layout

```
src/
  main.rs              argv parsing, global flags, dispatch
  cli/                 CLIInfo, Flags, Reporter (terminal output)
  commands/            one file per subcommand, plus help text
    mod.rs           dispatcher table + shared helpers
    add.rs           ...
    help/<cmd>.txt   per-command help, included at compile time
  execution/           orchestrates peko-core: compile / test / incremental
  packager/            installer + .pkpkg binary builder
  bundler/             per-platform app bundling (apk, ipa, .app, .exe, AppImage)
  project/             PekoProject struct + binary config format
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
