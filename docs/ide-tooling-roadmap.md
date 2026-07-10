# IDE and tooling roadmap

Plan of attack for the next PekoScript IDE/tooling features, plus the
Unicode/non-ASCII correctness work that underpins them. Ordered so the
position-encoding foundation lands first, because rename, references,
range-formatting, and textEdit-based completions all depend on correct ranges.

## Recon summary (state at time of writing)

- **peko-core lexer/parser: already Unicode-safe.** The lexer iterates
  `Vec<char>`; columns and indices count characters, not bytes (`'e'` with an
  accent is one column). Source: `lexer/mod.rs:497-584`,
  `asts/data_structures.rs:79-85`.
  - Gaps, not bugs: identifiers are ASCII-only by design
    (`lexer/mod.rs:568,578`); string escapes support `\xNN` bytes but there is
    no `\u{...}` form (`parser/mod.rs:701,732-752`); no Unicode normalization.
- **peko-core formatter: UTF-8 safe.** Operates on `str.lines()` and char
  iterators, rewrites only leading whitespace (`format.rs:28-74`).
- **peko-lsp: the real problem is a three-way encoding mismatch.** peko-core
  hands the LSP char-based line/column. LSP clients assume UTF-16 code units
  (because `offset_encoding: None` is advertised, `backend.rs:135`). But the
  LSP code treats `character` as raw bytes in some spots and chars in others:
  - byte-cast: `document.rs:68-77` (`char_at`, `offset_at`),
    `backend.rs:369-370` (formatting end range via `str.len()`),
    `converters.rs:262-270` (signature-help param label offsets via
    `String::find`/`len`).
  - char-based: `documents.rs:76-88` (ropey `len_chars`).
  - pass-through, no conversion: `converters.rs:20-42`,
    `helpers.rs:150-158` (`create_position`).
  For ASCII, byte == char == UTF-16, so it works today. Any emoji, accented,
  or CJK character drifts every position out of alignment. This same mismatch
  blocks the deferred `new`-completion-with-textEdit work (it needs a correct
  range).

## Phase 0 - decisions

1. **Negotiated position encoding.** Advertise a preference list preferring
   UTF-8 and falling back to UTF-16 when the client does not offer UTF-8.
   UTF-16 stays the guaranteed floor.
2. **Unicode identifiers.** Keep ASCII-only for now (matches the lexer, avoids
   UAX-31 scope creep) but emit a real diagnostic instead of silently dropping
   a non-ASCII char to an `Unknown` token. Revisit full Unicode identifiers
   only on demand.

## Phase 1 - position-encoding foundation (keystone) [DONE]

Status: complete and verified (workspace clippy and tests green; 11 new unit
tests). Internal canonical positions are now char-based end to end, with
transcoding confined to the wire boundary. New module
`crates/peko-lsp/src/server/encoding.rs` holds `WireEncoding`, `LineIndex`, and
`PosMapper`; `initialize` negotiates and advertises `positionEncoding`; every
handler transcodes inbound wire-to-char and outbound char-to-wire.

Collapse the three encodings into one clean boundary. Internal canonical stays
char-based (matches peko-core and ropey); transcode to and from the negotiated
protocol encoding in exactly one place.

- A single conversion layer keyed off the document rope: char column to wire
  position and wire position to char offset, honoring the negotiated encoding.
- Route every Position and Range construction through it: `converters.rs`
  (positions, ranges, diagnostics, symbols, hover, definition, signature-help
  offsets), `helpers.rs:create_position`, and the `document.rs` / `documents.rs`
  offset lookups.
- Delete the byte-cast shortcuts: `document.rs:char_at` (byte-to-char cast),
  `backend.rs:369-370` (`str.len()`), `converters.rs:262-270` (byte
  `find`/`len` for param labels).
- Fix the byte-vs-char back-search state machines: `analyzer/mod.rs:296`
  (`character -= 1`) and the object-access back-search near 401.
- Tests: non-ASCII fixtures (emoji, CJK, combining accents) with round-trip
  assertions `wire -> internal -> wire`.

## Phase 2 - lexer/string-literal Unicode completeness [DONE]

Status: complete and verified (peko-core clippy and tests green; 4 new tests).

- `\u{HEX}` escape (1 to 6 hex digits, any Unicode scalar value including
  astral) added to `parser/mod.rs` string parsing alongside `\xNN`. The braces
  and hex digits lex as separate tokens, so the escape consumes them from the
  token stream; surrogates and out-of-range code points are rejected with a
  diagnostic.
- Non-ASCII identifier characters now get a targeted diagnostic naming the
  ASCII-only rule, instead of the generic unexpected-token message.
- No-normalization stance: NFC and NFD strings stay distinct. Literal non-ASCII
  content in strings, character literals, and comments already passed through
  unchanged (the lexer is char-based).

## Phase 3 - deferred completion fixes (unblocked by Phase 1)

- Module-access `new` completion [DONE, needs in-editor verification]:
  `io::SomeClass` now completes as `new io::SomeClass(...)`. `CompletionItem`
  gained an optional `text_edit(range, new_text)` (`server/analysis.rs`), the
  converter maps the range through `PosMapper`, the backend builds a mapper for
  completion, and `get_symbols_at` surfaces a `ModuleAccessReplace` carrying the
  module prefix, the module-path start byte offset, the cursor byte offset, and
  an `after_new` guard so a `new` already in front of the path is not doubled.
  Object access (`.`) is unchanged. Workspace clippy and tests green;
  end-to-end completion behavior needs confirmation in a real editor (the
  JSON-RPC server cannot be driven headless in this environment).
