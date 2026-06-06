//! # Execution trait declarations
//!
//! Generic trait interfaces shared by every Pekoscript execution backend.
//!
//! `peko_core`'s simulator and the separate `peko_llvm` codegen crate both
//! consume Pekoscript ASTs by walking them through a *typed module
//! environment*: a tree of modules, each containing variables, functions,
//! classes, and nested submodules, plus generic function and class
//! declarations awaiting instantiation. This file defines the trait
//! interface every such environment must satisfy.
//!
//! All traits here are pure interfaces. They declare getter pairs (`get_X`
//! / `get_X_mut`) over an environment's structural data so that the
//! algorithms in [`crate::execution`] can be written once and apply to any
//! backend.
//!
//! ## Sharing model
//!
//! Modules participate in cycles (a submodule's parent points back at it,
//! a class's parent module points at the class's container, etc.) and are
//! shared across many lookups, so every cross-reference uses
//! `Arc<RwLock<ModuleType>>`. Implementations are expected to acquire the
//! read lock for the duration of any lookup.
//!
//! ## Trait parameter conventions
//!
//! The [`ExecutionModule`] trait is F-bounded: its first type parameter
//! (`ModuleType`) is itself constrained to implement `ExecutionModule<...>`
//! with the same companion types. This recursive bound is what lets
//! implementations like `SimulatorModule` plug their own structural types
//! back into the trait.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::asts::data_structures::{PositionedValue, VisibilityData};
use crate::asts::declarations::{ClassAST, FunctionDeclarationAST};
use crate::types;

/// A runtime value with a type.
///
/// The simulator uses this for tracking value types during execution;
/// codegen uses it for tracking value types during type-checking.
pub trait ExecutionValue {
    /// Returns the value's type.
    fn get_type(&self) -> types::PekoType;
}

/// A single argument in a function header, carrying its visibility flags,
/// type, and whether it has a default value.
pub trait ExecutionArgument {
    /// Returns the argument's visibility modifiers.
    fn get_visibility(&self) -> &VisibilityData;

    /// Mutable view of the argument's visibility modifiers.
    fn get_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Returns the argument's declared type.
    fn get_argument_type(&self) -> &types::PekoType;

    /// Mutable view of the argument's declared type.
    fn get_argument_type_mut(&mut self) -> &mut types::PekoType;

    /// Returns `true` if the argument has a default value supplied at its
    /// declaration site.
    fn has_default_value(&self) -> bool;
}

/// A declared function (not a generic function, see
/// [`ExecutionFunctionGeneric`] for those).
pub trait ExecutionFunction<ArgumentType, ModuleType> {
    /// Visibility modifiers from the function header.
    fn get_visibility(&self) -> &VisibilityData;

    /// Mutable view of the function's visibility modifiers.
    fn get_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Declared return type. `void` if no return type was specified at the
    /// declaration site.
    fn get_return_type(&self) -> &types::PekoType;

    /// Mutable view of the declared return type.
    fn get_return_type_mut(&mut self) -> &mut types::PekoType;

    /// Argument map (name -> metadata), preserving source order.
    fn get_arguments(&self) -> &IndexMap<String, ArgumentType>;

    /// Mutable view of the argument map.
    fn get_arguments_mut(&mut self) -> &mut IndexMap<String, ArgumentType>;

    /// Type of variadic arguments (`Args<T>`), if declared.
    fn get_var_args_type(&self) -> Option<&types::PekoType>;

    /// Mutable view of the variadic-args type.
    fn get_var_args_type_mut(&mut self) -> &mut Option<types::PekoType>;

    /// The module the function was declared in. Used to resolve the
    /// function's free identifiers.
    fn get_parent_module(&self) -> Arc<RwLock<ModuleType>>;
}

/// A declared top-level or class-level variable.
pub trait ExecutionVariable<ValueType, ModuleType> {
    /// Visibility modifiers from the variable's declaration.
    fn get_variable_visibility(&self) -> &VisibilityData;

    /// Mutable view of the variable's visibility modifiers.
    fn get_variable_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Declared variable type.
    fn get_variable_type(&self) -> &types::PekoType;

    /// Mutable view of the declared variable type.
    fn get_variable_type_mut(&mut self) -> &mut types::PekoType;

    /// The module the variable was declared in.
    fn get_parent_module(&self) -> Arc<RwLock<ModuleType>>;
}

/// A class attribute declaration (name + type + visibility).
pub trait ExecutionClassAttribute {
    /// Visibility modifiers from the attribute declaration.
    fn get_visibility(&self) -> &VisibilityData;

