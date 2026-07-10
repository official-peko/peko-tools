//! The mutable state threaded through every `format` call.
//!
//! [`FormatContext`] owns the output buffer and the current indentation level,
//! and exposes the small set of primitives every `Format` implementation uses:
//! [`write`](FormatContext::write) for inline text,
//! [`newline`](FormatContext::newline) to end a line, and
//! [`indent`](FormatContext::indent) / [`dedent`](FormatContext::dedent) to
//! change depth. Indentation is written lazily on the first non-empty write of
//! a line, so a line that receives no content stays truly blank.

use super::data_structures::FormatConfig;

/// Output buffer plus indentation state for one formatting run.
pub struct FormatContext {
    output: String,
    indent_level: usize,
    config: FormatConfig,
    /// True when nothing has been written on the current line yet, so the next
    /// non-empty write must first emit the line's indentation.
    at_line_start: bool,
    /// The lines of the source being formatted, used to decide whether the
    /// author left a blank line before a construct. Empty when the source is
    /// not available, which disables blank-line preservation.
    source_lines: Vec<String>,
}

impl FormatContext {
    /// Start an empty formatting run with the given configuration and the
    /// source text being formatted (for blank-line preservation).
    pub fn new(config: FormatConfig, source: &str) -> Self {
        Self {
            output: String::new(),
            indent_level: 0,
            config,
            at_line_start: true,
            source_lines: source.lines().map(str::to_string).collect(),
        }
    }

    /// Whether the source line immediately before the 1-based `line` is blank.
    /// Drives blank-line preservation from the reliable start line of a
    /// construct rather than the parser's less reliable end positions.
    pub fn blank_line_precedes(&self, line: usize) -> bool {
        line.checked_sub(2)
            .and_then(|index| self.source_lines.get(index))
            .is_some_and(|line| line.trim().is_empty())
    }

    /// Whether the 1-based `line` carries non-whitespace before the 0-based
    /// `column`. A comment beginning there follows code on its line, so it is a
    /// trailing comment rather than one on its own line. Reading the source text
    /// avoids depending on the parser's less reliable statement end positions.
    pub fn has_code_before(&self, line: usize, column: usize) -> bool {
        line.checked_sub(1)
            .and_then(|index| self.source_lines.get(index))
            .is_some_and(|text| {
                !text
                    .chars()
                    .take(column)
                    .collect::<String>()
                    .trim()
                    .is_empty()
            })
    }

    /// The configuration in effect for this run.
    pub fn config(&self) -> &FormatConfig {
        &self.config
    }

    /// The configured maximum line width, or `None` when width-based wrapping
    /// is disabled (a configured width of zero).
    pub fn max_width(&self) -> Option<usize> {
        match self.config.max_width {
            0 => None,
            width => Some(width),
        }
    }

    /// Render `render` into a throwaway context sharing this run's config and
    /// return the text it produced. Used to measure a construct's single-line
    /// width before deciding whether to wrap it. The scratch starts at column
    /// zero, so the result is the construct's intrinsic width with no leading
    /// indentation.
    pub fn measure(&self, render: impl FnOnce(&mut FormatContext)) -> String {
        let mut scratch = FormatContext::new(self.config.clone(), "");
        render(&mut scratch);
        scratch.output
    }

    /// The output width already used on the current line, in bytes. Used by
    /// width-based wrapping to decide whether a construct fits.
    pub fn current_column(&self) -> usize {
        if self.at_line_start {
            self.indent_level * self.config.indent_unit.len()
        } else {
            match self.output.rfind('\n') {
                Some(index) => self.output.len() - index - 1,
                None => self.output.len(),
            }
        }
    }

    /// Append inline text, emitting the pending line indentation first if this
    /// is the first content on the line. Empty text is ignored so it cannot
    /// trigger a stray indent on an otherwise-blank line.
    pub fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.at_line_start {
            for _ in 0..self.indent_level {
                self.output.push_str(&self.config.indent_unit);
            }
            self.at_line_start = false;
        }
        self.output.push_str(text);
    }

    /// End the current line, trimming any trailing spaces or tabs first so no
    /// line carries trailing whitespace.
    pub fn newline(&mut self) {
        while self.output.ends_with(' ') || self.output.ends_with('\t') {
            self.output.pop();
        }
        self.output.push('\n');
        self.at_line_start = true;
    }

    /// Emit a single blank separating line, collapsing a run so at most one
    /// blank line is produced.
    pub fn blank_line(&mut self) {
        if !self.at_line_start {
            self.newline();
        }
        if self.output.ends_with("\n\n") || self.output.is_empty() {
            return;
        }
        self.output.push('\n');
    }

    /// Increase the indentation level by one.
    pub fn indent(&mut self) {
        self.indent_level += 1;
    }

    /// Decrease the indentation level by one, saturating at zero.
    pub fn dedent(&mut self) {
        self.indent_level = self.indent_level.saturating_sub(1);
    }

    /// Finish the run, returning the buffer normalized to end with exactly one
    /// newline (or empty for empty input).
    pub fn finish(mut self) -> String {
        while self.output.ends_with('\n') {
            self.output.pop();
        }
        if !self.output.is_empty() {
            self.output.push('\n');
        }
        self.output
    }
}
