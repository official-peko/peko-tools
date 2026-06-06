//! Tests for AST-specific and shared parsing routines on [`PekoParser`].
//!
//! The top-level dispatchers [`PekoParser::parse`] and
//! [`PekoParser::secondary_parse`] aren't directly tested here. Coverage of
//! them comes via the AST-specific tests and the integration tests in the
//! simulator.

use std::path::PathBuf;

use itertools::Itertools;

use super::PekoParser;
use crate::asts::statements::ImportStatementAST;
use crate::asts::{data_structures::ClassMethod, PekoAST};
use crate::lexer::TokenList;

/// Constructs a parser over a standalone source string, with an empty file
/// path. Used by every test in this module.
fn create_parser_with_source(source: &str) -> PekoParser {
    PekoParser::new(TokenList::from_source(source, ""), PathBuf::new())
}

#[test]
fn test_comments() {
    let mut parser = create_parser_with_source(
        r#"// message1
endtest
// message no newline"#,
    );
    parser.skip_comment();

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");

    parser.tokens.increase_index();
    parser.skip_comment();
}

#[test]
fn test_visibility_parsing() {
    let mut parser = create_parser_with_source("[private external state] endtest");
    let visibility = parser.parse_visibility();

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");

    // The named modifiers should have been matched.
    assert!(visibility.private);
    assert!(visibility.external);
    assert!(visibility.state);

    // No other modifiers should have been set as a side effect.
    assert!(!visibility.blockexit);
    assert!(!visibility.constant);
    assert!(!visibility.hidden);
    assert!(!visibility.mutates);
    assert!(!visibility.notrack);
    assert!(!visibility.variadic);
}

#[test]
fn test_block_parsing() {
    let mut parser = create_parser_with_source(
        r#"{
    123
    123;123
    123
}endtest"#,
    );

    let parsed_block = parser.parse_block("");

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    assert_eq!(parsed_block.value.len(), 4);
}

