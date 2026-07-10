//! Position-encoding transcoding between Peko's char-based internal positions
//! and the wire encoding negotiated with the LSP client.
//!
//! peko_core reports source positions in Unicode scalar values (chars): a
//! column counts chars from the start of a line, not bytes or UTF-16 code
//! units. The LSP protocol lets client and server negotiate whether the
//! `character` field of a wire `Position` counts UTF-8, UTF-16, or UTF-32 code
//! units. UTF-16 is the mandatory default.
//!
//! Every internal position is char-based. This module converts to and from the
//! negotiated wire encoding at the protocol boundary. The conversion needs the
//! text of the relevant line, so it is driven by a [`LineIndex`] built from the
//! document source.

use tower_lsp_server::ls_types::{self as lsp, PositionEncodingKind};

use crate::server::analysis;

/// Code-unit encoding used for the `character` field of a wire `Position`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum WireEncoding {
    /// Character offsets count UTF-8 code units (bytes).
    Utf8,
    /// Character offsets count UTF-16 code units. The LSP default.
    #[default]
    Utf16,
    /// Character offsets count UTF-32 code units (Unicode scalar values).
    Utf32,
}

impl WireEncoding {
    /// Choose an encoding from the list the client says it supports. Prefers
    /// UTF-8, then UTF-16, then UTF-32. Falls back to UTF-16, the mandatory
    /// default, when the client advertises nothing usable.
    pub fn negotiate(client_supported: Option<&[PositionEncodingKind]>) -> Self {
        let Some(kinds) = client_supported else {
            return Self::Utf16;
        };
        let supports = |k: &PositionEncodingKind| kinds.iter().any(|c| c == k);
        if supports(&PositionEncodingKind::UTF8) {
            Self::Utf8
        } else if supports(&PositionEncodingKind::UTF16) {
            Self::Utf16
        } else if supports(&PositionEncodingKind::UTF32) {
            Self::Utf32
        } else {
            Self::Utf16
        }
    }

    /// The wire tag advertised back to the client in the server capabilities.
    pub fn as_kind(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
            Self::Utf32 => PositionEncodingKind::UTF32,
        }
    }

    /// Number of code units one char occupies in this encoding.
    fn width(self, ch: char) -> u32 {
        match self {
            Self::Utf8 => ch.len_utf8() as u32,
            Self::Utf16 => ch.len_utf16() as u32,
            Self::Utf32 => 1,
        }
    }
}

/// Number of wire code units spanned by the first `char_col` chars of `line`.
fn char_col_to_wire(line: &str, char_col: usize, enc: WireEncoding) -> u32 {
    line.chars().take(char_col).map(|ch| enc.width(ch)).sum()
}

/// Number of wire code units in `text` under this encoding. Used to place
/// substring offsets (such as signature-help parameter labels) that the client
/// interprets in the negotiated encoding.
pub fn wire_len(text: &str, enc: WireEncoding) -> u32 {
    text.chars().map(|ch| enc.width(ch)).sum()
}

/// Char column corresponding to a wire column inside `line`. A wire column that
/// lands inside a multi-unit char resolves to the char boundary at or before
/// it. A wire column past the end of the line clamps to the line length in
/// chars.
fn wire_col_to_char_col(line: &str, wire_col: u32, enc: WireEncoding) -> usize {
    let mut consumed = 0u32;
    let mut char_col = 0;
    for ch in line.chars() {
        let width = enc.width(ch);
        if consumed + width > wire_col {
            return char_col;
        }
        consumed += width;
        char_col += 1;
    }
    char_col
}

/// Line-start table over a source string. Converts positions between the
/// internal char-based form and the negotiated wire encoding.
pub struct LineIndex {
    source: String,
    /// Byte offset at which each line begins.
    line_starts: Vec<usize>,
}

impl LineIndex {
    /// Build a line-start table from source text in a single pass.
    pub fn new(source: &str) -> Self {
        let mut byte = 0;
        let line_starts = source
            .split('\n')
            .map(|segment| {
                let start = byte;
                byte += segment.len() + 1;
                start
            })
            .collect();
        Self {
            source: source.to_string(),
            line_starts,
        }
    }

