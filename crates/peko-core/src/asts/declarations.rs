//! Declaration AST nodes.
//!
//! A declaration introduces a new named entity into the surrounding scope:
//! a variable ([`NewVariableAST`]), a function ([`FunctionDeclarationAST`]),
//! a closure ([`ClosureAST`]), a class ([`ClassAST`]), or a module
//! ([`ModuleCreationAST`]).

use derive_new::new;
use indexmap::IndexMap;

use super::PekoAST;
use super::data_structures::{
    ClassAttributeData, ClassMethod, DeclarationArgumentData, DocInfo, PositionData,
    PositionedValue, Spanned, VisibilityData,
};
use crate::types;

/// A variable declaration: `const x: int = 0`, `name := ...`, etc.
///
/// `constant` records whether the variable was declared with the `const`
/// keyword (independent from the `[constant]` visibility modifier in
/// `visibility`). `variable_type` is `None` when the type was elided and
/// must be inferred from `variable_value`.
#[derive(Clone, new)]
pub struct NewVariableAST {
    pub start: PositionData,
    pub end: PositionData,
    pub visibility: VisibilityData,
    pub docinfo: Option<DocInfo>,
    pub constant: bool,
    pub name: PositionedValue<String>,
    pub variable_type: Option<types::PekoType>,
    pub variable_value: Box<PekoAST>,
}

impl Spanned for NewVariableAST {
    fn get_start(&self) -> &PositionData {
        // Start from the variable's name rather than the declaration head,
        // matching the original PekoAST::get_start behavior. This puts the
        // span on the identifier itself, which reads better in diagnostics
        // like "variable `x` is unused".
        &self.name.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A function declaration: `fn name<G>(args) => Type { body }`.
///
/// `function_body` is `None` for `external` functions whose body is
/// resolved at link time. `varargs_type` and `varargs_name` are different
/// from when the function declares a `[variadic]` argument; the `varargs_name`
/// always carries the in-source name even when the type is absent.
///
/// `class_order` is a tracking field used when this declaration is a class
/// method, recording the method's order within the class body.
#[derive(Clone, new)]
pub struct FunctionDeclarationAST {
    pub start: PositionData,
    pub end: PositionData,
    pub visibility: VisibilityData,
    pub docinfo: Option<DocInfo>,
    pub function_name: PositionedValue<String>,
    pub generic_types: Vec<PositionedValue<String>>,
    pub arguments: IndexMap<PositionedValue<String>, DeclarationArgumentData>,
    pub return_type: Option<types::PekoType>,
    pub function_body: Option<PositionedValue<Vec<PekoAST>>>,
    pub varargs_type: Option<types::PekoType>,
    pub varargs_name: PositionedValue<String>,
    pub class_order: usize,
}

impl Spanned for FunctionDeclarationAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A closure declaration: `closure[captures](args) => Type { body }`.
///
/// `captures` lists the outer-scope names this closure captures, in source
/// order.
#[derive(Clone, new)]
pub struct ClosureAST {
    pub start: PositionData,
    pub end: PositionData,
    pub arguments: IndexMap<PositionedValue<String>, DeclarationArgumentData>,
    pub captures: Vec<PositionedValue<String>>,
    pub return_type: Option<types::PekoType>,
    pub closure_body: PositionedValue<Vec<PekoAST>>,
}

impl Spanned for ClosureAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A class declaration: `class Name<G> from Parent { attrs; methods }`.
///
/// `derives_from` carries the list of parent types this class inherits from
/// (Pekoscript doesn't currently support multiple inheritance, added for future support).
#[derive(Clone, new)]
pub struct ClassAST {
    pub start: PositionData,
    pub end: PositionData,
    pub visibility: VisibilityData,
    pub docinfo: Option<DocInfo>,
    pub class_name: PositionedValue<String>,
    pub derives_from: Vec<types::PekoType>,
    pub attributes: IndexMap<PositionedValue<String>, ClassAttributeData>,
    pub methods: Vec<ClassMethod>,
    pub generics: Vec<PositionedValue<String>>,
}

impl Spanned for ClassAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A module declaration: `module name { ... }`.
#[derive(Clone, new)]
pub struct ModuleCreationAST {
    pub start: PositionData,
    pub end: PositionData,
    pub visibility: VisibilityData,
    pub docinfo: Option<DocInfo>,
    pub module_name: PositionedValue<String>,
    pub module_body: PositionedValue<Vec<PekoAST>>,
}

impl Spanned for ModuleCreationAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}