    /// Mutable view of the attribute's visibility modifiers.
    fn get_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Declared attribute type.
    fn get_attribute_type(&self) -> &types::PekoType;

    /// Mutable view of the attribute type.
    fn get_attribute_type_mut(&mut self) -> &mut types::PekoType;
}

/// Method-overload table for a class.
///
/// Each method name maps to a `Vec` of overloaded definitions; the
/// algorithms in [`crate::execution`] pick the best match for a given
/// argument-type list at call sites.
pub trait ExecutionClassVirtualTable<FunctionType> {
    /// Method-name -> overload-list map, preserving declaration order.
    fn get_methods(&self) -> &IndexMap<String, Vec<FunctionType>>;

    /// Mutable view of the method-name -> overload-list map.
    fn get_methods_mut(&mut self) -> &mut IndexMap<String, Vec<FunctionType>>;
}

/// A declared class, including its type, parent class (if any), attribute map, and
/// method table.
pub trait ExecutionClass<ClassType, ClassVirtualTableType, ClassAttributeType, ModuleType> {
    /// The Pekoscript type this class declares.
    fn get_class_type(&self) -> &types::PekoType;

    /// Mutable view of the declared class type.
    fn get_class_type_mut(&mut self) -> &mut types::PekoType;

    /// The parent class this one derives from, if any.
    fn get_parent_class(&self) -> Option<&ClassType>;

    /// Mutable view of the parent class.
    fn get_parent_class_mut(&mut self) -> &mut Option<Box<ClassType>>;

    /// Attribute-name -> attribute-data map, preserving declaration order.
    fn get_attributes(&self) -> &IndexMap<String, ClassAttributeType>;

    /// Mutable view of the attribute map.
    fn get_attributes_mut(&mut self) -> &mut IndexMap<String, ClassAttributeType>;

    /// The class's own method virtual table (excluding inherited methods).
    fn get_main_virtual_table(&self) -> &ClassVirtualTableType;

    /// Mutable view of the class's main virtual table.
    fn get_main_virtual_table_mut(&mut self) -> &mut ClassVirtualTableType;

    /// The module the class was declared in.
    fn get_parent_module(&self) -> Arc<RwLock<ModuleType>>;
}

/// A generic class declaration awaiting type substitution.
///
/// Implementations stash the original [`ClassAST`] so that
/// [`crate::execution::ExecutionContextAlgorithms::create_generic_class`]
/// can re-process it once concrete type arguments are supplied.
pub trait ExecutionClassGeneric<ModuleType> {
    /// Visibility modifiers from the original declaration.
    fn get_visibility(&self) -> &VisibilityData;

    /// Mutable view of the generic's visibility modifiers.
    fn get_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Names of the generic type parameters in declaration order.
    fn get_generic_typenames(&self) -> &Vec<PositionedValue<String>>;

    /// Mutable view of the generic type-parameter names.
    fn get_generic_typenames_mut(&mut self) -> &mut Vec<PositionedValue<String>>;

    /// The original class AST stashed for later substitution.
    fn get_class(&self) -> &ClassAST;

    /// Mutable view of the stashed class AST.
    fn get_class_mut(&mut self) -> &mut ClassAST;

    /// The module the generic class was declared in.
    fn get_module(&self) -> &Arc<RwLock<ModuleType>>;

    /// Mutable view of the declaring module reference.
    fn get_module_mut(&mut self) -> &mut Arc<RwLock<ModuleType>>;

    /// Path of the source file the generic class was declared in. Used by
    /// downstream diagnostics to report the original declaration site
    /// after instantiation.
    fn get_filename(&self) -> &Path;

    /// Mutable view of the source-file path.
    fn get_filename_mut(&mut self) -> &mut PathBuf;

    /// Same as [`Self::get_module`] but returns an owned `Arc` clone for
    /// callers that need to bind it across scopes.
    fn get_parent_module(&self) -> Arc<RwLock<ModuleType>>;
}

/// A generic function declaration awaiting type substitution.
pub trait ExecutionFunctionGeneric<ModuleType> {
    /// Visibility modifiers from the original declaration.
    fn get_visibility(&self) -> &VisibilityData;

    /// Mutable view of the generic's visibility modifiers.
    fn get_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Names of the generic type parameters in declaration order.
    fn get_generic_typenames(&self) -> &Vec<PositionedValue<String>>;

    /// Mutable view of the generic type-parameter names.
    fn get_generic_typenames_mut(&mut self) -> &mut Vec<PositionedValue<String>>;

    /// The original function AST stashed for later substitution.
    fn get_function(&self) -> &FunctionDeclarationAST;