    /// Content of one line with any trailing carriage return and newline
    /// removed. Empty for a line past the end of the source.
    fn line_str(&self, line: usize) -> &str {
        let Some(&start) = self.line_starts.get(line) else {
            return "";
        };
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.source.len());
        self.source
            .get(start..end)
            .unwrap_or("")
            .trim_end_matches('\n')
            .trim_end_matches('\r')
    }

    /// Convert a wire `Position` from the client into an internal char-based
    /// [`analysis::Position`].
    pub fn wire_to_internal(&self, wire: lsp::Position, enc: WireEncoding) -> analysis::Position {
        let char_col = wire_col_to_char_col(self.line_str(wire.line as usize), wire.character, enc);
        analysis::Position {
            line: wire.line,
            character: char_col as u32,
        }
    }

    /// Convert an internal char-based [`analysis::Position`] to a wire
    /// `Position` in the negotiated encoding.
    pub fn internal_to_wire(&self, pos: &analysis::Position, enc: WireEncoding) -> lsp::Position {
        let character = char_col_to_wire(
            self.line_str(pos.line as usize),
            pos.character as usize,
            enc,
        );
        lsp::Position {
            line: pos.line,
            character,
        }
    }

    /// Convert an internal char-based [`analysis::Range`] to a wire `Range`.
    pub fn internal_range_to_wire(&self, r: &analysis::Range, enc: WireEncoding) -> lsp::Range {
        lsp::Range {
            start: self.internal_to_wire(&r.start, enc),
            end: self.internal_to_wire(&r.end, enc),
        }
    }

    /// Wire position of the end of the source: the last line and its length in
    /// wire code units. Used to build a whole-document replacement range.
    pub fn end_of_source(&self, enc: WireEncoding) -> lsp::Position {
        let last_line = self.line_starts.len().saturating_sub(1);
        let character = wire_len(self.line_str(last_line), enc);
        lsp::Position {
            line: last_line as u32,
            character,
        }
    }
}

/// Pairs a [`LineIndex`] with the negotiated encoding so converters can map
/// internal char-based positions to wire positions for one file.
pub struct PosMapper<'a> {
    index: &'a LineIndex,
    encoding: WireEncoding,
}

impl<'a> PosMapper<'a> {
    pub fn new(index: &'a LineIndex, encoding: WireEncoding) -> Self {
        Self { index, encoding }
    }

    /// Map an internal range to a wire range.
    pub fn range(&self, r: &analysis::Range) -> lsp::Range {
        self.index.internal_range_to_wire(r, self.encoding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_identity_in_every_encoding() {
        for enc in [WireEncoding::Utf8, WireEncoding::Utf16, WireEncoding::Utf32] {
            assert_eq!(char_col_to_wire("hello", 5, enc), 5);
            assert_eq!(wire_col_to_char_col("hello", 5, enc), 5);
        }
    }

    #[test]
    fn accented_char_widths() {
        // "cafe" with a trailing e-acute: 4 chars, e-acute is 2 bytes / 1 utf16.
        let line = "caf\u{e9}";
        assert_eq!(char_col_to_wire(line, 4, WireEncoding::Utf8), 5);
        assert_eq!(char_col_to_wire(line, 4, WireEncoding::Utf16), 4);
        assert_eq!(char_col_to_wire(line, 4, WireEncoding::Utf32), 4);
    }

    #[test]
    fn astral_char_widths() {
        // A grinning face U+1F600: 4 bytes / 2 utf16 units / 1 scalar.
        let line = "a\u{1f600}b";
        assert_eq!(char_col_to_wire(line, 3, WireEncoding::Utf8), 6);
        assert_eq!(char_col_to_wire(line, 3, WireEncoding::Utf16), 4);
        assert_eq!(char_col_to_wire(line, 3, WireEncoding::Utf32), 3);
        // The char column of the 'b' that follows the emoji.
        assert_eq!(wire_col_to_char_col(line, 5, WireEncoding::Utf8), 2);
        assert_eq!(wire_col_to_char_col(line, 3, WireEncoding::Utf16), 2);
    }

    #[test]
    fn wire_column_inside_a_char_rounds_down() {
        // UTF-16 column 2 lands inside the astral char, which starts at 1.
        let line = "a\u{1f600}b";
        assert_eq!(wire_col_to_char_col(line, 2, WireEncoding::Utf16), 1);
    }

    #[test]
    fn round_trip_through_line_index() {
        let source = "let x = \"caf\u{e9}\"\nlet y = \"\u{1f600}\"\n";
        let index = LineIndex::new(source);
        for enc in [WireEncoding::Utf8, WireEncoding::Utf16, WireEncoding::Utf32] {
            for line in 0..2u32 {
                for character in 0..12u32 {
                    let internal = analysis::Position { line, character };
                    let wire = index.internal_to_wire(&internal, enc);
                    let back = index.wire_to_internal(wire, enc);
                    // Round trip is stable once clamped to the line length.
                    let reclamped = index.internal_to_wire(&back, enc);
                    assert_eq!(wire, reclamped, "enc {enc:?} line {line} char {character}");
                }
            }
        }
    }

    #[test]
    fn line_str_strips_crlf() {
        let index = LineIndex::new("foo\r\nbar\n");
        assert_eq!(index.line_str(0), "foo");
        assert_eq!(index.line_str(1), "bar");
        assert_eq!(index.line_str(9), "");
    }

    #[test]
    fn negotiate_prefers_utf8_then_falls_back() {
        assert_eq!(WireEncoding::negotiate(None), WireEncoding::Utf16);
        assert_eq!(
            WireEncoding::negotiate(Some(&[PositionEncodingKind::UTF16])),
            WireEncoding::Utf16
        );
        assert_eq!(
            WireEncoding::negotiate(Some(&[
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8
            ])),
            WireEncoding::Utf8
        );
    }
}
