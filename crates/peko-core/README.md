# Peko Core

Core compiler infrastructure for the [Pekoscript](https://pekoui.com) programming language.

`peko_core` is the **front end and static analyzer** of the Pekoscript toolchain. It turns source text into typed abstract syntax trees, runs a full type-checking and reachability pass over those trees, and produces a list of diagnostics describing any problems it found. It does *not* produce executable code or expose a command-line interface — those concerns live in sibling crates.

## Where this crate fits

The Pekoscript compiler is split into several focused crates:

```
                ┌──────────────┐
   .peko source │  peko_core   │  diagnostics + AST
       ────────►│ (this crate) │ ─────────────────►
                └──────┬───────┘
                       │ typed AST
                       ▼
                ┌──────────────┐
                │  peko_llvm   │  native binary
                │  (codegen)   │ ─────────────────►
                └──────────────┘
                       ▲
                       │
                ┌──────────────┐
                │  peko (CLI)  │   ◄── developer
                │  (driver)    │
                └──────────────┘
```

* **`peko_core`** (this crate) — lexer, parser, AST, type system, diagnostics, static analyzer.
* **`peko_llvm`** — code generation. Consumes the typed AST produced by `peko_core` and emits native code via LLVM.
* **`peko`** — the user-facing command-line tool. Drives `peko_core` and `peko_llvm`, manages projects, runs builds.

Splitting the front end from the code generator keeps each half independently testable. The simulator (the static analyzer in this crate) can run without ever invoking LLVM — so the Pekoscript language server, editor extensions, and CI lint passes all use `peko_core` directly, paying none of the cost of dragging in a code generator.

## Pipeline

A single end-to-end pass through `peko_core` looks like this:

```
                                                ┌──────────────────────┐
                                                │ DiagnosticList       │
                                                │  (errors + warnings) │
                                                └──────────▲───────────┘
                                                           │
                                                           │ collected
                                                           │ at every stage
                                                           │
   source ──► lexer ──► tokens ──► parser ──► AST ──► simulator
   (.peko)             (TokenList)         (PekoAST)   (type-checks
                                                        & resolves
                                                        references)
```

Every stage *appends* to the same diagnostic list rather than aborting on the first error. A file with a syntax error in one function will still have its other functions type-checked. This is what makes the language server experience usable.

## Module map

| Module | Purpose |
|---|---|
| `lexer` | Tokenize source text into a flat `TokenList`. Handles string interpolation, character literals, escape sequences, comments, and doc comments. |
| `parser` | Build typed ASTs from token streams. Recovery-friendly: collects multiple diagnostics per file rather than bailing on the first error. |
| `asts` | AST node definitions for every Pekoscript construct (values, expressions, statements, declarations), plus the `Spanned` trait for source-position queries. |
| `types` | `PekoType` and type-expansion logic. Knows about built-in primitives, classes, function/closure types, generics, references, and pointers. |
| `simulator` | The static analyzer. Walks the AST, threading scope and module context, and reports type mismatches, unresolved symbols, visibility violations, missing returns, and unreachable code. |
| `diagnostics` | `PekoDiagnostic` (a single finding with source position + severity) and `DiagnosticList` (the accumulator). |
| `target` | Descriptors for compilation targets (operating system + architecture + sub-flags like `windowsgui`). Used to gate `platform { ... }` blocks. |
| `packages` | `Package.json` parsing and external module discovery, used to resolve `import` statements against installed packages. |
| `execution` | Trait abstractions shared between the simulator and the (future) runtime interpreter. Lets both walkers reuse the same scope/module bookkeeping. |
| `error` | `PekoError`, `PekoResult`, and three small I/O helpers (`read_to_string`, `write`, `create_dir_all`) that wrap `std::fs` errors with source-path context. |

## Error handling

Two error channels run side-by-side, and the distinction matters:

* **`PekoError`** — environmental failures *from the tooling*. The source file couldn't be read, a `Package.json` was malformed, a path wasn't valid UTF-8. These propagate through `Result<T, PekoError>` in the normal Rust way.
* **`PekoDiagnostic`** — semantic findings *about user source code*. A type doesn't match, a variable isn't in scope, a function doesn't return on every path. These are collected into a `DiagnosticList` *without* halting compilation, so a single pass can surface dozens of independent issues.

In practice: if `peko_core` returns `Err`, the toolchain has a problem. If `peko_core` returns `Ok` with a non-empty diagnostic list, the *user's program* has a problem.

## Stability

This crate is at version `0.1.0` and is consumed solely by the Pekoscript toolchain (`peko_llvm`, the `peko` CLI, the language server, and the editor extensions maintained by the Peko team). The public API may change without notice between point releases — there is no commitment to semver compatibility yet. The crate is published primarily so that external users can inspect, audit, and build it themselves; it is not designed as a general-purpose library for embedding into unrelated projects.

* **Rust edition**: 2021.
* **MSRV**: not pinned. The crate currently builds on recent stable Rust; older versions may also work but are not tested.
* **Dependencies**: intentionally lean (`indexmap`, `itertools`, `derive-new`, `thiserror`, `serde`, `serde_json`). No async runtime, no FFI, no platform-specific code.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](../../CONTRIBUTING.md) in the project root for the development setup, the checks a change has to pass, and the pull request process. For anything larger than a small fix, open an issue first.

## License

This project is licensed under the **MIT License** — you may use, modify, and redistribute it, commercially or otherwise, provided the copyright notice and permission notice are retained.

See the [LICENSE](LICENSE) file in the project root for the full terms.
