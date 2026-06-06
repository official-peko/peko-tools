# Pekoscript Language Server

The official language server for [Pekoscript](https://pekoui.comt), implementing the [Language Server Protocol](https://microsoft.github.io/language-server-protocol/) over stdio. It provides rich editor integration for `.peko` files.

---

## Features

| Feature | Description |
|---|---|
| **Diagnostics** | Real-time errors and warnings as you type |
| **Hover** | Type information and documentation on symbol hover |
| **Completions** | Context-aware completion with snippet support |
| **Signature Help** | Parameter hints when calling functions |
| **Go to Definition** | Jump to where a symbol is declared |
| **Find References** | Find all usages of a symbol across the project |
| **Document Symbols** | Outline view of functions, classes, and variables in the current file |
| **Workspace Symbols** | Search for symbols across the entire project |
| **Formatting** | Format the current file using the Pekoscript formatter |

---

## Installation

Requires [Rust](https://rustup.rs/) 1.75 or later.

```bash
git clone https://github.com/you/pekoscript-lsp
cd pekoscript-lsp
cargo build --release
```

The compiled binary will be at `target/release/pekoscript-lsp`. Optionally install it to your system:

```bash
cargo install --path .
```
