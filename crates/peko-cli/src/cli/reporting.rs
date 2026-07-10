//! Unified reporting surface for the cli.
//!
//! Every line of user-facing output should go through a [`Reporter`]:
//!
//! - **CLI-originated messages** (argument errors, command failures, help
//!   text, success summaries) go through [`Reporter::error`], [`warning`],
//!   [`help`], [`info`], [`success`].
//! - **Compiler diagnostics** (parser / simulator / codegen) arrive as
//!   [`peko_core::diagnostics::PekoDiagnostic`] and are rendered via
//!   [`Reporter::report_diagnostic`] / [`report_diagnostics`].
//! - **Long-running work** (parse / simulate / codegen / link / bundle /
//!   network downloads) uses the [`ProgressSink`] trait, accessed through
//!   [`Reporter::progress`].
//!
//! ## Output streams
//!
//! Errors, warnings, and compiler diagnostics go to **stderr**. Info /
//! success / help-menu text goes to **stdout**. This matches Unix convention
//! and lets users pipe `2>/dev/null` to suppress diagnostics in scripts.
//!
//! ## Color
//!
//! Color is enabled when stderr is a terminal and the `NO_COLOR` environment
//! variable is unset. Color decisions are computed once at [`Reporter::new`]
//! and cached on the reporter.
//!
//! ## Source caching
//!
//! Rendering a diagnostic needs the file's source text to draw the source
//! snippet. A batch of N diagnostics for one file would otherwise re-read
//! the file N times. The reporter caches source text by `PathBuf` for the
//! lifetime of the run.
//!
//! [`warning`]: Reporter::warning
//! [`help`]: Reporter::help
//! [`info`]: Reporter::info
//! [`success`]: Reporter::success
//! [`report_diagnostics`]: Reporter::report_diagnostics

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Display;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use console::Style;
use peko_core::diagnostics::{DiagnosticList, DiagnosticType, PekoDiagnostic};

/// Width of the right-aligned verb column on status lines (the
/// `Building`, `Finished`, `error`, etc. prefix). Chosen to fit the
/// longest standard verb ("warning") with a couple of characters of
/// indent.
pub const STATUS_VERB_WIDTH: usize = 12;

// ---------------------------------------------------------------------------
// ProgressSink - the trait orchestration consumes.
// ---------------------------------------------------------------------------

/// Sink for fine-grained progress events emitted by the cli's orchestration
/// (parse / simulate / codegen / link / bundle stages, plus the
/// firebase-backed package commands).
///
/// Each method takes `&self` so a sink can be shared across the orchestration
/// without threading mutability through every call site. Implementations
/// must handle their own interior synchronization.
pub trait ProgressSink {
    /// Begin a new named top-level phase. Any prior phase is implicitly
    /// finished. The total unit count for the new phase is reset; call
    /// [`set_total`] once the count is known.
    ///
    /// [`set_total`]: ProgressSink::set_total
    fn start_phase(&self, name: &str);

    /// Set the unit count for the active phase. Safe to call after
    /// [`start_phase`] and again later to revise the count (e.g. after a
    /// parse pass resolves the full import graph).
    ///
    /// Use [`add_to_total`] when newly-discovered work needs to be tacked
    /// on without disturbing the current position; `set_total` overwrites
    /// the entire length and is for the initial-count case.
    ///
    /// [`start_phase`]: ProgressSink::start_phase
    /// [`add_to_total`]: ProgressSink::add_to_total
    fn set_total(&self, total: u64);

    /// Append additional work units to the active phase's total without
    /// resetting position. Useful when an orchestrator discovers more
    /// files to process mid-loop (e.g. the incremental compiler queueing
    /// newly-imported modules).
    fn add_to_total(&self, extra: u64);

    /// Advance the active phase by one unit, updating the in-progress
    /// label.
    fn tick(&self, label: &str);

    /// Update the active phase's message without advancing position. Useful
    /// for long single-unit operations that want to show sub-status during
    /// the work.
    fn message(&self, msg: &str);

    /// Mark the active phase as complete and clear any visible bar.
    fn finish_phase(&self);
}

