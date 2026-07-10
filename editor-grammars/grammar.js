// Tree-sitter grammar for Pekoscript.
// Precedence numbers mirror the parser's get_operator_precedence function.
// Higher binds tighter. Unary is the tightest binary-level rank at 8, with
// call, index, member, and unwrap sitting above it as postfix forms.

const PREC = {
  ASSIGN: 0,
  RANGE: 1,
  LOGIC: 2,
  COMPARE: 3,
  ADD: 4,
  MUL: 5,
  POW: 6,
  BITWISE: 7,
  UNARY: 8,
  CAST: 9,
  POSTFIX: 10,
};

// Fixed list of XML tag names. The list restricts the open and close tag
// names so that a leading '<' is read as an XML element only when a real tag
// name follows. A type-argument list or a less-than comparison never starts
// with one of these names in a way that also satisfies the XML structure.
const XML_TAGS = [
  "abbr",
  "acronym",
  "address",
  "a",
  "applet",
  "area",
  "article",
  "aside",
  "audio",
  "base",
  "basefont",
  "bdi",
  "bdo",
  "bgsound",
  "big",
  "blockquote",
  "body",
  "b",
  "br",
  "button",
  "caption",
  "canvas",
  "center",
  "cite",
  "code",
  "colgroup",
  "col",
  "data",
  "datalist",
  "dd",
  "dfn",
  "del",
  "details",
  "dialog",
  "dir",
  "div",
  "dl",
  "dt",
  "embed",
  "fieldset",
  "figcaption",
  "figure",
  "font",
  "footer",
  "form",
  "frame",
  "frameset",
  "head",
  "header",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "hgroup",
  "hr",
  "html",
  "iframe",
  "img",
  "input",
  "ins",
  "isindex",
  "i",
  "kbd",
  "keygen",
  "label",
  "legend",
  "li",
  "main",
  "mark",
  "marquee",
  "menuitem",
  "meta",
  "meter",
  "nav",
  "nobr",
  "noembed",
  "noscript",
  "object",
  "optgroup",
  "option",
  "output",
  "p",
  "param",
  "em",
  "pre",
  "progress",
  "q",
  "rp",
  "rt",
  "ruby",
  "samp",
  "script",
  "section",
  "small",
  "source",
  "spacer",
  "span",
  "strike",
  "strong",
  "sub",
  "sup",
  "summary",
  "svg",
  "table",
  "tbody",
  "td",
  "template",
  "textarea",
  "tfoot",
  "time",
  "title",
  "tr",
  "track",
  "tt",
  "u",
  "var",
  "video",
  "wbr",
  "xmp",
];

const VISIBILITY_KEYWORDS = [
  "public",
  "private",
  "mutates",
  "static",
  "serial",
  "external",
  "notrack",
  "gcsafe",
  "constant",
  "state",
  "variadic",
  "blockexit",
  "hide",
];

// The FFI scalar types and the std value types. pointer is intentionally
// omitted; it only appears as the generic pointer<T>, so it parses as a
// type identifier carrying type arguments.
const PRIMITIVE_TYPES = [
  "i1",
  "i8",
  "i16",
  "i32",
  "i64",
  "i128",
  "f16",
  "f32",
  "f64",
  "cstr",
  "opaque",
  "void",
  "number",
  "string",
  "char",
  "bool",
];

