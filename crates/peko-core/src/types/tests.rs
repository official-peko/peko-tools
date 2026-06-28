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

    assert_eq!(builtin_int.name(), "int");
    assert_eq!(builtin_int64.name(), "int64");
    assert_eq!(builtin_float.name(), "float");
    assert_eq!(builtin_bool.name(), "bool");
    assert_eq!(builtin_char.name(), "char");
    assert_eq!(builtin_double.name(), "double");
    assert_eq!(builtin_opaque.name(), "opaque");
    assert_eq!(builtin_cstr.name(), "cstr");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_simple_type_parsing() {
    let mut types_parser = create_parser_with_source("Type1 module1::module2::Type2");
    let simple_type = PekoType::from_tokens(&mut types_parser);
    let module_type = PekoType::from_tokens(&mut types_parser);

    assert_eq!(simple_type.name(), "Type1");
    assert_eq!(module_type.name(), "Type2");

    assert_eq!(module_type.module_names().len(), 2);
    assert_eq!(module_type.module_names()[0], "module1");
    assert_eq!(module_type.module_names()[1], "module2");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_generic_types_parsing() {
    let mut types_parser =
        create_parser_with_source("Generic1<int, string> module1::Generic2<int>");
    let generic_type = PekoType::from_tokens(&mut types_parser);
    let generic_type_with_modules = PekoType::from_tokens(&mut types_parser);

    assert_eq!(generic_type.name(), "Generic1");
    assert_eq!(generic_type_with_modules.name(), "Generic2");

    assert_eq!(generic_type.generics().len(), 2);
    assert_eq!(generic_type.generics()[0].to_string(), "int");
    assert_eq!(generic_type.generics()[1].to_string(), "string");

    assert_eq!(generic_type_with_modules.generics().len(), 1);
    assert_eq!(generic_type_with_modules.generics()[0].to_string(), "int");

    assert_eq!(generic_type_with_modules.module_names().len(), 1);
    assert_eq!(generic_type_with_modules.module_names()[0], "module1");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_closure_types_parsing() {
    let mut types_parser = create_parser_with_source("closure(int, string) closure() => string");
    let closure_type1 = PekoType::from_tokens(&mut types_parser);
    let closure_type2 = PekoType::from_tokens(&mut types_parser);

    assert!(closure_type1.is_closure());
    assert!(closure_type2.is_closure());

    assert_eq!(closure_type1.generics().len(), 2);
    assert_eq!(closure_type1.generics()[0].to_string(), "int");
    assert_eq!(closure_type1.generics()[1].to_string(), "string");

    assert!(closure_type2.is_function());
    assert_eq!(closure_type2.function_return().unwrap().to_string(), "string");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_function_types_parsing() {
    let mut types_parser = create_parser_with_source("(void)(int, string) (int)(opaque)");
    let function_type1 = PekoType::from_tokens(&mut types_parser);
    let function_type2 = PekoType::from_tokens(&mut types_parser);

    assert_eq!(function_type1.generics().len(), 2);
    assert_eq!(function_type1.generics()[0].to_string(), "int");
    assert_eq!(function_type1.generics()[1].to_string(), "string");

    assert_eq!(function_type2.generics().len(), 1);
    assert_eq!(function_type2.generics()[0].to_string(), "opaque");

    assert!(function_type1.is_function());
    assert_eq!(function_type1.function_return().unwrap().to_string(), "void");

    assert!(function_type2.is_function());
    assert_eq!(function_type2.function_return().unwrap().to_string(), "int");

    assert_eq!(types_parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn from_string_parses_simple_type() {
    let t = PekoType::from_string("int", "<inline>");
    assert_eq!(t.name(), "int");
    assert_eq!(t.array_depth, 0);
    assert!(t.optional_get_inner_type().is_none());
}

#[test]
fn from_string_parses_array_depth() {
    let t = PekoType::from_string("int[]", "<inline>");
    assert_eq!(t.name(), "int");
    assert_eq!(t.array_depth, 1);
}

#[test]
fn from_string_desugars_optional_to_option() {
    // A trailing `?` is sugar for standard::Option<T>.
    let t = PekoType::from_string("int?", "<inline>");
    assert_eq!(t.name(), "Option");
    assert_eq!(t.module_names(), ["standard".to_string()]);

    let inner = t.optional_get_inner_type().unwrap();
    assert_eq!(inner.name(), "int");
}

#[test]
fn from_string_desugars_optional_over_array() {
    // `int[]?` is an optional array: Option<int[]>.
    let t = PekoType::from_string("int[]?", "<inline>");
    assert_eq!(t.name(), "Option");

    let inner = t.optional_get_inner_type().unwrap();
    assert_eq!(inner.name(), "int");
    assert_eq!(inner.array_depth, 1);
}

#[test]
fn from_string_parses_reference_depth() {
    let t = PekoType::from_string("&int", "<inline>");
    assert_eq!(t.name(), "int");
    assert_eq!(t.reference_depth, 1);

    let tt = PekoType::from_string("&&int", "<inline>");
    assert_eq!(tt.reference_depth, 2);
}

#[test]
fn from_string_roundtrips_through_display() {
    // Optionals desugar to Option<T> during parsing, so the `?` surface form
    // does not round-trip and is excluded here.
    for input in [
        "int",
        "int[]",
        "&int",
        "const int",
        "const Foo<int>",
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
    assert!(e.is_error_type());
    assert_eq!(e.name(), "<<error_type>>");
}

#[test]
fn simple_type_has_no_extras() {
    let t = PekoType::simple_type("Foo");
    assert_eq!(t.name(), "Foo");
    assert!(t.module_names().is_empty());
    assert!(t.generics().is_empty());
    assert_eq!(t.array_depth, 0);
    assert_eq!(t.reference_depth, 0);
    assert!(!t.is_function());
    assert!(!t.is_closure());
    assert!(!t.is_error_type());
    assert!(!t.is_const());
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
    t.array_depth = 1;
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
    t.module_names_mut().push("m1".to_string());
    t.module_names_mut().push("m2".to_string());
    t.array_depth = 2;
    t.reference_depth = 1;

    let stripped = t.declutter();
    assert_eq!(stripped.name(), "Foo");
    assert!(stripped.module_names().is_empty());
    assert_eq!(stripped.array_depth, 0);
    assert_eq!(stripped.reference_depth, 0);
}

#[test]
fn no_depth_preserves_name_and_modules_clears_depth() {
    let mut t = PekoType::simple_type("Foo");
    t.module_names_mut().push("m1".to_string());
    t.array_depth = 3;
    t.reference_depth = 1;

    let stripped = t.no_depth();
    assert_eq!(stripped.name(), "Foo");
    assert_eq!(stripped.module_names(), ["m1".to_string()]);
    assert_eq!(stripped.array_depth, 0);
    assert_eq!(stripped.reference_depth, 0);
}

#[test]
fn const_is_carried_and_settable() {
    let mut t = PekoType::simple_type("Foo");
    assert!(!t.is_const());
    t.set_const(true);
    assert!(t.is_const());
}

#[test]
fn from_string_parses_leading_const() {
    let t = PekoType::from_string("const int", "<inline>");
    assert_eq!(t.name(), "int");
    assert!(t.is_const());

    let plain = PekoType::from_string("int", "<inline>");
    assert!(!plain.is_const());

    // `const` also applies ahead of a reference modifier.
    let reference = PekoType::from_string("const &Foo", "<inline>");
    assert_eq!(reference.name(), "Foo");
    assert_eq!(reference.reference_depth, 1);
    assert!(reference.is_const());
}

#[test]
fn optional_get_inner_type_unwraps_option_generic() {
    let mut t = PekoType::simple_type("Option");
    t.generics_mut().push(PekoType::simple_type("int"));

    let inner = t.optional_get_inner_type().unwrap();
    assert_eq!(inner.name(), "int");
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
    let t = PekoType::from_string("module::Foo<int>[]", "");
    assert_eq!(format!("{t}"), t.to_string());
}

#[test]
fn v2_ffi_scalar_names_canonicalize_to_internal_names() {
    assert_eq!(PekoType::from_string("i16", "").name(), "int16");
    assert_eq!(PekoType::from_string("i32", "").name(), "int");
    assert_eq!(PekoType::from_string("i64", "").name(), "int64");
    assert_eq!(PekoType::from_string("i128", "").name(), "int128");
    assert_eq!(PekoType::from_string("f32", "").name(), "float");
    assert_eq!(PekoType::from_string("f64", "").name(), "double");
}

#[test]
fn v2_ffi_scalars_are_builtins_and_have_the_right_class() {
    assert!(PekoType::from_string("i32", "").is_integer());
    assert!(PekoType::from_string("i64", "").is_integer());
    assert!(PekoType::from_string("f32", "").is_float());
    assert!(PekoType::from_string("f64", "").is_float());
    assert!(PekoType::from_string("i32", "").is_builtin_type());
}

#[test]
fn v2_half_float_is_recognized() {
    let half = PekoType::from_string("f16", "");
    assert_eq!(half.name(), "f16");
    assert!(half.is_float());
    assert!(half.is_builtin_type());
}

#[test]
fn v2_lowercase_pointer_canonicalizes_to_pointer() {
    let managed = PekoType::from_string("pointer<void>", "");
    assert_eq!(managed.name(), "Pointer");
    assert!(managed.is_pointer());
    assert_eq!(managed.generics().len(), 1);
    assert_eq!(managed.generics()[0].name(), "void");

    let typed = PekoType::from_string("pointer<Buffer>", "");
    assert_eq!(typed.name(), "Pointer");
    assert_eq!(typed.generics()[0].name(), "Buffer");
}
