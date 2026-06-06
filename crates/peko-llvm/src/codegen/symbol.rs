//! `SymbolName`: the mangled-name structure used throughout codegen.
//!
//! A symbol name is parsed from a string like `foo::bar::Baz<int>` into
//! its constituent module path, generic-type list, and (for class
//! methods) owning-class name. The `to_string(mangled)` method renders
//! the symbol back to either its source form (when `mangled = false`)
//! or its LLVM-safe mangled form (when `mangled = true`).

use std::path::PathBuf;

use derive_new::new;
use peko_core::lexer::TokenList;
use peko_core::parser::PekoParser;
use peko_core::types::PekoType;

#[derive(Debug, Clone, new)]
pub struct SymbolName {
    pub file_prefix: Option<PathBuf>,
    pub module_names: Vec<String>,
    owning_class: Option<String>,
    symbol_name: String,
    generic_types: Vec<PekoType>,
    argument_types: Option<Vec<PekoType>>,
}

impl SymbolName {
    /// Parse a `SymbolName` from a string of the form
    /// `module::submodule::Symbol<T1, T2>`. The `file_prefix`,
    /// `owning_class`, and `argument_types` are supplied separately by
    /// the caller. Only the module path, symbol name, and generic
    /// types are extracted from `symbol`.
    pub fn from(
        file_prefix: Option<PathBuf>,
        owning_class: Option<String>,
        symbol: impl ToString,
        argument_types: Option<Vec<PekoType>>,
    ) -> Self {
        let mut string_parser =
            PekoParser::new(TokenList::from_source(&symbol.to_string(), ""), "");

        let mut modules = Vec::new();
        let mut current_id = String::new();

        // Walk the token stream module-by-module until we hit `<` or run out.
        while !string_parser.tokens.finished() && !string_parser.tokens.current_token().equals("<")
        {
            while !string_parser.tokens.finished()
                && !string_parser.tokens.current_token().equals("<")
                && !string_parser.tokens.current_token().equals("::")
            {
                current_id.push_str(string_parser.tokens.current_token().get_value().as_str());
                string_parser.tokens.increase_index();
            }

            if string_parser.tokens.current_token().equals("::") {
                string_parser.tokens.increase_index();
                modules.push(current_id.clone());
                current_id = String::new();
            }
        }

        // Optional generic-type list.
        let mut generic_types = Vec::new();
        if !string_parser.tokens.finished() && string_parser.tokens.current_token().equals("<") {
            string_parser.tokens.increase_index();

            while !string_parser.tokens.finished()
                && !string_parser.tokens.current_token().equals(">")
            {
                generic_types.push(PekoType::from_tokens(&mut string_parser));

                if string_parser.tokens.current_token().equals(",") {
                    string_parser.tokens.increase_index();
                }
            }
        }

        SymbolName {
            file_prefix,
            module_names: modules,
            symbol_name: current_id,
            owning_class,
            generic_types,
            argument_types,
        }
    }

    /// Render the symbol as a string. When `mangled = true`, the
    /// separators are replaced with LLVM-safe sequences (no `:`, `<`,
    /// `>`, or `,`); when `mangled = false`, the result is the source
    /// form.
    pub fn to_string(&self, mangled: bool) -> String {
        let mut result = String::new();
        let module_separator = if mangled { "_$_$_$" } else { "::" };
        let generic_separator_left = if mangled { "_$$_" } else { "<" };
        let generic_separator_right = if mangled { "$__$" } else { ">" };
        let type_separator = if mangled { "$$__$$" } else { ", " };

        for module in &self.module_names {
            result.push_str(module);
            result.push_str(module_separator);
        }

        if let Some(class_name) = &self.owning_class {
            result.push_str(class_name);
            result.push_str(module_separator);
        }

        result.push_str(&self.symbol_name);

        if !self.generic_types.is_empty() {
            result.push_str(generic_separator_left);

            for generic in &self.generic_types {
                if mangled {
                    result.push_str(&generic.to_mangled_string());
                } else {
                    result.push_str(&generic.to_string());
                }
                result.push_str(type_separator);
            }

            // Trim the trailing separator.
            for _ in 0..type_separator.len() {
                result.pop();
            }

            result.push_str(generic_separator_right);
        }

        if mangled && self.argument_types.is_some() {
            let argument_types = self.argument_types.as_ref().unwrap();
            result.push_str("$$");
            for (idx, argtype) in argument_types.iter().enumerate() {
                result.push_str(&argtype.to_mangled_string());
                if idx < argument_types.len() - 1 {
                    result.push_str(type_separator);
                }
            }
            result.push_str("$$");
        }

        result
    }
}
