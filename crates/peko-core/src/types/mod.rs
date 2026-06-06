//! # Peko Core Types
//!
//! Type representation for Pekoscript.
//!
//! [`PekoType`] is the canonical structural representation of a Pekoscript
//! type expression. It captures the full surface form a user can write
//! module-qualified type names, generic parameters, pointer/optional/reference
//! depth indicators, and function and closure type forms, along with the
//! source span the type was parsed from.
//!
//! The module also exposes two ways to construct a `PekoType` from source:
//!
//! * [`PekoType::from_tokens`]: consume tokens from an existing
//!   [`PekoParser`](crate::parser::PekoParser) cursor. Used inside the parser
//!   when a type appears inline in a declaration.
//! * [`PekoType::from_string`]: lex a standalone type expression. Used by
//!   the simulator and tests when synthesizing types from string literals.

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;

use crate::asts::data_structures::PositionData;
use crate::lexer::{self, TokenType};
use crate::parser;

/// Structural representation of a Pekoscript type.
///
/// A `PekoType` carries every detail of a type as it appeared in source: its
/// module path, base name, generic parameters, depth modifiers (pointers,
/// optionals, references), an optional function-return type and closure
/// flag, and the source positions it was parsed from. Two `PekoType` values
/// describing the same underlying type can differ in their source positions;
/// type *equality* in the language-semantic sense lives on the simulator
/// (see `PekoSimulatorContext::types_equal`), not via `PartialEq` on this
/// struct.
#[derive(Clone, Debug)]
pub struct PekoType {
    /// Module path components of a qualified type (e.g. `["module1", "module2"]`
    /// for `module1::module2::Type`).
    pub module_names: Vec<String>,

    /// The bare type name (e.g. `"Type"`). For function-type forms this is
    /// empty and the role is taken over by `function_type` + `generic_types`.
    pub type_name: String,

    /// Generic type arguments. For function and closure type forms, this
    /// holds the *argument* types rather than user-supplied generics.
    pub generic_types: Vec<PekoType>,

    /// Pointer/array depth, counted by trailing `[]` suffixes.
    pub pointer_depth: usize,

    /// Optional depth, counted by trailing `?` suffixes.
    pub optional_depth: usize,

    /// Reference depth, counted by leading `&` prefixes (a `&&` counts as 2).
    pub reference_depth: usize,

    /// For function and closure type forms, this carries the return type.
    pub function_type: Option<Box<PekoType>>,

    /// `true` if this type was written as a `closure(...)` form rather than
    /// the bare `(ret)(args)` function-type form.
    pub is_closure: bool,

    /// Sentinel marker set by [`PekoType::error_type`] to indicate that a
    /// previous analysis step failed. Used by the simulator and codegen to
    /// avoid cascading errors.
    pub is_error_type: bool,

    /// Inclusive start position of the type expression in source.
    pub start_position: PositionData,

    /// Inclusive end position of the type expression in source.
    pub end_position: PositionData,

    /// Set by the simulator after generic substitution has finished;
    /// indicates the type contains no unresolved generic parameters.
    pub fully_expanded: bool,
}

