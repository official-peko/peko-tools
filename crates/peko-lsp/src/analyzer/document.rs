//! In-memory representation of a tracked source file.
//!
//! `PekoDocument` holds the source text plus a precomputed table of byte
//! offsets for the start of each line, so converting between LSP `Position`
//! values (line/character) and byte offsets is constant-time.
//!
//! The byte-vs-char distinction matters: all offsets used by this module are
//! byte offsets into `source`, and [`PekoDocument::char_at`] returns whatever
//! byte is at that offset cast to `char`.

use crate::char_is_peko_id_eligible;
use crate::server::analysis::Position;

/// A tracked source file plus the byte offsets of its line starts.
pub struct PekoDocument {
    source: String,
    /// Byte offset at which each line begins. Always has at least one entry
    /// (the start of line 0).
    line_start_indices: Vec<usize>,
}

impl PekoDocument {
    /// Construct a document from a source string. Computes the line-start
    /// table in one pass.
    pub fn from(source: impl ToString) -> Self {
        let source = source.to_string();
        let mut line_index = 0;
        let line_start_indices = source
            .split('\n')
            .map(|component| {
                let start = line_index;
                line_index += component.len() + 1;
                start
            })
            .collect();
        Self {
            source,
            line_start_indices,
        }
    }

    /// Owned copy of the full source text.
    pub fn contents(&self) -> String {
        self.source.clone()
    }

    /// Return a slice of up to `back` bytes ending at byte offset `offset`
    /// (inclusive). If `offset` is closer to the start than `back` bytes, the
    /// slice is clamped to the document's start.
    pub fn string_back(&self, offset: usize, back: usize) -> &str {
        let end = (offset + 1).min(self.source.len());
        let start = if offset < back { 0 } else { offset - back + 1 };
        let start = start.min(end);
        self.source.get(start..end).unwrap_or("")
    }

    /// Return a slice of up to `forward` bytes starting at byte offset
    /// `offset`. Clamped at the document's end.
    pub fn string_forward(&self, offset: usize, forward: usize) -> &str {
        let len = self.source.len();
        let start = offset.min(len);
        let end = offset.saturating_add(forward).min(len);
        self.source.get(start..end).unwrap_or("")
    }

    /// Return the byte at `offset` cast to `char`, or `None` if `offset` is
    /// out of bounds. This is byte-exact rather than char-exact
    pub fn char_at(&self, offset: usize) -> Option<char> {
        self.source.as_bytes().get(offset).map(|&b| b as char)
    }

    /// Byte offset of the given LSP position inside the source. Positions on a
    /// line past the end of the document resolve to the end of the source. The
    /// returned offset is clamped to the length of the source.
    pub fn offset_at(&self, position: &Position) -> usize {
        match self.line_start_indices.get(position.line as usize) {
            Some(&line_start) => (line_start + position.character as usize).min(self.source.len()),
            None => self.source.len(),
        }
    }

    /// Byte offset of the byte before the start of the given LSP position's
    /// line. Positions on a line past the end of the document resolve to the
    /// last byte of the source. The result never underflows for an empty
    /// source or for line zero.
    pub fn max_offset_at(&self, position: &Position) -> usize {
        match self.line_start_indices.get(position.line as usize) {
            Some(&line_start) => line_start.saturating_sub(1),
            None => self.source.len().saturating_sub(1),
        }
    }

    /// Extract the Peko identifier that contains the byte at `position`, or
    /// the empty string if the position is not inside an identifier.
    pub fn identifier_at(&self, position: &Position) -> String {
        let is_id = |offset: usize| {
            self.char_at(offset)
                .is_some_and(|c| char_is_peko_id_eligible!(c))
        };

        let offset = self.offset_at(position);
        if !is_id(offset) {
            return String::new();
        }

        let mut start_offset = offset;
        let mut end_offset = offset;

        while start_offset > 0 && is_id(start_offset) {
            start_offset -= 1;
        }
        if !is_id(start_offset) {
            start_offset += 1;
        }

        while end_offset < self.source.len() && is_id(end_offset) {
            end_offset += 1;
        }
        end_offset = end_offset.min(self.source.len());

        self.source
            .get(start_offset..end_offset)
            .unwrap_or("")
            .to_string()
    }
}
