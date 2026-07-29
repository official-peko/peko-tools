# peko-tools

[![CI](https://github.com/official-peko/peko-tools/actions/workflows/ci.yml/badge.svg)](https://github.com/official-peko/peko-tools/actions/workflows/ci.yml)
[![Nightly](https://github.com/official-peko/peko-tools/actions/workflows/nightly.yml/badge.svg)](https://github.com/official-peko/peko-tools/actions/workflows/nightly.yml)
[![Release](https://img.shields.io/github/v/release/official-peko/peko-tools.svg)](https://github.com/official-peko/peko-tools/releases)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

The core toolchain for Pekoscript: the compiler front end, the LLVM code
generator, the command line interface, and the language server. This is a
Cargo workspace that holds the four crates together so they share a single
dependency graph, a single lockfile, and one release cadence.

Alongside the Rust crates, the repository carries the runtime assets the CLI
uses to build and link user programs: the Pekoscript standard library and the
per-platform native toolchains, both under [`toolkit/`](#standard-library-and-toolchains).

## Crates

| Crate | Role |
|---|---|
| [`peko-core`](crates/peko-core/README.md) | Lexer, parser, AST, type system, static analysis, the formatter, and package discovery. |
| [`peko-llvm`](crates/peko-llvm/README.md) | LLVM IR code generation and linking, built on `peko-core`. Links libLLVM through llvm-sys and inkwell. |
| [`peko-cli`](crates/peko-cli/README.md) | The command line tool: compiling, bundling, project and package management, signing, and deploys. |
| [`peko-lsp`](crates/peko-lsp/README.md) | A language server library exposing the `peko-core` analysis engine to editors. Built into the CLI and run as `peko lsp`. |

Each crate documents its own internals in its own README. This file covers the
workspace as a whole.

## Standard library and toolchains

The `toolkit/` directory holds the assets the compiler ships with, distinct
from the Rust crates that build the toolchain itself:

| Path | Contents |
|---|---|
| [`toolkit/std`](toolkit/std/README.md) | The Pekoscript standard library: `core`, `collections`, `io`, `fs`, `random`, `crypto`, `threads`, `sockets`, `process`, `json`, `xml`, `lexer`, `bundle`, and `runtime`. |
| [`toolkit/pekoui`](toolkit/pekoui/README.md) | The UI framework: a native webview host, the bridge to the web front end, storage, keychain, menus, and dialogs, plus the `@peko/client` JavaScript SDK. |
| `toolkit/toolchains` | The per-platform toolchain descriptors, one directory each for `macos`, `linux`, `windows`, `ios`, and `android`. Each `toolchain.toml` describes its target triple, compile flags, includes, and link step. |
| `toolkit/peko.h` | The FFI umbrella header the standard library's C sources compile against. |

Several modules pair Pekoscript with native C under `c/`. `pekoui`, for example,
wraps a different webview on each platform: WKWebView on macOS, WebKitGTK on
Linux, WebView2 on Windows, UIKit and WebKit on iOS, and an
`android.webkit.WebView` driven through JNI on Android. One JavaScript-to-native
binding protocol is shared across all of them.

`std` and `pekoui` are published to the package registry and installed globally
by `peko setup`, so projects import them without vendoring anything.

Peko programs compile to native code for both the desktop platforms and the
mobile platforms, iOS and Android; the CLI drives the C compilation and the
final link through the matching toolchain.

## Architecture

```
peko-core    lexing, parsing, AST, static analysis, formatting, packages
   |
   +-- peko-llvm   LLVM IR codegen and lld linking, built on peko-core
   |        |
   |        +-- peko-cli   compiler driver, bundling, project and package tooling
   |
   +-- peko-lsp   language server backed by the peko-core analysis engine
```

`peko-core` is the foundation. `peko-llvm` consumes its AST and type
information to emit LLVM IR and produce native objects. `peko-cli` drives the
front end and the code generator end to end and adds the project and package
workflows. `peko-lsp` reuses the same analysis engine so editor diagnostics
match the compiler.

## Installing the toolchain

Building this repository is only needed to work on the toolchain itself. To use
Peko, install a release and let it lay down the rest of the environment:

```bash
peko setup
```

That downloads the compiler SDK and the linux and android toolchains, links the
Apple SDKs through `xcrun` on a macOS host, installs `std` and `pekoui`
globally, writes the toolchain descriptors, and configures `PATH`, all under
`~/.Peko`. Re-running it refreshes only the components whose release changed.
The Windows toolchain (the MSVC CRT and Windows SDK) is optional and comes
through xwin, so it needs `--windows --accept-microsoft-license`.

## Toolchain host platforms

The toolchain binaries themselves are built for five host platforms (distinct
from the platforms Peko programs compile to, which also include iOS and
Android):

| Platform | Architecture | Target triple |
|---|---|---|
| Linux | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux | arm64 | `aarch64-unknown-linux-gnu` |
| macOS | arm64 | `aarch64-apple-darwin` |
| macOS | x86_64 | `x86_64-apple-darwin` |
| Windows | x86_64 | `x86_64-pc-windows-msvc` |

The primary development target is arm64 macOS.

## Prerequisites

A recent stable Rust toolchain and LLVM 18.1.x. The code generator links
libLLVM through llvm-sys, so an LLVM 18 installation must be present at build
time and located through an environment variable.

There are two ways to provide LLVM 18:

1. Prebuilt distributions are published per target at
   [`official-peko/peko-llvm-dist`](https://github.com/official-peko/peko-llvm-dist).
   Download the archive for your platform and extract it. The extracted folder
   is the prefix root and contains `bin/`, `lib/`, and `include/`.
2. A system LLVM 18 install, where the prefix root is the directory that
   `llvm-config` reports.

Point llvm-sys at the prefix and confirm the version:

```bash
export LLVM_SYS_180_PREFIX="$HOME/llvm"          # folder containing bin/ lib/ include/
"$LLVM_SYS_180_PREFIX/bin/llvm-config" --version  # expect 18.1.x
```

Add the export to your shell profile so every build sees it.

## Building

```bash
cargo build --release
```

The release binaries land in `target/release/`. Build a single crate with
`cargo build --release -p peko-cli` and so on.

## Development

```bash
cargo test --all          # run the workspace test suite
cargo fmt --all           # format
cargo clippy --all-targets --all-features -- -D warnings   # lint
```

The same checks run in CI on every pull request and on every push to `main`.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup, the checks a change has to pass, and the rules around
vendored third-party code. Contributions are accepted under the MIT License,
the same terms as the rest of the project.

For anything larger than a small fix, open an issue before writing code.

## Releases and nightlies

Tagged releases are published on the
[releases page](https://github.com/official-peko/peko-tools/releases) with one
archive per target. Pushing a `v*` tag builds all five targets and publishes
the archives; tags in the 0.x series are marked as prereleases automatically.

A rolling nightly prerelease is rebuilt from the latest commit each day and is
available under the `nightly` release tag. Nightly archives track the head of
`main` and carry no stability guarantee.

## Continuous integration

Three workflows live in `.github/workflows/`:

- `ci.yml` runs format, clippy, and tests on Linux for pull requests and
  pushes to `main`.
- `nightly.yml` builds every target on a daily schedule and publishes the
  rolling nightly prerelease.
- `release.yml` builds every target on a version tag and publishes a release.

The multi-target matrix is factored into a reusable `build.yml` that both the
nightly and release workflows call. Each target builds natively on its own
runner and fetches the matching LLVM 18 distribution at build time.

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for the full text.

Copyright 2026 Peko UI Technologies LLC.

### Third-party notices

The toolchain links third-party code, and applications built with it link the
vendored native components (BearSSL, webview, the Android native app glue).
Attribution ships with the binaries:

| File | Covers |
|---|---|
| `THIRD-PARTY-NOTICES.txt` | Everything linked into the `peko` binary. Included in every release archive and written into `~/.Peko` by `peko setup`. Generated by `./scripts/gen-notices.sh`. |
| `NOTICE-native.txt` | The bundled native components, which `cargo` cannot see. Maintained by hand. |
| `NOTICE-app.txt` | The subset linked into applications built with Peko. The bundler writes it into every app as `OPEN-SOURCE-NOTICES.txt`. |