impl PekoType {
    /// Constructs a fully-specified `PekoType`.
    ///
    /// `is_error_type` and `fully_expanded` always start as `false`; use
    /// [`PekoType::error_type`] for the error sentinel, and the simulator's
    /// substitution machinery to mark a type fully expanded.
    ///
    /// This constructor is intentionally low-level because every existing
    /// call site already builds the argument list inline during parsing.
    /// Higher-level helpers like [`PekoType::simple_type`] cover common cases.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        module_names: Vec<String>,
        type_name: String,
        generic_types: Vec<PekoType>,
        pointer_depth: usize,
        optional_depth: usize,
        reference_depth: usize,
        function_type: Option<PekoType>,
        is_closure: bool,
        start_position: PositionData,
        end_position: PositionData,
    ) -> PekoType {
        PekoType {
            module_names,
            type_name,
            generic_types,
            pointer_depth,
            optional_depth,
            reference_depth,
            function_type: function_type.map(Box::new),
            is_closure,
            is_error_type: false,
            start_position,
            end_position,
            fully_expanded: false,
        }
    }

    /// Constructs the default "error" type used when a prior analysis step
    /// failed.
    ///
    /// Downstream code can check [`PekoType::is_error_type`] to suppress
    /// cascading diagnostics on already-failed expressions.
    #[must_use]
    pub fn error_type() -> PekoType {
        PekoType {
            module_names: Vec::new(),
            type_name: String::from("<<error_type>>"),
            generic_types: Vec::new(),
            pointer_depth: 0,
            optional_depth: 0,
            reference_depth: 0,
            function_type: None,
            is_closure: false,
            is_error_type: true,
            start_position: PositionData::default(),
            end_position: PositionData::default(),
            fully_expanded: false,
        }
    }

    /// Parses a `PekoType` from a standalone string.
    ///
    /// The string is lexed and then handed to [`PekoType::from_tokens`].
    /// Diagnostics produced during parsing are silently dropped; callers
    /// that need to surface them should use [`PekoType::from_tokens`]
    /// against an existing parser and inspect its diagnostic list.
    ///
    /// # Examples
    ///
    /// ```
    /// use peko_core::types::PekoType;
    ///
    /// let t = PekoType::from_string("int[]?", "<inline>");
    /// assert_eq!(t.type_name, "int");
    /// assert_eq!(t.pointer_depth, 1);
    /// assert_eq!(t.optional_depth, 1);
    /// ```
    pub fn from_string(string: &str, current_file: impl AsRef<Path>) -> PekoType {
        let file = current_file.as_ref().to_path_buf();
        let tokens = lexer::TokenList::from_source(string, &file);
        let mut parser = parser::PekoParser::new(tokens, file);

        PekoType::from_tokens(&mut parser)
    }

    /// Tests whether the tokens at the parser's current cursor parse as a
    /// type, without consuming any tokens.
    ///
    /// Used by the parser to disambiguate between expression and type
    /// contexts (e.g. when parsing generic argument lists).
    #[must_use]
    pub fn test_next_tokens_for_type(parser: &mut parser::PekoParser) -> bool {
        let mut index_forward = 0;
        Self::test_next_tokens_for_type_with_index(parser, &mut index_forward)
    }

    /// Tests whether the tokens at a caller-supplied lookahead offset parse
    /// as a type, without consuming any tokens from the parser cursor.
    ///
    /// On success the offset is advanced past the type; on failure the
    /// offset is left wherever the lookahead gave up. Callers that want a
    /// clean rollback should snapshot `index_forward` before calling.
    pub fn test_next_tokens_for_type_with_index(
        parser: &mut parser::PekoParser,
        index_forward: &mut usize,
    ) -> bool {
        // Eat any leading references (& or &&).
        while !parser.tokens.finished()
            && (parser.tokens.get_token_forward(*index_forward).equals("&")
                || parser.tokens.get_token_forward(*index_forward).equals("&&"))
        {
            parser.tokens.index_increase_index(index_forward);
        }

        // Closure type form: `closure(arg, arg) => ret`.
        if parser
            .tokens
            .get_token_forward(*index_forward)
            .equals("closure")
        {
            parser.tokens.index_increase_index(index_forward);

            if !parser.tokens.get_token_forward(*index_forward).equals("(") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);

            // Argument types.
            while !parser.tokens.finished()
                && parser.tokens.get_index() + *index_forward < parser.tokens.length()
                && !parser.tokens.get_token_forward(*index_forward).equals(")")
            {
                Self::test_next_tokens_for_type_with_index(parser, index_forward);

                if parser.tokens.get_token_forward(*index_forward).equals(",") {
                    parser.tokens.index_increase_index(index_forward);
                }
            }

            if !parser.tokens.get_token_forward(*index_forward).equals(")") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);

            // Optional return type after `=>`.
            if parser.tokens.get_token_forward(*index_forward).equals("=>") {
                parser.tokens.index_increase_index(index_forward);
                if !Self::test_next_tokens_for_type_with_index(parser, index_forward) {
                    return false;
                }
            }

            return eat_depth_suffixes(parser, index_forward);
        }

        // Function type form: `(ret)(arg, arg)`.
        if parser.tokens.get_token_forward(*index_forward).equals("(") {
            parser.tokens.index_increase_index(index_forward);

            // Return type.
            Self::test_next_tokens_for_type_with_index(parser, index_forward);

            if !parser.tokens.get_token_forward(*index_forward).equals(")") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);

            // Opening `(` of argument list.
            if !parser.tokens.get_token_forward(*index_forward).equals("(") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);

            // Argument types.
            while !parser.tokens.finished()
                && parser.tokens.get_index() + *index_forward < parser.tokens.length()
                && !parser.tokens.get_token_forward(*index_forward).equals(")")
            {
                Self::test_next_tokens_for_type_with_index(parser, index_forward);
                if parser.tokens.get_token_forward(*index_forward).equals(",") {
                    parser.tokens.index_increase_index(index_forward);
                }
            }

            if !parser.tokens.get_token_forward(*index_forward).equals(")") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);

            return eat_depth_suffixes(parser, index_forward);
        }

        // Plain type form: main type name, optional `::`-separated module
        // qualifiers, optional generic argument list, then depth suffixes.
        if !is_type_name_token(parser.tokens.get_token_forward(*index_forward).get_type()) {
            return false;
        }
        parser.tokens.index_increase_index(index_forward);

        // Module-qualified path components.
        while !parser.tokens.finished()
            && parser.tokens.get_index() + *index_forward < parser.tokens.length()
            && parser.tokens.get_token_forward(*index_forward).equals("::")
        {
            parser.tokens.index_increase_index(index_forward);

            if !is_type_name_token(parser.tokens.get_token_forward(*index_forward).get_type()) {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);
        }

        // Generic argument list: `<T, T, ...>`.
        if parser.tokens.get_token_forward(*index_forward).equals("<") {
            parser.tokens.index_increase_index(index_forward);

            while !parser.tokens.finished()
                && parser.tokens.get_index() + *index_forward < parser.tokens.length()
                && !parser.tokens.get_token_forward(*index_forward).equals(">")
            {
                Self::test_next_tokens_for_type_with_index(parser, index_forward);

                if parser.tokens.get_token_forward(*index_forward).equals(",") {
                    parser.tokens.index_increase_index(index_forward);
                }
            }

            if !parser.tokens.get_token_forward(*index_forward).equals(">") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);
        }

        eat_depth_suffixes(parser, index_forward)
    }

    /// Parses a `PekoType` from the current cursor position of a
    /// [`PekoParser`](crate::parser::PekoParser).
    ///
    /// Consumes tokens through the end of the type expression. On
    /// malformed input, emits diagnostics into the parser's diagnostic
    /// list and returns whatever partial type information was successfully
    /// parsed.
    pub fn from_tokens(parser: &mut parser::PekoParser) -> PekoType {
        let mut module_names: Vec<String> = Vec::new();
        let mut generic_types: Vec<PekoType> = Vec::new();
        let mut pointer_depth = 0;
        let mut optional_depth = 0;
        let mut reference_depth = 0;
        let mut function_type: Option<PekoType> = None;

        let starting_position = parser.get_current_position();
        let mut ending_position;

        // Leading references: `&` adds 1, `&&` adds 2.
        // Done this way since the parser will parse && together as one token.
        while parser.tokens.current_token().equals("&")
            || parser.tokens.current_token().equals("&&")
        {
            if parser.tokens.current_token().equals("&&") {
                reference_depth += 1;
            }
            reference_depth += 1;
            parser.tokens.increase_index();
        }

        // Closure type form: `closure(args) => ret`.
        if parser.tokens.current_token().equals("closure") {
            parser.tokens.increase_index();

            if parser.expect_token_value("(", "function type arguments") {
                parser.tokens.increase_index();
            }

            while !parser.tokens.finished() && !parser.tokens.current_token().equals(")") {
                generic_types.push(PekoType::from_tokens(parser));

                if parser.tokens.current_token().equals(",") {
                    parser.tokens.increase_index();
                }
            }

            ending_position = parser.tokens.current_token().get_end().clone();
            if parser.expect_token_value(")", "function type arguments") {
                parser.tokens.increase_index();
            }

            if parser.tokens.current_token().equals("=>") {
                parser.tokens.increase_index();

                let return_type = PekoType::from_tokens(parser);
                ending_position = return_type.end_position.clone();
                function_type = Some(return_type);
            } else {
                function_type = Some(PekoType::simple_type("void"));
            }

            (pointer_depth, optional_depth, ending_position) =
                parse_depth_suffixes(parser, pointer_depth, optional_depth, ending_position);

            return PekoType::new(
                module_names,
                String::new(),
                generic_types,
                pointer_depth,
                optional_depth,
                reference_depth,
                function_type,
                true,
                starting_position,
                ending_position,
            );
        }

        // Function type form: `(returntype)(arg, arg, ...)`.
        if parser.tokens.current_token().equals("(") {
            parser.tokens.increase_index();

            let function_return_type = PekoType::from_tokens(parser);
            function_type = Some(function_return_type);

            if parser.expect_token_value(")", "function type") {
                parser.tokens.increase_index();
            }

            if parser.expect_token_value("(", "function type arguments") {
                parser.tokens.increase_index();
            }

            while !parser.tokens.finished() && !parser.tokens.current_token().equals(")") {
                generic_types.push(PekoType::from_tokens(parser));

                if parser.tokens.current_token().equals(",") {
                    parser.tokens.increase_index();
                }
            }

            ending_position = parser.tokens.current_token().get_end().clone();
            if parser.expect_token_value(")", "function type arguments") {
                parser.tokens.increase_index();
            }

            (pointer_depth, optional_depth, ending_position) =
                parse_depth_suffixes(parser, pointer_depth, optional_depth, ending_position);

            return PekoType::new(
                module_names,
                String::new(),
                generic_types,
                pointer_depth,
                optional_depth,
                reference_depth,
                function_type,
                false,
                starting_position,
                ending_position,
            );
        }

        // Main type name.
        let mut type_name = parser.tokens.current_token().get_value().clone();
        if !is_type_name_token(parser.tokens.current_token().get_type()) {
            parser.report_diagnostic(
                format!(
                    "expected type token for type, got {} instead",
                    parser.tokens.current_token().get_value(),
                )
                .as_str(),
            );
        }

        ending_position = parser.tokens.current_token().get_end().clone();
        parser.tokens.increase_index();

        // Module qualifiers: each `::` shifts the previously-collected type
        // name into the module path.
        while !parser.tokens.finished() && parser.tokens.current_token().equals("::") {
            parser.tokens.increase_index();

            module_names.push(type_name);
            type_name = parser.tokens.current_token().get_value().clone();

            if !is_type_name_token(parser.tokens.current_token().get_type()) {
                parser.report_diagnostic(
                    format!(
                        "expected type token for type, got {} instead",
                        parser.tokens.current_token().get_value(),
                    )
                    .as_str(),
                );
            }

            ending_position = parser.tokens.current_token().get_end().clone();
            parser.tokens.increase_index();
        }

        // Generic argument list.
        if parser.tokens.current_token().equals("<") {
            parser.tokens.increase_index();

            while !parser.tokens.finished() && !parser.tokens.current_token().equals(">") {
                generic_types.push(PekoType::from_tokens(parser));

                if parser.tokens.current_token().equals(",") {
                    parser.tokens.increase_index();
                }
            }

            ending_position = parser.tokens.current_token().get_end().clone();
            if parser.expect_token_value(">", "type generics") {
                parser.tokens.increase_index();
            }
        }

        (pointer_depth, optional_depth, ending_position) =
            parse_depth_suffixes(parser, pointer_depth, optional_depth, ending_position);

        PekoType::new(
            module_names,
            type_name,
            generic_types,
            pointer_depth,
            optional_depth,
            reference_depth,
            function_type,
            false,
            starting_position,
            ending_position,
        )
    }

    /// Constructs a type with only a bare name, no module path, no
    /// generics, no depth, no source position.
    #[must_use]
    pub fn simple_type(type_name: &str) -> PekoType {
        PekoType::new(
            Vec::new(),
            String::from(type_name),
            Vec::new(),
            0,
            0,
            0,
            None,
            false,
            PositionData::default(),
            PositionData::default(),
        )
    }

    /// Returns `true` if this is a non-pointer, non-optional, non-reference
    /// numeric or boolean primitive (`int`, `float`, `double`, `bool`).
    ///
    /// Notably excludes `char` (see [`PekoType::is_integer`] for the
    /// integer-class predicate that includes it).
    #[must_use]
    pub fn is_datatype(&self) -> bool {
        if self.pointer_depth > 0
            || self.optional_depth > 0
            || self.reference_depth > 0
            || self.function_type.is_some()
        {
            return false;
        }

        matches!(
            self.type_name.as_str(),
            "int" | "int16" | "int128" | "int64" | "float" | "double" | "bool"
        )
    }

    /// Returns `true` if this is a non-pointer, non-optional, non-reference
    /// integer-class primitive (`int`, `char`, `bool`).
    #[must_use]
    pub fn is_integer(&self) -> bool {
        if self.pointer_depth > 0
            || self.optional_depth > 0
            || self.reference_depth > 0
            || self.function_type.is_some()
        {
            return false;
        }

        matches!(
            self.type_name.as_str(),
            "int" | "int16" | "int128" | "int64" | "char" | "bool"
        )
    }

    /// Returns `true` if this is a base type (a built-in primitive, a
    /// function or closure type, or `void`). Depth modifiers do not affect
    /// the result.
    #[must_use]
    pub fn is_base_type(&self) -> bool {
        matches!(
            self.type_name.as_str(),
            "int"
                | "int16"
                | "int128"
                | "int64"
                | "char"
                | "bool"
                | "string"
                | "cstr"
                | "opaque"
                | "void"
        ) || self.function_type.is_some()
            || self.is_closure
    }

    /// Returns `true` if this is a non-pointer, non-optional, non-reference
    /// floating-point primitive (`float`, `double`).
    #[must_use]
    pub fn is_float(&self) -> bool {
        if self.pointer_depth > 0
            || self.optional_depth > 0
            || self.reference_depth > 0
            || self.function_type.is_some()
        {
            return false;
        }

        matches!(self.type_name.as_str(), "float" | "double")
    }

    /// Returns the element type one level "inside" a pointer or reference.
    ///
    /// For a type with `pointer_depth = 2` this returns the same type with
    /// `pointer_depth = 1`; for a type with `reference_depth = 1` this
    /// returns the same type with `reference_depth = 0`. Types without any
    /// pointer or reference depth are returned unchanged.
    #[must_use]
    pub fn get_element_type(&self) -> PekoType {
        if self.pointer_depth == 0 || self.reference_depth == 0 {
            return self.clone();
        }

        let mut element_type = self.clone();

        if self.pointer_depth > 0 {
            element_type.pointer_depth -= 1;
        } else {
            element_type.reference_depth -= 1;
        }

        element_type
    }

    /// Decreases this type's pointer depth by one, falling back to reference
    /// depth if no pointer depth remains.
    ///
    /// Has a special case for `string` and `opaque` types: dereferencing one
    /// of those yields a `char`. This matches the language semantics of
    /// pointer-to-character types.
    pub fn decrease_pointer_depth(&mut self) {
        // Cache the rendered form to avoid two allocations per call.
        let stringified = self.to_string();
        if stringified == "string" || stringified == "opaque" {
            self.type_name = "char".to_string();
        }

        if self.pointer_depth > 0 {
            self.pointer_depth -= 1;
        } else if self.reference_depth > 0 {
            self.reference_depth -= 1;
        } else if self.type_name == "Pointer" && self.generic_types.len() >= 1 {
            *self = self.generic_types[0].clone();
        }
    }

    /// Returns `true` if this is a built-in primitive in its base form (no
    /// pointer, optional, reference, or function-type modifier).
    ///
    /// Unlike [`PekoType::is_base_type`], this excludes function and closure
    /// forms and is strict about depth modifiers.
    #[must_use]
    pub fn is_builtin_type(&self) -> bool {
        if self.pointer_depth > 0
            || self.optional_depth > 0
            || self.reference_depth > 0
            || self.function_type.is_some()
        {
            return false;
        }

        matches!(
            self.type_name.as_str(),
            "int"
                | "int64"
                | "int16"
                | "int128"
                | "float"
                | "double"
                | "char"
                | "string"
                | "cstr"
                | "bool"
                | "opaque"
                | "void"
        )
    }

    /// Returns a copy of this type with all "extra" information stripped.
    /// Including function type, closure flag, error flag, module names, and
    /// all depth modifiers. Useful when comparing the underlying named type
    /// independently of how it's wrapped at a given use-site.
    #[must_use]
    pub fn declutter(&self) -> Self {
        let mut decluttered = self.clone();
        decluttered.function_type = None;
        decluttered.is_closure = false;
        decluttered.is_error_type = false;
        decluttered.module_names.clear();
        decluttered.optional_depth = 0;
        decluttered.pointer_depth = 0;
        decluttered.reference_depth = 0;

        decluttered
    }

    /// Returns a copy of this type with all depth modifiers
    /// (pointer/optional/reference) zeroed but module path, generics, and
    /// function-type info preserved.
    #[must_use]
    pub fn no_depth(&self) -> Self {
        let mut decluttered = self.clone();
        decluttered.optional_depth = 0;
        decluttered.pointer_depth = 0;
        decluttered.reference_depth = 0;

        decluttered
    }

    /// Returns `true` if this type behaves as a pointer in codegen. Any
    /// non-zero pointer or reference depth, or `string` / `opaque`, which
    /// are pointer-typed at the implementation level.
    #[must_use]
    pub fn is_pointer(&self) -> bool {
        self.pointer_depth > 0
            || self.type_name == "opaque"
            || self.type_name == "string"
            || self.type_name == "cstr"
            || self.type_name == "Pointer"
            || self.reference_depth > 0
    }

    /// If this type is `Option<T>` or has `optional_depth > 0`, returns the
    /// inner unwrapped type. Otherwise returns `None`.
    #[must_use]
    pub fn optional_get_inner_type(&self) -> Option<PekoType> {
        if self.type_name == "Option" && self.generic_types.len() == 1 {
            Some(self.generic_types[0].clone())
        } else if self.optional_depth > 0 {
            let mut inner_type = self.clone();
            inner_type.optional_depth -= 1;
            Some(inner_type)
        } else {
            None
        }
    }

    /// Returns `true` if this type represents a string in any of its surface
    /// forms: bare `string`, `char[]`, `&char`, or `char*`.
    #[must_use]
    pub fn is_string_type(&self) -> bool {
        let stringified_type = self.to_string();
        stringified_type == "cstr"
            || stringified_type == "char[]"
            || stringified_type == "&char"
            || stringified_type == "char*"
    }

    /// Renders this type as a name-mangled identifier suitable for symbol
    /// emission.
    ///
    /// The mangling scheme uses distinctive ASCII separators (`_$_$_$`,
    /// `$$$_`, etc.) to encode module paths, generics, function-type
    /// structure, and depth modifiers without ambiguity.
    #[must_use]
    pub fn to_mangled_string(&self) -> String {
        const MODULE_SEPERATOR: &str = "_$_$_$";
        const PAREN_OPEN: &str = "$$$_";
        const PAREN_CLOSE: &str = "_$$$";
        const GENERIC_SEPERATOR_LEFT: &str = "_$$_";
        const GENERIC_SEPERATOR_RIGHT: &str = "$__$";
        const TYPE_SEPERATOR: &str = "$$__$$";
        const FUNCTION_TYPE: &str = "$$$__";
        const REFERENCE: &str = "$_$_$";
        const POINTER: &str = "_$_$_";
        const OPTIONAL: &str = "$$$$";

        let mut final_type = String::new();

        if self.is_closure {
            final_type.push_str("closure");
            final_type.push_str(PAREN_OPEN);

            let mangled_args: Vec<String> = self
                .generic_types
                .iter()
                .map(PekoType::to_mangled_string)
                .collect();
            final_type.push_str(&mangled_args.join(TYPE_SEPERATOR));

            final_type.push_str(PAREN_CLOSE);

            if let Some(ret) = &self.function_type {
                final_type.push_str(FUNCTION_TYPE);
                final_type.push_str(&ret.to_string());
            }
        } else if let Some(ret) = &self.function_type {
            final_type.push_str(PAREN_OPEN);
            final_type.push_str(&ret.to_mangled_string());
            final_type.push_str(PAREN_CLOSE);
            final_type.push_str(PAREN_OPEN);

            let arg_strs: Vec<String> = self
                .generic_types
                .iter()
                .map(PekoType::to_mangled_string)
                .collect();
            final_type.push_str(&arg_strs.join(TYPE_SEPERATOR));

            final_type.push_str(PAREN_CLOSE);
        } else {
            for name in &self.module_names {
                final_type.push_str(name);
                final_type.push_str(MODULE_SEPERATOR);
            }

            final_type.push_str(&self.type_name);

            if !self.generic_types.is_empty() {
                final_type.push_str(GENERIC_SEPERATOR_LEFT);

                let mangled_generics: Vec<String> = self
                    .generic_types
                    .iter()
                    .map(PekoType::to_mangled_string)
                    .collect();
                final_type.push_str(&mangled_generics.join(TYPE_SEPERATOR));

                final_type.push_str(GENERIC_SEPERATOR_RIGHT);
            }
        }

        for _ in 0..self.reference_depth {
            final_type.insert_str(0, REFERENCE);
        }
        for _ in 0..self.pointer_depth {
            final_type.push_str(POINTER);
        }
        for _ in 0..self.optional_depth {
            final_type.push_str(OPTIONAL);
        }

        final_type
    }
}