/// No-op [`ProgressSink`] for callers that don't want progress output (tests,
/// sub-tasks, the default reporter before any sink is attached).
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn start_phase(&self, _name: &str) {}
    fn set_total(&self, _total: u64) {}
    fn add_to_total(&self, _extra: u64) {}
    fn tick(&self, _label: &str) {}
    fn message(&self, _msg: &str) {}
    fn finish_phase(&self) {}
}

/// A [`ProgressSink`] backed by [`indicatif::ProgressBar`].
///
/// The bar is shown only when stderr is a terminal - when piped to a file or
/// non-tty, the sink degrades to silent (per indicatif's default `is_hidden`
/// detection). All bars share a single visual style across the cli.
pub struct IndicatifSink {
    bar: indicatif::ProgressBar,
}

impl Default for IndicatifSink {
    fn default() -> Self {
        Self::new()
    }
}

impl IndicatifSink {
    /// Bar style used by every cli progress bar. Matches the original
    /// per-command styles but in one place so every command looks the
    /// same. The bar uses solid block fill and dashes for empty space,
    /// so what's left to do reads clearly as a dashed-out continuation
    /// of the bar rather than as a colored void.
    const TEMPLATE: &'static str =
        "[{elapsed_precise}] {bar:30.cyan/white.dim} {pos:>4}/{len:<4} {wide_msg}";
    const SPINNER_TEMPLATE: &'static str = "[{elapsed_precise}] {spinner:.cyan} {wide_msg}";
    /// Filled, current, and empty characters used by the progress bar.
    /// The first is a solid Unicode full block for the filled portion; the
    /// second is a Unicode left-half block that marks the transition cell at
    /// the edge of progress; the third is a plain ASCII dash for the
    /// still-to-go portion, so the empty space reads as `----...` in dim white.
    const PROGRESS_CHARS: &'static str = "█▌-";

    /// Build a new sink with a hidden bar that will be revealed once a phase
    /// starts.
    pub fn new() -> Self {
        let bar = indicatif::ProgressBar::hidden();
        bar.set_style(
            indicatif::ProgressStyle::with_template(Self::TEMPLATE)
                .expect("static template")
                .progress_chars(Self::PROGRESS_CHARS),
        );
        Self { bar }
    }

    /// Returns the underlying [`indicatif::ProgressBar`] for callers that
    /// need direct access (e.g. computing elapsed time at the end of a
    /// command).
    pub fn bar(&self) -> &indicatif::ProgressBar {
        &self.bar
    }
}

impl ProgressSink for IndicatifSink {
    fn start_phase(&self, name: &str) {
        // Clear any finished state from a previous phase. A bar that was ended
        // with finish_and_clear is otherwise inert, so a later phase's draws
        // (this is the build phase, after the dependency-resolution phase
        // finished) would be silently dropped and no bar would appear.
        self.bar.reset();
        // Reveal the bar in case it was hidden, reset position and length.
        self.bar
            .set_draw_target(indicatif::ProgressDrawTarget::stderr());
        self.bar.set_style(
            indicatif::ProgressStyle::with_template(Self::TEMPLATE)
                .expect("static template")
                .progress_chars(Self::PROGRESS_CHARS),
        );
        self.bar.set_position(0);
        self.bar.set_length(0);
        self.bar.set_message(name.to_owned());
        self.bar.enable_steady_tick(Duration::from_millis(100));
    }

    fn set_total(&self, total: u64) {
        // If total is zero we're in a "work count unknown" situation;
        // switch to a spinner-style bar instead of an empty progress bar.
        if total == 0 {
            self.bar.set_style(
                indicatif::ProgressStyle::with_template(Self::SPINNER_TEMPLATE)
                    .expect("static template"),
            );
            self.bar.set_length(u64::MAX);
        } else {
            // Switch (back) to the bar template (set_total may be called
            // after a spinner phase to reveal a known length).
            self.bar.set_style(
                indicatif::ProgressStyle::with_template(Self::TEMPLATE)
                    .expect("static template")
                    .progress_chars(Self::PROGRESS_CHARS),
            );
            self.bar.set_length(total);
        }
    }

    fn add_to_total(&self, extra: u64) {
        let current = self.bar.length().unwrap_or(0);
        self.bar.set_length(current.saturating_add(extra));
    }

