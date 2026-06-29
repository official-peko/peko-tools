//! # Simulator data structures
//!
//! Concrete implementations of the [`execution`](crate::execution) trait
//! interfaces used by the simulator backend, plus a parallel set of
//! lightweight "scope" types that surface declaration info to IDE clients.
//!
//! There are two layers here:
//!
//! 1. **Execution types** (`SimulatorArg`, `SimulatorFunction`,
//!    `SimulatorVariable`, `SimulatorClass`, `SimulatorModule`, and the
//!    two generic variants). These plug into the
//!    [`ExecutionContextAlgorithms`](crate::execution::ExecutionContextAlgorithms)
//!    machinery and carry the bulk of type-checking state.
//!
//! 2. **Scope types** (`Scope`, `ScopeSymbol`, `ScopeVariable`, etc.).
//!    These are simpler value types returned from the simulator for
//!    consumption by tooling (i.e. language servers, documentation
//!    generators, or anything that needs a denormalized view of what's
//!    declared where).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use derive_new::new;
use indexmap::IndexMap;

use crate::asts::data_structures::{DocInfo, PositionData, PositionedValue, VisibilityData};
use crate::asts::declarations::{ClassAST, FunctionDeclarationAST};
use crate::execution::data_structures::{
    ExecutionArgument, ExecutionClass, ExecutionClassAttribute, ExecutionClassGeneric,
    ExecutionClassVirtualTable, ExecutionFunction, ExecutionFunctionGeneric, ExecutionModule,
    ExecutionVariable, TraitDefinition,
};
use crate::types::{self, PekoType};

use super::value::SimulatorValue;

// ----- Declaration data structures -----------------------------------------

/// Type and position info for a single argument in a function header.
#[derive(Clone, Debug, new)]
pub struct SimulatorArg {
    /// Position the argument appears at in source.
    pub position: PositionData,

    /// Argument visibility modifiers (`[const]`, etc).
    pub visibility: VisibilityData,

    /// Declared argument type.
    pub argument_type: types::PekoType,

    /// `true` if the argument has a default value supplied at the
    /// declaration site.
    pub default_value: bool,
}

impl ExecutionArgument for SimulatorArg {
    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }

    fn get_argument_type(&self) -> &types::PekoType {
        &self.argument_type
    }

    fn get_argument_type_mut(&mut self) -> &mut types::PekoType {
        &mut self.argument_type
    }

    fn has_default_value(&self) -> bool {
        self.default_value
    }
}

/// Type and position info for a declared function.
#[derive(Clone, new)]
pub struct SimulatorFunction {
    /// Position the function declaration appears at in source.
    pub position: PositionData,

    /// Function visibility modifiers.
    pub visibility: VisibilityData,

    /// Parsed doc-info comment block immediately preceding the function.
    pub docinfo: Option<DocInfo>,

    /// Declared return type. `void` if no return type was specified.
    pub return_type: types::PekoType,

    /// Argument map, preserving source declaration order.
    pub arguments: IndexMap<String, SimulatorArg>,

    /// Variadic argument type (`Args<T>`), if any.
    pub var_args_type: Option<types::PekoType>,

    /// Back-reference to the module the function was declared in.
    pub parent: Arc<RwLock<SimulatorModule>>,
}

impl ExecutionFunction<SimulatorArg, SimulatorModule> for SimulatorFunction {
    fn get_parent_module(&self) -> Arc<RwLock<SimulatorModule>> {
        self.parent.clone()
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }

    fn get_return_type(&self) -> &types::PekoType {
        &self.return_type
    }

    fn get_return_type_mut(&mut self) -> &mut types::PekoType {
        &mut self.return_type
    }

    fn get_arguments(&self) -> &IndexMap<String, SimulatorArg> {
        &self.arguments
    }

    fn get_arguments_mut(&mut self) -> &mut IndexMap<String, SimulatorArg> {
        &mut self.arguments
    }

    fn get_var_args_type(&self) -> Option<&types::PekoType> {
        self.var_args_type.as_ref()
    }

    fn get_var_args_type_mut(&mut self) -> &mut Option<types::PekoType> {
        &mut self.var_args_type
    }
}

/// Type and position info for a declared variable.
#[derive(Clone, new)]
pub struct SimulatorVariable {
    /// Position the variable declaration appears at in source.
    pub position: PositionData,

