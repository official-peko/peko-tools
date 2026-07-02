//! Formatter configuration.
//!
//! [`FormatConfig`] carries the knobs the formatter reads while printing:
//! the indentation unit and the target line width for wrapping. It is the
//! formatter's analogue of the simulator's tuning state, kept separate from
//! the mutable [`super::context::FormatContext`] so it can be constructed and
//! passed by value.

/// Tunable formatting options.
#[derive(Clone, Debug)]
pub struct FormatConfig {
    /// The string emitted for one level of indentation.
    pub indent_unit: String,

    /// Target maximum line width. Argument lists and other breakable
    /// constructs wrap when a single line would exceed this. A value of zero
    /// disables width-based wrapping.
    pub max_width: usize,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            indent_unit: "    ".to_string(),
            max_width: 100,
        }
    }
}