module.exports = grammar({
  name: "peko",

  word: ($) => $.identifier,

  // The semicolon is an optional, ignorable statement terminator: the lexer
  // treats it as skippable, so it may appear after any statement, import, or
  // field without being part of the grammar.
  extras: ($) => [/\s/, ";", $.line_comment, $.module_doc_comment, $.doc_comment],

  // Genuine ambiguities the GLR parser resolves by later tokens.
  // package vs unpack: an import beginning identifier::identifier could be
  // either form until the disambiguating { (unpack) or as/from (package).
  // _primary_expression vs call_item: foo(a = b) is a named argument rather
  // than an assignment-expression argument; call_item biases that with
  // dynamic precedence.
  conflicts: ($) => [
    [$._primary_expression, $.call_item],
    [$.package, $.unpack],
    // A comma after a generic bound may continue the bound list (`impl A,
    // impl B`) or begin the next type parameter (`T: impl A, U`). The GLR
    // parser resolves it by the `impl`/`from` keyword that a bound requires.
    [$.type_parameter],
  ],

  rules: {
    source_file: ($) => repeat($._statement),

    // ---------------------------------------------------------------------
    // Comments
    // ---------------------------------------------------------------------

    // Order matters at the lexer level. The three-slash and slash-bang forms
    // are longer matches than the two-slash form, so they win.
    line_comment: (_) => token(seq("//", /[^!\/].*|/)),
    module_doc_comment: (_) => token(seq("//!", /.*/)),
    doc_comment: (_) => token(seq("///", /.*/)),

    // ---------------------------------------------------------------------
    // Identifiers, numbers, strings, chars
    // ---------------------------------------------------------------------

    // An identifier starts with a letter, underscore, or dollar sign, and
    // continues with those plus digits. A leading digit is not allowed.
    identifier: (_) => /[A-Za-z_$][A-Za-z0-9_$]*/,

    // Digits with optional internal underscores and one optional decimal
    // part. The negative sign is the unary operator, never part of the token.
    number: (_) =>
      token(seq(/[0-9][0-9_]*/, optional(seq(".", /[0-9][0-9_]*/)))),

    escape_sequence: (_) =>
      token.immediate(/\\([nrt\\'"`0]|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f]+\})/),

    string: ($) =>
      seq('"', repeat(choice($.escape_sequence, $._string_content)), '"'),
    _string_content: (_) => token.immediate(prec(1, /[^"\\]+/)),

    protected_string: ($) =>
      seq('#"', repeat(choice($.escape_sequence, $._protected_content)), '"'),
    _protected_content: (_) => token.immediate(prec(1, /[^"\\]+/)),

    char: ($) =>
      seq("'", choice($.escape_sequence, token.immediate(/[^'\\]/)), "'"),

    // Format strings interpolate only with ${ }. A bare { is literal text.
    // The dollar sign and brace are not escapable. The two-character ${
    // token is longer than the lone $ run, so it always wins when a brace
    // follows.
    format_string: ($) =>
      seq(
        "`",
        repeat(choice($.escape_sequence, $.interpolation, $._format_content)),
        "`",
      ),
    _format_content: (_) => token.immediate(/[^`\\$]+|\$/),

    interpolation: ($) => seq("${", $._expression, "}"),

    // ---------------------------------------------------------------------
    // Types
    // ---------------------------------------------------------------------

    _type: ($) => choice($.reference_type, $._postfix_type),

    reference_type: ($) => prec.right(seq("&", $._type)),

    _postfix_type: ($) =>
      choice($.optional_type, $.array_type, $._primary_type),

    optional_type: ($) => prec.left(1, seq($._postfix_type, "?")),
    // The empty brackets are one adjacent token so a following `[` that begins
    // an attribute's visibility (`field: string` then `[private] ...`) is not
    // misread as an array-type suffix.
    array_type: ($) => prec.left(1, seq($._postfix_type, "[]")),

    _primary_type: ($) =>
      choice(
        $.primitive_type,
        $.generic_type,
        $.qualified_type,
        $.function_type,
        $.closure_type,
        $.type_identifier,
      ),

    primitive_type: (_) => choice(...PRIMITIVE_TYPES),

    type_identifier: ($) => alias($.identifier, $.type_identifier),

    generic_type: ($) =>
      prec(
        1,
        seq(choice($.type_identifier, $.qualified_type), $.type_arguments),
      ),

    type_arguments: ($) => seq("<", commaSep($._type), ">"),

    // mod::mod::...::Type
    qualified_type: ($) =>
      prec.right(
        seq(
          $.identifier,
          "::",
          choice(
            $.qualified_type,
            $.generic_type,
            $.type_identifier,
            $.primitive_type,
          ),
        ),
      ),

    // (ReturnType)(typelist)
    function_type: ($) =>
      seq("(", field("return_type", $._type), ")", "(", commaSep($._type), ")"),

    // closure(typelist) or closure(typelist) => Type
    closure_type: ($) =>
      prec.right(
        seq(
          "closure",
          "(",
          commaSep($._type),
          ")",
          optional(seq("=>", field("return_type", $._type))),
        ),
      ),

    // ---------------------------------------------------------------------
    // Expressions
    // ---------------------------------------------------------------------

    _expression: ($) =>
      choice(
        $._primary_expression,
        $.unary_expression,
        $.binary_expression,
        $.cast_expression,
        $.assignment_expression,
      ),

    _primary_expression: ($) =>
      choice(
        $.identifier,
        $.number,
        $.string,
        $.protected_string,
        $.format_string,
        $.char,
        $.array_literal,
        $.map_literal,
        $.new_expression,
        $.constant_expression,
        $.danger_cast_expression,
        $.switch_expression,
        $.call_expression,
        $.index_expression,
        $.member_expression,
        $.module_expression,
        $.unwrap_expression,
        $.parenthesized_expression,
        $.closure,
        $.xml_element,
      ),

    // new Type(args), new mod::Type<T>(args)
    new_expression: ($) =>
      prec.right(
        seq(
          "new",
          field("type", choice($.generic_type, $.qualified_type, $.type_identifier)),
          $.argument_list,
        ),
      ),

    // constant<Type>(value): a compile-time FFI constant.
    constant_expression: ($) =>
      seq("constant", $.type_arguments, $.argument_list),

    // danger_cast<Type>(value): a forced, unchecked cast.
    danger_cast_expression: ($) =>
      seq("danger_cast", $.type_arguments, $.argument_list),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    array_literal: ($) => seq("#[", commaSep($._expression), "]"),

    map_literal: ($) =>
      seq(
        "#{",
        commaSep(
          seq(field("key", $._expression), ":", field("value", $._expression)),
        ),
        "}",
      ),

    // The callee is any expression, not just an identifier. Explicit type
    // arguments are not accepted inline here: an unadorned `<` after an
    // expression is always the comparison operator, so `a < b` is never
    // misread as a generic call. Generic construction uses new/constant/
    // danger_cast, which lead with a keyword, and method generics are inferred.
    call_expression: ($) =>
      prec(
        PREC.POSTFIX,
        seq(
          field("function", $._primary_expression),
          field("arguments", $.argument_list),
        ),
      ),

    argument_list: ($) => seq("(", commaSep($.call_item), ")"),

    call_item: ($) =>
      choice(
        prec.dynamic(
          1,
          seq(field("name", $.identifier), "=", field("value", $._expression)),
        ),
        field("value", $._expression),
      ),

    index_expression: ($) =>
      prec(
        PREC.POSTFIX,
        seq(
          field("array", $._primary_expression),
          // The subscript bracket must abut the array expression. A `[` that
          // starts a new line begins a visibility attribute on the next
          // declaration, not an index into the previous value.
          token.immediate("["),
          field("index", $._expression),
          "]",
        ),
      ),

    member_expression: ($) =>
      prec(
        PREC.POSTFIX,
        seq(
          field("object", $._primary_expression),
          ".",
          field("property", $.identifier),
        ),
      ),

    module_expression: ($) =>
      prec.right(
        PREC.POSTFIX,
        seq(
          field("module", $.identifier),
          "::",
          field("member", $._primary_expression),
        ),
      ),

    unwrap_expression: ($) =>
      prec(
        PREC.POSTFIX,
        seq($._primary_expression, "?", optional(seq("else", $.block))),
      ),

    // switch subject { Pattern => { body }, _ => { body } }
    switch_expression: ($) =>
      prec.right(
        seq(
          "switch",
          field("subject", $._expression),
          "{",
          repeat($.switch_arm),
          "}",
        ),
      ),

    // Arms are consecutive, not comma-separated; the block ends each arm. An
    // optional trailing comma is tolerated.
    switch_arm: ($) =>
      seq(
        field("pattern", choice($._expression, "_")),
        "=>",
        $.block,
        optional(","),
      ),

    unary_expression: ($) =>
      prec(
        PREC.UNARY,
        seq(
          field("operator", choice("&", "*", "-", "!")),
          field("operand", $._expression),
        ),
      ),

    binary_expression: ($) => {
      const table = [
        ["..", PREC.RANGE],
        ["&&", PREC.LOGIC],
        ["||", PREC.LOGIC],
        ["==", PREC.COMPARE],
        ["!=", PREC.COMPARE],
        ["<", PREC.COMPARE],
        [">", PREC.COMPARE],
        ["<=", PREC.COMPARE],
        [">=", PREC.COMPARE],
        ["+", PREC.ADD],
        ["-", PREC.ADD],
        ["*", PREC.MUL],
        ["/", PREC.MUL],
        ["%", PREC.MUL],
        ["^", PREC.POW],
        ["&", PREC.BITWISE],
        ["!", PREC.BITWISE],
      ];
      return choice(
        ...table.map(([op, p]) =>
          prec.left(
            p,
            seq(
              field("left", $._expression),
              field("operator", op),
              field("right", $._expression),
            ),
          ),
        ),
      );
    },

    cast_expression: ($) =>
      prec.left(
        PREC.CAST,
        seq(field("value", $._expression), "as", field("type", $._type)),
      ),

    // Reassignment sits below the precedence-climbed operators and is right
    // associative so chained assignment groups to the right.
    assignment_expression: ($) =>
      prec.right(
        PREC.ASSIGN,
        seq(field("left", $._expression), "=", field("right", $._expression)),
      ),

    closure: ($) =>
      prec.right(
        seq(
          "closure",
          optional($.capture_list),
          $.parameter_list,
          optional(seq("=>", field("return_type", $._type))),
          $.block,
        ),
      ),

    capture_list: ($) => seq("[", commaSep($.identifier), "]"),

    // ---------------------------------------------------------------------
    // XML expressions
    // ---------------------------------------------------------------------

    xml_element: ($) =>
      choice(
        $._xml_self_closing,
        seq($._xml_opening, repeat($._xml_child), $._xml_closing),
      ),

    _xml_opening: ($) =>
      seq("<", field("name", $.xml_name), repeat($.xml_attribute), ">"),

    _xml_self_closing: ($) =>
      seq("<", field("name", $.xml_name), repeat($.xml_attribute), "/>"),

    _xml_closing: ($) => seq("</", field("name", $.xml_name), ">"),

    xml_name: (_) => choice(...XML_TAGS),

    xml_attribute: ($) =>
      seq(
        field("name", $.identifier),
        "=",
        field("value", choice($.xml_event, $._expression)),
      ),

    // An event handler attribute value is a brace-delimited body, the same
    // shape as a function body.
    xml_event: ($) => $.block,

    _xml_child: ($) =>
      choice(
        $.xml_element,
        $.xml_value_interpolation,
        $.xml_interpolation,
        $.xml_text,
      ),

    // ${ expr } carries a semantic meaning but highlights the same as the
    // brace form. The two-character ${ token wins over the lone $ in text.
    xml_value_interpolation: ($) => seq("${", $._expression, "}"),
    xml_interpolation: ($) => seq("{", $._expression, "}"),

    // Text excludes the angle brackets, braces, and dollar so that tags,
    // interpolations, and value interpolations all take priority. A lone
    // dollar is allowed as a separate run.
    xml_text: (_) => token(prec(-1, /([^<>{}$]+|\$)/)),

    // ---------------------------------------------------------------------
    // Visibility
    // ---------------------------------------------------------------------

    // One or more attribute brackets. Both `[public mutates]` (grouped) and
    // `[public] [mutates]` (separate brackets) are accepted.
    visibility: ($) => repeat1(seq("[", repeat1($.visibility_keyword), "]")),
    visibility_keyword: (_) => choice(...VISIBILITY_KEYWORDS),

    // ---------------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------------

    // let/const bindings, with an optional type annotation and initializer, or
    // a tuple destructuring pattern: `let (a, b) = pair`.
    variable_declaration: ($) =>
      seq(
        optional($.visibility),
        choice("let", "const"),
        field("name", choice($.identifier, $.destructure_pattern)),
        choice(
          seq(
            ":",
            field("type", $._type),
            optional(seq("=", field("value", $._expression))),
          ),
          seq("=", field("value", $._expression)),
          seq(":=", field("value", $._expression)),
        ),
      ),

    destructure_pattern: ($) => seq("(", commaSep1($.identifier), ")"),

    function_declaration: ($) =>
      seq(
        optional($.visibility),
        "fn",
        field("name", $.identifier),
        optional($.type_parameters),
        $.parameter_list,
        optional(seq("=>", field("return_type", $._type))),
        optional($.block),
      ),

    type_parameters: ($) => seq("<", commaSep1($.type_parameter), ">"),

    type_parameter: ($) =>
      seq(
        field("name", $.identifier),
        optional(seq(":", commaSep1($.type_bound))),
      ),

    // A generic bound: `impl Trait` or `from Class`.
    type_bound: ($) => seq(choice("impl", "from"), $._type),

    parameter_list: ($) =>
      seq(
        "(",
        optional(
          choice(
            seq(
              commaSep1($.parameter),
              optional(seq(",", $.variadic_parameter)),
            ),
            $.variadic_parameter,
          ),
        ),
        ")",
      ),

    parameter: ($) =>
      seq(
        field("name", $.identifier),
        ":",
        field("type", $._type),
        optional(seq("=", field("default", $._expression))),
      ),

    variadic_parameter: ($) =>
      seq(
        "Args",
        "<",
        field("type", $._type),
        ">",
        "=>",
        field("name", $.identifier),
      ),

    class_declaration: ($) =>
      seq(
        optional($.visibility),
        "class",
        field("name", $.identifier),
        optional($.type_parameters),
        optional(seq("from", commaSep1(field("parent", $._type)))),
        optional(seq("impl", commaSep1(field("trait", $._type)))),
        $.class_body,
      ),

    trait_declaration: ($) =>
      seq(
        optional($.visibility),
        "trait",
        field("name", $.identifier),
        optional($.type_parameters),
        $.trait_body,
      ),

    trait_body: ($) => seq("{", repeat($.trait_method), "}"),

    // A trait method is a signature (ending in ;) or a default body.
    trait_method: ($) =>
      seq(
        optional($.visibility),
        "fn",
        field("name", $.identifier),
        optional($.type_parameters),
        $.parameter_list,
        optional(seq("=>", field("return_type", $._type))),
        optional($.block),
      ),

    enum_declaration: ($) =>
      seq(
        optional($.visibility),
        "enum",
        field("name", $.identifier),
        "{",
        optional(seq(commaSep1($.enum_variant), optional(","))),
        "}",
      ),

    enum_variant: ($) => field("name", $.identifier),

    class_body: ($) => seq("{", repeat($._class_member), "}"),

    _class_member: ($) =>
      choice($.constructor, $.operator_overload, $.method, $.attribute),

    constructor: ($) =>
      seq(
        "constructor",
        $.parameter_list,
        optional(seq("=>", $.super_call)),
        $.block,
      ),

    super_call: ($) => seq("super", $.argument_list),

    operator_overload: ($) =>
      seq(
        "[",
        "operator",
        field("operator", $.operator_name),
        "]",
        $.parameter_list,
        optional(seq("=>", field("return_type", $._type))),
        $.block,
      ),

    operator_name: ($) =>
      choice(
        $.identifier,
        "+",
        "-",
        "*",
        "/",
        "%",
        "^",
        "..",
        "||",
        "&&",
        "==",
        "!=",
        "<",
        ">",
        "<=",
        ">=",
        "&",
        "!",
        "=",
      ),

    method: ($) =>
      seq(
        optional($.visibility),
        "fn",
        field("name", $.identifier),
        optional($.type_parameters),
        $.parameter_list,
        optional(seq("=>", field("return_type", $._type))),
        optional($.block),
      ),

    attribute: ($) =>
      seq(
        optional($.visibility),
        field("name", $.identifier),
        ":",
        field("type", $._type),
      ),

    module_declaration: ($) =>
      seq(
        "module",
        field("name", $.identifier),
        "{",
        repeat($._statement),
        "}",
      ),

    // ---------------------------------------------------------------------
    // Control flow
    // ---------------------------------------------------------------------

    platform_statement: ($) =>
      seq("platform", field("name", $.identifier), $.block),
    arch_statement: ($) => seq("arch", field("name", $.identifier), $.block),

    break_statement: (_) => "break",
    continue_statement: (_) => "continue",

    return_statement: ($) => prec.right(seq("return", optional($._expression))),

    if_statement: ($) =>
      prec.right(
        seq(
          "if",
          field("condition", $._expression),
          $.block,
          optional($.else_clause),
        ),
      ),

    else_clause: ($) => seq("else", choice($.if_statement, $.block)),

    for_statement: ($) =>
      seq(
        "for",
        field("item", choice($.identifier, $.destructure_pattern)),
        "in",
        field("iterable", $._expression),
        $.block,
      ),

    while_statement: ($) =>
      seq("while", field("condition", $._expression), $.block),

    // ---------------------------------------------------------------------
    // Imports, links, styles
    // ---------------------------------------------------------------------

    link_statement: ($) =>
      seq(
        "link",
        $.module_path,
        "as",
        field("kind", choice("object", "lib", "archive")),
      ),

    module_path: ($) => seq($.identifier, repeat(seq("::", $.identifier))),

    import_statement: ($) =>
      seq(
        "import",
        choice(
          seq($.package, optional(seq("as", field("alias", $.identifier)))),
          seq($.unpack, "from", $.package),
        ),
      ),

    package: ($) =>
      seq(
        $.identifier,
        optional(seq("@", field("version", $.string))),
        repeat(seq("::", $.identifier)),
      ),

    unpack: ($) =>
      seq(optional($._unpack_prefix), "{", commaSep1($._unpack_item), "}"),

    _unpack_prefix: ($) => repeat1(seq($.identifier, "::")),

    _unpack_item: ($) => choice($.identifier, "*", $.unpack),

    style_statement: ($) => seq("style", $.package),

    // ---------------------------------------------------------------------
    // Blocks and statements
    // ---------------------------------------------------------------------

    block: ($) => seq("{", repeat($._statement), "}"),

    _statement: ($) =>
      choice(
        $.variable_declaration,
        $.function_declaration,
        $.class_declaration,
        $.trait_declaration,
        $.enum_declaration,
        $.module_declaration,
        $.platform_statement,
        $.arch_statement,
        $.break_statement,
        $.continue_statement,
        $.return_statement,
        $.if_statement,
        $.for_statement,
        $.while_statement,
        $.link_statement,
        $.import_statement,
        $.style_statement,
        $.expression_statement,
      ),

    expression_statement: ($) => $._expression,
  },
});

function commaSep(rule) {
  return optional(commaSep1(rule));
}

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}