    /// Variable visibility modifiers.
    pub variable_visibility: VisibilityData,

    /// Declared variable type.
    pub variable_type: types::PekoType,

    /// Simulator value tracked alongside the variable. For top-level
    /// variables this is the initializer's simulated value.
    pub variable_value: SimulatorValue,

    /// Back-reference to the module the variable was declared in.
    pub parent: Arc<RwLock<SimulatorModule>>,
}

impl ExecutionVariable<SimulatorValue, SimulatorModule> for SimulatorVariable {
    fn get_parent_module(&self) -> Arc<RwLock<SimulatorModule>> {
        self.parent.clone()
    }

    fn get_variable_type(&self) -> &types::PekoType {
        &self.variable_type
    }

    fn get_variable_type_mut(&mut self) -> &mut types::PekoType {
        &mut self.variable_type
    }

    fn get_variable_visibility(&self) -> &VisibilityData {
        &self.variable_visibility
    }

    fn get_variable_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.variable_visibility
    }
}

/// Type and position info for a class attribute declaration.
#[derive(Clone, Debug, new)]
pub struct SimulatorClassAttribute {
    /// Position the attribute appears at in source.
    pub position: PositionData,

    /// Attribute visibility modifiers.
    pub visibility: VisibilityData,

    /// Parsed doc-info comment block immediately preceding the attribute.
    pub docinfo: Option<DocInfo>,

    /// Declared attribute type.
    pub attribute_type: types::PekoType,
}

impl ExecutionClassAttribute for SimulatorClassAttribute {
    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }

    fn get_attribute_type(&self) -> &types::PekoType {
        &self.attribute_type
    }

    fn get_attribute_type_mut(&mut self) -> &mut types::PekoType {
        &mut self.attribute_type
    }
}

/// Virtual table for a class: a flat map from method name to its overload
/// set.
#[derive(Clone, new)]
pub struct SimulatorClassVirtualTable {
    /// Method-name -> overload-list. Overload selection happens at the
    /// call-site via [`crate::execution::ExecutionContextAlgorithms`].
    /// Each overload is held behind a lock shared by all modules that
    /// reference the method.
    pub methods: IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>>,
}

impl ExecutionClassVirtualTable<SimulatorFunction> for SimulatorClassVirtualTable {
    fn get_methods(&self) -> &IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>> {
        &self.methods
    }

    fn get_methods_mut(&mut self) -> &mut IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>> {
        &mut self.methods
    }
}

/// Type and position info for a declared class.
#[derive(Clone, new)]
pub struct SimulatorClass {
    /// Position the class declaration appears at in source.
    pub position: PositionData,

    /// The class's own Pekoscript type.
    pub class_type: types::PekoType,

    /// Parent class (via `from`), if any. Boxed because [`SimulatorClass`]
    /// would otherwise be infinitely recursive.
    pub parent_class: Option<Box<SimulatorClass>>,

    /// Attribute map, preserving source declaration order.
    pub attributes: IndexMap<String, SimulatorClassAttribute>,

    /// The class's own virtual table (excluding inherited methods).
    pub main_virtual_table: SimulatorClassVirtualTable,

    /// Names of the traits this class implements (via the `impl` clause).
    /// A safe `value as Trait` cast is allowed when the trait is listed here.
    pub implements: Vec<String>,

    /// Back-reference to the module the class was declared in.
    pub parent: Arc<RwLock<SimulatorModule>>,
}

impl
    ExecutionClass<
        SimulatorClass,
        SimulatorClassVirtualTable,
        SimulatorClassAttribute,
        SimulatorModule,
    > for SimulatorClass
{
    fn get_parent_module(&self) -> Arc<RwLock<SimulatorModule>> {
        self.parent.clone()
    }

    fn get_class_type(&self) -> &types::PekoType {
        &self.class_type
    }

    fn get_class_type_mut(&mut self) -> &mut types::PekoType {
        &mut self.class_type
    }

    fn get_parent_class(&self) -> Option<&SimulatorClass> {
        self.parent_class.as_deref()
    }

    fn get_parent_class_mut(&mut self) -> &mut Option<Box<SimulatorClass>> {
        &mut self.parent_class
    }

    fn get_main_virtual_table(&self) -> &SimulatorClassVirtualTable {
        &self.main_virtual_table
    }

    fn get_main_virtual_table_mut(&mut self) -> &mut SimulatorClassVirtualTable {
        &mut self.main_virtual_table
    }

    fn get_attributes(&self) -> &IndexMap<String, SimulatorClassAttribute> {
        &self.attributes
    }

    fn get_attributes_mut(&mut self) -> &mut IndexMap<String, SimulatorClassAttribute> {
        &mut self.attributes
    }
}

