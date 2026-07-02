//! In-memory representation of a tracked source file.
//!
//! `PekoDocument` holds the source text plus a precomputed table of byte
//! offsets for the start of each line.
//!
//! Two coordinate spaces meet here. An [`analysis::Position`] is char-based:
//! its `character` counts Unicode scalar values from the start of the line,
//! matching peko_core. The cursor-context back-search walks the source in byte
//! offsets. [`PekoDocument::offset_at`] converts a char-based position to a
//! byte offset, and [`PekoDocument::position_data_at_byte`] converts a byte
//! offset back to a char-based peko_core position.

use std::path::Path;

use peko_core::asts::data_structures::PositionData;

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

    /// Byte offset of a char-based position inside the source. The `character`
    /// field counts chars from the start of the line; it is advanced that many
    /// chars within the line to reach a byte offset. A character past the end
    /// of the line clamps to the end of that line's content. A line past the
    /// end of the document resolves to the end of the source.
    pub fn offset_at(&self, position: &Position) -> usize {
        let Some(&line_start) = self.line_start_indices.get(position.line as usize) else {
            return self.source.len();
        };
        // End of this line's content, excluding the terminating newline.
        let line_end = self
            .line_start_indices
            .get(position.line as usize + 1)
            .map(|&next| next.saturating_sub(1))
            .unwrap_or(self.source.len())
            .min(self.source.len());
        let line = self.source.get(line_start..line_end).unwrap_or("");
        let byte_in_line = line
            .char_indices()
            .nth(position.character as usize)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len());
        (line_start + byte_in_line).min(self.source.len())
    }

    /// Largest char boundary at or before `byte`. Used to snap a byte offset
    /// produced by the back-search onto a char boundary before deriving a
    /// char-based position from it.
    fn floor_char_boundary(&self, mut byte: usize) -> usize {
        byte = byte.min(self.source.len());
        while byte > 0 && !self.source.is_char_boundary(byte) {
            byte -= 1;
        }
        byte
    }

    /// Build a peko_core [`PositionData`] for a byte offset into the source.
    /// The column and index are char-based (Unicode scalar counts), matching
    /// the positions peko_core produces, and the line is 1-based. The byte
    /// offset is clamped to the source length and snapped to a char boundary.
    pub fn position_data_at_byte(&self, byte: usize, file: &Path) -> PositionData {
        let byte = self.floor_char_boundary(byte);
        // 0-based line: the last line whose start is at or before `byte`.
        let line = self
            .line_start_indices
            .partition_point(|&start| start <= byte)
            .saturating_sub(1);
        let line_start = self.line_start_indices.get(line).copied().unwrap_or(0);
        let column = self
            .source
            .get(line_start..byte)
            .map(|slice| slice.chars().count())
            .unwrap_or(0);
        let index = self
            .source
            .get(..byte)
            .map(|slice| slice.chars().count())
            .unwrap_or(0);
        PositionData::new(column, index, line + 1, file.to_path_buf())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn offset_at_maps_char_column_to_byte_across_multibyte() {
        // "let caf" is 7 bytes; the accented e that follows is 2 bytes.
        let doc = PekoDocument::from("let caf\u{e9} = x\n");
        // Char column 7 is the accented e; its byte offset is 7.
        assert_eq!(doc.offset_at(&pos(0, 7)), 7);
        // Char column 8 is the space after it; the accented e spans two bytes,
        // so the byte offset is 9, not 8.
        assert_eq!(doc.offset_at(&pos(0, 8)), 9);
    }

    #[test]
    fn offset_at_clamps_character_past_line_end() {
        let doc = PekoDocument::from("ab\ncd\n");
        // Far past the end of line 0 clamps to the end of that line's content.
        assert_eq!(doc.offset_at(&pos(0, 99)), 2);
    }

    #[test]
    fn position_data_at_byte_is_char_based() {
        // Line 0 holds an accented e (2 bytes). Line 1 starts after the newline.
        let doc = PekoDocument::from("x = \"\u{e9}\"\ny = 1\n");
        let line1_start = "x = \"\u{e9}\"\n".len();
        let position = doc.position_data_at_byte(line1_start, Path::new("t.peko"));
        // Line is 1-based; column and index count chars, not bytes.
        assert_eq!(position.line, 2);
        assert_eq!(position.column, 0);
        // Eight chars precede the byte: x, space, =, space, ", e-acute, ", newline.
        assert_eq!(position.index, 8);
    }

    #[test]
    fn position_data_at_byte_snaps_into_char_boundary() {
        let doc = PekoDocument::from("\u{1f600}b\n");
        // A byte offset inside the leading emoji snaps back to its start.
        let position = doc.position_data_at_byte(2, Path::new("t.peko"));
        assert_eq!(position.column, 0);
        assert_eq!(position.index, 0);
    }
}