#[test]
fn test_argument_parsing() {
    let mut parser = create_parser_with_source(
        "(arg1, arg2, 123, 456) endtest (arg1=123, arg2=456) <string, int>(123, 456) <int>()",
    );

    let (_, argument_list1, _) = parser.parse_arguments();
    assert_eq!(argument_list1.len(), 4);

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    let (_, argument_list2, _) = parser.parse_arguments();
    assert_eq!(argument_list2.len(), 2);

    let (generic_types, argument_list3, _) = parser.parse_arguments();
    assert_eq!(generic_types.len(), 2);
    assert_eq!(argument_list3.len(), 2);

    assert_eq!(generic_types[0].to_string(), "string");
    assert_eq!(generic_types[1].to_string(), "int");

    let (generic_types, _, _) = parser.parse_arguments();
    assert_eq!(generic_types.len(), 1);
    assert_eq!(generic_types[0].to_string(), "int");

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_function_header_parsing() {
    let mut parser = create_parser_with_source(
        "(arg1: string, const arg2: int, arg3: Array<int> = default, Args<string> => varargs) \
         endtest () => Array<int>",
    );

    let (arguments, _, var_args_type, var_args_name) = parser.parse_function_header(true);

    assert_eq!(arguments.len(), 3);

    assert_eq!(arguments.get_index(0).unwrap().0.value, "arg1");
    assert_eq!(arguments.get_index(1).unwrap().0.value, "arg2");
    assert_eq!(arguments.get_index(2).unwrap().0.value, "arg3");

    assert_eq!(
        arguments.get_index(0).unwrap().1.argument_type.to_string(),
        "string"
    );
    assert_eq!(
        arguments.get_index(1).unwrap().1.argument_type.to_string(),
        "int"
    );
    assert_eq!(
        arguments.get_index(2).unwrap().1.argument_type.to_string(),
        "Array<int>"
    );

    assert!(arguments.get_index(2).unwrap().1.default_value.is_some());

    assert!(var_args_type.is_some());
    assert!(!var_args_name.value.is_empty());

    assert_eq!(var_args_type.unwrap().to_string(), "string");
    assert_eq!(var_args_name.value, "varargs");

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    let (_, return_type, _, _) = parser.parse_function_header(true);

    assert!(return_type.is_some());
    assert_eq!(return_type.unwrap().to_string(), "Array<int>");

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_boolean_parsing() {
    let mut parser = create_parser_with_source("true false");
    let boolean_true = parser.parse_boolean();
    let boolean_false = parser.parse_boolean();

    assert!(boolean_true.value.value);
    assert!(!boolean_false.value.value);
}

#[test]
fn test_number_parsing() {
    let mut parser = create_parser_with_source("1.23 123 123_000");
    let number1 = parser.parse_number();
    let number2 = parser.parse_number();
    let number3 = parser.parse_number();

    assert_eq!(number1.value.value, 1.23);
    assert_eq!(number2.value.value, 123.0);
    assert_eq!(number3.value.value, 123000.0);

    assert!(!number1.integer);
    assert!(number2.integer);
    assert!(number3.integer);
}

#[test]
fn test_encrypted_string_parsing() {
    let mut parser = create_parser_with_source("#\"hello world \n\"endtest");
    let string = parser.parse_encrypted_string();

    assert_eq!(string.encrypt.value, "hello world \n");

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
}

#[test]
fn test_string_parsing() {
    let mut parser =
        create_parser_with_source("\"hello world \\n\"endtest`interpolation ${value1;value2} `");

    let string1 = parser.parse_string();
    assert_eq!(string1.chunks.len(), 1);
    assert!(string1.chunks[0].is_text());
    assert_eq!(string1.chunks[0].get_text(), "hello world \n");

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    let string2 = parser.parse_string();
    assert_eq!(string2.chunks.len(), 3);
    assert!(string2.chunks[0].is_text());
    assert!(!string2.chunks[1].is_text());
    assert!(string2.chunks[2].is_text());

    assert_eq!(string2.chunks[0].get_text(), "interpolation ");
    assert_eq!(string2.chunks[2].get_text(), " ");

    assert_eq!(string2.chunks[1].get_interpolation().len(), 2);

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_char_parsing() {
    let mut parser = create_parser_with_source("'a'endtest'\n' ' '");
    let char1 = parser.parse_char();
    assert_eq!(char1.value.value, 'a');

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    let char2 = parser.parse_char();
    assert_eq!(char2.value.value, '\n');

    let char3 = parser.parse_char();
    assert_eq!(char3.value.value, ' ');
}

#[test]
fn test_variable_declaration_parsing() {
    let mut parser = create_parser_with_source(
        "variable := value; variable: string = value; const variable := value",
    );

    let declaration1 = parser.parse_variable_declaration();

    assert!(!declaration1.constant);
    assert_eq!(declaration1.name.value, "variable");
    assert!(declaration1.variable_type.is_none());

    assert_eq!(parser.tokens.current_token().get_value(), ";");
    parser.tokens.increase_index();

    let declaration2 = parser.parse_variable_declaration();

    assert!(!declaration2.constant);
    assert_eq!(declaration2.name.value, "variable");
    assert!(declaration2.variable_type.is_some());
    assert_eq!(declaration2.variable_type.unwrap().to_string(), "string");

    assert_eq!(parser.tokens.current_token().get_value(), ";");
    parser.tokens.increase_index();

    let declaration3 = parser.parse_variable_declaration();
    assert!(declaration3.constant);

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_function_declaration_parsing() {
    let mut parser = create_parser_with_source("fn function_name<T, T2>() {} fn function_name();");

    let declaration1 = parser.parse_function_declaration();

    assert_eq!(declaration1.function_name.value, "function_name");
    assert!(declaration1.function_body.is_some());

    assert_eq!(declaration1.generic_types.len(), 2);
    assert_eq!(declaration1.generic_types[0].value, "T");
    assert_eq!(declaration1.generic_types[1].value, "T2");

    let declaration2 = parser.parse_function_declaration();
    assert!(declaration2.function_body.is_none());

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_closure_parsing() {
    let mut parser = create_parser_with_source("closure[capture1, capture2]() {} closure() {}");

    let closure1 = parser.parse_closure_declaration();
    assert_eq!(closure1.captures.len(), 2);

    let closure2 = parser.parse_closure_declaration();
    assert_eq!(closure2.captures.len(), 0);

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_class_declaration_parsing() {
    let mut parser = create_parser_with_source(
        r#"class ClassName {
    [state] attr1: string;
    attr2: int;

    constructor(arg1: string, const arg2: int, arg3: Array<int> = default) {}

    fn method1() {}

    [operator op](arg1: int) => string {}
}

class GenericClass<T, T2> from BaseClass {
    constructor() => super(arg1, arg2) {}
}"#,
    );

    let class1 = parser.parse_class_declaration();

    // Attributes.
    assert_eq!(class1.attributes.len(), 2);

    assert!(class1.attributes.get_index(0).unwrap().1.visibility.state);

    assert_eq!(class1.attributes.get_index(0).unwrap().0.value, "attr1");
    assert_eq!(class1.attributes.get_index(1).unwrap().0.value, "attr2");

    assert_eq!(
        class1
            .attributes
            .get_index(0)
            .unwrap()
            .1
            .attribute_type
            .to_string(),
        "string"
    );
    assert_eq!(
        class1
            .attributes
            .get_index(1)
            .unwrap()
            .1
            .attribute_type
            .to_string(),
        "int"
    );

    assert_eq!(class1.methods.len(), 3);

    // Constructor.
    match &class1.methods[0] {
        ClassMethod::Constructor(method_info, _) => {
            assert_eq!(method_info.arguments.len(), 3);

            assert_eq!(method_info.arguments.get_index(0).unwrap().0.value, "arg1");
            assert_eq!(method_info.arguments.get_index(1).unwrap().0.value, "arg2");
            assert_eq!(method_info.arguments.get_index(2).unwrap().0.value, "arg3");

            assert_eq!(
                method_info
                    .arguments
                    .get_index(0)
                    .unwrap()
                    .1
                    .argument_type
                    .to_string(),
                "string"
            );
            assert_eq!(
                method_info
                    .arguments
                    .get_index(1)
                    .unwrap()
                    .1
                    .argument_type
                    .to_string(),
                "int"
            );
            assert_eq!(
                method_info
                    .arguments
                    .get_index(2)
                    .unwrap()
                    .1
                    .argument_type
                    .to_string(),
                "Array<int>"
            );

            assert!(method_info
                .arguments
                .get_index(2)
                .unwrap()
                .1
                .default_value
                .is_some());
        }
        _ => panic!("error parsing class"),
    }

    // Regular method.
    match &class1.methods[1] {
        ClassMethod::Method(_, _) => {}
        _ => panic!("error parsing class"),
    }

    // Operator overload.
    match &class1.methods[2] {
        ClassMethod::Method(method_info, _) => {
            assert_eq!(method_info.name.value, "[operator op]");
        }
        _ => panic!("error parsing class"),
    }

    let class2 = parser.parse_class_declaration();

    assert_eq!(class2.generics.len(), 2);
    assert_eq!(class2.generics[0].value, "T");
    assert_eq!(class2.generics[1].value, "T2");

    assert_eq!(class2.derives_from.len(), 1);
    assert_eq!(class2.derives_from[0].to_string(), "BaseClass");

    assert_eq!(class2.methods.len(), 1);

    match &class2.methods[0] {
        ClassMethod::Constructor(_, super_call) => {
            assert!(super_call.is_some());
        }
        _ => panic!("error in class parsing"),
    }

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_identifier_parsing() {
    let mut parser = create_parser_with_source(
        "array[idx] endtest \
         function(value1, value2) endtest \
         generic_function<int, string>(value1, value2) endtest \
         object.attr endtest \
         object.method() endtest \
         object.attr = value endtest \
         variable += value endtest \
         variable = value endtest \
         optional? endtest",
    );

    // Array access.
    let array_access = match parser.parse_identifier() {
        PekoAST::ArrayAccess(array_access) => array_access,
        _ => panic!("array access parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match array_access.array.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "array")
        }
        _ => panic!("array access parsing error"),
    }

    match array_access.access.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "idx")
        }
        _ => panic!("array access parsing error"),
    }

    // Function call.
    let function_call = match parser.parse_identifier() {
        PekoAST::FunctionCall(function_call) => function_call,
        _ => panic!("function call parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match function_call.function_reference.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "function")
        }
        _ => panic!("function call parsing error"),
    }

    // Generic function call.
    let generic_function_call = match parser.parse_identifier() {
        PekoAST::FunctionCall(generic_function_call) => generic_function_call,
        _ => panic!("generic function call parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match generic_function_call.function_reference.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "generic_function")
        }
        _ => panic!("generic function call parsing error"),
    }

    // Object attribute access.
    let object_attribute_access = match parser.parse_identifier() {
        PekoAST::ObjectAccess(object_attribute_access) => object_attribute_access,
        _ => panic!("object attribute access parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match object_attribute_access.object.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "object")
        }
        _ => panic!("object attribute access parsing error"),
    }

    match object_attribute_access.access.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "attr")
        }
        _ => panic!("object attribute access parsing error"),
    }

    // Object method call.
    let object_method_call = match parser.parse_identifier() {
        PekoAST::ObjectAccess(object_method_call) => object_method_call,
        _ => panic!("object method call parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match object_method_call.object.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "object")
        }
        _ => panic!("object method call parsing error"),
    }

    match object_method_call.access.as_ref() {
        PekoAST::FunctionCall(function_call) => match function_call.function_reference.as_ref() {
            PekoAST::VariableReference(variable_reference) => {
                assert_eq!(variable_reference.variable_name.value, "method")
            }
            _ => panic!("object method call parsing error"),
        },
        _ => panic!("object method call parsing error"),
    }

    // Object attribute reassignment.
    let object_attribute_reassignment = match parser.parse_identifier() {
        PekoAST::ObjectAccess(object_attribute_reassignment) => object_attribute_reassignment,
        _ => panic!("object attribute reassignments parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match object_attribute_reassignment.object.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "object")
        }
        _ => panic!("object attribute reassignments parsing error"),
    }

    match object_attribute_reassignment.access.as_ref() {
        PekoAST::VariableReassignment(variable_reassignment) => {
            match variable_reassignment.variable_reference.as_ref() {
                PekoAST::VariableReference(variable_reference) => {
                    assert_eq!(variable_reference.variable_name.value, "attr")
                }
                _ => panic!("object attribute reassignments parsing error"),
            }

            match variable_reassignment.variable_value.as_ref() {
                PekoAST::VariableReference(variable_reference) => {
                    assert_eq!(variable_reference.variable_name.value, "value")
                }
                _ => panic!("object attribute reassignments parsing error"),
            }
        }
        _ => panic!("object attribute reassignments parsing error"),
    }

    // Variable compound assignment.
    let variable_operator_reassignment = match parser.parse_identifier() {
        PekoAST::VariableReassignment(variable_operator_reassignment) => {
            variable_operator_reassignment
        }
        _ => panic!("variable reassignment with operator parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    assert!(variable_operator_reassignment.assignment_operator.is_some());
    assert_eq!(
        variable_operator_reassignment.assignment_operator.unwrap(),
        "+"
    );

    match variable_operator_reassignment.variable_reference.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "variable")
        }
        _ => panic!("variable reassignment with operator parsing error"),
    }

    match variable_operator_reassignment.variable_value.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "value")
        }
        _ => panic!("variable reassignment with operator parsing error"),
    }

    // Plain variable reassignment.
    let variable_reassignment = match parser.parse_identifier() {
        PekoAST::VariableReassignment(variable_reassignment) => variable_reassignment,
        _ => panic!("variable reassignment parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match variable_reassignment.variable_reference.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "variable")
        }
        _ => panic!("variable reassignment parsing error"),
    }

    match variable_reassignment.variable_value.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "value")
        }
        _ => panic!("variable reassignment parsing error"),
    }

    // Optional unwrap.
    let optional_unwrap = match parser.parse_identifier() {
        PekoAST::Unwrap(optional_unwrap) => optional_unwrap,
        _ => panic!("optional unwrap parsing error"),
    };

    assert_eq!(parser.tokens.current_token().get_value(), "endtest");
    parser.tokens.increase_index();

    match optional_unwrap.optional.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "optional")
        }
        _ => panic!("optional unwrap parsing error"),
    }

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_module_access_parsing() {
    let mut parser = create_parser_with_source("module1::module2::variable");

    let module_access = parser.parse_module_access();

    assert_eq!(module_access.module_names.len(), 2);
    assert_eq!(module_access.module_names[0].value, "module1");
    assert_eq!(module_access.module_names[1].value, "module2");

    match module_access.accessor.as_ref() {
        PekoAST::VariableReference(variable_reference) => {
            assert_eq!(variable_reference.variable_name.value, "variable")
        }
        _ => panic!("errors in parsing module access"),
    }

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_if_statement_parsing() {
    let mut parser = create_parser_with_source("if true {} else if false {} else {}");
    let if_statement = parser.parse_if_statement();

    assert_eq!(if_statement.conditional_bodies.len(), 2);
    assert!(if_statement.else_body.is_some());

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_for_loop_parsing() {
    let mut parser = create_parser_with_source("for item in list {}");
    let for_loop = parser.parse_for_loop();

    assert_eq!(for_loop.item_id.value, "item");

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}

#[test]
fn test_module_creation_parsing() {
    let mut parser = create_parser_with_source("module module1 {}");
    let module_declaration = parser.parse_module_creation();

    assert_eq!(module_declaration.module_name.value, "module1");
}

#[test]
fn test_import_parsing() {
    let mut parser = create_parser_with_source(
        "import module1 \
         import module1::submodule \
         import { * } from module1 \
         import { symbol1, symbol2 } from module1 \
         import module1 as module2 \
         import module1@\"v0.1.0\"",
    );
    let import1 = parser.parse_import();
    let import2 = parser.parse_import();
    let import3 = parser.parse_import();
    let import4 = parser.parse_import();
    let import5 = parser.parse_import();
    let import6 = parser.parse_import();
    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    // Module paths are stored as a list of identifier segments.
    let segments = |import: &ImportStatementAST| -> Vec<String> {
        import.module_path.iter().map(|s| s.value.clone()).collect()
    };
    assert_eq!(segments(&import1), vec!["module1"]);
    assert_eq!(segments(&import2), vec!["module1", "submodule"]);
    assert_eq!(segments(&import3), vec!["module1"]);
    assert_eq!(segments(&import4), vec!["module1"]);
    assert_eq!(segments(&import5), vec!["module1"]);
    assert_eq!(segments(&import6), vec!["module1"]);

    assert!(import1.import_as.is_none());
    assert!(import2.import_as.is_none());
    assert!(import3.import_as.is_none());
    assert!(import4.import_as.is_none());
    assert!(import5.import_as.is_some());
    assert_eq!(import5.import_as.unwrap().value, "module2");
    assert!(import6.import_as.is_none());

    // A version pin is present only when `@"..."` was written.
    assert!(import1.module_version.is_none());
    assert!(import2.module_version.is_none());
    assert!(import3.module_version.is_none());
    assert!(import4.module_version.is_none());
    assert!(import5.module_version.is_none());
    assert_eq!(import6.module_version.unwrap().value, "v0.1.0");

    assert_eq!(import1.symbols_to_unpack.len(), 0);
    assert_eq!(import2.symbols_to_unpack.len(), 0);
    assert_eq!(import3.symbols_to_unpack.len(), 1);
    assert_eq!(import4.symbols_to_unpack.len(), 2);
    assert_eq!(import5.symbols_to_unpack.len(), 0);
    assert_eq!(import6.symbols_to_unpack.len(), 0);
}

#[test]
fn test_link_parsing() {
    let mut parser =
        create_parser_with_source("link object1 as object link folder::library1 as library");
    let link1 = parser.parse_link();
    let link2 = parser.parse_link();

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    assert_eq!(link1.object.value, "object1");
    assert_eq!(link2.object.value, "folder/library1");

    assert_eq!(link1.link_as.value, "object");
    assert_eq!(link2.link_as.value, "library");
}

#[test]
fn test_style_parsing() {
    let mut parser = create_parser_with_source("style stylesheet1 style folder::stylesheet1");

    let sheet1 = parser.parse_style();
    let sheet2 = parser.parse_style();

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    assert_eq!(sheet1.stylesheet.value, "stylesheet1");
    assert_eq!(sheet2.stylesheet.value, "folder/stylesheet1");
}

#[test]
fn test_platform_statement_parsing() {
    let mut parser = create_parser_with_source("platform macos|windows {} arch x86_64|arm {}");
    let platform = parser.parse_platform();
    let arch = parser.parse_platform();

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    assert!(!platform.architecture_test);
    assert!(arch.architecture_test);

    assert_eq!(platform.targets.len(), 2);
    assert_eq!(platform.targets[0].value, "macos");
    assert_eq!(platform.targets[1].value, "windows");

    assert_eq!(arch.targets.len(), 2);
    assert_eq!(arch.targets[0].value, "x86_64");
    assert_eq!(arch.targets[1].value, "arm");
}

#[test]
fn test_array_parsing() {
    let mut parser = create_parser_with_source(
        "#[value1, value2, value3] #[#[value1, value2], #[value1, value2]]",
    );
    let array1 = parser.parse_array();
    let array2 = parser.parse_array();

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    assert_eq!(array1.values.len(), 3);
    assert_eq!(array2.values.len(), 2);
}

#[test]
fn test_map_parsing() {
    let mut parser = create_parser_with_source(
        "#{key: value, key: value} #{key: #{key: value}, key: #[value1, value2]}",
    );

    let map1 = parser.parse_map();
    let map2 = parser.parse_map();

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    assert_eq!(map1.key_values.len(), 2);
    assert_eq!(map2.key_values.len(), 2);
}

#[test]
fn test_xml_parsing() {
    let mut parser = create_parser_with_source(
        "<h1>test text</h1> \
         <span>${string_value}</span> \
         <div>{otherxml}</div> \
         <img /> \
         <span attr1=\"stringvalue\" attr2=value></span> \
         <button onclick={expression}></button>",
    );
    let simple_tag = parser.parse_pekox();
    let string_interpolated_tag = parser.parse_pekox();
    let xml_interpolated_tag = parser.parse_pekox();
    let single_tag = parser.parse_pekox();
    let attributes_tag = parser.parse_pekox();
    let event_tag = parser.parse_pekox();

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);

    assert_eq!(simple_tag.tag, "h1");
    assert_eq!(string_interpolated_tag.tag, "span");
    assert_eq!(xml_interpolated_tag.tag, "div");
    assert_eq!(single_tag.tag, "img");
    assert_eq!(attributes_tag.tag, "span");
    assert_eq!(event_tag.tag, "button");

    assert_eq!(simple_tag.children.len(), 1);
    assert_eq!(string_interpolated_tag.children.len(), 1);
    assert_eq!(xml_interpolated_tag.children.len(), 1);

    match &simple_tag.children[0] {
        PekoAST::PekoXTag(inner_text_tag) => {
            assert_eq!(inner_text_tag.inner_text.len(), 1);
            assert!(inner_text_tag.inner_text[0].is_text());
            assert_eq!(inner_text_tag.inner_text[0].get_text(), "test text");
        }
        _ => panic!("error in parsing inner text"),
    }

    match &string_interpolated_tag.children[0] {
        PekoAST::PekoXTag(inner_text_tag) => {
            assert_eq!(inner_text_tag.inner_text.len(), 1);
            assert!(!inner_text_tag.inner_text[0].is_text());
            assert_eq!(inner_text_tag.inner_text[0].get_interpolation().len(), 1);
        }
        _ => panic!("error in parsing inner text interpolation"),
    }

    match &xml_interpolated_tag.children[0] {
        PekoAST::VariableReference(xml_interpolated) => {
            assert_eq!(xml_interpolated.variable_name.value, "otherxml");
        }
        _ => panic!("error in parsing inner text interpolation"),
    }

    // Attributes: order isn't guaranteed (HashMap), so check by set.
    assert_eq!(attributes_tag.attributes.len(), 2);

    let mut attributes_to_find = vec!["attr1", "attr2"];
    for (attribute_name, _) in &attributes_tag.attributes {
        if attributes_to_find.contains(&attribute_name.as_str()) {
            attributes_to_find.remove(
                attributes_to_find
                    .iter()
                    .find_position(|attr| attr == &&attribute_name.as_str())
                    .unwrap()
                    .0,
            );
        }
    }

    assert_eq!(attributes_to_find.len(), 0);

    // Event.
    assert_eq!(event_tag.events.len(), 1);
    assert_eq!(
        event_tag.events.iter().collect_vec()[0].0,
        &"onclick".to_string()
    );
}

#[test]
fn test_expression_parsing() {
    let mut parser = create_parser_with_source(
        "1+2-3; 1-2*3; 3*(2-1); 4 == 2+3*2 == false; (1+2 == 3); -1+2; ('2' as int)*3",
    );

    // 1+2-3  ->  (1+2) - 3
    let simple_expression1 = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(simple_expression1.operator, "-");

    match simple_expression1.get_lhs() {
        PekoAST::BinaryExpression(plus_expression) => {
            assert_eq!(plus_expression.operator, "+");
            match plus_expression.get_lhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 1.0),
                _ => panic!("error parsing binary expression"),
            }

            match plus_expression.get_rhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 2.0),
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    match simple_expression1.get_rhs() {
        PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 3.0),
        _ => panic!("error parsing binary expression"),
    }

    // 1-2*3  ->  1 - (2*3)
    let simple_expression2 = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(simple_expression2.operator, "-");

    match simple_expression2.get_lhs() {
        PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 1.0),
        _ => panic!("error parsing binary expression"),
    }

    match simple_expression2.get_rhs() {
        PekoAST::BinaryExpression(plus_expression) => {
            assert_eq!(plus_expression.operator, "*");
            match plus_expression.get_lhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 2.0),
                _ => panic!("error parsing binary expression"),
            }

            match plus_expression.get_rhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 3.0),
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    // 3*(2-1)
    let parens_expression = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(parens_expression.operator, "*");

    match parens_expression.get_lhs() {
        PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 3.0),
        _ => panic!("error parsing binary expression"),
    }

    match parens_expression.get_rhs() {
        PekoAST::BinaryExpression(plus_expression) => {
            assert_eq!(plus_expression.operator, "-");
            match plus_expression.get_lhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 2.0),
                _ => panic!("error parsing binary expression"),
            }

            match plus_expression.get_rhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 1.0),
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    // 4 == 2+3*2 == false
    let equation_expression = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(equation_expression.operator, "==");

    match equation_expression.get_lhs() {
        PekoAST::BinaryExpression(plus_expression) => {
            assert_eq!(plus_expression.operator, "==");
            match plus_expression.get_lhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 4.0),
                _ => panic!("error parsing binary expression"),
            }

            match plus_expression.get_rhs() {
                PekoAST::BinaryExpression(_) => {}
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    match equation_expression.get_rhs() {
        PekoAST::Boolean(boolean_ast) => assert!(!boolean_ast.value.value),
        _ => panic!("error parsing binary expression"),
    }

    // (1+2 == 3)
    let parens_equation_expression = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(parens_equation_expression.operator, "==");

    match parens_equation_expression.get_rhs() {
        PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 3.0),
        _ => panic!("error parsing binary expression"),
    }

    match parens_equation_expression.get_lhs() {
        PekoAST::BinaryExpression(plus_expression) => {
            assert_eq!(plus_expression.operator, "+");
            match plus_expression.get_lhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 1.0),
                _ => panic!("error parsing binary expression"),
            }

            match plus_expression.get_rhs() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 2.0),
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    // -1+2
    let unary_expression = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(unary_expression.operator, "+");

    match unary_expression.get_lhs() {
        PekoAST::UnaryExpression(unary_expression) => {
            assert_eq!(unary_expression.operator, "-");
            match unary_expression.get_operand() {
                PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 1.0),
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    match unary_expression.get_rhs() {
        PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 2.0),
        _ => panic!("error parsing binary expression"),
    }

    // ('2' as int)*3
    let type_cast_expression = match parser.parse_expression() {
        PekoAST::BinaryExpression(binary) => binary,
        _ => panic!("error parsing expression"),
    };
    parser.tokens.increase_index();

    assert_eq!(type_cast_expression.operator, "*");

    match type_cast_expression.get_lhs() {
        PekoAST::Cast(cast_expression) => {
            assert_eq!(cast_expression.cast_to.to_string(), "int");

            match cast_expression.value.as_ref() {
                PekoAST::Char(char_ast) => assert_eq!(char_ast.value.value, '2'),
                _ => panic!("error parsing binary expression"),
            }
        }
        _ => panic!("error parsing binary expression"),
    }

    match type_cast_expression.get_rhs() {
        PekoAST::Number(number_ast) => assert_eq!(number_ast.value.value, 3.0),
        _ => panic!("error parsing binary expression"),
    }

    assert_eq!(parser.get_diagnostics().get_error_count(), 0);
}