/// Info on a generic class declaration so it can be re-processed when
/// invoked with concrete type arguments.
#[derive(Clone, new)]
pub struct SimulatorClassGeneric {
    /// Visibility modifiers from the original declaration.
    pub visibility: VisibilityData,

    /// Names of the generic type parameters in declaration order.
    pub generic_typenames: Vec<PositionedValue<String>>,

    /// The original class AST, stashed for re-processing under
    /// substitution.
    pub class: ClassAST,

    /// The module the generic was declared in.
    pub module: Arc<RwLock<SimulatorModule>>,

    /// The scope the generic was declared in. Used when re-processing so
    /// the generic's body sees the same surrounding names that were
    /// visible at declaration time.
    pub scope: Arc<RwLock<Scope>>,

    /// Source-file path the generic was declared in.
    pub filename: PathBuf,
}

impl ExecutionClassGeneric<SimulatorModule> for SimulatorClassGeneric {
    fn get_parent_module(&self) -> Arc<RwLock<SimulatorModule>> {
        self.module.clone()
    }

    fn get_class(&self) -> &ClassAST {
        &self.class
    }

    fn get_class_mut(&mut self) -> &mut ClassAST {
        &mut self.class
    }

    fn get_filename(&self) -> &std::path::Path {
        self.filename.as_path()
    }

    fn get_filename_mut(&mut self) -> &mut PathBuf {
        &mut self.filename
    }

    fn get_generic_typenames(&self) -> &Vec<PositionedValue<String>> {
        &self.generic_typenames
    }

    fn get_generic_typenames_mut(&mut self) -> &mut Vec<PositionedValue<String>> {
        &mut self.generic_typenames
    }

    fn get_module(&self) -> &Arc<RwLock<SimulatorModule>> {
        &self.module
    }

