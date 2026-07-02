//! Open-document tracking.
//!
//! Keeps the current text of every open file in a `ropey::Rope` so that the
//! incremental `textDocument/didChange` deltas sent by the editor can be
//! applied cheaply. The full text is reconstituted on demand for the analysis
//! engine, which currently does a full reparse on every change.

use std::path::PathBuf;

use dashmap::DashMap;
use ropey::Rope;
use tower_lsp_server::ls_types::{TextDocumentContentChangeEvent, Uri};

use crate::server::converters::uri_to_path;

/// A single tracked document.
#[derive(Debug)]
struct Document {
    version: i32,
    text: Rope,
}

impl Document {
    fn new(version: i32, text: &str) -> Self {
        Self {
            version,
            text: Rope::from_str(text),
        }
    }

    /// Full source text as an owned `String`.
    fn full_text(&self) -> String {
        self.text.to_string()
    }

    /// Apply incremental or full-replacement change events in order. Any event
    /// whose range is out-of-bounds is logged and treated as a full replacement
    /// so the rope cannot end up corrupted.
    fn apply_changes(&mut self, changes: Vec<TextDocumentContentChangeEvent>) {
        for change in changes {
            match change.range {
                // Full replacement: no range given.
                None => {
                    self.text = Rope::from_str(&change.text);
                }
                // Incremental update.
                Some(range) => {
                    let bounds = rope_offset(&self.text, range.start)
                        .zip(rope_offset(&self.text, range.end));

                    let Some((start, end)) = bounds else {
                        tracing::warn!(
                            "edit range outside rope of length `{}`. Falling back to full replacement",
                            self.text.len_chars()
                        );
                        self.text = Rope::from_str(&change.text);
                        continue;
                    };

                    if start > end {
                        tracing::warn!(
                            "inverted edit range `[{start}..{end}]`. Falling back to full replacement"
                        );
                        self.text = Rope::from_str(&change.text);
                        continue;
                    }

                    self.text.remove(start..end);
                    self.text.insert(start, &change.text);
                }
            }
        }
    }
}

/// Convert an LSP `Position` (0-based line + character) to a char offset
/// inside the rope. Returns `None` when the line is past the end of the rope.
/// The character is clamped to the length of its line.
fn rope_offset(rope: &Rope, pos: tower_lsp_server::ls_types::Position) -> Option<usize> {
    let line = pos.line as usize;
    if line >= rope.len_lines() {
        return None;
    }
    let line_start = rope.line_to_char(line);
    let line_len = rope.line(line).len_chars();
    let character = (pos.character as usize).min(line_len);
    Some(line_start + character)
}

// ---------------------------------------------------------------------------
// Document store
// ---------------------------------------------------------------------------

/// Thread-safe map of open documents, keyed by file path.
///
/// Backed by `DashMap` so concurrent LSP handlers (hover, completion, etc.)
/// can read without blocking each other.
#[derive(Default)]
pub struct DocumentStore {
    docs: DashMap<PathBuf, Document>,
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    /// Open (or replace) a document.
    pub fn open(&self, uri: &Uri, version: i32, text: &str) {
        let path = uri_to_path(uri);
        tracing::debug!(uri = %uri.as_str(), version, "DocumentStore::open");
        self.docs.insert(path, Document::new(version, text));
    }

    /// Apply incremental or full-replacement change events and bump the version.
    pub fn update(&self, uri: &Uri, version: i32, changes: Vec<TextDocumentContentChangeEvent>) {
        let path = uri_to_path(uri);
        tracing::debug!(uri = %uri.as_str(), version, num_changes = changes.len(), "DocumentStore::update");
        if let Some(mut doc) = self.docs.get_mut(&path) {
            doc.version = version;
            doc.apply_changes(changes);
        } else {
            tracing::warn!(uri = %uri.as_str(), "received change for unknown document. Ignoring");
        }
    }

    /// Remove a document when it is closed.
    pub fn close(&self, uri: &Uri) {
        let path = uri_to_path(uri);
        tracing::debug!(uri = %uri.as_str(), "DocumentStore::close");
        self.docs.remove(&path);
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    /// Return the full current text for a tracked document, or `None` if the
    /// document is not open.
    pub fn get_text(&self, uri: &Uri) -> Option<String> {
        let path = uri_to_path(uri);
        self.docs.get(&path).map(|d| d.full_text())
    }

    /// Return the full current text for a tracked document by path, or `None`
    /// if the document is not open. Used to map positions that point into a
    /// file other than the request document.
    pub fn get_text_by_path(&self, path: &std::path::Path) -> Option<String> {
        self.docs.get(path).map(|d| d.full_text())
    }
}
