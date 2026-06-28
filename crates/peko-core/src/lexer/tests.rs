use super::*;

#[test]
fn test_constants() {
    // strings / chars
    let string_list = TokenList::from_source("\"'`", "");

    assert_eq!(string_list.length(), 3);

    assert_eq!(string_list.tokens[0].get_value(), "\"");
    assert_eq!(string_list.tokens[0].get_type(), &TokenType::DoubleString);

    assert_eq!(string_list.tokens[1].get_value(), "'");
    assert_eq!(string_list.tokens[1].get_type(), &TokenType::Character);

    assert_eq!(string_list.tokens[2].get_value(), "`");
    assert_eq!(
        string_list.tokens[2].get_type(),
        &TokenType::InterpolatedString
    );

    // numbers (underscores are visual separators and dropped)
    let numbers = "0.9 0.009 999 900.01 90.0 99_000";
    let number_list = TokenList::from_source(numbers, "");

    assert_eq!(number_list.length(), 6);

    for (number_token, number_value) in number_list.tokens.iter().zip(numbers.split(' ')) {
        let expected: String = number_value.split('_').collect();
        assert_eq!(number_token.get_value(), &expected);
        assert_eq!(number_token.get_type(), &TokenType::Number);
    }

    // constant-value keywords
    let constant_list = TokenList::from_source("true false null", "");

    assert_eq!(constant_list.length(), 3);

    assert_eq!(constant_list.tokens[0].get_type(), &TokenType::True);
    assert_eq!(constant_list.tokens[1].get_type(), &TokenType::False);
    assert_eq!(constant_list.tokens[2].get_type(), &TokenType::Null);
}

#[test]
fn test_visibility_keywords() {
    let keywords =
        "const state external private constant public notrack blockexit hide variadic mutates";
    let keyword_types = [
        TokenType::Const,
        TokenType::State,
        TokenType::External,
        TokenType::Private,
        TokenType::Constant,
        TokenType::Public,
        TokenType::Notrack,
        TokenType::Blockexit,
        TokenType::Hide,
        TokenType::Variadic,
        TokenType::Mutates,
    ];
    let keyword_list = TokenList::from_source(keywords, "");

    assert_eq!(keyword_list.length(), keyword_types.len());

    for ((tok, expected_value), expected_type) in keyword_list
        .tokens
        .iter()
        .zip(keywords.split(' '))
        .zip(keyword_types.iter())
    {
        assert_eq!(tok.get_value(), expected_value);
        assert_eq!(tok.get_type(), expected_type);
    }
}

#[test]
fn test_operators() {
    let operators =
        "= := => [ ] ( ) { } . ? .. + - * / % ^ == != > < >= <= :: : += -= /= *= %= ^= @";
    let operator_types = [
        TokenType::Equals,
        TokenType::Walrus,
        TokenType::Returns,
        TokenType::LBracket,
        TokenType::RBracket,
        TokenType::LParen,
        TokenType::RParen,
        TokenType::LBrace,
        TokenType::RBrace,
        TokenType::ObjectAccessor,
        TokenType::QuestionMark,
        TokenType::RangeOp,
        TokenType::Operator,
        TokenType::Operator,
        TokenType::Operator,
        TokenType::Operator,
        TokenType::Operator,
        TokenType::Operator,
        TokenType::BooleanOperator,
        TokenType::BooleanOperator,
        TokenType::BooleanOperator,
        TokenType::BooleanOperator,
        TokenType::BooleanOperator,
        TokenType::BooleanOperator,
        TokenType::ModuleAccessor,
        TokenType::Colon,
        TokenType::AssignmentWithOperator,
        TokenType::AssignmentWithOperator,
        TokenType::AssignmentWithOperator,
        TokenType::AssignmentWithOperator,
        TokenType::AssignmentWithOperator,
        TokenType::AssignmentWithOperator,
        TokenType::AtSymbol,
    ];
    let operator_list = TokenList::from_source(operators, "");

    assert_eq!(operator_list.length(), operator_types.len());

    for ((tok, expected_value), expected_type) in operator_list
        .tokens
        .iter()
        .zip(operators.split(' '))
        .zip(operator_types.iter())
    {
        assert_eq!(tok.get_value(), expected_value);
        assert_eq!(tok.get_type(), expected_type);
    }
}

