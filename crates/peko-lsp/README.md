# peko-lsp

The language server for [Pekoscript](https://pekoui.com), implementing the
[Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
over stdio for `.peko` files.

This is a library crate compiled into the `peko` binary rather than a separate
executable. Editors start it by running:

```bash
peko lsp
```

It reuses the `peko-core` analysis engine, so the diagnostics an editor shows are
the ones the compiler produces.

## Requests handled

| Request | Behavior |
|---|---|
| Diagnostics | Errors and warnings, published as a file is edited. |
| Hover | Type information and doc comments for the symbol under the cursor. |
| Completion | Context-aware completions, including snippets. |
| Signature help | Parameter hints while writing a call. |
| Go to definition | Jumps to where a symbol is declared. |
| Find references | Locates a symbol's uses across the project. |
| Document symbols | The outline of functions, classes, and variables in a file. |
| Workspace symbols | Symbol search across the project. |
| Formatting | Formats a file through the `peko-core` formatter, the same one `peko format` uses. |

Positions are exchanged in whatever encoding the client negotiates. The server's
canonical representation is character offsets, transcoded at the wire boundary,
so files containing characters outside the Basic Multilingual Plane map
correctly.

## Editor setup

Any LSP-capable editor can use it by registering `peko lsp` as the server command
for the `.peko` file type. Peko Studio wires this up already: its native host
spawns `peko lsp` and frames the protocol between the editor and the server.

The `peko` binary has to be on `PATH`, which `peko setup` configures.

## Building

The crate builds as part of the workspace:

```bash
cargo build --release
```

There is no separate language server binary to install. See the
[workspace README](../../README.md) for the LLVM 18 prerequisite that building
the workspace requires.

## License

MIT. See [LICENSE](../../LICENSE) in the project root for the full text.

Copyright 2026 Peko UI Technologies LLC.
