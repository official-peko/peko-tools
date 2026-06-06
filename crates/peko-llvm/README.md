# peko_llvm

`peko_llvm` is the code-generation and link backend for the Pekoscript compiler. It takes the AST produced by `peko_core` and lowers it through LLVM IR into object files, then drives `lld` to link those objects into final executables or shared libraries.

This crate is meant to be consumed programmatically by the compiler driver (`peko_cli`). It is not a standalone tool and is not published to crates.io.

## What it does

`peko_llvm` extends `peko_core`'s `ExecutionContext` machinery with a concrete implementation that produces LLVM IR. The crate has two top-level modules:

The `codegen` module owns a `PekoCodegenContext` — the workhorse type that holds LLVM context state (modules, blocks, the current function being built, scoped variables, type expansion caches) and exposes a layered set of builder traits for producing LLVM values. AST nodes implement `PekoValueBuilder`, whose single `build_value` method drives codegen for that node. The implementations live in `value_gen` (literals), `expression_gen` (operators, calls, member access), `statement_gen` (control flow, imports, asset/style/link statements), and `declaration_gen` (functions, classes, modules, variables).

The `linker` module wraps the `lld` driver via a small C++ shim (`rust_lld/lldentry.cc`) compiled into a static archive. The single entry point, `lld_link`, accepts a target description plus a list of input objects and produces an executable for the target platform. The linker selects the right LLD driver — `ld.lld` for Linux and Android, `ld64.lld` for macOS and iOS, `lld-link` for Windows — and assembles the platform-specific argument string, search paths, runtime objects, and system libraries before handing the whole thing off to LLD.

The `PekoCodegenContext`'s builder API is split across ten traits under `codegen/builders/`, organized in dependency layers so each trait only calls into ones at the same or lower level. Layer 0 (`LlvmTypeBuilder`, `LlvmConstantBuilder`) covers LLVM types and constants. Layer 1 adds instructions and memory (`LlvmInstructionBuilder`, `LlvmMemoryBuilder`). Layer 2 has arithmetic, function definition, and globals (`LlvmArithmeticBuilder`, `FunctionBuilder`, `GlobalBuilder`). Layer 3 sits on top of those for higher-level operations (`HighLevelCodegen`, `ScopeManager`). Layer 4 is `ModuleManager`, the cross-module orchestration on top of everything else. The split exists so that adding a new high-level operation doesn't require putting it on the giant `PekoCodegenContext` inherent impl — it goes in the right trait under `builders/` and inherits the lower layers for free.

## Building

The crate requires three things in the build environment:

`LLVM_SYS_180_PREFIX` must point to an LLVM 18 install root — the directory containing `bin/`, `lib/`, and `include/`. The `llvm-sys-180` package alias used in `Cargo.toml` selects the LLVM 18 series; other versions will not work without dependency changes.

`ZSTD_LIB_PREFIX` must point to a directory containing `libzstd.{a,lib}`. LLVM 18 links against zstd for object-file compression and Cargo cannot find it on its own.

The `rust_lld/` directory at the crate root must contain the prebuilt LLD static archives for the host platform, under `rust_lld/<os>/<arch>/`. The build script (`build.rs`) picks the right subdirectory based on `std::env::consts::OS` and `ARCH`. Supported hosts are macOS (x86_64 and arm), Linux (x86_64 and arm), and Windows (x86_64 only). Anything else fails the build with an explicit panic.

Changes to either env var, anything inside `rust_lld/`, or `build.rs` itself will trigger a rebuild on the next `cargo build` — no `cargo clean` needed.

## Using the crate

The two consumer-facing entry points are `PekoCodegenContext` and `lld_link`.

A driver calls into codegen by constructing a `PekoCodegenContext`, walking the parsed AST, and calling `build_value` on each top-level node. Once codegen finishes, `TopLevelModuleInfo` (reachable from the root module) exposes `output_binary` to emit a target-specific object file and `emit_ir` to dump the `.ll` text form for debugging. `check_module` runs LLVM's verifier and prints any complaints.

To link the produced objects, the driver calls `lld_link` with the same target description, the list of objects to link, the sysroot for the target, and the desired output path. The function returns a `bool` indicating link success.

Both `PekoCodegenContext` and `lld_link` carry their own diagnostic state. Errors that fire during codegen are reported through `peko_core`'s `DiagnosticList` machinery, which the driver should drain and present to the user.

## Source layout

```
peko_llvm/
├── build.rs                       link search/path setup and rerun hints
├── Cargo.toml                     dependency manifest
├── rust_lld/                      prebuilt LLD static archives by platform
│   ├── linux/{x86_64,arm}/
│   ├── macos/{x86_64,arm}/
│   └── windows/
├── src/
│   ├── lib.rs                     crate root
│   ├── codegen/
│   │   ├── mod.rs                 PekoValueBuilder trait, dispatch macro,
│   │   │                          cstring helpers
│   │   ├── context.rs             PekoCodegenContext struct, constructor,
│   │   │                          and ExecutionContextAlgorithms impl
│   │   ├── data_structures.rs     concrete types for peko_core's Execution*
│   │   │                          traits (CodegenValue, CodegenFunction,
│   │   │                          CodegenClass, CodegenModule, ...)
│   │   ├── symbol.rs              SymbolName: parsed/mangled symbol names
│   │   ├── value_gen.rs           PekoValueBuilder impls for literals
│   │   ├── expression_gen.rs      PekoValueBuilder impls for expressions
│   │   ├── statement_gen.rs       PekoValueBuilder impls for statements
│   │   ├── declaration_gen.rs     PekoValueBuilder impls for declarations
│   │   └── builders/
│   │       ├── mod.rs             prelude re-exporting all builder traits
│   │       ├── llvm_types.rs      layer 0: LLVM type construction
│   │       ├── llvm_constants.rs  layer 0: LLVM constant values
│   │       ├── llvm_instructions.rs  layer 1: load, store, GEP, branch
│   │       ├── llvm_memory.rs     layer 1: stack/heap allocation
│   │       ├── llvm_arithmetic.rs layer 2: integer and float arithmetic
│   │       ├── functions.rs       layer 2: function definition / call
│   │       ├── globals.rs         layer 2: global variable definition
│   │       ├── high_level.rs      layer 3: boxing, class allocation
│   │       ├── scope.rs           layer 3: scope and variable lookup
│   │       └── modules.rs         layer 4: cross-module orchestration
│   └── linker/
│       └── mod.rs                 lld_link entry point and per-OS argument
│                                  assembly
```

## Layered constraints on the codegen builders

The dependency layering of the builder traits is the main thing to watch when adding new methods. Each trait can call into traits at its own layer or any lower layer, but not upward. Adding a method that needs to reach across layers usually means it belongs in a higher layer than where it's tempting to put it. The three minor exceptions where a lower-layer trait method ends up calling a higher-layer one are documented at the definition site.

The split also makes it possible to write tests against a single layer in isolation, though no such tests exist yet.
