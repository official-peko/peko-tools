# peko-tools

[![CI](https://github.com/official-peko/peko-tools/actions/workflows/ci.yml/badge.svg)](https://github.com/official-peko/peko-tools/actions/workflows/ci.yml)
[![Nightly](https://github.com/official-peko/peko-tools/actions/workflows/nightly.yml/badge.svg)](https://github.com/official-peko/peko-tools/actions/workflows/nightly.yml)
[![Status](https://img.shields.io/badge/status-prerelease-orange.svg)](#status)
[![License](https://img.shields.io/badge/license-PSAL--1.0-blue.svg)](LICENSE)

The core toolchain for Pekoscript: the compiler front end, the LLVM code
generator, the command line interface, and the language server. This is a
Cargo workspace that holds the four crates together so they share a single
dependency graph, a single lockfile, and one release cadence.

## Status

This project is pre-1.0 and in active development. Every published build is a
prerelease, the public API and the language surface can change between
versions, and binaries are not yet intended for production use. The version
series stays in the 0.x range until the first stable launch.

## Crates

| Crate | Role |
|---|---|
| [`peko_core`](peko_core/README.md) | Lexer, parser, AST, type system, static analysis, and the simple package registry. |
| [`peko_llvm`](peko_llvm/README.md) | LLVM IR code generation and linking, built on top of `peko_core`. Links libLLVM through llvm-sys and inkwell. |
| [`peko_cli`](peko_cli/README.md) | The user facing command line tool: compiling, bundling, project management, and package management. |
| [`peko_lsp`](peko_lsp/README.md) | A language server that exposes the `peko_core` analysis engine to editors. |

Each crate documents its own internals in its own README. This file covers the
workspace as a whole.

## Architecture

```
peko_core    lexing, parsing, AST, static analysis, package registry
   |
   +-- peko_llvm   LLVM IR codegen and lld linking, built on peko_core
   |        |
   |        +-- peko_cli   compiler driver, bundling, project and package tooling
   |
   +-- peko_lsp   language server backed by the peko_core analysis engine
```

`peko_core` is the foundation. `peko_llvm` consumes its AST and type
information to emit LLVM IR and produce native objects. `peko_cli` drives the
front end and the code generator end to end and adds the project and package
workflows. `peko_lsp` reuses the same analysis engine so editor diagnostics
match the compiler.

## Supported targets

Native builds are produced for five targets:

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
`cargo build --release -p peko_cli` and so on.

## Development

```bash
cargo test --all          # run the workspace test suite
cargo fmt --all           # format
cargo clippy --all-targets --all-features -- -D warnings   # lint
```

The same checks run in CI on every pull request and on every push to `main`.

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

## Conventions

Source files use ASCII only. Functions and variables are snake_case; classes
and globals are PascalCase. See the per-crate READMEs for details specific to
each component.

## License

Licensed under PSAL-1.0. See [LICENSE](LICENSE) for the full text.

Copyright 2026 Peko UI Technologies LLC.
