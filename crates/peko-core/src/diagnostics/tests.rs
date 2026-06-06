use super::*;
use crate::asts::data_structures::PositionData;
use std::path::PathBuf;

fn pos(line: usize, col: usize) -> PositionData {
    PositionData::new(col, 0, line, PathBuf::from("test.peko"))
}

fn err(message: &str) -> PekoDiagnostic {
    PekoDiagnostic::new(
        pos(1, 1),
        pos(1, 5),
        message.to_owned(),
        DiagnosticType::Error,
        PathBuf::from("test.peko"),
    )
}

fn warn(message: &str) -> PekoDiagnostic {
    PekoDiagnostic::new(
        pos(2, 3),
        pos(2, 7),
        message.to_owned(),
        DiagnosticType::Warning,
        PathBuf::from("test.peko"),
    )
}

#[test]
fn new_list_is_empty_and_zero_counts() {
    let list = DiagnosticList::new();
    assert!(list.is_empty());
    assert_eq!(list.len(), 0);
    assert_eq!(list.get_error_count(), 0);
    assert_eq!(list.get_warning_count(), 0);
    assert!(!list.has_errors());
}

#[test]
fn default_matches_new() {
    let a = DiagnosticList::new();
    let b = DiagnosticList::default();
    assert_eq!(a.len(), b.len());
    assert_eq!(a.get_error_count(), b.get_error_count());
    assert_eq!(a.get_warning_count(), b.get_warning_count());
}

#[test]
fn reporting_increments_only_the_matching_counter() {
    let mut list = DiagnosticList::new();
    list.report_diagnostic(err("e1"));
    list.report_diagnostic(warn("w1"));
    list.report_diagnostic(err("e2"));

    assert_eq!(list.len(), 3);
    assert_eq!(list.get_error_count(), 2);
    assert_eq!(list.get_warning_count(), 1);
    assert!(list.has_errors());
}

#[test]
fn has_errors_is_false_for_warnings_only() {
    let mut list = DiagnosticList::new();
    list.report_diagnostic(warn("just a warning"));
    assert!(!list.has_errors());
    assert_eq!(list.get_warning_count(), 1);
}

#[test]
fn diagnostics_preserve_insertion_order() {
    let mut list = DiagnosticList::new();
    list.report_diagnostic(err("first"));
    list.report_diagnostic(warn("second"));
    list.report_diagnostic(err("third"));

    let messages: Vec<_> = list
        .get_diagnostics()
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(messages, vec!["first", "second", "third"]);
}

#[test]
fn extend_merges_diagnostics_and_counts() {
    let mut a = DiagnosticList::new();
    a.report_diagnostic(err("a1"));

    let mut b = DiagnosticList::new();
    b.report_diagnostic(err("b1"));
    b.report_diagnostic(warn("b2"));

    a.extend(b);
    assert_eq!(a.len(), 3);
    assert_eq!(a.get_error_count(), 2);
    assert_eq!(a.get_warning_count(), 1);
    let messages: Vec<_> = a.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(messages, vec!["a1", "b1", "b2"]);
}

#[test]
fn intoiterator_for_ref_yields_each_diagnostic() {
    let mut list = DiagnosticList::new();
    list.report_diagnostic(err("x"));
    list.report_diagnostic(err("y"));

    let collected: Vec<_> = (&list).into_iter().map(|d| d.message.as_str()).collect();
    assert_eq!(collected, vec!["x", "y"]);
}

#[test]
fn diagnostic_display_includes_file_position_severity_and_message() {
    let d = err("unexpected token");
    let s = format!("{d}");
    assert!(s.contains("test.peko"));
    assert!(s.contains("1:1"));
    assert!(s.contains("error"));
    assert!(s.contains("unexpected token"));
}

#[test]
fn diagnostic_type_display_matches_lowercase() {
    assert_eq!(format!("{}", DiagnosticType::Error), "error");
    assert_eq!(format!("{}", DiagnosticType::Warning), "warning");
}
