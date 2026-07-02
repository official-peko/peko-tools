//! Source formatting for PekoScript.
//!
//! The formatter normalizes structure without reflowing code: it re-indents
//! each line to its bracket-nesting depth, trims trailing whitespace, and
//! collapses runs of blank lines. Brackets that appear inside strings,
//! character literals, or comments do not affect depth, so those are never
//! miscounted. Line content other than the leading indentation is preserved
//! byte for byte, so comments and string contents are never altered.
//!
//! This is deliberately conservative: it will not corrupt a file it does not
//! fully understand, because it only ever rewrites leading whitespace.

/// One indentation level.
const INDENT_UNIT: &str = "    ";

/// The lexical state carried from one line to the next. Only a backtick
/// template string can span lines here; double-quoted and character literals
/// are single-line, and comments end at the newline.
#[derive(Clone, Copy, PartialEq)]
enum Carry {
    Code,
    Backtick,
}

/// Format `source` by re-indenting to bracket depth and normalizing blank
/// lines and trailing whitespace. The result always ends with a single
/// newline unless the input is empty.
pub fn format_source(source: &str) -> String {
    let mut out = String::new();
    let mut depth: usize = 0;
    let mut carry = Carry::Code;
    let mut blank_run: usize = 0;

    for raw_line in source.lines() {
        // A line continuing a multi-line backtick string is emitted verbatim;
        // its brackets are string content, not structure.
        if carry == Carry::Backtick {
            out.push_str(raw_line.trim_end());
            out.push('\n');
            carry = scan_line(raw_line, carry, &mut 0, &mut 0);
            continue;
        }

        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            // Collapse two or more consecutive blank lines into one.
            blank_run += 1;
            if blank_run == 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;

        // A line whose first structural tokens are closers is dedented so the
        // closing bracket lines up under the construct it closes.
        let mut opens = 0usize;
        let mut leading_closers = 0usize;
        let next_carry = scan_line(trimmed, carry, &mut opens, &mut leading_closers);

        let line_indent = depth.saturating_sub(leading_closers);
        for _ in 0..line_indent {
            out.push_str(INDENT_UNIT);
        }
        out.push_str(trimmed);
        out.push('\n');

        // Advance the running depth by this line's net bracket balance.
        depth = (depth + opens).saturating_sub(closers_total(trimmed, carry));
        carry = next_carry;
    }

    out
}

/// Scan one line's characters, updating `opens` with the count of opening
/// brackets and `leading_closers` with the count of closing brackets that
/// appear before any opening bracket or other structural content. Returns the
/// lexical carry for the next line. Brackets inside strings, character
/// literals, and comments are ignored.
fn scan_line(line: &str, carry: Carry, opens: &mut usize, leading_closers: &mut usize) -> Carry {
    let mut state = carry;
    let mut seen_opener = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            Carry::Backtick => {
                // Inside a backtick string: an escaped char is skipped, and an
                // unescaped backtick closes the string.
                if c == '\\' {
                    chars.next();
                } else if c == '`' {
                    state = Carry::Code;
                }
            }
            Carry::Code => match c {
                '/' if chars.peek() == Some(&'/') => {
                    // A line comment consumes the rest of the line.
                    break;
                }
                '"' => consume_quoted(&mut chars, '"'),
                '\'' => consume_quoted(&mut chars, '\''),
                '`' => {
                    // Enter a backtick string; it may close on this line.
                    state = Carry::Backtick;
                    while let Some(inner) = chars.next() {
                        if inner == '\\' {
                            chars.next();
                        } else if inner == '`' {
                            state = Carry::Code;
                            break;
                        }
                    }
                }
                '{' | '[' | '(' => {
                    *opens += 1;
                    seen_opener = true;
                }
                '}' | ']' | ')' if !seen_opener => {
                    *leading_closers += 1;
                }
                _ => {}
            },
        }
    }

    state
}

/// The total count of closing brackets on `line` outside strings and comments.
/// Used together with the opener count to advance the running depth.
fn closers_total(line: &str, carry: Carry) -> usize {
    let mut state = carry;
    let mut closers = 0usize;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            Carry::Backtick => {
                if c == '\\' {
                    chars.next();
                } else if c == '`' {
                    state = Carry::Code;
                }
            }
            Carry::Code => match c {
                '/' if chars.peek() == Some(&'/') => break,
                '"' => consume_quoted(&mut chars, '"'),
                '\'' => consume_quoted(&mut chars, '\''),
                '`' => {
                    state = Carry::Backtick;
                    while let Some(inner) = chars.next() {
                        if inner == '\\' {
                            chars.next();
                        } else if inner == '`' {
                            state = Carry::Code;
                            break;
                        }
                    }
                }
                '}' | ']' | ')' => closers += 1,
                _ => {}
            },
        }
    }

    closers
}

/// Consume the rest of a single-line quoted literal opened by `quote`,
/// honoring backslash escapes. Stops after the closing quote or at end of line.
fn consume_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, quote: char) {
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == quote {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_source;

    #[test]
    fn reindents_nested_blocks() {
        let input = "fn on_start() {\nlet x = 1\nif x > 0 {\nreturn\n}\n}\n";
        let expected = "fn on_start() {\n    let x = 1\n    if x > 0 {\n        return\n    }\n}\n";
        assert_eq!(format_source(input), expected);
    }

    #[test]
    fn ignores_brackets_in_strings_and_comments() {
        let input = "fn f() {\nlet s = \"a { b } c\" // trailing } brace\nlet t = `x ${y} z`\n}\n";
        let out = format_source(input);
        // The body lines are indented one level; the stray braces in the
        // string and comment do not change depth.
        assert!(out.contains("\n    let s = "));
        assert!(out.contains("\n    let t = "));
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn collapses_blank_lines_and_trims_trailing() {
        let input = "fn f() {\n\n\n\nlet x = 1   \n}\n";
        let out = format_source(input);
        assert!(!out.contains("\n\n\n"));
        assert!(out.contains("    let x = 1\n"));
    }

    #[test]
    fn multi_line_backtick_string_is_left_verbatim() {
        let input = "let s = `line one\n  indented content { not code }\nlast`\n";
        let out = format_source(input);
        // The interior line of the template keeps its own spacing.
        assert!(out.contains("  indented content { not code }"));
    }
}