    fn tick(&self, label: &str) {
        self.bar.set_message(label.to_owned());
        self.bar.inc(1);
    }

    fn message(&self, msg: &str) {
        self.bar.set_message(msg.to_owned());
    }

    fn finish_phase(&self) {
        self.bar.finish_and_clear();
    }
}

// --------------------------------------------------------------------------
// Reporter: owns color settings, source cache, and the progress sink.
// --------------------------------------------------------------------------

/// Central output type for the cli.
///
/// A single [`Reporter`] is constructed at the top of each subcommand and
/// passed by reference to anything that needs to print. Cloning is cheap
/// (the source cache is shared via [`std::rc::Rc`]-style interior mutability,
/// but `Reporter` is not `Send` since most cli work is single-threaded).
pub struct Reporter {
    use_color: bool,
    /// Maps source file paths to their textual contents, so a batch of
    /// diagnostics covering the same file only reads it once.
    source_cache: RefCell<HashMap<PathBuf, Option<String>>>,
    /// Progress sink. Defaults to [`NoProgress`]; callers swap in an
    /// [`IndicatifSink`] via [`with_progress`] when they want a visible bar.
    ///
    /// [`with_progress`]: Reporter::with_progress
    progress: Box<dyn ProgressSink>,
    verbosity: Verbosity,
    /// When set, output is emitted as newline-delimited JSON events on stdout
    /// (one object per line) for machine consumption (`--json`), instead of the
    /// human-readable colored lines.
    json: bool,
}

