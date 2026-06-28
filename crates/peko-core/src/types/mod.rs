//! # Peko Core Types
//!
//! Type representation for Pekoscript.
//!
//! [`PekoType`] is the canonical structural representation of a Pekoscript
//! type expression. It pairs a [`PekoTypeKind`] (the V2 enum form that carries
//! `const`-ness, generics, and restraints) with the array and reference depth
//! modifiers and the source span the type was parsed from.
//!
//! The kind is one of three forms:
//!
//! * [`PekoTypeKind::Basic`]: a named type with a module path and generic
//!   arguments (`module::Name<T>`).
//! * [`PekoTypeKind::Function`]: a function or closure type with argument
//!   types, an optional return type, and a closure flag.
//! * [`PekoTypeKind::Generic`]: a generic parameter with its restraints.
//!
//! The module exposes two ways to construct a `PekoType` from source:
//!
//! * [`PekoType::from_tokens`]: consume tokens from an existing
//!   [`PekoParser`](crate::parser::PekoParser) cursor. Used inside the parser
//!   when a type appears inline in a declaration.
//! * [`PekoType::from_string`]: lex a standalone type expression. Used by
//!   the simulator and tests when synthesizing types from string literals.
//!
//! A trailing `?` desugars to `std::core::Option<T>` during parsing, so an
//! optional is an ordinary named type rather than a depth modifier.

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;

use crate::asts::data_structures::PositionData;
use crate::lexer::{self, TokenType};
use crate::parser;

/// The named-type payload of a [`PekoTypeKind::Basic`]: a module path, a bare
/// name, and the generic arguments written at the use site.
#[derive(Clone, Debug)]
pub struct PekoTypeInfo {
    /// Module path components of a qualified type (e.g. `["module1", "module2"]`
    /// for `module1::module2::Type`).
    pub module_names: Vec<String>,

    /// The bare type name (e.g. `"Type"`).
    pub name: String,

    /// Generic type arguments written at the use site.
    pub generics: Vec<PekoType>,
}

/// A restraint on a generic parameter.
#[derive(Clone, Debug)]
pub enum TypeRestraint {
    /// The parameter must implement a trait (`T: impl Drawable`).
    Impl(PekoType),

    /// The parameter must inherit from a type (`T: from Animal`).
    From(PekoType),
}

/// The V2 enum form of a type: a function, a basic named type, or a generic
/// parameter. Each value-bearing form carries its own `const`-ness.
#[derive(Clone, Debug)]
pub enum PekoTypeKind {
    /// A function or closure type. `arguments` are the parameter types,
    /// `return_type` is the return type, and `is_closure` distinguishes the
    /// `closure(...)` surface form from the `(ret)(args)` function form.
    Function {
        is_const: bool,
        arguments: Vec<PekoType>,
        return_type: Option<Box<PekoType>>,
        is_closure: bool,
    },

    /// A basic named type with a module path and generic arguments.
    Basic { is_const: bool, info: PekoTypeInfo },

    /// A generic parameter with its restraints.
    Generic {
        name: String,
        restraints: Vec<TypeRestraint>,
    },
}

/// Structural representation of a Pekoscript type.
///
/// A `PekoType` carries the [`PekoTypeKind`] enum form plus the array and
/// reference depth modifiers and the source positions it was parsed from. Two
/// `PekoType` values describing the same underlying type can differ in their
/// source positions; type *equality* in the language-semantic sense lives on
/// the simulator (see `PekoSimulatorContext::types_equal`), not via `PartialEq`
/// on this struct.
#[derive(Clone, Debug)]
pub struct PekoType {
    /// The enum form of this type.
    pub kind: PekoTypeKind,

    /// Array depth, counted by trailing `[]` suffixes.
    pub array_depth: usize,

    /// Reference depth, counted by leading `&` prefixes (a `&&` counts as 2).
    pub reference_depth: usize,

    /// Inclusive start position of the type expression in source.
    pub start_position: PositionData,

    /// Inclusive end position of the type expression in source.
    pub end_position: PositionData,

    /// Set by the simulator after generic substitution has finished;
    /// indicates the type contains no unresolved generic parameters.
    pub fully_expanded: bool,
}