#[test]
fn test_keywords() {
    let keywords = "fn Args closure return class from constructor super if else while for break in module import as link platform arch style";
    let keyword_types = [
        TokenType::FunctionDeclarator,
        TokenType::Args,
        TokenType::Closure,
        TokenType::Return,
        TokenType::Class,
        TokenType::From,
        TokenType::Constructor,
        TokenType::Super,
        TokenType::If,
        TokenType::Else,
        TokenType::While,
        TokenType::For,
        TokenType::Break,
        TokenType::In,
        TokenType::Module,
        TokenType::Import,
        TokenType::As,
        TokenType::Link,
        TokenType::Platform,
        TokenType::Arch,
        TokenType::Style,
    ];
    let keyword_list = TokenList::from_source(keywords, "");

    assert_eq!(keyword_list.length(), keyword_types.len());

    for ((tok, expected_value), expected_type) in keyword_list
        .tokens
        .iter()
        .zip(keywords.split(' '))
        .zip(keyword_types.iter())
    {
        assert_eq!(tok.get_value(), expected_value);
        assert_eq!(tok.get_type(), expected_type);
    }
}

#[test]
fn test_identifiers() {
    let test_ids = "id Id ID iD id1 i1d _id i_d id_ id__1 $id";
    let id_list = TokenList::from_source(test_ids, "");

    assert_eq!(id_list.length(), 11);

    for (tok, expected) in id_list.tokens.iter().zip(test_ids.split(' ')) {
        assert_eq!(tok.get_value(), expected);
        assert_eq!(tok.get_type(), &TokenType::Identifier);
    }
}

#[test]
fn test_builtin_types() {
    // `opaque` is the only type spelled with its own keyword. The FFI scalars
    // and the boxed value types lex as plain identifiers.
    let test_types = "i32 i64 f32 f64 string bool char number opaque";
    let type_tokens = [
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::Identifier,
        TokenType::OpaqueType,
    ];
    let type_list = TokenList::from_source(test_types, "");

    assert_eq!(type_list.length(), 9);

    for ((tok, expected_value), expected_type) in type_list
        .tokens
        .iter()
        .zip(test_types.split(' '))
        .zip(type_tokens.iter())
    {
        assert_eq!(tok.get_value(), expected_value);
        assert_eq!(tok.get_type(), expected_type);
    }
}

#[test]
fn test_comments() {
    let comment_list = TokenList::from_source("//words //// word", "");

    assert_eq!(comment_list.length(), 5);

    assert_eq!(comment_list.tokens[1].get_value(), "words");
    assert_eq!(comment_list.tokens[4].get_value(), "word");
    assert_eq!(comment_list.tokens[1].get_type(), &TokenType::Identifier);
    assert_eq!(comment_list.tokens[4].get_type(), &TokenType::Identifier);

    assert_eq!(comment_list.tokens[0].get_value(), "//");
    assert_eq!(comment_list.tokens[2].get_value(), "//");
    assert_eq!(comment_list.tokens[3].get_value(), "//");

    assert_eq!(comment_list.tokens[0].get_type(), &TokenType::Comment);
    assert_eq!(comment_list.tokens[2].get_type(), &TokenType::Comment);
    assert_eq!(comment_list.tokens[3].get_type(), &TokenType::Comment);
}

#[test]
fn empty_source_produces_empty_list() {
    let list = TokenList::from_source("", "");
    assert_eq!(list.length(), 0);
    assert!(list.finished());
}

#[test]
fn whitespace_only_source_produces_no_tokens() {
    let list = TokenList::from_source("   \t\n\r  ", "");
    assert_eq!(list.length(), 0);
}

#[test]
fn doc_comment_is_distinct_from_line_comment() {
    let list = TokenList::from_source("/// doc\n// regular", "");
    assert_eq!(list.tokens[0].get_type(), &TokenType::DocComment);
    assert_eq!(list.tokens[0].get_value(), "///");

    // Find the line-comment token (skipping any identifiers between).
    let line_comment = list
        .tokens
        .iter()
        .find(|t| t.get_type() == &TokenType::Comment)
        .unwrap();
    assert_eq!(line_comment.get_value(), "//");
}

#[test]
fn is_comment_only_matches_line_comments_not_doc_comments() {
    let list = TokenList::from_source("// regular\n/// doc", "");
    let line = list.tokens.iter().find(|t| t.get_value() == "//").unwrap();
    let doc = list.tokens.iter().find(|t| t.get_value() == "///").unwrap();
    assert!(line.is_comment());
    assert!(
        !doc.is_comment(),
        "doc comments must not be classified as comments"
    );
}

