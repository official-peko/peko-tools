//! Tests for shared AST data structures.

use super::data_structures::*;
use std::path::PathBuf;

// ----- PositionData --------------------------------------------------------

#[test]
fn position_data_default_matches_legacy_sentinel() {
    let p = PositionData::default();
    assert_eq!(p.column, 1);
    assert_eq!(p.index, 0);
    assert_eq!(p.line, 1);
    assert_eq!(p.file, PathBuf::new());
}

#[test]
fn position_data_equals_compares_index_and_file() {
    let a = PositionData::new(5, 10, 2, PathBuf::from("a.peko"));
    let b = PositionData::new(99, 10, 99, PathBuf::from("a.peko")); // same index/file, diff col/line
    let c = PositionData::new(5, 10, 2, PathBuf::from("b.peko")); // diff file
    let d = PositionData::new(5, 11, 2, PathBuf::from("a.peko")); // diff index

    assert!(a.equals(b));
    assert!(!a.equals(c));
    assert!(!a.equals(d));
}

#[test]
fn position_data_ordering_uses_byte_index() {
    let a = PositionData::new(1, 5, 1, PathBuf::from("x"));
    let b = PositionData::new(1, 10, 1, PathBuf::from("x"));

    assert!(a.positioned_before(b.clone()));
    assert!(!b.positioned_before(a.clone()));
    assert!(a.positioned_before_inclusive(a.clone()));
    assert!(a.positioned_before_inclusive(b));
}

// ----- PositionedValue -----------------------------------------------------

#[test]
fn positioned_value_equality_ignores_position() {
    let v1 = PositionedValue::new(
        "x".to_owned(),
        PositionData::new(1, 0, 1, PathBuf::from("a")),
        PositionData::new(2, 1, 1, PathBuf::from("a")),
    );
    let v2 = PositionedValue::new(
        "x".to_owned(),
        PositionData::new(50, 500, 50, PathBuf::from("b")),
        PositionData::new(60, 600, 50, PathBuf::from("b")),
    );
    assert_eq!(v1, v2, "equality must ignore position");
}

#[test]
fn positioned_value_hash_ignores_position() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let v1 = PositionedValue::new(
        "x".to_owned(),
        PositionData::new(1, 0, 1, PathBuf::from("a")),
        PositionData::new(2, 1, 1, PathBuf::from("a")),
    );
    let v2 = PositionedValue::new(
        "x".to_owned(),
        PositionData::default(),
        PositionData::default(),
    );

    let mut h1 = DefaultHasher::new();
    let mut h2 = DefaultHasher::new();
    v1.hash(&mut h1);
    v2.hash(&mut h2);
    assert_eq!(h1.finish(), h2.finish());
}

#[test]
fn create_no_position_uses_default_positions() {
    let v = PositionedValue::create_no_position(42_u32);
    assert_eq!(v.value, 42);
    assert_eq!(v.start.file, PathBuf::new());
    assert_eq!(v.end.file, PathBuf::new());
}

#[test]
fn holds_position_within_span_same_raw_file() {
    // Use a synthetic path that won't exist on disk so canonicalize falls back
    // to raw equality (exercises both the comparison logic and the fallback).
    let file = PathBuf::from("/nonexistent/test.peko");

    let v: PositionedValue<()> = PositionedValue::new(
        (),
        PositionData::new(1, 5, 1, file.clone()),
        PositionData::new(1, 10, 1, file.clone()),
    );

    let inside = PositionData::new(1, 7, 1, file.clone());
    let at_start = PositionData::new(1, 5, 1, file.clone());
    let at_end = PositionData::new(1, 10, 1, file.clone());
    let before = PositionData::new(1, 4, 1, file.clone());
    let after = PositionData::new(1, 11, 1, file.clone());

    assert!(v.holds_position(inside));
    assert!(v.holds_position(at_start), "boundaries are inclusive");
    assert!(v.holds_position(at_end), "boundaries are inclusive");
    assert!(!v.holds_position(before));
    assert!(!v.holds_position(after));
}

#[test]
fn holds_position_rejects_different_file() {
    let v: PositionedValue<()> = PositionedValue::new(
        (),
        PositionData::new(1, 5, 1, PathBuf::from("/x/a.peko")),
        PositionData::new(1, 10, 1, PathBuf::from("/x/a.peko")),
    );
    let other_file = PositionData::new(1, 7, 1, PathBuf::from("/x/b.peko"));
    assert!(!v.holds_position(other_file));
}

// ----- VisibilityData ------------------------------------------------------

#[test]
fn open_visibility_renders_as_empty_brackets() {
    let v = VisibilityData::open_visibility();
    assert_eq!(v.to_string(), "[]");
}

#[test]
fn constant_visibility_renders_one_flag() {
    let v = VisibilityData::constant();
    assert_eq!(v.to_string(), "[constant]");
}

#[test]
fn visibility_renders_flags_in_declaration_order() {
    // Set every rendered modifier flag and lock the exact rendering. The
    // internal `scoped` placement flag has no rendered name, so it stays false.
    let v = VisibilityData::new(
        true, true, true, true, true, true, true, true, true, true, true, false, true, true,
    );
    assert_eq!(
        v.to_string(),
        "[private public constant external notrack variadic blockexit hidden state mutates gcsafe static serial]"
    );
}

#[test]
fn visibility_renders_only_active_flags() {
    let mut v = VisibilityData::open_visibility();
    v.private = true;
    v.external = true;
    v.mutates = true;
    assert_eq!(v.to_string(), "[private external mutates]");
}

#[test]
fn visibility_to_string_matches_format_macro() {
    let v = VisibilityData::constant();
    // Display is wired in correctly when both routes agree.
    assert_eq!(v.to_string(), format!("{v}"));
}

// ----- StringChunk ---------------------------------------------------------

#[test]
fn string_chunk_text_constructor_is_text() {
    let chunk = StringChunk::new_text(
        PositionData::default(),
        PositionData::default(),
        "hello".to_owned(),
    );
    assert!(chunk.is_text());
    assert_eq!(chunk.get_text(), "hello");
    assert!(matches!(chunk.content, StringChunkContent::Text(_)));
}

#[test]
fn string_chunk_interpolation_constructor_is_not_text() {
    let chunk = StringChunk::new_interpolation(
        PositionData::default(),
        PositionData::default(),
        Vec::new(),
    );
    assert!(!chunk.is_text());
    assert!(matches!(
        chunk.content,
        StringChunkContent::Interpolation(_)
    ));
}

#[test]
#[should_panic(expected = "interpolation chunk")]
fn string_chunk_get_text_on_interpolation_panics() {
    let chunk = StringChunk::new_interpolation(
        PositionData::default(),
        PositionData::default(),
        Vec::new(),
    );
    let _ = chunk.get_text();
}

#[test]
#[should_panic(expected = "text chunk")]
fn string_chunk_get_interpolation_on_text_panics() {
    let chunk = StringChunk::new_text(
        PositionData::default(),
        PositionData::default(),
        "x".to_owned(),
    );
    let _ = chunk.get_interpolation();
}

// ----- ExpressionOperatorType ----------------------------------------------

#[test]
fn expression_operator_type_supports_eq_and_debug() {
    let u = ExpressionOperatorType::Unary;
    let b = ExpressionOperatorType::Binary;
    assert_eq!(u, ExpressionOperatorType::Unary);
    assert_ne!(u, b);
    // Debug round-trip, just confirms the impl exists.
    let _ = format!("{u:?}");
}