- Override-completion [PENDING]: typing `fn to_` in a `class X from Y` body
  suggests inherited method signatures. Needs class-body method-position
  detection plus a parent-chain walk, and the symbol model does not yet expose
  superclass methods (traits are not even surfaced as `ScopeSymbol`s). Larger
  feature; deferred.

## Editor-feedback fixes (from in-editor testing)

Three issues found by running the language server in a real editor, plus the
`new`-insert regression, all fixed this pass (workspace clippy and tests green;
in-editor confirmation still welcome):

1. Import-path completion: `import package::` now offers the package's module
   names. `get_symbols_at` detects the `import` keyword before a single-segment
   module path and returns `SymbolSearchResult::ImportModules`, whose names come
   from listing the `.peko` files in the package source root
   (`PekoAnalyzer::package_module_names`, using `ExternalModuleInfo`'s
   `source_root`); `completions` early-returns plain module items with no
   expression snippets.
2. The `new` insertion for `module::Class` now uses `additional_text_edits`
   (insert `new ` before the module path) instead of a range-replacing
   `textEdit`. The main insertion still replaces the typed identifier, so the
   editor's word-filtering is unaffected and the leading `new ` is applied as a
   separate, non-overlapping edit. Skipped when a `new` already precedes the
   path.
3. `module::` access no longer lists the module's imports. `io::` was surfacing
   `runtime`/`console` aliases and unpacked `core` symbols.
   `get_available_symbols_from_module` now keeps only symbols whose start AND
   end are in the module's own file (an import alias has the import site as its
   start but the imported module's file as its end; an unpacked symbol keeps its
   origin file). Applied in both resolution branches; the fallback branch
   previously had no filter at all.

## Phase 4 - new LSP features

- References + rename (biggest): both need a symbol-occurrence index the
  analyzer does not build yet. One pass mapping declarations to all use sites;
  references reads it, rename edits it with Phase 1 ranges. Advertise the
  capabilities.
- Semantic tokens: leverage the parser token stream plus simulator symbol
  kinds.
- Code actions (quick-fixes for common diagnostics) and range formatting.
- Workspace symbols plus the multi-root fix (`backend.rs:116` picks the first
  folder blindly; resolve the folder holding `peko.toml`).

## Phase 5 - simulator performance (parallelizable) [DONE: memoization]

Per-keystroke regression: every LSP request re-parses and re-simulates the full
document plus the entire `std` import chain.

Delivered: `simulate_document` is memoized per file by a hash of the source
(`PekoAnalyzer::simulation_cache`, a `Mutex<HashMap<PathBuf, (u64,
SimulationResult)>>`). The several requests the editor fires on one text
version (completion, hover, signature help, diagnostics) now share one
simulation instead of each re-running it. A cache hit returns a clone of the
prior result, which is cheap because the simulator's modules are
reference-counted (`Arc<RwLock<SimulatorModule>>`) and every query method the
LSP calls is read-only w.r.t. them (verified: no `.write()` through the module
Arcs). `update_file` clears the whole memo, so a change to any file (the edited
one or one it imports) forces a fresh simulation; `close_file` drops that
file's entry. Workspace clippy and tests green.

Not done, and why: a bigger win would cache the simulated `std` submodules
globally and let each keystroke's simulation reuse them through the resolver's
reuse path. That is rejected because it reintroduces exactly the issue-3
problem - the `context.rs:1042` fallback resolves a `module::` access from
`top_level_modules` by name, so a globally-present `io` would offer `io::`
completions with no `import`. Enabling it safely first requires tightening that
fallback to only resolve modules actually imported by the current file, which
needs its own validation.

Investigation findings (this pass):

- The import resolver DOES have a reuse path
  (`statement_sims.rs:1167`): when simulating `import std::io` it searches
  `top_level_modules` for a module whose file canonicalizes to the target and
  reuses it, skipping re-read/re-parse/re-simulate. So an already-loaded module
  is genuinely cheap.
- BUT naive startup preloading is NOT safe. `default_preloaded_imports` is
  auto-injected into every file, so adding `io`/`fs`/etc there would make them
  importable without an `import`. Preloading into `preloaded_modules` instead
  still changes behavior: `get_available_symbols_from_module`
  (`context.rs:1042`) has a fallback that resolves a `module::` access directly
  from `top_level_modules` by name, so a preloaded `io` would offer `io::`
  completions with no `import` (and the build would then fail). This is a
  misleading side-effect.
- Therefore prefer MEMOIZATION over preloading:
  1. Memoize imported-module simulations by `(path, content-hash)` and reuse the
     cached `SimulatorModule`. This only speeds up modules the file actually
     imports; it does not change which modules are visible.
  2. Memoize `simulate_document` by `(path, text-hash)` so the several
     per-request calls (completion + hover + signatureHelp + diagnostics on one
     edit) do not each re-simulate.

Profile first to confirm std re-simulation is the hotspot. Both memoization
steps want profiling plus in-editor validation before shipping.

## Phase 6 - deep simulator generic-body gap

The latent `type T is not defined` on imported generic-class bodies
(`Box<T>`/`Option<T>`) is worked around by file-scoping diagnostics, not fixed.
Apply `check_generics_erased`'s param binding when simulating any module's
generic-class body, not just the `--astir-std` current module. Removes the
workaround and makes editor diagnostics trustworthy.

## Suggested execution order

Phase 1 (foundation) -> Phase 5 in parallel (perf is felt on every keystroke)
-> Phase 3 (quick wins riding on Phase 1) -> Phase 2 -> Phase 4 (features) ->
Phase 6.
