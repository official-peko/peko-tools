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
        if offset < back {
            &self.source[0..offset]
        } else {
            &self.source[(offset - back + 1)..offset + 1]
        }
    }

    /// Return a slice of up to `forward` bytes starting at byte offset
    /// `offset`. Clamped at the document's end.
    pub fn string_forward(&self, offset: usize, forward: usize) -> &str {
        if forward + offset >= self.source.len() {
            &self.source[offset..self.source.len()]
        } else {
            &self.source[offset..offset + forward]
        }
    }

    /// Return the byte at `offset` cast to `char`, or `None` if `offset` is
    /// out of bounds. This is byte-exact rather than char-exact
    pub fn char_at(&self, offset: usize) -> Option<char> {
        self.source.as_bytes().get(offset).map(|&b| b as char)
    }

    /// Byte offset of the given LSP position inside the source.
    pub fn offset_at(&self, position: &Position) -> usize {
        if position.line > 0 && (position.line - 1) as usize > self.line_start_indices.len() {
            self.source.len() - 1
        } else {
            self.line_start_indices[position.line as usize] + (position.character as usize)
        }
    }

    /// Byte offset of the last character on the given LSP position's line.
    pub fn max_offset_at(&self, position: &Position) -> usize {
        if position.line > 0 && (position.line - 1) as usize > self.line_start_indices.len() {
            self.source.len() - 1
        } else {
            self.line_start_indices[position.line as usize] - 1
        }
    }

    /// Extract the Peko identifier that contains the byte at `position`, or
    /// the empty string if the position is not inside an identifier.
    pub fn identifier_at(&self, position: &Position) -> String {
        if !char_is_peko_id_eligible!(self.char_at(self.offset_at(position)).unwrap()) {
            return String::new();
        }

        let mut start_offset = self.offset_at(position);
        let mut end_offset = start_offset;

        while start_offset > 0 && char_is_peko_id_eligible!(self.char_at(start_offset).unwrap()) {
            start_offset -= 1;
        }
        if !char_is_peko_id_eligible!(self.char_at(start_offset).unwrap()) {
            start_offset += 1;
        }

        while end_offset < self.source.len()
            && char_is_peko_id_eligible!(self.char_at(end_offset).unwrap())
        {
            end_offset += 1;
        }
        end_offset = std::cmp::min(end_offset, self.source.len() - 1);

        self.source[start_offset..end_offset].to_string()
    }
}