#[test]
fn range_operator_is_not_swallowed_by_number_lexing() {
    let list = TokenList::from_source("1..10", "");
    assert_eq!(list.length(), 3);
    assert_eq!(list.tokens[0].get_type(), &TokenType::Number);
    assert_eq!(list.tokens[0].get_value(), "1");
    assert_eq!(list.tokens[1].get_type(), &TokenType::RangeOp);
    assert_eq!(list.tokens[2].get_type(), &TokenType::Number);
    assert_eq!(list.tokens[2].get_value(), "10");
}

#[test]
fn double_decimal_in_number_stops_at_second_dot() {
    // `1.2.3` should lex as `1.2` then `.` then `3` -- second decimal terminates the number.
    let list = TokenList::from_source("1.2.3", "");
    assert_eq!(list.length(), 3);
    assert_eq!(list.tokens[0].get_value(), "1.2");
    assert_eq!(list.tokens[1].get_type(), &TokenType::ObjectAccessor);
    assert_eq!(list.tokens[2].get_value(), "3");
}

#[test]
fn unknown_character_is_emitted_not_skipped() {
    let list = TokenList::from_source("~", "");
    assert_eq!(list.length(), 1);
    assert_eq!(list.tokens[0].get_type(), &TokenType::Unknown);
    assert_eq!(list.tokens[0].get_value(), "~");
}

#[test]
fn single_pipe_is_unknown_double_pipe_is_or() {
    let list = TokenList::from_source("| ||", "");
    assert_eq!(list.length(), 2);
    assert_eq!(list.tokens[0].get_type(), &TokenType::Unknown);
    assert_eq!(list.tokens[0].get_value(), "|");
    assert_eq!(list.tokens[1].get_type(), &TokenType::BooleanOperator);
    assert_eq!(list.tokens[1].get_value(), "||");
}

#[test]
fn whitespace_attaches_to_surrounding_tokens() {
    let list = TokenList::from_source("a b", "");
    assert_eq!(list.length(), 2);
    // The space between `a` and `b` attaches to one of them.
    let has_space = list.tokens.iter().any(|t| {
        t.following_whitespace
            .iter()
            .any(|w| matches!(w, Whitespace::Space))
            || t.preceeding_whitespace
                .iter()
                .any(|w| matches!(w, Whitespace::Space))
    });
    assert!(
        has_space,
        "expected at least one token to record the inter-token space"
    );
}

#[test]
fn has_newline_detects_newline_in_either_run() {
    let list = TokenList::from_source("a\nb", "");
    assert!(list.tokens.iter().any(Token::has_newline));
}

#[test]
fn cursor_advances_and_retreats() {
    let mut list = TokenList::from_source("a b c", "");
    assert_eq!(list.get_index(), 0);
    list.increase_index();
    assert_eq!(list.get_index(), 1);
    list.increase_index();
    list.increase_index();
    assert!(list.finished());

    // Past-end is a no-op.
    list.increase_index();
    assert_eq!(list.get_index(), 3);

    // Retreat.
    list.decrease_index();
    assert_eq!(list.get_index(), 2);
}

#[test]
fn decrease_index_at_zero_is_a_noop() {
    let mut list = TokenList::from_source("a", "");
    assert_eq!(list.get_index(), 0);
    list.decrease_index();
    assert_eq!(list.get_index(), 0);
}

#[test]
fn get_token_forward_saturates_at_end() {
    let list = TokenList::from_source("a b", "");
    // Asking far past the end returns the last token, not a panic.
    let last = list.get_token_forward(99);
    assert_eq!(last.get_value(), "b");
}

#[test]
fn index_increase_index_respects_bounds() {
    let mut list = TokenList::from_source("a b c", "");
    let mut idx = 0;
    list.index_increase_index(&mut idx);
    assert_eq!(idx, 1);

    // Capped: cursor is at 0, list has 3, so idx can reach 3 then stop.
    list.index_increase_index(&mut idx);
    list.index_increase_index(&mut idx);
    list.index_increase_index(&mut idx);
    assert_eq!(idx, 3, "must not advance past end relative to cursor");
}

#[test]
fn token_type_get_name_returns_label() {
    assert_eq!(TokenType::FunctionDeclarator.get_name(), "'fn'");
    assert_eq!(TokenType::Identifier.get_name(), "identifier");
    assert_eq!(TokenType::Number.get_name(), "number");
    assert_eq!(TokenType::Returns.get_name(), "'=>'");
}

#[test]
fn file_path_propagates_into_position_data() {
    let list = TokenList::from_source("fn", "/tmp/example.peko");
    let start = list.tokens[0].get_start();
    assert_eq!(start.file, std::path::PathBuf::from("/tmp/example.peko"));
}
