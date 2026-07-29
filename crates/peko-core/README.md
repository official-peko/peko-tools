# peko-core

Core compiler infrastructure for the [Pekoscript](https://pekoui.com) programming
language.

`peko-core` is the front end and static analyzer of the Pekoscript toolchain. It
turns source text into typed abstract syntax trees, runs type checking and
reachability over those trees, and produces a list of diagnostics describing what
it found. It does not generate executable code and has no command line interface;
those live in sibling crates.

## Where this crate fits

```
   .peko source
        |
        v
   peko-core  (this crate)     lexing, parsing, types, static analysis
        |
        +--> diagnostics       errors and warnings, for the CLI and the LSP
        |
        +--> typed AST
                |
                v
           peko-llvm           LLVM IR codegen and linking
                |
                v
           peko-cli            the `peko` command that drives both
```

Keeping the front end separate from the code generator means the analyzer runs
without LLVM present. The language server, the editor extensions, and CI lint
passes all use `peko-core` directly and pay none of the cost of loading a code
generator.

## Pipeline

```
   source ---> lexer ---> tokens ---> parser ---> AST ---> simulator
   (.peko)              (TokenList)            (PekoAST)   (type checks and
                                                            resolves references)

   every stage appends to one DiagnosticList
```

Each stage appends to the same diagnostic list instead of stopping at the first
error, so a file with a syntax error in one function still gets its other
functions type checked. That behavior is what makes the language server usable.

## Module map

| Module | Purpose |
|---|---|
| `lexer` | Tokenize source into a flat `TokenList`, including string interpolation, character literals, escapes, comments, and doc comments. |
| `parser` | Build typed ASTs from token streams. Recovery-friendly: collects several diagnostics per file rather than bailing on the first. |
| `asts` | AST node definitions for every construct (values, expressions, statements, declarations), plus the `Spanned` trait for source-position queries. |
| `types` | `PekoType` and type expansion. Covers primitives, classes, function and closure types, generics, references, and pointers. |
| `simulator` | The static analyzer. Walks the AST threading scope and module context, reporting type mismatches, unresolved symbols, visibility violations, missing returns, and unreachable code. |
| `diagnostics` | `PekoDiagnostic`, a single finding with position and severity, and `DiagnosticList`, the accumulator. |
| `config` | The on-disk configuration files: `peko.toml` project manifests and the install manifest the toolchain resolver reads. |
| `packages` | Discovery of installed packages in the registry source cache, used to resolve `import` statements. |
| `ffi` | Parsing `.peko.h` headers, the C headers that double as a Peko FFI surface. |
| `formatter` | Pretty-prints ASTs back into canonical source, preserving comments. Drives `peko format` and the LSP formatting request. |
| `target` | Descriptors for compilation targets (operating system, architecture, and sub-flags), used to gate `platform { ... }` blocks. |
| `execution` | Backend-agnostic algorithms shared by the simulator and the code generator, so both reuse the same scope and module bookkeeping. |
| `error` | `PekoError`, `PekoResult`, and small I/O helpers that wrap `std::fs` errors with source-path context. |

## Two error channels

The distinction matters when reading the API:

- `PekoError` covers environmental failures from the tooling. A source file could
  not be read, a manifest was malformed, a path was not valid UTF-8. These
  propagate as `Result<T, PekoError>`.
- `PekoDiagnostic` covers semantic findings about user source. A type does not
  match, a variable is not in scope, a function does not return on every path.
  These accumulate in a `DiagnosticList` without halting the pass, so one run
  surfaces many independent problems.

In short: an `Err` means the toolchain has a problem, and an `Ok` carrying a
non-empty diagnostic list means the user's program has one.

## Stability

The crate version tracks the workspace and is consumed by the rest of the
toolchain: `peko-llvm`, `peko-cli`, the language server, and the editor
extensions. The public API changes between releases and carries no semver
commitment yet. It is published so it can be inspected, audited, and built from
source, not as a general-purpose library to embed elsewhere.

Rust edition 2024. The MSRV is not pinned; the crate builds on recent stable.
Dependencies are deliberately few (`indexmap`, `itertools`, `derive-new`,
`thiserror`, `serde`, `serde_json`), with no async runtime and no
platform-specific code.

## Contributing

See [CONTRIBUTING.md](../../CONTRIBUTING.md) in the project root for the
development setup, the checks a change has to pass, and the pull request process.
For anything larger than a small fix, open an issue first.

## License

MIT. See [LICENSE](../../LICENSE) in the project root for the full text.

Copyright 2026 Peko UI Technologies LLC.