/// How much of the reporter's output is actually printed. Set once at
/// startup from the cli's global flags (`--quiet`, `--verbose`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    /// Only errors and warnings are printed. `info` and `success` are
    /// suppressed.
    Quiet,
    /// Default level: errors, warnings, info, success, help.
    #[default]
    Normal,
    /// Same as Normal at the moment, but reserved for future
    /// extra-noisy output. Reporter callers can branch on
    /// [`Reporter::is_verbose`] to skip emitting expensive details
    /// unless the user opted in.
    Verbose,
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter {
    /// Build a reporter with color auto-detected (on if stderr is a TTY and
    /// `NO_COLOR` is unset) and a no-op progress sink.
    pub fn new() -> Self {
        let use_color = io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            use_color,
            source_cache: RefCell::new(HashMap::new()),
            progress: Box::new(NoProgress),
            verbosity: Verbosity::Normal,
            json: false,
        }
    }

    /// Switch to machine-readable JSON output (`--json`). Every status, info,
    /// success, warning, error, and diagnostic is emitted as one JSON object per
    /// line on stdout. Color is turned off so the stream stays clean.
    pub fn set_json(&mut self, on: bool) {
        self.json = on;
        if on {
            self.use_color = false;
        }
    }

    /// Whether JSON output is active.
    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Emit one JSON event line on stdout.
    fn emit_event(&self, kind: &str, message: &str) {
        let event = serde_json::json!({ "type": kind, "message": message });
        println!("{event}");
    }

    /// Attach a progress sink. Replaces any previously attached sink.
    pub fn with_progress<S: ProgressSink + 'static>(mut self, sink: S) -> Self {
        self.progress = Box::new(sink);
        self
    }

    /// Borrow the attached progress sink. Orchestration code should accept
    /// `&dyn ProgressSink` and receive this borrow.
    pub fn progress(&self) -> &dyn ProgressSink {
        &*self.progress
    }

    /// Force-disable color (e.g. when a parent process has signalled
    /// `--no-color`). Reporter will print plain text regardless of the
    /// detected terminal.
    pub fn disable_color(&mut self) {
        self.use_color = false;
    }

    /// Returns `true` if the reporter is rendering with ANSI color.
    pub fn use_color(&self) -> bool {
        self.use_color
    }

    /// Set the verbosity dial.
    pub fn set_verbosity(&mut self, verbosity: Verbosity) {
        self.verbosity = verbosity;
    }

    /// Returns `true` if the reporter is in Verbose mode.
    pub fn is_verbose(&self) -> bool {
        self.verbosity == Verbosity::Verbose
    }

    /// Returns `true` if the reporter is in Quiet mode.
    pub fn is_quiet(&self) -> bool {
        self.verbosity == Verbosity::Quiet
    }

    // ----- Top-level message kinds -----------------------------------------

    /// Print an error line to stderr.
    ///
    /// Errors are always printed regardless of verbosity.
    ///
    /// Format: a right-aligned bold red `error` verb followed by the
    /// message. Width matches `success`, `info`, etc. for vertical
    /// alignment when rendered next to each other.
    pub fn error(&self, msg: impl Display) {
        self.write_status_stderr("error", Style::new().red().bold(), msg);
    }

    /// Print a warning line to stderr.
    ///
    /// Warnings are always printed regardless of verbosity.
    ///
    /// Format: a right-aligned bold yellow `warning` verb followed by
    /// the message.
    pub fn warning(&self, msg: impl Display) {
        self.write_status_stderr("warning", Style::new().yellow().bold(), msg);
    }

    /// Print a help / suggestion line to stdout.
    ///
    /// Help lines are suppressed in Quiet mode.
    ///
    /// Format: a right-aligned bold cyan `help` verb followed by the
    /// message.
    pub fn help(&self, msg: impl Display) {
        if self.is_quiet() {
            return;
        }
        self.write_status_stdout("help", Style::new().cyan().bold(), msg);
    }

    /// Print an informational action line to stdout.
    ///
    /// Action lines are suppressed in Quiet mode.
    ///
    /// Format: a right-aligned bold blue `info` verb followed by the
    /// message. Use [`status`] when there's a more specific verb
    /// (e.g. `Building`, `Compiling`, `Cleaning`).
    ///
    /// [`status`]: Reporter::status
    pub fn info(&self, msg: impl Display) {
        if self.is_quiet() {
            return;
        }
        self.write_status_stdout("info", Style::new().blue().bold(), msg);
    }

    /// Print a custom action line to stdout with a caller-provided
    /// verb. The verb is right-aligned and rendered in bold green.
    ///
    /// This is the right method for build-style action lines:
    ///
    /// ```ignore
    /// reporter.status("Building", format!("{} as a CLI project", name));
    /// reporter.status("Cleaning", "build info");
    /// reporter.status("Finished", format!("{name} in {elapsed:.1?}"));
    /// ```
    ///
    /// Action lines are suppressed in Quiet mode.
    pub fn status(&self, verb: &str, detail: impl Display) {
        if self.is_quiet() {
            return;
        }
        self.write_status_stdout(verb, Style::new().green().bold(), detail);
    }

    /// Print a success line to stdout.
    ///
    /// Success lines are suppressed in Quiet mode.
    ///
    /// Format: a right-aligned bold green `Finished` verb followed by
    /// the message. For task-specific completion lines use [`status`]
    /// directly with whatever verb fits.
    ///
    /// [`status`]: Reporter::status
    pub fn success(&self, msg: impl Display) {
        if self.is_quiet() {
            return;
        }
        self.write_status_stdout("Finished", Style::new().green().bold(), msg);
    }

    /// Report download progress for a named component. In JSON mode this emits
    /// one `download` event carrying the byte counts and percent, for a host
    /// that renders a progress bar. In human mode it draws a carriage-returned
    /// status line on stderr, and prints a trailing newline when the download
    /// completes. Quiet mode suppresses the human line but still emits JSON.
    pub fn download_progress(&self, label: &str, downloaded: u64, total: Option<u64>) {
        if self.json {
            let percent = total
                .filter(|t| *t > 0)
                .map(|t| (downloaded as f64 / t as f64 * 100.0).round() as u64);
            let event = serde_json::json!({
                "type": "download",
                "component": label,
                "downloaded": downloaded,
                "total": total,
                "percent": percent,
            });
            println!("{event}");
            return;
        }
        if self.is_quiet() || !self.use_color {
            return;
        }
        match total.filter(|t| *t > 0) {
            Some(total) => {
                let percent = (downloaded as f64 / total as f64 * 100.0).round() as u64;
                let done = downloaded >= total;
                let end = if done { "\n" } else { "" };
                eprint!(
                    "\r{:>width$} {label} {}% ({} / {} MiB){end}",
                    "downloading",
                    percent.min(100),
                    downloaded / 1_048_576,
                    total / 1_048_576,
                    width = STATUS_VERB_WIDTH
                );
            }
            None => {
                eprint!(
                    "\r{:>width$} {label} ({} MiB)",
                    "downloading",
                    downloaded / 1_048_576,
                    width = STATUS_VERB_WIDTH
                );
            }
        }
        let _ = io::Write::flush(&mut io::stderr());
    }

    /// Print a raw line to stdout with no verb prefix or styling.
    /// Intended for command output that's structured data (e.g.
    /// `peko clangflags` writing flags to stdout, or
    /// `peko project show-info` rendering a config table) rather than
    /// progress narration.
    ///
    /// Raw lines are suppressed in Quiet mode.
    pub fn raw(&self, msg: impl Display) {
        if self.is_quiet() {
            return;
        }
        println!("{msg}");
    }

    /// Render a styled action line to stdout. The verb is
    /// right-aligned in a fixed-width column so successive lines
    /// align vertically.
    fn write_status_stdout(&self, verb: &str, style: Style, detail: impl Display) {
        if self.json {
            self.emit_event(&verb.to_lowercase(), &detail.to_string());
            return;
        }
        let padded = format!("{verb:>width$}", verb = verb, width = STATUS_VERB_WIDTH);
        let label = self.styled(padded, style);
        println!("{label} {detail}");
    }

    /// Same as [`write_status_stdout`] but to stderr.
    fn write_status_stderr(&self, verb: &str, style: Style, detail: impl Display) {
        if self.json {
            self.emit_event(&verb.to_lowercase(), &detail.to_string());
            return;
        }
        let padded = format!("{verb:>width$}", verb = verb, width = STATUS_VERB_WIDTH);
        let label = self.styled(padded, style);
        eprintln!("{label} {detail}");
    }

    /// Apply a style if color is enabled; otherwise return the input as
    /// plain text. Returned value implements `Display`.
    fn styled<T: Display>(&self, value: T, style: Style) -> impl Display {
        if self.use_color {
            style.apply_to(value).to_string()
        } else {
            value.to_string()
        }
    }

    // ----- Diagnostics from peko_core --------------------------------------

    /// Render every diagnostic in `list`, in order, to stderr.
    pub fn report_diagnostics(&self, list: &DiagnosticList) {
        for diagnostic in list.get_diagnostics() {
            self.report_diagnostic(diagnostic);
        }
    }

    /// Render a single diagnostic to stderr.
    ///
    /// The output preserves the cli's existing visual:
    ///
    /// - Header line: `Error <message>` or `Warning <message>`, colored
    ///   label, plain message.
    /// - Location line: `--> path:line:col`.
    /// - Source snippet: one line for single-line spans, or top-line /
    ///   ellipsis / bottom-line for multi-line spans.
    /// - Squiggle: caret line of `^` characters under the offending span.
    ///   For multi-line spans, the squiggle is drawn under the **bottom**
    ///   line, aligned with the bottom line's leading whitespace.
    ///
    /// If the source file can't be read (deleted, permission denied, etc.),
    /// the header and location lines are still printed; the snippet and
    /// squiggle are omitted.
    pub fn report_diagnostic(&self, diagnostic: &PekoDiagnostic) {
        if self.json {
            let event = serde_json::json!({
                "type": "diagnostic",
                "severity": diagnostic.diagnostic_type.to_string(),
                "file": diagnostic.file.display().to_string(),
                "line": diagnostic.start.line,
                "column": diagnostic.start.column,
                "endLine": diagnostic.end.line,
                "endColumn": diagnostic.end.column,
                "message": diagnostic.message,
            });
            println!("{event}");
            return;
        }

        // Header
        let header_label = match diagnostic.diagnostic_type {
            DiagnosticType::Error => self.styled("Error", Style::new().red().bold()),
            DiagnosticType::Warning => self.styled("Warning", Style::new().yellow().bold()),
        };
        eprintln!("{header_label} {message}", message = diagnostic.message);

        // Location
        eprintln!(
            "--> {}:{}:{}",
            diagnostic.file.display(),
            diagnostic.start.line,
            diagnostic.start.column,
        );

        // Source snippet + squiggle
        let Some(source) = self.read_source(&diagnostic.file) else {
            // File unreadable; header + location is all we can show.
            return;
        };

        if diagnostic.start.line == diagnostic.end.line {
            self.render_single_line(diagnostic, &source);
        } else {
            self.render_multi_line(diagnostic, &source);
        }
    }

    /// Render the source snippet + squiggle for a single-line span.
    ///
    /// Algorithm preserved from the original cli renderer with the
    /// per-character collection hoisted out of the inner loops. Byte-index
    /// math is safe because `PositionData::index` from the lexer always
    /// falls on a char boundary.
    fn render_single_line(&self, diagnostic: &PekoDiagnostic, source: &str) {
        let bytes = source.as_bytes();

        // Walk backward from `start.index - 1` to find the previous newline
        // (or fall off the front of the file). `line_start` ends up at the
        // first byte of the diagnostic's line. Tabs in the indent are
        // preserved as tabs so the squiggle aligns when the terminal
        // renders tabs at any width.
        let (line_start, indent) = find_line_start_and_indent(bytes, diagnostic.start.index);

        // Pad the indent past the gutter ("LINE | ").
        let gutter_width = diagnostic.start.line.to_string().len() + 3;
        let mut prefix = " ".repeat(gutter_width);
        prefix.push_str(&indent);

        // Walk forward from end.index to find the line's end.
        let mut line_end = diagnostic.end.index;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }

        // Defensive clamp so the byte slice is always valid.
        let line_end = line_end.min(source.len());
        let line_start = line_start.min(line_end);

        eprintln!(
            "{} | {}",
            diagnostic.start.line,
            &source[line_start..line_end]
        );

        let squiggle_len = diagnostic.end.index.saturating_sub(diagnostic.start.index);
        let squiggle = "^".repeat(squiggle_len);
        eprintln!(
            "{prefix}{}",
            self.styled(squiggle, Style::new().red().bold())
        );
    }

    /// Render the source snippet + squiggle for a multi-line span.
    ///
    /// The output shape is:
    ///
    /// ```text
    /// LINE  | top source line
    /// ...   |     ...
    /// END   | bottom source line
    ///             ^^^^^^^^^^^^^
    /// ```
    ///
    /// The squiggle sits under the **bottom** line. Its left-padding is the
    /// bottom line's leading whitespace (tabs preserved as tabs, other ws
    /// as spaces), so the carets line up with the bottom-line text. Its
    /// length is `end.column - 1` (i.e. how far into the bottom line the
    /// diagnostic extends).
    fn render_multi_line(&self, diagnostic: &PekoDiagnostic, source: &str) {
        let bytes = source.as_bytes();

        // ----- Top line bounds -----
        // The top line spans from the first byte after the previous newline
        // (or 0 for line 1) to the next newline (or EOF).
        let (top_start, _) = find_line_start_and_indent(bytes, diagnostic.start.index);
        let mut top_end = diagnostic.start.index;
        while top_end < bytes.len() && bytes[top_end] != b'\n' {
            top_end += 1;
        }
        let top_end = top_end.min(source.len());
        let top_start = top_start.min(top_end);

        // ----- Bottom line bounds -----
        // Original logic: if end.index is past EOF, or sits exactly on a
        // newline, back up one before locating the line. Guarded with
        // saturating_sub so an empty source doesn't underflow.
        let probe = if diagnostic.end.index >= bytes.len() {
            bytes.len().saturating_sub(1)
        } else if diagnostic.end.index > 0 && bytes[diagnostic.end.index] == b'\n' {
            diagnostic.end.index - 1
        } else {
            diagnostic.end.index
        };

        let (bottom_start, _) = find_line_start_and_indent(bytes, probe);
        let mut bottom_end = probe;
        while bottom_end < bytes.len() && bytes[bottom_end] != b'\n' {
            bottom_end += 1;
        }
        let bottom_end = bottom_end.min(source.len());
        let bottom_start = bottom_start.min(bottom_end);

        // ----- Gutter sizing -----
        // The gutter shows the largest line number with a minimum width of
        // 3, so single-digit lines and "..." both fit cleanly.
        let line_number_width = diagnostic.end.line.to_string().len().max(3);
        let top_pad = " ".repeat(line_number_width - diagnostic.start.line.to_string().len());
        let bottom_pad = " ".repeat(line_number_width - diagnostic.end.line.to_string().len());
        let ellipsis_pad = " ".repeat(line_number_width - 3);

        // ----- Ellipsis-line indent -----
        // Walk the top line's leading whitespace so the "..." sits inside
        // the source's indent column.
        let mut ellipsis_indent = String::new();
        let mut i = top_start;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() && bytes[i] != b'\n' {
            ellipsis_indent.push(bytes[i] as char);
            i += 1;
        }
        ellipsis_indent.push('\t');

        // ----- Squiggle indent (bottom line's leading whitespace) -----
        // Tabs preserved as tabs; other whitespace as spaces. This places
        // the squiggle under the bottom line's text.
        let mut squiggle_indent = String::new();
        let mut i = bottom_start;
        while i < bytes.len() && bytes[i] != b'\n' && (bytes[i] as char).is_whitespace() {
            squiggle_indent.push(if bytes[i] == b'\t' { '\t' } else { ' ' });
            i += 1;
        }

        // Pad the squiggle indent past the gutter ("END  | ").
        let gutter_width = line_number_width + 3;
        let mut squiggle_prefix = " ".repeat(gutter_width);
        squiggle_prefix.push_str(&squiggle_indent);

        // ----- Squiggle length: end.column - 1 -----
        // Column is 1-based, so column N means N-1 characters of the
        // bottom line precede the end. Subtract the leading whitespace
        // (already taken by squiggle_indent) so the carets don't double up
        // on it.
        let leading_ws_chars = squiggle_indent.chars().count();
        let squiggle_len = diagnostic
            .end
            .column
            .saturating_sub(1)
            .saturating_sub(leading_ws_chars);

        // ----- Emit -----
        eprintln!(
            "{line}{pad} | {text}",
            line = diagnostic.start.line,
            pad = top_pad,
            text = &source[top_start..top_end],
        );
        eprintln!("...{ellipsis_pad} | {ellipsis_indent}...");
        eprintln!(
            "{line}{pad} | {text}",
            line = diagnostic.end.line,
            pad = bottom_pad,
            text = &source[bottom_start..bottom_end],
        );

        let squiggle = "^".repeat(squiggle_len);
        eprintln!(
            "{squiggle_prefix}{}",
            self.styled(squiggle, Style::new().red().bold())
        );
    }

    /// Read a source file, caching the result for the lifetime of the
    /// reporter. Returns `None` and caches the negative result if the file
    /// can't be read.
    fn read_source(&self, path: &Path) -> Option<String> {
        // Borrow the cache once; fast path for cache hits.
        if let Some(cached) = self.source_cache.borrow().get(path) {
            return cached.clone();
        }
        let contents = std::fs::read_to_string(path).ok();
        self.source_cache
            .borrow_mut()
            .insert(path.to_path_buf(), contents.clone());
        contents
    }
}

