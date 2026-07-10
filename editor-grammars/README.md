# editor-grammars

Syntax highlighting grammars for PekoScript, the single source of truth for the
language's editor tooling. Peko Studio is the first-party editor; these grammars
are kept here for reference and reuse.

Two grammars live side by side:

- **Tree-sitter** - `grammar.js` is the source of truth; `src/` holds the
  generated parser. Run `tree-sitter generate` after editing `grammar.js`.
- **TextMate** - `pekoscript.tmLanguage.json`, edited directly. Used by editors
  and highlighters that consume TextMate grammars.

## Tree-sitter

```bash
tree-sitter generate      # regenerate src/ from grammar.js
tree-sitter parse path/to/file.peko
```

## License

Licensed under PSAL-1.0. See [LICENSE](LICENSE) for the full text.

Copyright 2026 Peko UI Technologies LLC.
