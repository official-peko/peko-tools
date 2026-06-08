use crate::lexer::TokenList;
use crate::parser::PekoParser;
use std::path::PathBuf;

use super::PekoType;

/// Constructs a parser over a standalone source string, with an empty file
/// path. Used by every test in this module.
fn create_parser_with_source(source: &str) -> PekoParser {
    PekoParser::new(TokenList::from_source(source, ""), PathBuf::new())
}

#[test]
fn test_builtin_types_parsing() {
    let mut types_parser =
        create_parser_with_source("int int64 float bool char double opaque cstr");
    let builtin_int = PekoType::from_tokens(&mut types_parser);
    let builtin_int64 = PekoType::from_tokens(&mut types_parser);
    let builtin_float = PekoType::from_tokens(&mut types_parser);
    let builtin_bool = PekoType::from_tokens(&mut types_parser);
    let builtin_char = PekoType::from_tokens(&mut types_parser);
    let builtin_double = PekoType::from_tokens(&mut types_parser);
    let builtin_opaque = PekoType::from_tokens(&mut types_parser);
    let builtin_cstr = PekoType::from_tokens(&mut types_parser);

    assert_eq!(builtin_int.type_name, "int");
    assert_eq!(builtin_int64.type_name, "int64");
    assert_eq!(builtin_float.type_name, "float");
    assert_eq!(builtin_bool.type_name, "bool");
    assert_eq!(builtin_char.type_name, "char");
    assert_eq!(builtin_double.type_name, "double");
    assert_eq!(builtin_opaque.type_name, "opaque");
    assert_eq!(builtin_cstr.type_name, "cstr");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_simple_type_parsing() {
    let mut types_parser = create_parser_with_source("Type1 module1::module2::Type2");
    let simple_type = PekoType::from_tokens(&mut types_parser);
    let module_type = PekoType::from_tokens(&mut types_parser);

    assert_eq!(simple_type.type_name, "Type1");
    assert_eq!(module_type.type_name, "Type2");

    assert_eq!(module_type.module_names.len(), 2);
    assert_eq!(module_type.module_names[0], "module1");
    assert_eq!(module_type.module_names[1], "module2");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_generic_types_parsing() {
    let mut types_parser =
        create_parser_with_source("Generic1<int, string> module1::Generic2<int>");
    let generic_type = PekoType::from_tokens(&mut types_parser);
    let generic_type_with_modules = PekoType::from_tokens(&mut types_parser);

    assert_eq!(generic_type.type_name, "Generic1");
    assert_eq!(generic_type_with_modules.type_name, "Generic2");

    assert_eq!(generic_type.generic_types.len(), 2);
    assert_eq!(generic_type.generic_types[0].to_string(), "int");
    assert_eq!(generic_type.generic_types[1].to_string(), "string");

    assert_eq!(generic_type_with_modules.generic_types.len(), 1);
    assert_eq!(
        generic_type_with_modules.generic_types[0].to_string(),
        "int"
    );

    assert_eq!(generic_type_with_modules.module_names.len(), 1);
    assert_eq!(generic_type_with_modules.module_names[0], "module1");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_closure_types_parsing() {
    let mut types_parser = create_parser_with_source("closure(int, string) closure() => string");
    let closure_type1 = PekoType::from_tokens(&mut types_parser);
    let closure_type2 = PekoType::from_tokens(&mut types_parser);

    assert!(closure_type1.is_closure);
    assert!(closure_type2.is_closure);

    assert_eq!(closure_type1.generic_types.len(), 2);
    assert_eq!(closure_type1.generic_types[0].to_string(), "int");
    assert_eq!(closure_type1.generic_types[1].to_string(), "string");

    assert!(closure_type2.function_type.is_some());
    assert_eq!(closure_type2.function_type.unwrap().to_string(), "string");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_function_types_parsing() {
    let mut types_parser = create_parser_with_source("(void)(int, string) (int)(opaque)");
    let function_type1 = PekoType::from_tokens(&mut types_parser);
    let function_type2 = PekoType::from_tokens(&mut types_parser);

    assert_eq!(function_type1.generic_types.len(), 2);
    assert_eq!(function_type1.generic_types[0].to_string(), "int");
    assert_eq!(function_type1.generic_types[1].to_string(), "string");

    assert_eq!(function_type2.generic_types.len(), 1);
    assert_eq!(function_type2.generic_types[0].to_string(), "opaque");

    assert!(function_type1.function_type.is_some());
    assert_eq!(function_type1.function_type.unwrap().to_string(), "void");

    assert!(function_type2.function_type.is_some());
    assert_eq!(function_type2.function_type.unwrap().to_string(), "int");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn from_string_parses_simple_type() {
    let t = PekoType::from_string("int", "<inline>");
    assert_eq!(t.type_name, "int");
    assert_eq!(t.pointer_depth, 0);
    assert_eq!(t.optional_depth, 0);
}

#[test]
fn from_string_parses_depth_modifiers() {
    let t = PekoType::from_string("int[]?", "<inline>");
    assert_eq!(t.type_name, "int");
    assert_eq!(t.pointer_depth, 1);
    assert_eq!(t.optional_depth, 1);
}

#[test]
fn from_string_parses_reference_depth() {
    let t = PekoType::from_string("&int", "<inline>");
    assert_eq!(t.type_name, "int");
    assert_eq!(t.reference_depth, 1);

    let tt = PekoType::from_string("&&int", "<inline>");
    assert_eq!(tt.reference_depth, 2);
}

#[test]
fn from_string_roundtrips_through_display() {
    for input in [
        "int",
        "int[]",
        "int?",
        "&int",
        "module::Foo<int, string>",
        "closure(int, string) => bool",
        "(int)(string, bool)",
    ] {
        let parsed = PekoType::from_string(input, "<inline>");
        let rendered = parsed.to_string();
        assert_eq!(rendered, input, "round-trip failed for {input:?}");
    }
}

#[test]
fn error_type_is_marked_as_such() {
    let e = PekoType::error_type();
    assert!(e.is_error_type);
    assert_eq!(e.type_name, "<<error_type>>");
}

#[test]
fn simple_type_has_no_extras() {
    let t = PekoType::simple_type("Foo");
    assert_eq!(t.type_name, "Foo");
    assert!(t.module_names.is_empty());
    assert!(t.generic_types.is_empty());
    assert_eq!(t.pointer_depth, 0);
    assert_eq!(t.optional_depth, 0);
    assert_eq!(t.reference_depth, 0);
    assert!(t.function_type.is_none());
    assert!(!t.is_closure);
    assert!(!t.is_error_type);
}

#[test]
fn is_datatype_excludes_char() {
    assert!(PekoType::simple_type("int").is_datatype());
    assert!(PekoType::simple_type("float").is_datatype());
    assert!(PekoType::simple_type("bool").is_datatype());
    assert!(!PekoType::simple_type("char").is_datatype());
    assert!(!PekoType::simple_type("string").is_datatype());
}

#[test]
fn is_integer_includes_char_and_bool() {
    assert!(PekoType::simple_type("int").is_integer());
    assert!(PekoType::simple_type("char").is_integer());
    assert!(PekoType::simple_type("bool").is_integer());
    assert!(!PekoType::simple_type("float").is_integer());
}

#[test]
fn depth_excludes_predicates_from_matching() {
    let mut t = PekoType::simple_type("int");
    t.pointer_depth = 1;
    assert!(!t.is_datatype());
    assert!(!t.is_integer());
    assert!(!t.is_float());
    assert!(!t.is_builtin_type());
}

#[test]
fn is_pointer_includes_string_and_opaque() {
    assert!(PekoType::simple_type("string").is_pointer());
    assert!(PekoType::simple_type("opaque").is_pointer());
    assert!(!PekoType::simple_type("int").is_pointer());
}

#[test]
fn declutter_strips_everything_extra() {
    let mut t = PekoType::simple_type("Foo");
    t.module_names.push("m1".to_string());
    t.module_names.push("m2".to_string());
    t.pointer_depth = 2;
    t.optional_depth = 1;
    t.reference_depth = 1;
    t.is_closure = true;

    let stripped = t.declutter();
    assert_eq!(stripped.type_name, "Foo");
    assert!(stripped.module_names.is_empty());
    assert_eq!(stripped.pointer_depth, 0);
    assert_eq!(stripped.optional_depth, 0);
    assert_eq!(stripped.reference_depth, 0);
    assert!(!stripped.is_closure);
}

#[test]
fn no_depth_preserves_name_and_modules_clears_depth() {
    let mut t = PekoType::simple_type("Foo");
    t.module_names.push("m1".to_string());
    t.pointer_depth = 3;
    t.optional_depth = 2;
    t.reference_depth = 1;

    let stripped = t.no_depth();
    assert_eq!(stripped.type_name, "Foo");
    assert_eq!(stripped.module_names, vec!["m1".to_string()]);
    assert_eq!(stripped.pointer_depth, 0);
    assert_eq!(stripped.optional_depth, 0);
    assert_eq!(stripped.reference_depth, 0);
}

#[test]
fn optional_get_inner_type_unwraps_option_generic() {
    let mut t = PekoType::simple_type("Option");
    t.generic_types.push(PekoType::simple_type("int"));

    let inner = t.optional_get_inner_type().unwrap();
    assert_eq!(inner.type_name, "int");
}

#[test]
fn optional_get_inner_type_decrements_optional_depth() {
    let mut t = PekoType::simple_type("int");
    t.optional_depth = 2;

    let inner = t.optional_get_inner_type().unwrap();
    assert_eq!(inner.type_name, "int");
    assert_eq!(inner.optional_depth, 1);
}

#[test]
fn optional_get_inner_type_returns_none_for_non_optional() {
    let t = PekoType::simple_type("int");
    assert!(t.optional_get_inner_type().is_none());
}

#[test]
fn is_string_type_matches_all_string_forms() {
    assert!(PekoType::from_string("char[]", "").is_string_type());
    assert!(PekoType::from_string("&char", "").is_string_type());
    assert!(!PekoType::from_string("int", "").is_string_type());
}

#[test]
fn display_matches_to_string() {
    // Sanity-check that the ToString blanket via Display produces the same
    // string used by the rest of the codebase.
    let t = PekoType::from_string("module::Foo<int>[]?", "");
    assert_eq!(format!("{t}"), t.to_string());
}