impl fmt::Display for PekoType {
    /// Renders this type in human-readable Pekoscript surface form.
    ///
    /// Output is round-trippable through [`PekoType::from_string`] for all
    /// well-formed types. The exact format:
    ///
    /// * Closure: `closure(arg, arg) => ret`
    /// * Function: `(ret)(arg, arg)`
    /// * Plain:    `module::path::Name<T, U>`
    ///
    /// Depth modifiers are appended (`[]` for pointer, `?` for optional) or
    /// prepended (`&` for reference).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut final_type = String::new();

        if self.is_closure {
            final_type.push_str("closure(");
            let arg_strs: Vec<String> =
                self.generic_types.iter().map(PekoType::to_string).collect();
            final_type.push_str(&arg_strs.join(", "));
            final_type.push(')');

            if let Some(ret) = &self.function_type {
                final_type.push_str(" => ");
                final_type.push_str(&ret.to_string());
            }
        } else if let Some(ret) = &self.function_type {
            final_type.push('(');
            final_type.push_str(&ret.to_string());
            final_type.push_str(")(");

            let arg_strs: Vec<String> =
                self.generic_types.iter().map(PekoType::to_string).collect();
            final_type.push_str(&arg_strs.join(", "));

            final_type.push(')');
        } else {
            for name in &self.module_names {
                final_type.push_str(name);
                final_type.push_str("::");
            }

            final_type.push_str(&self.type_name);

            if !self.generic_types.is_empty() {
                final_type.push('<');
                let gen_strs: Vec<String> =
                    self.generic_types.iter().map(PekoType::to_string).collect();
                final_type.push_str(&gen_strs.join(", "));
                final_type.push('>');
            }
        }

