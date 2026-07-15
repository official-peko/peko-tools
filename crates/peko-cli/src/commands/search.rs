//! `peko search` - project-wide text search and replace for the IDE.
//!
//! Prints a JSON result to stdout. In search mode:
//!   {"matches":[{path,line,column,text,start,length}],"truncated":bool}
//! In replace mode (when --replace is given):
//!   {"replaced":<count>,"files":<count>}
//!
//! Flags:
//!   --query <text>       the search text or regex (required)
//!   --regex              treat query as a regular expression
//!   --case               case-sensitive (default is case-insensitive)
//!   --word               match whole words only
//!   --include <globs>    comma-separated globs; only matching files are searched
//!   --exclude <globs>    comma-separated globs; matching files are skipped
//!   --replace <text>     replace mode: substitute this for every match
//!   --root <dir>         search root (defaults to the working directory)

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regex::{Regex, RegexBuilder};
use serde::Serialize;
use walkdir::WalkDir;

use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// A single match, in the shape the IDE search panel consumes.
#[derive(Serialize)]
struct Match {
    path: String,
    line: usize,
    column: usize,
    text: String,
    start: usize,
    length: usize,
}

const SEARCH_CAP: usize = 5000;
const MAX_FILE_BYTES: u64 = 4_000_000;
const SKIP_DIRS: &[&str] = &["node_modules", "build", "dist", "target", ".git"];
const TEXT_EXT: &[&str] = &[
    "peko", "ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "jsonc", "css", "scss", "sass", "less",
    "html", "htm", "md", "markdown", "toml", "yaml", "yml", "txt", "c", "m", "h", "vue", "svg",
    "xml", "sh", "rs", "py", "go", "java", "kt", "swift",
];

pub async fn execute(cli: &CLIInfo, _reporter: &Reporter) -> ExitCode {
    let query = cli.flags.get_flag("query").unwrap_or_default();
    if query.is_empty() {
        println!("{{\"matches\":[],\"truncated\":false}}");
        return ExitCode::SUCCESS;
    }

    let is_regex = cli.flags.has_flag("regex");
    let case_sensitive = cli.flags.has_flag("case");
    let whole_word = cli.flags.has_flag("word");
    let includes = compile_globs(cli.flags.get_flag("include").as_deref());
    let excludes = compile_globs(cli.flags.get_flag("exclude").as_deref());
    let replacement = cli.flags.get_flag("replace");
    let root = cli
        .flags
        .get_flag("root")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Build the matcher. A literal query is escaped; whole-word wraps it in \b.
    let mut pattern = if is_regex {
        query.clone()
    } else {
        regex::escape(&query)
    };
    if whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    let matcher = match RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .multi_line(true)
        .build()
    {
        Ok(re) => re,
        Err(e) => {
            // Surface a regex error as an empty result plus a message the panel
            // can show.
            let msg = serde_json::to_string(&e.to_string()).unwrap_or_else(|_| "\"\"".to_string());
            println!("{{\"matches\":[],\"truncated\":false,\"error\":{msg}}}");
            return ExitCode::SUCCESS;
        }
    };

    if let Some(replacement) = replacement {
        replace_all(&root, &matcher, &replacement, is_regex, &includes, &excludes)
    } else {
        search_all(&root, &matcher, &includes, &excludes)
    }
}

fn search_all(
    root: &Path,
    matcher: &Regex,
    includes: &[Regex],
    excludes: &[Regex],
) -> ExitCode {
    let mut matches: Vec<Match> = Vec::new();
    let mut truncated = false;
    for path in walk(root, includes, excludes) {
        if matches.len() >= SEARCH_CAP {
            truncated = true;
            break;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let path_str = path.to_string_lossy().into_owned();
        for (i, line) in content.lines().enumerate() {
            if matches.len() >= SEARCH_CAP {
                truncated = true;
                break;
            }
            for m in matcher.find_iter(line) {
                let column = line[..m.start()].chars().count() + 1;
                let length = line[m.start()..m.end()].chars().count();
                let text: String = line.chars().take(400).collect();
                matches.push(Match {
                    path: path_str.clone(),
                    line: i + 1,
                    column,
                    text,
                    start: column - 1,
                    length,
                });
                if matches.len() >= SEARCH_CAP {
                    truncated = true;
                    break;
                }
            }
        }
    }
    let body = serde_json::to_string(&matches).unwrap_or_else(|_| "[]".to_string());
    println!("{{\"matches\":{body},\"truncated\":{truncated}}}");
    ExitCode::SUCCESS
}

fn replace_all(
    root: &Path,
    matcher: &Regex,
    replacement: &str,
    is_regex: bool,
    includes: &[Regex],
    excludes: &[Regex],
) -> ExitCode {
    // In literal mode a `$` in the replacement must not be read as a capture
    // reference; escape it. Regex mode keeps $1, ${name}, etc.
    let repl = if is_regex {
        replacement.to_string()
    } else {
        replacement.replace('$', "$$")
    };
    let mut replaced_total = 0usize;
    let mut files_changed = 0usize;
    for path in walk(root, includes, excludes) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let count = matcher.find_iter(&content).count();
        if count == 0 {
            continue;
        }
        let updated = matcher.replace_all(&content, repl.as_str()).into_owned();
        if updated != content && std::fs::write(&path, updated).is_ok() {
            replaced_total += count;
            files_changed += 1;
        }
    }
    println!("{{\"replaced\":{replaced_total},\"files\":{files_changed}}}");
    ExitCode::SUCCESS
}

/// Walk the root, yielding searchable text files, honoring skip dirs and the
/// include/exclude globs.
fn walk(root: &Path, includes: &[Regex], excludes: &[Regex]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
        let name = entry.file_name().to_string_lossy();
        if name.starts_with('.') && entry.depth() > 0 {
            return false;
        }
        if entry.file_type().is_dir() && SKIP_DIRS.contains(&name.as_ref()) {
            return false;
        }
        true
    });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_text(path) {
            continue;
        }
        if entry.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
        if !includes.is_empty() && !includes.iter().any(|g| g.is_match(&rel)) {
            continue;
        }
        if excludes.iter().any(|g| g.is_match(&rel)) {
            continue;
        }
        out.push(path.to_path_buf());
    }
    out
}

fn is_text(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| TEXT_EXT.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Compile comma-separated globs into anchored regexes matched against the
/// relative path. Supports `*`, `**`, and `?`.
fn compile_globs(spec: Option<&str>) -> Vec<Regex> {
    let Some(spec) = spec else {
        return Vec::new();
    };
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|glob| Regex::new(&glob_to_regex(glob)).ok())
        .collect()
}

fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("(?i)");
    let bytes: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            '*' => {
                if bytes.get(i + 1) == Some(&'*') {
                    re.push_str(".*");
                    i += 1;
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            other => re.push(other),
        }
        i += 1;
    }
    // Match anywhere in the path unless the glob is path-anchored; a bare
    // "*.ts" should match a file at any depth.
    format!(".*{re}$")
}