// ---------------------------------------------------------------------------
// Internal helpers.
// ---------------------------------------------------------------------------

/// Locate the first byte of the line that contains `position`, and build the
/// per-character indent for the bytes between that line start and `position`.
///
/// Tabs in the indent are preserved as `\t` so the squiggle aligns correctly
/// when the terminal renders tabs at any width; every other byte becomes a
/// single space. The indent is for the bytes in `[line_start, position)`
/// (i.e. it does not include the character `position` points at).
///
/// Returns `(line_start, indent)`. `line_start` is `0` for line 1 of the
/// file, and otherwise the index immediately following the preceding `\n`.
fn find_line_start_and_indent(bytes: &[u8], position: usize) -> (usize, String) {
    // Defensive clamp: if `position` is past EOF (an `end.index` at the
    // file end, for example), pull it back so the indent walk uses real
    // bytes.
    let mut position = position.min(bytes.len());
    let mut indent = String::new();

    while position > 0 {
        let byte = bytes[position - 1];
        if byte == b'\n' {
            // We stopped on a newline; the line starts on the next byte.
            return (position, indent);
        }
        indent.insert(0, if byte == b'\t' { '\t' } else { ' ' });
        position -= 1;
    }

    // Fell off the front of the file: this position is on line 1, which
    // starts at index 0.
    (0, indent)
}