        for _ in 0..self.reference_depth {
            final_type.insert(0, '&');
        }
        for _ in 0..self.pointer_depth {
            final_type.push_str("[]");
        }
        for _ in 0..self.optional_depth {
            final_type.push('?');
        }

        f.write_str(&final_type)
    }
}

// ----- Private helpers ------------------------------------------------------

/// Returns `true` if a token type can introduce or continue a type name.
///
/// Type names can be plain identifiers or any of the keyword tokens that
/// double as built-in type names (`string`, `int`, `bool`, etc.).
fn is_type_name_token(ty: &lexer::TokenType) -> bool {
    matches!(
        ty,
        TokenType::Identifier
            | TokenType::StringType
            | TokenType::Int64Type
            | TokenType::IntType
            | TokenType::Int16Type
            | TokenType::Int128Type
            | TokenType::BoolType
            | TokenType::CharType
            | TokenType::FloatType
            | TokenType::DoubleType
            | TokenType::OpaqueType
            | TokenType::CStrType
    )
}

/// Lookahead variant of the depth-suffix loop used by
/// [`PekoType::test_next_tokens_for_type_with_index`].
///
/// Advances `index_forward` past any sequence of `[]` and `?` suffixes.
/// Returns `false` if a `[` is seen without a matching `]`.
fn eat_depth_suffixes(parser: &mut parser::PekoParser, index_forward: &mut usize) -> bool {
    while !parser.tokens.finished()
        && parser.tokens.get_index() + *index_forward < parser.tokens.length()
        && (parser.tokens.get_token_forward(*index_forward).equals("[")
            || parser.tokens.get_token_forward(*index_forward).equals("?"))
    {
        if parser.tokens.get_token_forward(*index_forward).equals("[") {
            parser.tokens.index_increase_index(index_forward);

            if !parser.tokens.get_token_forward(*index_forward).equals("]") {
                return false;
            }
            parser.tokens.index_increase_index(index_forward);
        } else {
            parser.tokens.index_increase_index(index_forward);
        }
    }

    true
}

/// Consuming variant of the depth-suffix loop used by [`PekoType::from_tokens`].
///
/// Returns the updated `(pointer_depth, optional_depth, ending_position)`
/// tuple after walking the suffix sequence.
fn parse_depth_suffixes(
    parser: &mut parser::PekoParser,
    mut pointer_depth: usize,
    mut optional_depth: usize,
    mut ending_position: PositionData,
) -> (usize, usize, PositionData) {
    while !parser.tokens.finished()
        && (parser.tokens.current_token().equals("[") || parser.tokens.current_token().equals("?"))
    {
        if parser.tokens.current_token().equals("[") {
            pointer_depth += 1;
            parser.tokens.increase_index();

            ending_position = parser.tokens.current_token().get_end().clone();
            if parser.expect_token_value("]", "type array") {
                parser.tokens.increase_index();
            }
        } else {
            optional_depth += 1;
            ending_position = parser.tokens.current_token().get_end().clone();
            parser.tokens.increase_index();
        }
    }

    (pointer_depth, optional_depth, ending_position)
}