impl PekoType {
    /// Constructs a fully-specified `PekoType` from the legacy field set.
    ///
    /// A `function_type` (or `is_closure`) produces a [`PekoTypeKind::Function`];
    /// everything else produces a [`PekoTypeKind::Basic`]. A non-zero
    /// `optional_depth` wraps the type that many times in `std::core::Option`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        module_names: Vec<String>,
        type_name: String,
        generic_types: Vec<PekoType>,
        array_depth: usize,
        optional_depth: usize,
        reference_depth: usize,
        function_type: Option<PekoType>,
        is_closure: bool,
        start_position: PositionData,
        end_position: PositionData,
    ) -> PekoType {
        let kind = if function_type.is_some() || is_closure {
            PekoTypeKind::Function {
                is_const: false,
                arguments: generic_types,
                return_type: function_type.map(Box::new),
                is_closure,
            }
        } else {
            PekoTypeKind::Basic {
                is_const: false,
                info: PekoTypeInfo {
                    module_names,
                    name: canonical_builtin_name(type_name),
                    generics: generic_types,
                },
            }
        };

        let mut peko_type = PekoType {
            kind,
            array_depth,
            reference_depth,
            start_position,
            end_position,
            fully_expanded: false,
        };

        // A trailing `?` is sugar for `std::core::Option<T>`.
        for _ in 0..optional_depth {
            peko_type = PekoType::option_of(peko_type);
        }

        peko_type
    }

    /// Wraps a type in `Option<T>`, preserving its source span. `Option` is
    /// resolved bare because std::core is unpacked into every module.
    #[must_use]
    pub fn option_of(inner: PekoType) -> PekoType {
        let start = inner.start_position.clone();
        let end = inner.end_position.clone();

        PekoType {
            kind: PekoTypeKind::Basic {
                is_const: false,
                info: PekoTypeInfo {
                    module_names: Vec::new(),
                    name: "Option".to_string(),
                    generics: vec![inner],
                },
            },
            array_depth: 0,
            reference_depth: 0,
            start_position: start,
            end_position: end,
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
        PekoType::simple_type("<<error_type>>")
    }

    /// Constructs a type with only a bare name, no module path, no generics,
    /// no depth, no source position.
    #[must_use]
    pub fn simple_type(type_name: &str) -> PekoType {
        PekoType {
            kind: PekoTypeKind::Basic {
                is_const: false,
                info: PekoTypeInfo {
                    module_names: Vec::new(),
                    name: canonical_builtin_name(String::from(type_name)),
                    generics: Vec::new(),
                },
            },
            array_depth: 0,
            reference_depth: 0,
            start_position: PositionData::default(),
            end_position: PositionData::default(),
            fully_expanded: false,
        }
    }

    /// Constructs a generic-parameter type with its restraints.
    #[must_use]
    pub fn generic_type(name: impl Into<String>, restraints: Vec<TypeRestraint>) -> PekoType {
        PekoType {
            kind: PekoTypeKind::Generic {
                name: name.into(),
                restraints,
            },
            array_depth: 0,
            reference_depth: 0,
            start_position: PositionData::default(),
            end_position: PositionData::default(),
            fully_expanded: false,
        }
    }

    // ----- Accessors over the kind enum -------------------------------------

    /// The bare type name. For a function type this is empty; for a generic
    /// parameter it is the parameter name.
    #[must_use]
    pub fn name(&self) -> &str {
        match &self.kind {
            PekoTypeKind::Basic { info, .. } => &info.name,
            PekoTypeKind::Generic { name, .. } => name,
            PekoTypeKind::Function { .. } => "",
        }
    }

    /// Sets the bare type name. A no-op for function types.
    pub fn set_name(&mut self, new_name: impl Into<String>) {
        match &mut self.kind {
            PekoTypeKind::Basic { info, .. } => info.name = new_name.into(),
            PekoTypeKind::Generic { name, .. } => *name = new_name.into(),
            PekoTypeKind::Function { .. } => {}
        }
    }

    /// The module path components of a qualified named type. Empty for
    /// function and generic forms.
    #[must_use]
    pub fn module_names(&self) -> &[String] {
        match &self.kind {
            PekoTypeKind::Basic { info, .. } => &info.module_names,
            _ => &[],
        }
    }

    /// Mutable view of the module path. Only meaningful for basic named types.
    pub fn module_names_mut(&mut self) -> &mut Vec<String> {
        match &mut self.kind {
            PekoTypeKind::Basic { info, .. } => &mut info.module_names,
            _ => panic!("module_names_mut on a non-basic type"),
        }
    }

    /// The generic arguments. For a function type this is the argument-type
    /// list. Empty for a generic parameter.
    #[must_use]
    pub fn generics(&self) -> &[PekoType] {
        match &self.kind {
            PekoTypeKind::Basic { info, .. } => &info.generics,
            PekoTypeKind::Function { arguments, .. } => arguments,
            PekoTypeKind::Generic { .. } => &[],
        }
    }

    /// Mutable view of the generic arguments (or function argument types).
    pub fn generics_mut(&mut self) -> &mut Vec<PekoType> {
        match &mut self.kind {
            PekoTypeKind::Basic { info, .. } => &mut info.generics,
            PekoTypeKind::Function { arguments, .. } => arguments,
            PekoTypeKind::Generic { .. } => panic!("generics_mut on a generic-parameter type"),
        }
    }

    /// `true` if this type is `const`.
    #[must_use]
    pub fn is_const(&self) -> bool {
        match &self.kind {
            PekoTypeKind::Basic { is_const, .. } | PekoTypeKind::Function { is_const, .. } => {
                *is_const
            }
            PekoTypeKind::Generic { .. } => false,
        }
    }

    /// Sets this type's `const`-ness. A no-op for generic parameters.
    pub fn set_const(&mut self, value: bool) {
        match &mut self.kind {
            PekoTypeKind::Basic { is_const, .. } | PekoTypeKind::Function { is_const, .. } => {
                *is_const = value
            }
            PekoTypeKind::Generic { .. } => {}
        }
    }

    /// `true` if this is a function or closure type.
    #[must_use]
    pub fn is_function(&self) -> bool {
        matches!(self.kind, PekoTypeKind::Function { .. })
    }

    /// `true` if this type was written as a `closure(...)` form.
    #[must_use]
    pub fn is_closure(&self) -> bool {
        matches!(
            self.kind,
            PekoTypeKind::Function {
                is_closure: true,
                ..
            }
        )
    }

    /// Sets the closure flag on a function type. A no-op otherwise.
    pub fn set_closure(&mut self, value: bool) {
        if let PekoTypeKind::Function { is_closure, .. } = &mut self.kind {
            *is_closure = value;
        }
    }

    /// The return type of a function or closure type, if any.
    #[must_use]
    pub fn function_return(&self) -> Option<&PekoType> {
        match &self.kind {
            PekoTypeKind::Function { return_type, .. } => return_type.as_deref(),
            _ => None,
        }
    }

    /// Sets the return type, converting this value into a function type if it
    /// is not one already.
    pub fn set_function_return(&mut self, return_type: Option<PekoType>) {
        match &mut self.kind {
            PekoTypeKind::Function {
                return_type: slot, ..
            } => *slot = return_type.map(Box::new),
            _ => {
                self.kind = PekoTypeKind::Function {
                    is_const: self.is_const(),
                    arguments: self.generics().to_vec(),
                    return_type: return_type.map(Box::new),
                    is_closure: false,
                };
            }
        }
    }

    /// `true` if this is the error sentinel type.
    #[must_use]
    pub fn is_error_type(&self) -> bool {
        self.name() == "<<error_type>>"
    }

    /// `true` if this is a generic-parameter type.
    #[must_use]
    pub fn is_generic_param(&self) -> bool {
        matches!(self.kind, PekoTypeKind::Generic { .. })
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
    /// let t = PekoType::from_string("int[]", "<inline>");
    /// assert_eq!(t.name(), "int");
    /// assert_eq!(t.array_depth, 1);
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
        // Eat a leading `const` modifier.
        if parser.tokens.get_token_forward(*index_forward).equals("const") {
            parser.tokens.index_increase_index(index_forward);
        }

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
                let index_before = *index_forward;
                Self::test_next_tokens_for_type_with_index(parser, index_forward);
                // If the recursive call made no progress and the current lookahead
                // token is not the closing ')' or a ',', force past it so this
                // loop always terminates.
                if *index_forward == index_before
                    && !parser.tokens.get_token_forward(*index_forward).equals(")")
                {
                    parser.tokens.index_increase_index(index_forward);
                }

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
                let index_before = *index_forward;
                Self::test_next_tokens_for_type_with_index(parser, index_forward);
                // If the recursive call made no progress and the current lookahead
                // token is not the closing ')' or a ',', force past it so this
                // loop always terminates.
                if *index_forward == index_before
                    && !parser.tokens.get_token_forward(*index_forward).equals(")")
                {
                    parser.tokens.index_increase_index(index_forward);
                }
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
                let index_before = *index_forward;
                Self::test_next_tokens_for_type_with_index(parser, index_forward);
                // If the recursive call made no progress and the current lookahead
                // token is not the closing '>' or a ',', force past it so this
                // loop always terminates.
                if *index_forward == index_before
                    && !parser.tokens.get_token_forward(*index_forward).equals(">")
                {
                    parser.tokens.index_increase_index(index_forward);
                }

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
    /// parsed. A trailing `?` desugars to `std::core::Option<T>`.
    pub fn from_tokens(parser: &mut parser::PekoParser) -> PekoType {
        let mut module_names: Vec<String> = Vec::new();
        let mut generic_types: Vec<PekoType> = Vec::new();
        let mut array_depth = 0;
        let mut optional_depth = 0;
        let mut reference_depth = 0;
        let mut function_type: Option<PekoType> = None;

        let starting_position = parser.get_current_position();
        let mut ending_position;

        // Leading `const` modifier marks the type as immutable.
        let is_const = parser.tokens.current_token().equals("const");
        if is_const {
            parser.tokens.increase_index();
        }

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

            (array_depth, optional_depth, ending_position) =
                parse_depth_suffixes(parser, array_depth, optional_depth, ending_position);

            let mut closure_type = PekoType::new(
                module_names,
                String::new(),
                generic_types,
                array_depth,
                optional_depth,
                reference_depth,
                function_type,
                true,
                starting_position,
                ending_position,
            );
            closure_type.set_const(is_const);
            return closure_type;
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

            (array_depth, optional_depth, ending_position) =
                parse_depth_suffixes(parser, array_depth, optional_depth, ending_position);

            let mut function_form = PekoType::new(
                module_names,
                String::new(),
                generic_types,
                array_depth,
                optional_depth,
                reference_depth,
                function_type,
                false,
                starting_position,
                ending_position,
            );
            function_form.set_const(is_const);
            return function_form;
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

        (array_depth, optional_depth, ending_position) =
            parse_depth_suffixes(parser, array_depth, optional_depth, ending_position);

        let mut basic_type = PekoType::new(
            module_names,
            type_name,
            generic_types,
            array_depth,
            optional_depth,
            reference_depth,
            function_type,
            false,
            starting_position,
            ending_position,
        );
        basic_type.set_const(is_const);
        basic_type
    }

    /// Returns `true` if this is a non-array, non-reference numeric or boolean
    /// primitive (`int`, `float`, `double`, `bool`).
    ///
    /// Notably excludes `char` (see [`PekoType::is_integer`] for the
    /// integer-class predicate that includes it).
    #[must_use]
    pub fn is_datatype(&self) -> bool {
        if self.array_depth > 0 || self.reference_depth > 0 || self.is_function() {
            return false;
        }

        matches!(
            self.name(),
            "int" | "int16" | "int128" | "int64" | "float" | "double" | "f16" | "bool"
        )
    }

    /// Returns `true` if this is a non-array, non-reference integer-class
    /// primitive (`int`, `char`, `bool`).
    #[must_use]
    pub fn is_integer(&self) -> bool {
        if self.array_depth > 0 || self.reference_depth > 0 || self.is_function() {
            return false;
        }

        matches!(
            self.name(),
            "int" | "int16" | "int128" | "int64" | "char" | "bool"
        )
    }

    /// Returns `true` if this is a base type (a built-in primitive, a
    /// function or closure type, or `void`). Depth modifiers do not affect
    /// the result.
    #[must_use]
    pub fn is_base_type(&self) -> bool {
        matches!(
            self.name(),
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
        ) || self.is_function()
    }

    /// Returns `true` if this is a non-array, non-reference floating-point
    /// primitive (`float`, `double`).
    #[must_use]
    pub fn is_float(&self) -> bool {
        if self.array_depth > 0 || self.reference_depth > 0 || self.is_function() {
            return false;
        }

        matches!(self.name(), "float" | "double" | "f16")
    }

    /// Returns the element type one level "inside" an array or reference.
    ///
    /// For a type with `array_depth = 2` this returns the same type with
    /// `array_depth = 1`; for a type with `reference_depth = 1` this returns
    /// the same type with `reference_depth = 0`. Types without any array or
    /// reference depth are returned unchanged.
    #[must_use]
    pub fn get_element_type(&self) -> PekoType {
        if self.array_depth == 0 || self.reference_depth == 0 {
            return self.clone();
        }

        let mut element_type = self.clone();

        if self.array_depth > 0 {
            element_type.array_depth -= 1;
        } else {
            element_type.reference_depth -= 1;
        }

        element_type
    }

    /// Decreases this type's array depth by one, falling back to reference
    /// depth if no array depth remains.
    ///
    /// Has a special case for `string` and `opaque` types: dereferencing one
    /// of those yields a `char`. This matches the language semantics of
    /// pointer-to-character types.
    pub fn decrease_pointer_depth(&mut self) {
        // Cache the rendered form to avoid two allocations per call.
        let stringified = self.to_string();
        if stringified == "string" || stringified == "opaque" {
            self.set_name("char");
        }

        if self.array_depth > 0 {
            self.array_depth -= 1;
        } else if self.reference_depth > 0 {
            self.reference_depth -= 1;
        } else if self.name() == "Pointer" && !self.generics().is_empty() {
            *self = self.generics()[0].clone();
        }
    }

    /// Returns `true` if this is a built-in primitive in its base form (no
    /// array, reference, or function-type modifier).
    ///
    /// Unlike [`PekoType::is_base_type`], this excludes function and closure
    /// forms and is strict about depth modifiers.
    #[must_use]
    pub fn is_builtin_type(&self) -> bool {
        if self.array_depth > 0 || self.reference_depth > 0 || self.is_function() {
            return false;
        }

        matches!(
            self.name(),
            "int"
                | "int64"
                | "int16"
                | "int128"
                | "float"
                | "double"
                | "f16"
                | "char"
                | "string"
                | "cstr"
                | "bool"
                | "opaque"
                | "void"
        )
    }

    /// Returns a copy of this type with all "extra" information stripped:
    /// module names, generics-wrapping function/closure structure, and all
    /// depth modifiers. The result is the underlying named type, useful when
    /// comparing names independently of how a type is wrapped at a use site.
    #[must_use]
    pub fn declutter(&self) -> Self {
        PekoType {
            kind: PekoTypeKind::Basic {
                is_const: false,
                info: PekoTypeInfo {
                    module_names: Vec::new(),
                    name: self.name().to_string(),
                    generics: self.generics().to_vec(),
                },
            },
            array_depth: 0,
            reference_depth: 0,
            start_position: self.start_position.clone(),
            end_position: self.end_position.clone(),
            fully_expanded: self.fully_expanded,
        }
    }

    /// Returns a copy of this type with all depth modifiers (array/reference)
    /// zeroed but module path, generics, and function-type info preserved.
    #[must_use]
    pub fn no_depth(&self) -> Self {
        let mut decluttered = self.clone();
        decluttered.array_depth = 0;
        decluttered.reference_depth = 0;

        decluttered
    }

    /// Returns `true` if this type behaves as a pointer in codegen. Any
    /// non-zero array or reference depth, or `string` / `opaque`, which are
    /// pointer-typed at the implementation level.
    #[must_use]
    pub fn is_pointer(&self) -> bool {
        self.array_depth > 0
            || self.name() == "opaque"
            || self.name() == "string"
            || self.name() == "cstr"
            || self.name() == "Pointer"
            || self.reference_depth > 0
    }

    /// If this type is `Option<T>`, returns the inner unwrapped type.
    /// Otherwise returns `None`.
    #[must_use]
    pub fn optional_get_inner_type(&self) -> Option<PekoType> {
        if self.name() == "Option" && self.generics().len() == 1 {
            Some(self.generics()[0].clone())
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
        const CONST: &str = "$_const_$";

        let mut final_type = String::new();

        if self.is_closure() {
            final_type.push_str("closure");
            final_type.push_str(PAREN_OPEN);

            let mangled_args: Vec<String> =
                self.generics().iter().map(PekoType::to_mangled_string).collect();
            final_type.push_str(&mangled_args.join(TYPE_SEPERATOR));

            final_type.push_str(PAREN_CLOSE);

            if let Some(ret) = self.function_return() {
                final_type.push_str(FUNCTION_TYPE);
                final_type.push_str(&ret.to_string());
            }
        } else if let Some(ret) = self.function_return() {
            final_type.push_str(PAREN_OPEN);
            final_type.push_str(&ret.to_mangled_string());
            final_type.push_str(PAREN_CLOSE);
            final_type.push_str(PAREN_OPEN);

            let arg_strs: Vec<String> =
                self.generics().iter().map(PekoType::to_mangled_string).collect();
            final_type.push_str(&arg_strs.join(TYPE_SEPERATOR));

            final_type.push_str(PAREN_CLOSE);
        } else {
            for name in self.module_names() {
                final_type.push_str(name);
                final_type.push_str(MODULE_SEPERATOR);
            }

            final_type.push_str(self.name());

            if !self.generics().is_empty() {
                final_type.push_str(GENERIC_SEPERATOR_LEFT);

                let mangled_generics: Vec<String> =
                    self.generics().iter().map(PekoType::to_mangled_string).collect();
                final_type.push_str(&mangled_generics.join(TYPE_SEPERATOR));

                final_type.push_str(GENERIC_SEPERATOR_RIGHT);
            }
        }

        for _ in 0..self.reference_depth {
            final_type.insert_str(0, REFERENCE);
        }
        for _ in 0..self.array_depth {
            final_type.push_str(POINTER);
        }

        if self.is_const() {
            final_type.insert_str(0, CONST);
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
    /// Depth modifiers are appended (`[]` for array) or prepended (`&` for
    /// reference).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut final_type = String::new();

        if self.is_closure() {
            final_type.push_str("closure(");
            let arg_strs: Vec<String> = self.generics().iter().map(PekoType::to_string).collect();
            final_type.push_str(&arg_strs.join(", "));
            final_type.push(')');

            if let Some(ret) = self.function_return() {
                final_type.push_str(" => ");
                final_type.push_str(&ret.to_string());
            }
        } else if let Some(ret) = self.function_return() {
            final_type.push('(');
            final_type.push_str(&ret.to_string());
            final_type.push_str(")(");

            let arg_strs: Vec<String> = self.generics().iter().map(PekoType::to_string).collect();
            final_type.push_str(&arg_strs.join(", "));

            final_type.push(')');
        } else {
            for name in self.module_names() {
                final_type.push_str(name);
                final_type.push_str("::");
            }

            final_type.push_str(self.name());

            if !self.generics().is_empty() {
                final_type.push('<');
                let gen_strs: Vec<String> =
                    self.generics().iter().map(PekoType::to_string).collect();
                final_type.push_str(&gen_strs.join(", "));
                final_type.push('>');
            }
        }

        for _ in 0..self.reference_depth {
            final_type.insert(0, '&');
        }
        for _ in 0..self.array_depth {
            final_type.push_str("[]");
        }

        if self.is_const() {
            final_type.insert_str(0, "const ");
        }

        f.write_str(&final_type)
    }
}

// ----- Private helpers ------------------------------------------------------

/// Maps a V2 FFI builtin spelling to the internal type name the analyzer and
/// codegen already handle. The V2 surface names the integers `i1` through
/// `i128`, the floats `f32` and `f64`, and the managed pointer `pointer<T>`.
/// `i1` and `i8` map to `bool` and `char` (their matching widths). Each lowers
/// to the same representation as the matching internal name, so downstream type
/// checks and codegen need no separate cases. The half float `f16` keeps its
/// own name and is recognized directly. Names without a V2 alias, including
/// user types and `cstr`, `opaque`, `char`, `bool`, `void`, and `string`, pass
/// through unchanged.
fn canonical_builtin_name(name: String) -> String {
    match name.as_str() {
        "i1" => String::from("bool"),
        "i8" => String::from("char"),
        "i16" => String::from("int16"),
        "i32" => String::from("int"),
        "i64" => String::from("int64"),
        "i128" => String::from("int128"),
        "f32" => String::from("float"),
        "f64" => String::from("double"),
        "pointer" => String::from("Pointer"),
        _ => name,
    }
}

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
        && ((parser.tokens.get_token_forward(*index_forward).equals("[")
            && parser.tokens.get_token_forward(*index_forward + 1).equals("]"))
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
/// Returns the updated `(array_depth, optional_depth, ending_position)` tuple
/// after walking the suffix sequence.
fn parse_depth_suffixes(
    parser: &mut parser::PekoParser,
    mut array_depth: usize,
    mut optional_depth: usize,
    mut ending_position: PositionData,
) -> (usize, usize, PositionData) {
    // A `[` is an array suffix only when followed immediately by `]`. A `[`
    // that introduces something else (a following member's `[mutates]`
    // modifier, for example) is left for the surrounding parser.
    while !parser.tokens.finished()
        && ((parser.tokens.current_token().equals("[")
            && parser.tokens.get_token_forward(1).equals("]"))
            || parser.tokens.current_token().equals("?"))
    {
        if parser.tokens.current_token().equals("[") {
            array_depth += 1;
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

    (array_depth, optional_depth, ending_position)
}