    /// Mutable view of the stashed function AST.
    fn get_function_mut(&mut self) -> &mut FunctionDeclarationAST;

    /// The module the generic function was declared in.
    fn get_module(&self) -> &Arc<RwLock<ModuleType>>;

    /// Mutable view of the declaring module reference.
    fn get_module_mut(&mut self) -> &mut Arc<RwLock<ModuleType>>;

    /// Same as [`Self::get_module`] but returns an owned `Arc` clone.
    fn get_parent_module(&self) -> Arc<RwLock<ModuleType>>;
}

/// A single Pekoscript module (the central typed namespace).
///
/// Modules hold every kind of declaration the language supports (sub-modules,
/// functions, variables, classes, and the generic forms of functions and
/// classes) along with the source file and module name for diagnostics.
///
/// The trait is F-bounded on `ModuleType`: implementations plug their own
/// concrete module type back in as the first parameter so that
/// cross-references between modules remain typed.
pub trait ExecutionModule<
    ModuleType: ExecutionModule<
        ModuleType,
        ValueType,
        VariableType,
        FunctionType,
        FunctionGenericType,
        ArgumentType,
        ClassType,
        ClassGenericType,
        ClassVirtualTableType,
        ClassAttributeType,
    >,
    ValueType: ExecutionValue,
    VariableType: ExecutionVariable<ValueType, ModuleType>,
    FunctionType: ExecutionFunction<ArgumentType, ModuleType>,
    FunctionGenericType: ExecutionFunctionGeneric<ModuleType>,
    ArgumentType: ExecutionArgument,
    ClassType: ExecutionClass<ClassType, ClassVirtualTableType, ClassAttributeType, ModuleType>,
    ClassGenericType: ExecutionClassGeneric<ModuleType>,
    ClassVirtualTableType: ExecutionClassVirtualTable<FunctionType>,
    ClassAttributeType: ExecutionClassAttribute,
>
{
    /// Visibility modifiers attached to the module's declaration.
    fn get_visibility(&self) -> &VisibilityData;

    /// Mutable view of the module's visibility modifiers.
    fn get_visibility_mut(&mut self) -> &mut VisibilityData;

    /// Source-file path the module was declared in.
    fn get_file(&self) -> &Path;

    /// Mutable view of the module's source-file path.
    fn get_file_mut(&mut self) -> &mut PathBuf;

    /// Parent module in the module tree, if any. Root modules have `None`.
    fn get_parent(&self) -> Option<&Arc<RwLock<ModuleType>>>;

    /// Mutable view of the parent module reference.
    fn get_parent_mut(&mut self) -> &mut Option<Arc<RwLock<ModuleType>>>;

    /// The module's own name.
    fn get_name(&self) -> &str;

    /// Mutable view of the module's name.
    fn get_name_mut(&mut self) -> &mut String;

    /// Sub-module map (name -> module), preserving declaration order.
    fn get_modules(&self) -> &IndexMap<String, Arc<RwLock<ModuleType>>>;

    /// Mutable view of the sub-module map.
    fn get_modules_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<ModuleType>>>;

    /// Function map (name -> overload list), preserving declaration order.
    fn get_functions(&self) -> &IndexMap<String, Vec<FunctionType>>;

    /// Mutable view of the function map.
    fn get_functions_mut(&mut self) -> &mut IndexMap<String, Vec<FunctionType>>;

    /// Variable map (name -> variable data), preserving declaration order.
    fn get_variables(&self) -> &IndexMap<String, VariableType>;

    /// Mutable view of the variable map.
    fn get_variables_mut(&mut self) -> &mut IndexMap<String, VariableType>;

    /// Class map (name -> class data), preserving declaration order.
    fn get_classes(&self) -> &IndexMap<String, ClassType>;

    /// Mutable view of the class map.
    fn get_classes_mut(&mut self) -> &mut IndexMap<String, ClassType>;

    /// Generic-class map (name -> generic-class data), preserving
    /// declaration order. Keyed on the bare name without type parameters.
    fn get_class_generics(&self) -> &IndexMap<String, ClassGenericType>;

    /// Mutable view of the generic-class map.
    fn get_class_generics_mut(&mut self) -> &mut IndexMap<String, ClassGenericType>;

    /// Generic-function map (name -> generic-function data), preserving
    /// declaration order. Keyed on the bare name without type parameters.
    fn get_function_generics(&self) -> &IndexMap<String, FunctionGenericType>;

    /// Mutable view of the generic-function map.
    fn get_function_generics_mut(&mut self) -> &mut IndexMap<String, FunctionGenericType>;
}