    fn get_module_mut(&mut self) -> &mut Arc<RwLock<SimulatorModule>> {
        &mut self.module
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

/// Info on a generic function declaration so it can be re-processed when
/// invoked with concrete type arguments.
#[derive(Clone, new)]
pub struct SimulatorFunctionGeneric {
    /// Visibility modifiers from the original declaration.
    pub visibility: VisibilityData,

    /// Names of the generic type parameters in declaration order.
    pub generic_typenames: Vec<PositionedValue<String>>,

    /// The original function AST, stashed for re-processing under
    /// substitution.
    pub function: FunctionDeclarationAST,

    /// The module the generic was declared in.
    pub module: Arc<RwLock<SimulatorModule>>,
}

impl ExecutionFunctionGeneric<SimulatorModule> for SimulatorFunctionGeneric {
    fn get_parent_module(&self) -> Arc<RwLock<SimulatorModule>> {
        self.module.clone()
    }

    fn get_function(&self) -> &FunctionDeclarationAST {
        &self.function
    }

    fn get_function_mut(&mut self) -> &mut FunctionDeclarationAST {
        &mut self.function
    }

    fn get_generic_typenames(&self) -> &Vec<PositionedValue<String>> {
        &self.generic_typenames
    }

    fn get_generic_typenames_mut(&mut self) -> &mut Vec<PositionedValue<String>> {
        &mut self.generic_typenames
    }

    fn get_module(&self) -> &Arc<RwLock<SimulatorModule>> {
        &self.module
    }

    fn get_module_mut(&mut self) -> &mut Arc<RwLock<SimulatorModule>> {
        &mut self.module
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

/// A single Pekoscript module which holds every declaration that lives at
/// this module's scope, plus links to parent / submodules / importers.
#[derive(Clone, new)]
pub struct SimulatorModule {
    /// Position the module declaration appears at in source.
    pub position: PositionData,

    /// Module visibility modifiers.
    pub visibility: VisibilityData,

    /// Source-file path the module was loaded from.
    pub file: PathBuf,

    /// Parsed module-level doc-info (`//!`) for this module.
    pub docinfo: Option<DocInfo>,

    /// Parent module in the tree, if any. Root (top-level) modules have
    /// `None` here.
    pub parent: Option<Arc<RwLock<SimulatorModule>>>,

    /// Module name. Combined with the parent chain to form fully-qualified
    /// symbol names.
    pub name: String,

    /// Sub-modules.
    pub modules: IndexMap<String, Arc<RwLock<SimulatorModule>>>,

    /// Function overload sets, indexed by function name. Each overload is
    /// held behind a lock shared by all modules that reference it.
    pub functions: IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>>,

    /// Top-level variables. Each is held behind a lock shared by all
    /// modules that reference it.
    pub variables: IndexMap<String, Arc<RwLock<SimulatorVariable>>>,

    /// Classes. Each is held behind a lock shared by all modules that
    /// reference it.
    pub classes: IndexMap<String, Arc<RwLock<SimulatorClass>>>,

    /// Generic class declarations awaiting type substitution. Each is held
    /// behind a lock shared by all modules that reference it.
    pub class_generics: IndexMap<String, Arc<RwLock<SimulatorClassGeneric>>>,

    /// Generic function declarations awaiting type substitution. Each is
    /// held behind a lock shared by all modules that reference it.
    pub function_generics: IndexMap<String, Arc<RwLock<SimulatorFunctionGeneric>>>,

    /// The module's top-level scope, used by tooling for symbol lookup.
    pub scope: Arc<RwLock<Scope>>,

    /// Other modules that imported this one. Used for cycle detection
    /// during the import resolution pass.
    pub imported_by: Vec<Arc<RwLock<SimulatorModule>>>,

    /// Enums, keyed by name, holding their variant names in declaration
    /// order. Enums are immutable once declared, so they are stored by value.
    pub enums: IndexMap<String, Vec<String>>,

    /// Traits, keyed by name. Traits are immutable once declared, so they are
    /// stored by value.
    pub traits: IndexMap<String, TraitDefinition>,
}

impl SimulatorModule {
    /// Walks `submodule`'s parent chain to the top-level root and returns
    /// that root.
    pub fn get_top_parent(
        mut submodule: Arc<RwLock<SimulatorModule>>,
    ) -> Arc<RwLock<SimulatorModule>> {
        loop {
            if submodule.read().unwrap().parent.is_none() {
                return submodule;
            }

            let parent = submodule.read().unwrap().parent.as_ref().unwrap().clone();
            submodule = parent;
        }
    }

    /// Returns `true` if this module was imported by `other` (transitively
    /// through `other`'s top-level ancestor).
    pub fn is_imported_by(&self, other: Arc<RwLock<SimulatorModule>>) -> bool {
        // Walk to other's top-level ancestor so we compare imports at the
        // module-tree root rather than at the leaf.
        let mut other_toplevel = other.clone();
        loop {
            if other_toplevel.read().unwrap().parent.is_none() {
                break;
            }
            let parent = other_toplevel
                .read()
                .unwrap()
                .parent
                .as_ref()
                .unwrap()
                .clone();
            other_toplevel = parent;
        }

        self.imported_by
            .iter()
            .any(|module| module.read().unwrap().name == other.read().unwrap().name)
    }
}

impl
    ExecutionModule<
        SimulatorModule,
        SimulatorValue,
        SimulatorVariable,
        SimulatorFunction,
        SimulatorFunctionGeneric,
        SimulatorArg,
        SimulatorClass,
        SimulatorClassGeneric,
        SimulatorClassVirtualTable,
        SimulatorClassAttribute,
    > for SimulatorModule
{
    fn get_file(&self) -> &std::path::Path {
        self.file.as_path()
    }

    fn get_file_mut(&mut self) -> &mut PathBuf {
        &mut self.file
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }

    fn get_modules(&self) -> &IndexMap<String, Arc<RwLock<SimulatorModule>>> {
        &self.modules
    }

    fn get_variables(&self) -> &IndexMap<String, Arc<RwLock<SimulatorVariable>>> {
        &self.variables
    }

    fn get_functions(&self) -> &IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>> {
        &self.functions
    }

    fn get_function_generics(&self) -> &IndexMap<String, Arc<RwLock<SimulatorFunctionGeneric>>> {
        &self.function_generics
    }

    fn get_classes(&self) -> &IndexMap<String, Arc<RwLock<SimulatorClass>>> {
        &self.classes
    }

    fn get_class_generics(&self) -> &IndexMap<String, Arc<RwLock<SimulatorClassGeneric>>> {
        &self.class_generics
    }

    fn get_parent(&self) -> Option<&Arc<RwLock<SimulatorModule>>> {
        self.parent.as_ref()
    }

    fn get_name(&self) -> &str {
        self.name.as_str()
    }

    fn get_modules_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<SimulatorModule>>> {
        &mut self.modules
    }

    fn get_variables_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<SimulatorVariable>>> {
        &mut self.variables
    }

    fn get_functions_mut(&mut self) -> &mut IndexMap<String, Vec<Arc<RwLock<SimulatorFunction>>>> {
        &mut self.functions
    }

    fn get_function_generics_mut(
        &mut self,
    ) -> &mut IndexMap<String, Arc<RwLock<SimulatorFunctionGeneric>>> {
        &mut self.function_generics
    }

    fn get_classes_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<SimulatorClass>>> {
        &mut self.classes
    }

    fn get_enums(&self) -> &IndexMap<String, Vec<String>> {
        &self.enums
    }

    fn get_enums_mut(&mut self) -> &mut IndexMap<String, Vec<String>> {
        &mut self.enums
    }

    fn get_traits(&self) -> &IndexMap<String, TraitDefinition> {
        &self.traits
    }

    fn get_traits_mut(&mut self) -> &mut IndexMap<String, TraitDefinition> {
        &mut self.traits
    }

    fn get_class_generics_mut(
        &mut self,
    ) -> &mut IndexMap<String, Arc<RwLock<SimulatorClassGeneric>>> {
        &mut self.class_generics
    }

    fn get_parent_mut(&mut self) -> &mut Option<Arc<RwLock<SimulatorModule>>> {
        &mut self.parent
    }

    fn get_name_mut(&mut self) -> &mut String {
        &mut self.name
    }
}

// ----- Scope data structures -----------------------------------------------

/// A variable as surfaced to tooling, including name, type, definition span, and
/// `[state]` flag (for class attributes participating in change tracking).
#[derive(Clone, new)]
pub struct ScopeVariable {
    /// Doc-info comment block preceding the declaration, if any.
    pub docinfo: Option<DocInfo>,

    /// Variable name.
    pub name: String,

    /// Declared type.
    pub value_type: types::PekoType,

    /// Start of the declaration's span.
    pub definition_start: PositionData,

    /// End of the declaration's span.
    pub definition_end: PositionData,

    /// `true` if this represents a class attribute rather than a free
    /// variable.
    pub attribute: bool,
}

/// A function as surfaced to tooling.
#[derive(Clone, new)]
pub struct ScopeFunction {
    /// Doc-info comment block preceding the declaration, if any.
    pub docinfo: Option<DocInfo>,

    /// Function name.
    pub name: String,

    /// Declared return type.
    pub return_type: types::PekoType,

    /// Start of the declaration's span.
    pub definition_start: PositionData,

    /// End of the declaration's span.
    pub definition_end: PositionData,

    /// `true` if this represents a generic-function declaration awaiting
    /// instantiation rather than a concrete function.
    pub generic: bool,

    /// Argument map preserving source declaration order. Each value pairs
    /// the argument's visibility flags with its declared type.
    pub arguments: IndexMap<String, (VisibilityData, types::PekoType)>,

    /// Generic type-parameter names, in declaration order. Empty for
    /// non-generic functions.
    pub generic_type_names: Vec<String>,
}

/// A class as surfaced to tooling.
#[derive(Clone, new)]
pub struct ScopeClass {
    /// Doc-info comment block preceding the declaration, if any.
    pub docinfo: Option<DocInfo>,

    /// Class name.
    pub name: String,

    /// Start of the declaration's span.
    pub definition_start: PositionData,

    /// End of the declaration's span.
    pub definition_end: PositionData,

    /// `true` if this represents a generic-class declaration awaiting
    /// instantiation.
    pub generic: bool,

    /// Generic type-parameter names, in declaration order. Empty for
    /// non-generic classes.
    pub generic_types: Vec<String>,

    /// The first constructor's argument list, surfaced separately so
    /// tooling can show constructor signatures without descending into
    /// the full method table.
    pub first_constructor_arguments: IndexMap<String, PekoType>,
}

/// A module as surfaced to tooling.
#[derive(Clone, new)]
pub struct ScopeModule {
    /// Doc-info comment block preceding the declaration, if any.
    pub docinfo: Option<DocInfo>,

    /// Module name.
    pub name: String,

    /// Start of the declaration's span.
    pub definition_start: PositionData,

    /// End of the declaration's span.
    pub definition_end: PositionData,
}

/// Tagged-union view of any kind of scoped symbol.
///
/// Each variant pairs the symbol-specific info with the visibility flags
/// from the declaration's modifier list, so tooling can filter on
/// visibility without re-inspecting the underlying declaration.
#[derive(Clone)]
pub enum ScopeSymbol {
    /// A variable or class attribute.
    Variable(ScopeVariable, VisibilityData),

    /// A function (generic or non-generic).
    Function(ScopeFunction, VisibilityData),

    /// A class (generic or non-generic).
    Class(ScopeClass, VisibilityData),

    /// A submodule.
    Module(ScopeModule, VisibilityData),
}

impl ScopeSymbol {
    /// Returns a human-readable name for this symbol's kind.
    ///
    /// Distinguishes attributes from variables and generics from
    /// non-generics: returns one of `"attribute"`, `"variable"`,
    /// `"function"`, `"function-generic"`, `"class"`, `"class-generic"`,
    /// or `"module"`.
    #[must_use]
    pub fn get_kind(&self) -> &'static str {
        match self {
            Self::Variable(symbol, _) => {
                if symbol.attribute {
                    "attribute"
                } else {
                    "variable"
                }
            }
            Self::Function(symbol, _) => {
                if symbol.generic {
                    "function-generic"
                } else {
                    "function"
                }
            }
            Self::Class(symbol, _) => {
                if symbol.generic {
                    "class-generic"
                } else {
                    "class"
                }
            }
            Self::Module(_, _) => "module",
        }
    }

    /// Returns this symbol's function view if it's a function, else `None`.
    #[must_use]
    pub fn as_function(&self) -> Option<ScopeFunction> {
        match self {
            Self::Function(symbol, _) => Some(symbol.clone()),
            _ => None,
        }
    }

    /// Returns this symbol's class view if it's a class, else `None`.
    #[must_use]
    pub fn as_class(&self) -> Option<ScopeClass> {
        match self {
            Self::Class(symbol, _) => Some(symbol.clone()),
            _ => None,
        }
    }

    /// Returns this symbol's variable view if it's a variable, else `None`.
    #[must_use]
    pub fn as_variable(&self) -> Option<ScopeVariable> {
        match self {
            Self::Variable(symbol, _) => Some(symbol.clone()),
            _ => None,
        }
    }

    /// Returns this symbol's module view if it's a module, else `None`.
    #[must_use]
    pub fn as_module(&self) -> Option<ScopeModule> {
        match self {
            Self::Module(symbol, _) => Some(symbol.clone()),
            _ => None,
        }
    }

    /// Returns the start position of this symbol's declaration span.
    #[must_use]
    pub fn get_start(&self) -> PositionData {
        match self {
            Self::Variable(scoped, _) => scoped.definition_start.clone(),
            Self::Function(scoped, _) => scoped.definition_start.clone(),
            Self::Class(scoped, _) => scoped.definition_start.clone(),
            Self::Module(scoped, _) => scoped.definition_start.clone(),
        }
    }

    /// Returns the end position of this symbol's declaration span.
    #[must_use]
    pub fn get_end(&self) -> PositionData {
        match self {
            Self::Variable(scoped, _) => scoped.definition_end.clone(),
            Self::Function(scoped, _) => scoped.definition_end.clone(),
            Self::Class(scoped, _) => scoped.definition_end.clone(),
            Self::Module(scoped, _) => scoped.definition_end.clone(),
        }
    }

    /// Returns the visibility flags from this symbol's declaration.
    #[must_use]
    pub fn get_visibility(&self) -> VisibilityData {
        match self {
            Self::Variable(_, vis) => vis.clone(),
            Self::Function(_, vis) => vis.clone(),
            Self::Class(_, vis) => vis.clone(),
            Self::Module(_, vis) => vis.clone(),
        }
    }

    /// Returns the doc-info comment block preceding this symbol's
    /// declaration, if any.
    #[must_use]
    pub fn get_doc_info(&self) -> Option<DocInfo> {
        match self {
            Self::Variable(var, _) => var.docinfo.clone(),
            Self::Function(func, _) => func.docinfo.clone(),
            Self::Class(class, _) => class.docinfo.clone(),
            Self::Module(module, _) => module.docinfo.clone(),
        }
    }

    /// Returns the name of this symbol.
    #[must_use]
    pub fn get_name(&self) -> String {
        match self {
            Self::Variable(scoped, _) => scoped.name.clone(),
            Self::Function(scoped, _) => scoped.name.clone(),
            Self::Class(scoped, _) => scoped.name.clone(),
            Self::Module(scoped, _) => scoped.name.clone(),
        }
    }
}

/// A lexical scope, including a module's top-level scope, a block scope, or any
/// other named region of code.
///
/// Scopes form a tree (each scope has child sub-scopes) and hold the
/// symbols visible at that level. Tooling walks the tree to answer
/// "what's in scope at position X" queries.
#[derive(Clone)]
pub struct Scope {
    /// `true` if this is a module's top-level scope rather than a nested
    /// block.
    pub top_level: bool,

    /// `true` if this scope is embedded in a larger expression (e.g. an
    /// `if`-as-expression branch) rather than being a standalone block.
    pub embedded: bool,

    /// Visibility flags from the surrounding declaration, if any.
    pub scope_visibility: VisibilityData,

    /// Start position of the scope's source span.
    pub start: PositionData,

    /// End position of the scope's source span.
    pub end: PositionData,

    /// Child sub-scopes nested inside this one.
    pub scopes: Vec<Arc<RwLock<Scope>>>,

    /// Symbols visible at this scope level (excluding inherited ones).
    pub symbols: IndexMap<String, ScopeSymbol>,

    /// Names of symbols declared in this scope that were referenced at least
    /// once. Drives the unused-symbol warning (24.1).
    pub used_symbols: std::collections::HashSet<String>,

    /// Human-readable scope name (e.g. the containing function or module
    /// name).
    pub scope_name: String,
}

impl Scope {
    /// Constructs a new scope with the given metadata; child scope and
    /// symbol collections start empty.
    #[must_use]
    pub fn new(
        top_level: bool,
        embedded: bool,
        scope_visibility: VisibilityData,
        start: PositionData,
        end: PositionData,
        scope_name: String,
    ) -> Scope {
        Scope {
            top_level,
            embedded,
            scope_visibility,
            start,
            end,
            scopes: Vec::new(),
            symbols: IndexMap::new(),
            used_symbols: std::collections::HashSet::new(),
            scope_name,
        }
    }

    /// Returns `true` if `position` falls within this scope's span and the
    /// two refer to the same source file.
    ///
    /// File paths are canonicalized before comparison so that
    /// logically-equal paths (e.g. with `./` components or symlinks)
    /// compare equal. If canonicalization fails on either side (typically
    /// because the file no longer exists or refers to in-memory source)
    /// the raw paths are compared instead as a best-effort fallback.
    #[must_use]
    pub fn holds_position(&self, position: PositionData) -> bool {
        let self_file = self
            .start
            .file
            .canonicalize()
            .unwrap_or_else(|_| self.start.file.clone());
        let other_file = position
            .file
            .canonicalize()
            .unwrap_or_else(|_| position.file.clone());

        self_file == other_file
            && self.start.positioned_before_inclusive(position.clone())
            && position.positioned_before_inclusive(self.end.clone())
    }
}

/// Bookkeeping for a tracked object reference inside a method body.
///
/// Used by the simulator to remember the type and span of `this`-bound
/// values and other tracked references during method-call resolution.
#[derive(Clone, new)]
pub struct DefinedObject {
    /// `true` if this entry tracks the `this` reference rather than a
    /// named local.
    pub object_is_this: bool,

    /// Tracked type of the object.
    pub object_type: types::PekoType,

    /// End-of-scope position for the binding.
    pub ending_position: PositionData,
}

/// Trace of a single function call recorded during simulation.
///
/// The simulator builds a tree of these as it walks the source so that
/// tooling can answer "what function is this position inside" and "what
/// argument am I currently typing" questions.
#[derive(Clone)]
pub struct FunctionCall {
    /// Start position of the call site.
    pub start: PositionData,

    /// End position of the call site.
    pub end: PositionData,

    /// Doc-info comment block preceding the called function, if any.
    pub docinfo: Option<DocInfo>,

    /// Function name (fully-qualified by the simulator).
    pub name: String,

    /// Span of each positional argument at the call site.
    pub argument_positions: Vec<(PositionData, PositionData)>,

    /// Signature of the function as resolved at this call site (argument
    /// names paired with their types).
    pub signature_arguments: IndexMap<String, PekoType>,

    /// Resolved return type of this call.
    pub return_type: PekoType,

    /// Nested calls made from within the arguments of this call.
    pub subcalls: Vec<Arc<RwLock<FunctionCall>>>,
}

impl FunctionCall {
    /// Constructs a new call trace. The `subcalls` list starts empty and
    /// is filled in as nested calls are simulated.
    #[must_use]
    pub fn new(
        start: PositionData,
        end: PositionData,
        docinfo: Option<DocInfo>,
        name: String,
        argument_positions: Vec<(PositionData, PositionData)>,
        signature_arguments: IndexMap<String, PekoType>,
        return_type: PekoType,
    ) -> FunctionCall {
        FunctionCall {
            start,
            end,
            docinfo,
            name,
            argument_positions,
            signature_arguments,
            return_type,
            subcalls: Vec::new(),
        }
    }

    /// Returns `true` if `position` falls within this call's span.
    ///
    /// Doesn't compare file paths, callers are expected to have already
    /// narrowed by file.
    #[must_use]
    pub fn holds_position(&self, position: PositionData) -> bool {
        self.start.positioned_before_inclusive(position.clone())
            && position.positioned_before_inclusive(self.end.clone())
    }

    /// Returns the index of the argument whose span contains `position`,
    /// or `self.argument_positions.len()` if no argument matches (so
    /// callers can treat the result as "past the end").
    #[must_use]
    pub fn argument_index_at_position(&self, position: PositionData) -> usize {
        for (idx, (start, end)) in self.argument_positions.iter().enumerate() {
            if start.positioned_before_inclusive(position.clone())
                && position.positioned_before_inclusive(end.clone())
            {
                return idx;
            }
        }

        self.argument_positions.len()
    }

    /// Returns the function's signature rendered as a string, in the form
    /// `name(arg: T, arg: T) => Ret`.
    #[must_use]
    pub fn get_signature(&self) -> String {
        let mut sig = self.name.clone();

        sig.push('(');

        // Build the argument list using `.join(", ")` instead of manually
        // popping the trailing separator.
        let parts: Vec<String> = self
            .signature_arguments
            .iter()
            .map(|(argument_name, argument_type)| {
                if argument_name.is_empty() {
                    argument_type.to_string()
                } else {
                    format!("{argument_name}: {argument_type}")
                }
            })
            .collect();
        sig.push_str(&parts.join(", "));

        sig.push_str(") => ");
        sig.push_str(&self.return_type.to_string());

        sig
    }
}

/// Whether a type is a managed pointer `Pointer<T>`: the address space 1
/// pointer wrapper, as opposed to a raw `T*` (which uses pointer depth).
pub fn is_managed_pointer(ty: &PekoType) -> bool {
    // `string` is a managed char buffer (address space 1), morally a
    // `Pointer<char>`: it is indexable and participates in the managed-pointer
    // coercion/indexing paths exactly like `Pointer<T>`. `cstr` is the raw
    // (address space 0) counterpart and is deliberately NOT managed here.
    (ty.name() == "Pointer" || ty.name() == "string")
        && ty.array_depth == 0
        && ty.reference_depth == 0
}

/// The type produced by loading through or dereferencing a pointer.
///
/// For a managed pointer `Pointer<T>` the result is `T`. For `string` (a
/// managed `char` buffer) the result is `char`. For a raw pointer `T*` the
/// result is `T` with one less pointer depth. This is the single place that
/// knows how the managed and raw pointer forms each "decrease" by one level.
pub fn pointee_type(ty: &PekoType) -> PekoType {
    if ty.name() == "string" && ty.array_depth == 0 && ty.reference_depth == 0 {
        return PekoType::simple_type("char");
    }
    if is_managed_pointer(ty) {
        ty.generics()
            .first()
            .cloned()
            .unwrap_or_else(|| PekoType::simple_type("void"))
    } else {
        let mut inner = ty.clone();
        inner.decrease_pointer_depth();
        inner
    }
}
