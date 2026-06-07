//! # Simulator context
//!
//! The central state for a simulation run: target info, diagnostics,
//! module-resolution state, scope stack, and assorted bookkeeping for
//! IDE tooling (function-call traces, defined-object spans, global
//! symbol tables).
//!
//! This file holds three things:
//!
//! 1. [`PekoSimulatorContext`] itself and its inherent methods (module
//!    import, scope queries, snapshot/restore around generic
//!    instantiation, and a small set of helpers).
//! 2. [`SimulatorContextSnapshot`] — a named bundle of the seven fields
//!    that need to be saved and restored around generic instantiation,
//!    replacing the prior 7-tuple.
//! 3. The [`ExecutionContextAlgorithms`] trait impl that plugs the
//!    simulator into the backend-agnostic execution algorithms in
//!    [`crate::execution`].

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use itertools::Itertools;

use crate::ExternalModuleInfo;
use crate::asts::data_structures::{PositionData, PositionedValue, UnpackItem, VisibilityData};
use crate::diagnostics;
use crate::execution::{ExecutionContextAlgorithms, ExecutionModuleContext};
use crate::simulator::PekoValueSimulator;
use crate::simulator::data_structures::FunctionCall;
use crate::target;
use crate::types::{self, PekoType};

use super::data_structures::{
    DefinedObject, Scope, ScopeFunction, ScopeModule, ScopeSymbol, ScopeVariable, SimulatorArg,
    SimulatorClass, SimulatorClassAttribute, SimulatorClassGeneric, SimulatorClassVirtualTable,
    SimulatorFunction, SimulatorFunctionGeneric, SimulatorModule, SimulatorVariable,
};
use super::value::SimulatorValue;

/// Snapshot of the seven simulator-context fields that get reset around
/// generic-class and generic-function instantiation.
///
/// Generic instantiation reuses the same [`PekoSimulatorContext`] to
/// re-simulate an AST under a type substitution, but the surrounding
/// scope and module state belong to the call site, not the generic's
/// declaration. [`PekoSimulatorContext::snapshot_context`] saves these
/// fields before instantiation and
/// [`PekoSimulatorContext::reset_context`] restores them after.
#[derive(Clone)]
pub struct SimulatorContextSnapshot {
    /// Saved value of [`PekoSimulatorContext::previous_scoped_variables`].
    pub previous_scoped_variables: Vec<HashMap<String, SimulatorVariable>>,

    /// Saved value of [`PekoSimulatorContext::scoped_variables`].
    pub scoped_variables: HashMap<String, SimulatorVariable>,

    /// Saved value of [`PekoSimulatorContext::local_scope`].
    pub local_scope: bool,

    /// Saved value of [`PekoSimulatorContext::attributes_to_set`].
    pub attributes_to_set: Vec<String>,

    /// Saved value of [`PekoSimulatorContext::module_context`].
    pub module_context: ExecutionModuleContext<SimulatorModule>,

    /// Saved value of [`PekoSimulatorContext::current_scope`].
    pub current_scope: Option<Arc<RwLock<Scope>>>,

    /// Saved value of [`PekoSimulatorContext::current_this`].
    pub current_this: Option<SimulatorVariable>,
}

/// Mutable state tracked across an entire simulation run.
///
/// One instance is constructed per top-level simulation and threaded
/// through every AST node's `simulate(&mut self, &mut context)` call. The
/// fields fall into a handful of categories:
///
/// * **Diagnostics / target** — what we're compiling for and what's gone
///   wrong so far.
/// * **Module / scope state** — which module and scope is currently
///   active, plus the lookup tables for symbols visible to tooling.
/// * **Type-inference state** — running expected types, return types,
///   and generic substitutions in scope.
/// * **Object-access state** — `this` binding, attributes pending
///   initialization, and "previous expression was `this`" used to
///   resolve method calls.
/// * **Trace bookkeeping** — function-call traces and defined-object
///   spans surfaced to IDE clients.
#[derive(Clone)]
pub struct PekoSimulatorContext {
    /// When `true`, errors raised inside imported modules are collapsed
    /// to a single error at the import site in the primary file rather
    /// than reported in their original location.
    pub minified_import_errors: bool,

    /// Diagnostics accumulated during simulation.
    pub diagnostics: diagnostics::DiagnosticList,

    /// Compilation target descriptor.
    pub target: target::PekoTarget,

    /// Native files that should be linked alongside the compiled output.
    pub files_to_link: Vec<std::path::PathBuf>,

    /// `true` if the program is being compiled as a Windows GUI app.
    pub windowsgui: bool,

    /// All modules discoverable in the Peko packages directory, keyed by
    /// module name.
    pub external_modules: HashMap<String, ExternalModuleInfo>,

    /// Module-resolution state: active module stack and top-level module
    /// registry.
    pub module_context: ExecutionModuleContext<SimulatorModule>,

    /// Stack of scoped-variable maps, one per enclosing block. Pushed
    /// when entering a block, popped when leaving.
    pub previous_scoped_variables: Vec<HashMap<String, SimulatorVariable>>,

    /// The current block's scoped variables, keyed by name.
    pub scoped_variables: HashMap<String, SimulatorVariable>,

    /// `true` if the current block is inside a function body (as opposed
    /// to module-level code).
    pub local_scope: bool,

    /// `true` if downstream AST nodes should produce references rather
    /// than loaded values. Used for lvalue contexts (assignment targets,
    /// `&` references, etc.).
    pub return_references: bool,

    /// In-scope generic-type substitutions (e.g. `T` → `String`).
    pub generic_types: HashMap<String, types::PekoType>,

    /// Declared return type of the function we're currently inside.
    /// Used for type-inference when the type-checker has to consult what
    /// the surrounding function expects.
    pub current_return_type: Option<types::PekoType>,

    /// Expected type(s) at the current expression position — typically
    /// set by variable declarations to inform inference of the RHS.
    pub current_expected_type_options: Option<Vec<types::PekoType>>,

    /// `this` binding when simulating a method body.
    pub current_this: Option<SimulatorVariable>,

    /// `true` if the most recently simulated identifier was `this`. Used
    /// to disambiguate method-call resolution from free-function calls.
    pub previous_was_this: bool,

    /// Class attributes that still need to be assigned in the current
    /// constructor body.
    pub attributes_to_set: Vec<String>,

    /// `true` if the current block is inside a loop. Affects break/continue
    /// validity.
    pub current_loop_finish: bool,

    /// The active scope for the current module.
    pub current_scope: Option<Arc<RwLock<Scope>>>,

    /// Globally-accessible symbols (top-level symbols across all
    /// modules). Surfaced to IDE clients for completion.
    pub global_symbols: IndexMap<String, ScopeSymbol>,

    /// Spans of objects defined or accessed throughout the codebase.
    /// Surfaced to IDE clients for hover info and reference lookup.
    pub defined_objects: Vec<DefinedObject>,

    /// Trace tree of every function call simulated. Surfaced to IDE
    /// clients for signature help and "what function am I inside"
    /// queries.
    pub function_calls: Vec<Arc<RwLock<FunctionCall>>>,

    /// The function call currently being simulated, if any.
    pub current_function_call: Option<Arc<RwLock<FunctionCall>>>,

    /// The folder that `project::` imports resolve against and that
    /// canonical module ids are rooted at. The import logic reassigns
    /// this while a registry package loads and restores it afterward.
    pub root_folder: std::path::PathBuf,
}

impl PekoSimulatorContext {
    /// Constructs a fresh simulator context with `main` and `extern`
    /// modules wired in.
    ///
    /// `current_file` is the path to the source file being compiled; it
    /// becomes the file path on the synthesized scopes' position
    /// markers. `file_end_position` is the EOF position used to bound
    /// the `main` scope's span.
    #[must_use]
    pub fn new(
        target: target::PekoTarget,
        current_file: std::path::PathBuf,
        file_end_position: PositionData,
        root_folder: std::path::PathBuf,
    ) -> PekoSimulatorContext {
        // Synthesize the main and extern scopes/modules with a position
        // marker that carries the source file path. Numeric fields use
        // PositionData::default() for everything except the file.
        let main_scope_start = PositionData {
            file: current_file.clone(),
            ..PositionData::default()
        };

        let main_scope = Arc::new(RwLock::new(Scope::new(
            true,
            false,
            VisibilityData::open_visibility(),
            main_scope_start.clone(),
            file_end_position,
            String::from("main"),
        )));

        let main_module = Arc::new(RwLock::new(SimulatorModule::new(
            PositionData::default(),
            VisibilityData::open_visibility(),
            current_file.clone(),
            None,
            None,
            String::from("main"),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            Arc::clone(&main_scope),
            Vec::new(),
        )));

        let extern_scope_pos = PositionData {
            file: current_file.clone(),
            ..PositionData::default()
        };

        let extern_scope = Arc::new(RwLock::new(Scope::new(
            true,
            false,
            VisibilityData::open_visibility(),
            extern_scope_pos.clone(),
            extern_scope_pos,
            String::from("extern"),
        )));

        let extern_module = Arc::new(RwLock::new(SimulatorModule::new(
            PositionData::default(),
            VisibilityData::open_visibility(),
            current_file,
            None,
            None,
            String::from("extern"),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            IndexMap::new(),
            Arc::clone(&extern_scope),
            Vec::new(),
        )));

        PekoSimulatorContext {
            minified_import_errors: false,
            diagnostics: diagnostics::DiagnosticList::new(),
            external_modules: HashMap::new(),
            target,
            files_to_link: Vec::new(),
            module_context: ExecutionModuleContext::new(main_module, extern_module),
            previous_scoped_variables: Vec::new(),
            scoped_variables: HashMap::new(),
            local_scope: false,
            generic_types: HashMap::new(),
            return_references: false,
            current_return_type: None,
            current_expected_type_options: None,
            current_this: None,
            previous_was_this: false,
            attributes_to_set: Vec::new(),
            current_loop_finish: false,
            current_scope: Some(main_scope),
            global_symbols: IndexMap::new(),
            defined_objects: Vec::new(),
            windowsgui: false,
            function_calls: Vec::new(),
            current_function_call: None,
            root_folder,
        }
    }

    /// Returns `true` if the current execution stack ultimately resolves
    /// to the `main` module (after stepping past any active inter-module
    /// access frames).
    pub fn in_main(&mut self) -> bool {
        let post_stack = self.module_context.step_back();
        let is_main = SimulatorModule::get_top_parent(self.module_context.current_module())
            .read()
            .unwrap()
            .name
            == "main";
        self.module_context.step_forward(post_stack);
        is_main
    }

    /// Imports declarations from `from` into `to`, recreating per-symbol
    /// scope entries as needed.
    ///
    /// The `unpacked_symbols` list controls which symbols are imported:
    ///
    /// * If empty, every symbol is imported.
    /// * Otherwise, only symbols named in the list are imported.
    /// * [`UnpackItem::All`] forces "import everything" even if the
    ///   list also contains specific symbols.
    /// * [`UnpackItem::ModuleSymbols`] entries recurse: the submodule
    ///   itself is replaced with a *copy* containing only the listed
    ///   inner symbols.
    ///
    /// Diagnostics are emitted for any explicitly-requested symbol that
    /// can't be found in `from`.
    pub fn import_modules(
        &mut self,
        from: Arc<RwLock<SimulatorModule>>,
        to: Arc<RwLock<SimulatorModule>>,
        unpacked_symbols: Vec<UnpackItem>,
    ) {
        let mut current_symbols = Vec::new();
        let mut current_module_unpacks = HashMap::new();
        let mut unpack_all = false;
        for unpacked_symbol in &unpacked_symbols {
            match unpacked_symbol {
                UnpackItem::Symbol(symbol) => {
                    current_symbols.push(symbol.clone());
                }
                UnpackItem::ModuleSymbols(module_unpack) => {
                    current_module_unpacks.insert(
                        module_unpack.module_name.clone(),
                        module_unpack.unpacked_items.clone(),
                    );
                }
                UnpackItem::All => {
                    unpack_all = true;
                }
            };
        }

        // Import global variables.
        for (variable_name, variable) in from.read().unwrap().variables.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(variable_name.clone()))
            {
                continue;
            }
            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name
                                == &&PositionedValue::create_no_position(variable_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .variables
                .insert(variable_name.clone(), variable);

            match from
                .read()
                .unwrap()
                .scope
                .read()
                .unwrap()
                .symbols
                .get(&variable_name)
            {
                Some(variable_symbol) => to
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(variable_name.clone(), variable_symbol.clone()),
                None => continue,
            };
        }

        // Import global functions.
        for (function_name, function_options) in from.read().unwrap().functions.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(function_name.clone()))
            {
                continue;
            }

            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name
                                == &&PositionedValue::create_no_position(function_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .functions
                .insert(function_name.clone(), function_options);

            match from
                .read()
                .unwrap()
                .scope
                .read()
                .unwrap()
                .symbols
                .get(&function_name)
            {
                Some(function_symbol) => to
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(function_name.clone(), function_symbol.clone()),
                None => continue,
            };
        }

        // Import generic-function declarations.
        for (function_name, function) in from.read().unwrap().function_generics.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(function_name.clone()))
            {
                continue;
            }

            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name
                                == &&PositionedValue::create_no_position(function_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .function_generics
                .insert(function_name.clone(), function);

            match from
                .read()
                .unwrap()
                .scope
                .read()
                .unwrap()
                .symbols
                .get(&function_name)
            {
                Some(function_symbol) => to
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(function_name.clone(), function_symbol.clone()),
                None => continue,
            };
        }

        // Import classes.
        for (class_name, class) in from.read().unwrap().classes.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(class_name.clone()))
            {
                continue;
            }
            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name == &&PositionedValue::create_no_position(class_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .classes
                .insert(class_name.clone(), class);
            match from
                .read()
                .unwrap()
                .scope
                .read()
                .unwrap()
                .symbols
                .get(&class_name)
            {
                Some(class_symbol) => to
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(class_name.clone(), class_symbol.clone()),
                None => continue,
            };
        }

        // Import generic-class declarations.
        for (class_name, class) in from.read().unwrap().class_generics.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(class_name.clone()))
            {
                continue;
            }
            if !current_symbols.is_empty() {
                current_symbols.remove(
                    current_symbols
                        .iter()
                        .find_position(|symbol_name| {
                            symbol_name == &&PositionedValue::create_no_position(class_name.clone())
                        })
                        .unwrap()
                        .0,
                );
            }

            to.write()
                .unwrap()
                .class_generics
                .insert(class_name.clone(), class);
            match from
                .read()
                .unwrap()
                .scope
                .read()
                .unwrap()
                .symbols
                .get(&class_name)
            {
                Some(class_symbol) => to
                    .write()
                    .unwrap()
                    .scope
                    .write()
                    .unwrap()
                    .symbols
                    .insert(class_name.clone(), class_symbol.clone()),
                None => continue,
            };
        }

        // Lastly, import each submodule. This branch is more involved
        // than the others because submodules can be either imported
        // wholesale or unpacked further (only some inner symbols).
        for (module_name, submodule) in from.read().unwrap().modules.clone() {
            let import_module = unpacked_symbols.is_empty()
                || unpack_all
                || current_symbols
                    .contains(&PositionedValue::create_no_position(module_name.clone()));

            let unpack_module = current_module_unpacks
                .contains_key(&PositionedValue::create_no_position(module_name.clone()));

            if !unpack_module && !import_module {
                continue;
            }

            let new_module = if import_module {
                if !current_symbols.is_empty() {
                    current_symbols.remove(
                        current_symbols
                            .iter()
                            .find_position(|symbol_name| {
                                symbol_name
                                    == &&PositionedValue::create_no_position(module_name.clone())
                            })
                            .unwrap()
                            .0,
                    );
                }

                Arc::new(RwLock::new(SimulatorModule::new(
                    submodule.read().unwrap().position.clone(),
                    submodule.read().unwrap().visibility.clone(),
                    submodule.read().unwrap().file.clone(),
                    submodule.read().unwrap().docinfo.clone(),
                    submodule.read().unwrap().parent.clone(),
                    module_name.clone(),
                    IndexMap::new(),
                    IndexMap::new(),
                    IndexMap::new(),
                    IndexMap::new(),
                    submodule.read().unwrap().class_generics.clone(),
                    submodule.read().unwrap().function_generics.clone(),
                    submodule.read().unwrap().scope.clone(),
                    Vec::new(),
                )))
            } else {
                current_module_unpacks
                    .remove(&PositionedValue::create_no_position(module_name.clone()));
                Arc::clone(&to)
            };

            self.import_modules(
                submodule,
                Arc::clone(&new_module),
                if unpack_module {
                    let unpack_key = PositionedValue::create_no_position(module_name.clone());
                    let items = &current_module_unpacks[&unpack_key];
                    if items.len() == 1 && matches!(items[0], UnpackItem::All) {
                        Vec::new()
                    } else {
                        items.clone()
                    }
                } else {
                    Vec::new()
                },
            );

            if import_module {
                to.write()
                    .unwrap()
                    .modules
                    .insert(module_name.clone(), new_module);
                match from
                    .read()
                    .unwrap()
                    .scope
                    .read()
                    .unwrap()
                    .symbols
                    .get(&module_name)
                {
                    Some(module_symbol) => to
                        .write()
                        .unwrap()
                        .scope
                        .write()
                        .unwrap()
                        .symbols
                        .insert(module_name.clone(), module_symbol.clone()),
                    None => continue,
                };
            }
        }

        // Report diagnostics for any explicitly-requested symbol that
        // wasn't found in `from`.
        let current_file = self.get_current_file();
        let from_module_name = from.read().unwrap().name.clone();

        for unfound_symbol in current_symbols {
            self.diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    unfound_symbol.start.clone(),
                    unfound_symbol.end.clone(),
                    format!(
                    "cannot find symbol `{}` in module `{}`. It is not exported or does not exist",
                    unfound_symbol.value, from_module_name,
                ),
                    diagnostics::DiagnosticType::Error,
                    current_file.clone(),
                ));
        }

        for (unfound_module, _) in current_module_unpacks {
            self.diagnostics
                .report_diagnostic(diagnostics::PekoDiagnostic::new(
                    unfound_module.start.clone(),
                    unfound_module.end.clone(),
                    format!(
                        "cannot find submodule `{}` in module `{}`. Check the module name and that it is exported",
                        unfound_module.value, from_module_name,
                    ),
                    diagnostics::DiagnosticType::Error,
                    current_file.clone(),
                ));
        }
    }

    /// Imports `imported_module` into the currently-active module.
    ///
    /// Records the reciprocal `imported_by` link on both the imported
    /// module and `extern`, then either copies the module wholesale (no
    /// unpack) or runs [`Self::import_modules`] to copy a filtered subset.
    pub fn import_module(
        &mut self,
        imported_module: Arc<RwLock<SimulatorModule>>,
        unpacked_symbols: Vec<UnpackItem>,
    ) {
        self.module_context
            .extern_module
            .write()
            .unwrap()
            .imported_by
            .push(self.module_context.current_module().clone());

        imported_module
            .write()
            .unwrap()
            .imported_by
            .push(self.module_context.current_module().clone());

        if unpacked_symbols.is_empty() {
            self.module_context
                .current_module()
                .write()
                .unwrap()
                .scope
                .write()
                .unwrap()
                .scopes
                .push(imported_module.read().unwrap().scope.clone());

            let imported_module_info = imported_module.read().unwrap();

            self.module_context
                .current_module()
                .write()
                .unwrap()
                .scope
                .write()
                .unwrap()
                .symbols
                .insert(
                    imported_module_info.name.clone(),
                    ScopeSymbol::Module(
                        ScopeModule::new(
                            imported_module_info.docinfo.clone(),
                            imported_module_info.name.clone(),
                            imported_module_info.position.clone(),
                            imported_module_info.position.clone(),
                        ),
                        VisibilityData::open_visibility(),
                    ),
                );
            return;
        }

        self.import_modules(
            Arc::clone(&imported_module),
            self.module_context.current_module().clone(),
            unpacked_symbols.clone(),
        );
    }

    // ----- Scope symbol searching ------------------------------------------

    /// Returns the innermost function-call trace containing `position`,
    /// or `None` if no function call covers it.
    ///
    /// Walks down the function-call tree picking sub-calls that also
    /// contain the position; stops at the deepest one. Used by IDE
    /// tooling to answer "what function call am I inside" queries.
    pub fn get_function_call_from_position_in_main(
        &mut self,
        position: PositionData,
    ) -> Option<FunctionCall> {
        for function_call in &self.function_calls {
            if function_call
                .read()
                .unwrap()
                .holds_position(position.clone())
            {
                let mut lowest_call = function_call.clone();
                let mut is_lowest = false;

                while !lowest_call.read().unwrap().subcalls.is_empty() && !is_lowest {
                    let mut found_lower = false;

                    let subcalls = lowest_call.read().unwrap().subcalls.clone();
                    for subcall in &subcalls {
                        if subcall.read().unwrap().holds_position(position.clone()) {
                            lowest_call = subcall.clone();
                            found_lower = true;
                            break;
                        }
                    }

                    is_lowest = !found_lower;
                }

                return Some(lowest_call.read().unwrap().clone());
            }
        }

        None
    }

    /// Returns symbols visible from a position immediately following an
    /// object reference (e.g. autocomplete after `someObject.`).
    ///
    /// Looks up the [`DefinedObject`] whose `ending_position` matches
    /// `position`, finds the object's class, and returns the class's
    /// methods and attributes as a list of [`ScopeSymbol`]s — filtered
    /// by visibility (private members are hidden unless the object is
    /// `this`).
    pub fn get_symbols_from_object_at_position(
        &mut self,
        position: PositionData,
    ) -> Vec<ScopeSymbol> {
        // Find the object at the given position.
        let mut object: Option<DefinedObject> = None;

        for defined_object in &self.defined_objects {
            if defined_object.ending_position.equals(position.clone()) {
                object = Some(defined_object.clone());
                break;
            }
        }

        let Some(object) = object else {
            return Vec::new();
        };

        // Ensure the object actually has a class type.
        let mut object_symbols = Vec::new();
        let Some(object_class) = self.get_class_by_type(&object.object_type) else {
            return Vec::new();
        };

        // Surface every accessible method.
        for (method_name, method_functions) in &object_class.main_virtual_table.methods {
            // Only take the first overload — completion just needs the
            // name and a representative signature.
            let first_function = method_functions[0].clone();

            // Skip private methods unless we're inside the class (i.e.
            // accessing via `this`).
            if !first_function.visibility.private || object.object_is_this {
                let mut converted_arguments = IndexMap::new();
                for (argument_name, argument) in &first_function.arguments {
                    converted_arguments.insert(
                        argument_name.clone(),
                        (argument.visibility.clone(), argument.argument_type.clone()),
                    );
                }

                object_symbols.push(ScopeSymbol::Function(
                    ScopeFunction::new(
                        first_function.docinfo.clone(),
                        method_name.clone(),
                        first_function.return_type.clone(),
                        first_function.position.clone(),
                        first_function.position.clone(),
                        false,
                        converted_arguments,
                        Vec::new(),
                    ),
                    first_function.visibility.clone(),
                ));
            }
        }

        // Surface every accessible attribute, with the same private/this
        // filtering as methods.
        for (attribute_name, attribute) in &object_class.attributes {
            if !attribute.visibility.private || object.object_is_this {
                object_symbols.push(ScopeSymbol::Variable(
                    ScopeVariable::new(
                        attribute.docinfo.clone(),
                        attribute_name.clone(),
                        attribute.attribute_type.clone(),
                        attribute.position.clone(),
                        attribute.position.clone(),
                        true,
                    ),
                    attribute.visibility.clone(),
                ));
            }
        }

        object_symbols
    }

    /// Walks down the scope tree from `lowest_scope` searching for a
    /// scope named `scope_name` that contains `position`.
    ///
    /// Returns the deepest matching scope reached. If no scope with the
    /// requested name is found, returns the deepest scope visited.
    pub fn find_scope_at_position_named(
        &mut self,
        mut lowest_scope: Arc<RwLock<Scope>>,
        position: PositionData,
        scope_name: String,
    ) -> Arc<RwLock<Scope>> {
        let mut scope_changed: bool = true;

        while lowest_scope.as_ref().read().unwrap().scope_name != scope_name
            && scope_changed
            && !lowest_scope.as_ref().read().unwrap().scopes.is_empty()
        {
            scope_changed = false;

            for scope in &lowest_scope.clone().as_ref().read().unwrap().scopes {
                if ((scope.as_ref().read().unwrap().scope_name == scope_name
                    && scope
                        .as_ref()
                        .read()
                        .unwrap()
                        .end
                        .positioned_before(position.clone()))
                    || scope
                        .as_ref()
                        .read()
                        .unwrap()
                        .holds_position(position.clone()))
                    && !scope.as_ref().read().unwrap().scope_visibility.private
                {
                    lowest_scope = scope.clone();
                    scope_changed = true;
                }
            }
        }

        lowest_scope
    }

    /// Returns the symbols visible after a `module::submodule::...`
    /// access at `position`.
    ///
    /// `module_full_name` is the colon-separated chain (e.g.
    /// `"fs::path"`). Looks up the chain in the active scope tree first,
    /// then falls back to top-level modules. Symbols defined later in
    /// the same file than `position` are filtered out.
    pub fn get_available_symbols_from_module(
        &mut self,
        module_full_name: String,
        position: PositionData,
    ) -> Vec<ScopeSymbol> {
        let mut available_symbols = Vec::new();

        // Split the dotted module path into segments.
        let mut module_path: Vec<&str> = module_full_name.split("::").collect();
        let first_name = module_path.remove(0);

        // First, try resolving the first segment as a scope reachable
        // from main's scope tree.
        let top_level_scope = self.module_context.top_level_modules["main"]
            .as_ref()
            .read()
            .unwrap()
            .scope
            .clone();
        let mut current_scope = self.find_scope_at_position_named(
            top_level_scope,
            position.clone(),
            first_name.to_owned(),
        );

        // If found, continue descending into submodules' scopes.
        if current_scope.as_ref().read().unwrap().scope_name == first_name {
            let mut found = true;

            while !module_path.is_empty() {
                let current_module_to_find = module_path.remove(0);

                found = false;
                for scope in &current_scope.clone().as_ref().read().unwrap().scopes {
                    if scope.as_ref().read().unwrap().scope_name == current_module_to_find {
                        current_scope = scope.clone();
                        found = true;
                        break;
                    }
                }

                if !found {
                    break;
                }
            }

            // Collect symbols visible at the resolved scope.
            if found {
                for (_, symbol) in current_scope.as_ref().read().unwrap().symbols.iter() {
                    if symbol.get_start().file != position.file
                        || symbol.get_start().positioned_before(position.clone())
                    {
                        available_symbols.push(symbol.clone());
                    }
                }
            }

            return available_symbols;
        }

        // Otherwise, the first segment must be a top-level module
        // (e.g. an imported package).
        if self
            .module_context
            .top_level_modules
            .contains_key(first_name)
        {
            let mut module_referenced = self.module_context.top_level_modules[first_name].clone();

            // Descend into submodules.
            let mut found = true;
            while !module_path.is_empty() {
                let current_module_to_find = module_path.remove(0);

                found = module_referenced
                    .as_ref()
                    .read()
                    .unwrap()
                    .modules
                    .contains_key(current_module_to_find);
                if module_referenced
                    .clone()
                    .as_ref()
                    .read()
                    .unwrap()
                    .modules
                    .contains_key(current_module_to_find)
                {
                    module_referenced = module_referenced.clone().as_ref().read().unwrap().modules
                        [current_module_to_find]
                        .clone();
                }

                if !found {
                    break;
                }
            }

            // If found, surface all symbols from the resolved module's
            // scope.
            if found {
                for (_, symbol) in &module_referenced
                    .as_ref()
                    .read()
                    .unwrap()
                    .scope
                    .as_ref()
                    .read()
                    .unwrap()
                    .symbols
                {
                    available_symbols.push(symbol.clone());
                }
            }
        }

        available_symbols
    }

    /// Returns every symbol visible at `position` — globals, plus every
    /// scope-tree symbol on the path from main down to the innermost
    /// scope containing the position.
    ///
    /// Symbols defined later in the same file than `position` are
    /// filtered out so that completion can't suggest forward references
    /// to local bindings.
    pub fn get_available_symbols_from_position(
        &mut self,
        position: PositionData,
    ) -> Vec<ScopeSymbol> {
        let mut available_symbols = Vec::new();

        // Globals are always in scope.
        for (_, global_symbol) in &self.global_symbols {
            available_symbols.push(global_symbol.clone());
        }

        // Walk down the scope tree, surfacing each level's symbols as
        // we go.
        let main_module = self.module_context.top_level_modules["main"]
            .read()
            .unwrap();

        let mut lowest_scope = main_module.scope.clone();
        let mut scope_changed: bool = false;

        while !lowest_scope.clone().read().unwrap().scopes.is_empty() {
            // Add this scope's symbols (modulo the same-file
            // forward-reference filter).
            for (_, symbol) in lowest_scope.clone().read().unwrap().symbols.iter() {
                if symbol.get_start().file != position.file
                    || symbol.get_start().positioned_before(position.clone())
                {
                    available_symbols.push(symbol.clone());
                }
            }

            scope_changed = false;

            // Descend into the child scope containing `position`.
            for scope in &lowest_scope.clone().read().unwrap().scopes {
                if scope
                    .as_ref()
                    .read()
                    .unwrap()
                    .holds_position(position.clone())
                {
                    lowest_scope = scope.clone();
                    scope_changed = true;
                }
            }

            if !scope_changed {
                break;
            }
        }

        // If we did descend, also surface the final scope's symbols.
        if scope_changed {
            for (_, symbol) in lowest_scope.clone().read().unwrap().symbols.iter() {
                if symbol.get_start().file != position.file
                    || symbol.get_start().positioned_before(position.clone())
                {
                    available_symbols.push(symbol.clone());
                }
            }
        }

        available_symbols
    }

    /// Returns a synthetic error-typed [`SimulatorValue`].
    ///
    /// Used as the simulator's "I couldn't make sense of this expression"
    /// fallback so that downstream code can keep walking the AST without
    /// hitting type-check cascades.
    #[must_use]
    pub fn create_error_value(&self) -> SimulatorValue {
        SimulatorValue::Value(PekoType::error_type())
    }

    /// Saves the seven context fields that get clobbered by generic
    /// instantiation, returning a snapshot that can later be passed to
    /// [`Self::reset_context`].
    ///
    /// `attributes_to_set` is *moved* into the snapshot (replaced with an
    /// empty `Vec` on `self`) rather than cloned, since the caller will
    /// repopulate it during instantiation.
    pub fn snapshot_context(&mut self) -> SimulatorContextSnapshot {
        let snapshot = SimulatorContextSnapshot {
            previous_scoped_variables: self.previous_scoped_variables.clone(),
            scoped_variables: self.scoped_variables.clone(),
            local_scope: self.local_scope,
            attributes_to_set: self.attributes_to_set.clone(),
            module_context: self.module_context.clone(),
            current_scope: self.current_scope.clone(),
            current_this: self.current_this.clone(),
        };

        self.attributes_to_set = Vec::new();
        snapshot
    }

    /// Restores a context snapshot previously produced by
    /// [`Self::snapshot_context`].
    pub fn reset_context(&mut self, snapshot: SimulatorContextSnapshot) {
        self.current_scope = snapshot.current_scope;
        self.previous_scoped_variables.clear();
        self.previous_scoped_variables
            .extend(snapshot.previous_scoped_variables);
        self.scoped_variables.clear();
        self.scoped_variables.extend(snapshot.scoped_variables);
        self.local_scope = snapshot.local_scope;
        self.module_context = snapshot.module_context;
        self.attributes_to_set = snapshot.attributes_to_set;
        self.current_this = snapshot.current_this;
    }

    /// Type-checks a cast of `value` to `expected_type`.
    ///
    /// Returns `Some(value_of_expected_type)` if the cast is valid (per
    /// [`Self::types_similar`]), `None` otherwise.
    pub fn box_value_to_type(
        &mut self,
        expected_type: &PekoType,
        value: &SimulatorValue,
    ) -> Option<SimulatorValue> {
        if !self.types_similar(&value.get_type(), expected_type) {
            return None;
        }

        Some(SimulatorValue::Value(expected_type.clone()))
    }

    /// Simulator-side counterpart of the codegen's GC allocation
    /// primitive.
    ///
    /// The simulator doesn't actually allocate anything — it just
    /// produces a [`SimulatorValue::Value`] of the requested type. The
    /// method exists for one-to-one parity with `peko_llvm`'s
    /// codegen-context API so that AST simulators and code generators
    /// can share structurally similar bodies.
    fn gc_allocate_type(&mut self, type1: &types::PekoType) -> SimulatorValue {
        SimulatorValue::Value(type1.clone())
    }

    /// Diagnostic-recovery overload picker.
    ///
    /// Distinct from the trait's [`choose_function`](ExecutionContextAlgorithms::choose_function):
    ///
    /// * **Always returns `Some`** when given a non-empty `function_choices`
    ///   list. The trait version returns `None` when no candidate is a
    ///   real match; this one falls back to the highest-scoring
    ///   *partial* match — or, failing that, the first choice.
    /// * **Does not reset the score on a dissimilar-type argument.**
    ///   Wrong-type arguments simply don't contribute points; right-type
    ///   ones do.
    ///
    /// Callers use this *after* `choose_function` returns `None`, to
    /// populate function-call-info for the language server even when the
    /// call is ill-typed.
    pub fn choose_most_similar_function(
        &mut self,
        function_choices: Vec<SimulatorFunction>,
        argument_types: Vec<PekoType>,
        provided_arguments: Option<HashMap<String, PekoType>>,
        class_function: bool,
    ) -> Option<SimulatorFunction> {
        if function_choices.is_empty() {
            return None;
        } else if function_choices.len() == 1 {
            return Some(function_choices[0].clone());
        }

        let mut max_type_match_score = 0;
        let mut closest_function = function_choices.first().unwrap().clone();

        for function in &function_choices {
            let mut current_type_match_score = 0;

            // Detect "every-argument-has-a-default" so we can credit a
            // keyword-only invocation.
            let mut all_arguments_keywords =
                !function.arguments.is_empty() || (class_function && function.arguments.len() > 1);

            let arguments_iter = if class_function {
                function.arguments.iter().skip(1)
            } else {
                function.arguments.iter().skip(0)
            };

            for (_, arg) in arguments_iter {
                if !arg.default_value {
                    all_arguments_keywords = false;
                    break;
                }
            }

            // Score keyword-supplied arguments.
            if let Some(provided) = &provided_arguments {
                for (argument_name, argument_type) in provided.iter() {
                    if function.arguments.contains_key(argument_name)
                        && function.arguments[argument_name].default_value
                    {
                        if self.types_equal(
                            &function.arguments[argument_name].argument_type,
                            argument_type,
                        ) {
                            current_type_match_score += 2;
                        } else if self.types_similar(
                            &function.arguments[argument_name].argument_type,
                            argument_type,
                        ) {
                            current_type_match_score += 1;
                        }
                        // Note: unlike the strict choose_function, no
                        // score-reset happens on a non-similar type —
                        // wrong-type arguments simply don't contribute.
                    }
                }
            } else if all_arguments_keywords
                && (argument_types.is_empty() || (class_function && argument_types.len() == 1))
            {
                current_type_match_score += 1;
            } else if function.arguments.len() <= argument_types.len() {
                if argument_types.len() == function.arguments.len() {
                    current_type_match_score = 1;
                }

                // Score positional arguments.
                for (index, (_, arg)) in function.arguments.iter().enumerate() {
                    if index >= argument_types.len() {
                        break;
                    }

                    if self.types_equal(&arg.argument_type, &argument_types[index])
                        || (arg.argument_type.is_float() && argument_types[index].is_float())
                    {
                        current_type_match_score += 2;
                    } else if self.types_similar(&arg.argument_type, &argument_types[index]) {
                        current_type_match_score += 1;
                    }
                }

                // Score variadic arguments if applicable.
                if function.var_args_type.is_some()
                    && argument_types.len() > function.arguments.len()
                {
                    for argument_type in
                        &argument_types[(function.arguments.len())..argument_types.len()]
                    {
                        if self.types_equal(argument_type, function.var_args_type.as_ref().unwrap())
                        {
                            current_type_match_score += 2;
                        } else if self
                            .types_similar(argument_type, function.var_args_type.as_ref().unwrap())
                        {
                            current_type_match_score += 1;
                        }
                    }
                }
            }

            // Pick this candidate if it beat the running best — but only
            // if it could plausibly be called at all (right arg count or
            // a variadic / all-keywords escape hatch).
            if current_type_match_score > max_type_match_score
                && (argument_types.len() <= function.arguments.len()
                    || (argument_types.len() > function.arguments.len()
                        && (function.visibility.variadic || function.var_args_type.is_some()))
                    || (all_arguments_keywords
                        && (argument_types.is_empty()
                            || (class_function && argument_types.len() == 1))))
            {
                max_type_match_score = current_type_match_score;
                closest_function = function.clone();
            }
        }

        // Always return *something* — fall back to the first candidate
        // if no partial match scored above zero. This is what makes the
        // method useful for diagnostic recovery.
        if max_type_match_score == 0 {
            Some(function_choices[0].clone())
        } else {
            Some(closest_function)
        }
    }
}

// ----- ExecutionContextAlgorithms impl -------------------------------------

impl
    ExecutionContextAlgorithms<
        SimulatorModule,
        SimulatorValue,
        SimulatorVariable,
        SimulatorFunction,
        SimulatorArg,
        SimulatorFunctionGeneric,
        SimulatorClass,
        SimulatorClassVirtualTable,
        SimulatorClassAttribute,
        SimulatorClassGeneric,
    > for PekoSimulatorContext
{
    fn get_module_context(&self) -> &ExecutionModuleContext<SimulatorModule> {
        &self.module_context
    }

    fn get_module_context_mut(&mut self) -> &mut ExecutionModuleContext<SimulatorModule> {
        &mut self.module_context
    }

    fn get_external_modules(&self) -> &HashMap<String, ExternalModuleInfo> {
        &self.external_modules
    }

    fn get_root_folder(&self) -> &std::path::PathBuf {
        &self.root_folder
    }

    fn get_root_folder_mut(&mut self) -> &mut std::path::PathBuf {
        &mut self.root_folder
    }

    fn get_generic_types(&self) -> &HashMap<String, types::PekoType> {
        &self.generic_types
    }

    fn get_generic_types_mut(&mut self) -> &mut HashMap<String, types::PekoType> {
        &mut self.generic_types
    }

    fn get_current_this(&self) -> &Option<SimulatorVariable> {
        &self.current_this
    }

    fn get_current_this_mut(&mut self) -> &mut Option<SimulatorVariable> {
        &mut self.current_this
    }

    // ----- Custom algorithm implementations --------------------------------

    /// Instantiates a generic function with concrete type parameters.
    ///
    /// Names the resulting function `Generic<T1, T2, ...>` (using each
    /// expanded type's string form), substitutes the type parameters
    /// into the generic-types map, snapshots the surrounding context,
    /// moves to the generic's declaration module, re-simulates the AST,
    /// then restores the context and returns the freshly-generated
    /// function.
    fn create_generic_function(
        &mut self,
        generic: &SimulatorFunctionGeneric,
        type_parameters: Vec<types::PekoType>,
    ) -> Option<SimulatorFunction> {
        let mut type_parameters_expanded = Vec::new();
        let mut name_parts: Vec<String> = Vec::new();

        let post_stack = self.module_context.step_back();

        for parameter in type_parameters {
            let Some(type_expanded) = self.expand_type(&parameter) else {
                return None;
            };
            name_parts.push(type_expanded.to_string());
            type_parameters_expanded.push(type_expanded);
        }

        self.module_context.step_forward(post_stack);

        // Build the qualified generic function name: `Foo<T, U>`.
        let mut generic_function_name = generic.function.function_name.clone();
        generic_function_name.value.push('<');
        generic_function_name.value.push_str(&name_parts.join(", "));
        generic_function_name.value.push('>');

        // Arity mismatch — caller asked for the wrong number of type
        // parameters.
        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        // Map each generic typename to its concrete expansion.
        let mut new_generic_types = HashMap::new();
        for (type_name, generic_type) in generic
            .generic_typenames
            .iter()
            .zip(type_parameters_expanded.iter())
        {
            new_generic_types.insert(
                type_name.value.clone(),
                self.expand_type(generic_type).unwrap(),
            );
        }

        // Install the new substitutions, saving the previous map for
        // restoration.
        let previous_context_generic_types = self.get_generic_types().clone();
        self.get_generic_types_mut().clear();
        self.get_generic_types_mut().extend(new_generic_types);

        let mut generic_function = generic.function.clone();
        generic_function.function_name = generic_function_name.clone();

        // Snapshot the context before re-simulating, switch to the
        // declaration module, then simulate.
        let context = self.snapshot_context();
        self.module_context
            .move_to_module(generic.module.clone(), false, true);

        let module = self.module_context.current_module();
        generic_function.generic_types.clear();
        generic_function.simulate(self);

        self.module_context.move_out_of_module();

        // Capture a reference to the newly-generated function before
        // restoring the context.
        let function_reference =
            Some(module.read().unwrap().functions[&generic_function_name.value][0].clone());

        self.reset_context(context);
        self.generic_types.clear();
        self.generic_types.extend(previous_context_generic_types);

        function_reference
    }

    /// Instantiates a generic class with concrete type parameters.
    ///
    /// Structurally identical to [`Self::create_generic_function`]: name
    /// the result `Generic<T, U, ...>`, expand each type parameter,
    /// snapshot the context, move to the declaration module, re-simulate
    /// the class AST, restore, and return the new class.
    fn create_generic_class(
        &mut self,
        generic: &SimulatorClassGeneric,
        type_parameters: Vec<types::PekoType>,
    ) -> Option<SimulatorClass> {
        let mut type_parameters_expanded = Vec::new();
        let mut name_parts: Vec<String> = Vec::new();

        let post_stack = self.module_context.step_back();

        for parameter in type_parameters {
            let Some(type_expanded) = self.expand_type(&parameter) else {
                return None;
            };
            name_parts.push(type_expanded.to_string());
            type_parameters_expanded.push(type_expanded);
        }

        self.module_context.step_forward(post_stack);

        let mut generic_class_name = generic.class.class_name.clone();
        generic_class_name.value.push('<');
        generic_class_name.value.push_str(&name_parts.join(", "));
        generic_class_name.value.push('>');

        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        let mut new_generic_types = HashMap::new();
        for (type_name, generic_type) in generic
            .generic_typenames
            .iter()
            .zip(type_parameters_expanded.iter())
        {
            new_generic_types.insert(
                type_name.value.clone(),
                self.expand_type(generic_type).unwrap(),
            );
        }

        let previous_context_generic_types = self.generic_types.clone();
        self.generic_types.clear();
        self.generic_types.extend(new_generic_types);

        let mut generic_class = generic.class.clone();
        generic_class.class_name = generic_class_name.clone();

        let context = self.snapshot_context();
        self.module_context
            .move_to_module(generic.module.clone(), false, true);

        self.current_scope = Some(generic.scope.clone());
        let module = self.module_context.current_module().clone();

        generic_class.simulate(self);

        self.module_context.move_out_of_module();

        let class_reference =
            Some(module.read().unwrap().classes[&generic_class_name.value].clone());

        self.reset_context(context);
        self.generic_types.clear();
        self.generic_types.extend(previous_context_generic_types);

        class_reference
    }

    /// Calls a named function by its fully-qualified name (including any
    /// `module::submodule::` path).
    ///
    /// Returns `None` if the function name can't be expanded, can't be
    /// resolved to a module, or no overload accepts the supplied
    /// argument types.
    fn call_named_function(
        &mut self,
        function_name: impl ToString,
        function_arguments: Vec<SimulatorValue>,
    ) -> Option<SimulatorValue> {
        // Expand the function reference to its fully-qualified form.
        let mut function_name_type =
            types::PekoType::from_string(&function_name.to_string(), String::new());
        let Some(expanded) = self.expand_type(&function_name_type) else {
            return None;
        };
        function_name_type = expanded;

        // Walk the module path to find the function's defining module.
        let mut next_module =
            self.module_context.top_level_modules[&function_name_type.module_names[0]].clone();
        for i in 1..function_name_type.module_names.len() {
            let child =
                next_module.read().unwrap().modules[&function_name_type.module_names[i]].clone();
            next_module = child;
        }

        let argument_types: Vec<PekoType> = function_arguments
            .iter()
            .map(SimulatorValue::get_type)
            .collect();

        let Some(function_to_call) = self.choose_function(
            next_module.read().unwrap().functions[&function_name_type.type_name].clone(),
            argument_types,
            None,
            false,
        ) else {
            return None;
        };

        // Verify argument types one more time at the chosen overload —
        // belt-and-braces against choose_function returning a candidate
        // whose argument types we then need to validate.
        for (argument, (_, arg)) in
            itertools::izip!(&function_arguments, &function_to_call.arguments)
        {
            if !self.types_similar(&argument.get_type(), &arg.argument_type) {
                return None;
            }
        }

        Some(SimulatorValue::Value(function_to_call.return_type))
    }

    /// Looks up and "calls" a method on `object`, returning the method's
    /// declared return type.
    ///
    /// Returns descriptive `Err` strings on every failure mode so the
    /// caller can surface them as diagnostics.
    fn call_object_method(
        &mut self,
        object: &SimulatorValue,
        method_name: impl ToString,
        arguments: Vec<SimulatorValue>,
        provided_arguments: Option<HashMap<String, SimulatorValue>>,
    ) -> Result<SimulatorValue, String> {
        let object_value_type = object.get_type();
        let method_name_str = method_name.to_string();

        // Resolve the object's class.
        let Some(class) = self.get_class_by_type(&object_value_type) else {
            return Err(format!(
                "method `{method_name_str}` cannot be called: type `{}` has no associated class, so it has no methods",
                object_value_type,
            ));
        };

        // Look up the method's overload set in the class's virtual table.
        let method_options = if class
            .main_virtual_table
            .methods
            .contains_key(&method_name_str)
        {
            class.main_virtual_table.methods[&method_name_str].clone()
        } else {
            return Err(format!(
                "no method `{method_name_str}` on type `{object_value_type}`. Check the method name, that the method is declared on this class (or a parent), and that you are accessing it via the correct object",
            ));
        };

        // Argument types — both positional and keyword.
        let argument_types: Vec<PekoType> =
            arguments.iter().map(SimulatorValue::get_type).collect();

        let provided_function_argument_types = if let Some(provided) = &provided_arguments {
            let mut argument_type_map = HashMap::new();
            for (argument_name, argument_value) in provided {
                argument_type_map.insert(argument_name.clone(), argument_value.get_type());
            }
            Some(argument_type_map)
        } else {
            None
        };

        let post_stack = self.module_context.step_back();

        let method_choice = self.choose_function(
            method_options,
            argument_types.clone(),
            provided_function_argument_types,
            true,
        );

        let Some(method) = method_choice else {
            return Err(format!(
                "no overload of method `{method_name_str}` on type `{object_value_type}` matches the supplied argument types. The call's positional or keyword argument types do not match any declared overload",
            ));
        };

        // Visibility check: private methods can only be called from
        // within a method on the same class (i.e. when we have a `this`).
        if method.visibility.private && self.current_this.is_none() {
            return Err(format!(
                "cannot call private method `{method_name_str}` on type `{object_value_type}` from outside the class. Private methods are only accessible from other methods of the same class",
            ));
        }

        // Detect "every argument has a default" — relevant to the
        // keyword-call path below.
        let mut all_args_keywords = !method.arguments.is_empty();
        for (_, arg) in &method.arguments {
            if !arg.default_value {
                all_args_keywords = false;
                break;
            }
        }

        // Positional-call path: verify each argument's type against the
        // matching parameter, and validate variadic arguments if any.
        if provided_arguments.is_none() && (!argument_types.is_empty() || !all_args_keywords) {
            let mut break_index = 0;

            for (argument, (_, arg)) in itertools::izip!(&arguments, &method.arguments) {
                if !self.types_similar(&argument.get_type(), &arg.argument_type) {
                    return Err(format!(
                        "argument {} to method `{method_name_str}` has type `{}` but the method declares parameter type `{}`",
                        break_index + 1,
                        argument.get_type(),
                        arg.argument_type,
                    ));
                }

                break_index += 1;
            }

            // Variadic-arguments path.
            if method.var_args_type.is_some() {
                let mut var_arguments = Vec::new();
                for i in break_index..arguments.len() {
                    var_arguments.push(arguments[i].clone());
                }

                if self
                    .create_standard_array(method.var_args_type.as_ref().unwrap(), var_arguments)
                    .is_none()
                {
                    return Err(format!(
                        "variadic arguments to method `{method_name_str}` could not be packed into an array of type `{}`. At least one variadic argument has a type that does not match the declared variadic type",
                        method.var_args_type.clone().unwrap(),
                    ));
                }
            }
        } else {
            // Keyword-call path.
            let provided_arguments = if let Some(provided) = provided_arguments {
                provided.clone()
            } else {
                HashMap::new()
            };

            for (argument_name, arg) in &method.arguments {
                let argument_value = if provided_arguments.contains_key(argument_name) {
                    provided_arguments[argument_name].clone()
                } else {
                    SimulatorValue::Value(arg.argument_type.clone())
                };

                if !self.types_similar(&argument_value.get_type(), &arg.argument_type) {
                    return Err(format!(
                        "keyword argument `{argument_name}` to method `{method_name_str}` has type `{}` but the method declares parameter type `{}`",
                        argument_value.get_type(),
                        arg.argument_type,
                    ));
                }
            }
        }
        self.module_context.step_forward(post_stack);

        Ok(SimulatorValue::Value(method.return_type.clone()))
    }

    /// Constructs a `standard::Array<T>` simulator value.
    ///
    /// Returns `None` if any element's type is not similar to
    /// `array_type`.
    fn create_standard_array(
        &mut self,
        array_type: &types::PekoType,
        values: Vec<SimulatorValue>,
    ) -> Option<SimulatorValue> {
        for value in &values {
            if !self.types_similar(&value.get_type(), array_type) {
                return None;
            }
        }

        let mut array_object_type = types::PekoType::from_string("standard::Array", String::new());
        array_object_type.generic_types.push(array_type.clone());

        Some(SimulatorValue::Value(array_object_type))
    }

    /// Constructs a `standard::Map<K, V>` simulator value.
    ///
    /// Returns `None` if any key or value type is not similar to the
    /// declared key or value type.
    fn create_standard_map(
        &mut self,
        key_type: &types::PekoType,
        value_type: &types::PekoType,
        key_value_pairs: Vec<(SimulatorValue, SimulatorValue)>,
    ) -> Option<SimulatorValue> {
        for (key, value) in key_value_pairs {
            if !self.types_similar(&key.get_type(), key_type)
                || !self.types_similar(&value.get_type(), value_type)
            {
                return None;
            }
        }

        let mut map_object_type = types::PekoType::from_string("standard::Map", String::new());
        map_object_type.generic_types.push(key_type.clone());
        map_object_type.generic_types.push(value_type.clone());

        Some(SimulatorValue::Value(map_object_type))
    }

    /// Constructs an object value of `class_type` by simulating its
    /// constructor invocation.
    ///
    /// For *struct-style* classes (no methods declared at all), the
    /// constructor is implicit: arguments are matched positionally to
    /// attributes and assigned in order.
    fn create_object(
        &mut self,
        class_type: &types::PekoType,
        constructor_arguments: Vec<SimulatorValue>,
    ) -> Option<SimulatorValue> {
        let Some(class_to_create) = self.get_class_by_type(class_type) else {
            return None;
        };

        // The simulator's "allocation" is just a typed value — see
        // gc_allocate_type's rustdoc.
        let allocate_object = self.gc_allocate_type(&class_to_create.class_type);

        // Class with methods: call its explicit constructor.
        if !class_to_create.main_virtual_table.methods.is_empty()
            && self
                .call_object_method(
                    &allocate_object,
                    "constructor".to_string(),
                    constructor_arguments.clone(),
                    None,
                )
                .is_err()
        {
            return None;
        }

        // Struct-style class (no methods): match arguments to attributes
        // positionally.
        if class_to_create.main_virtual_table.methods.is_empty() {
            if constructor_arguments.len() != class_to_create.attributes.len() {
                return None;
            }

            for ((attribute_name, _), attribute_value) in class_to_create
                .attributes
                .iter()
                .zip(&constructor_arguments)
            {
                if !self.set_object_attribute(&allocate_object, attribute_name, attribute_value) {
                    return None;
                }
            }
        }

        Some(allocate_object)
    }

    /// Applies a binary operator to two simulator values, returning the
    /// resulting typed value or `None` if the operator can't be applied.
    ///
    /// Handles, in order:
    ///
    /// * Closure-to-closure operators (always return `bool`).
    /// * User-defined operator overloads via `[operator <op>]` methods.
    /// * User-defined cast-and-retry via `[operator to_<type>]` methods.
    /// * Built-in numeric/boolean operators on integers, floats, chars,
    ///   and bools.
    /// * String/string equality.
    /// * Pointer equality.
    fn apply_operator(
        &mut self,
        operator: impl ToString,
        lhs: &SimulatorValue,
        rhs: &SimulatorValue,
    ) -> Option<SimulatorValue> {
        let lhs_value_type = self.expand_type(&lhs.get_type());
        let rhs_value_type = self.expand_type(&rhs.get_type());

        if lhs_value_type.is_none() || rhs_value_type.is_none() {
            return None;
        }

        let lhs_value_type = lhs_value_type.unwrap();
        let rhs_value_type = rhs_value_type.unwrap();

        // Closure-to-closure: any operator returns bool (closures are
        // first-class and the language permits comparisons).
        if lhs_value_type.is_closure
            && rhs_value_type.is_closure
            && self.types_equal(&lhs_value_type, &rhs_value_type)
        {
            return Some(SimulatorValue::Value(types::PekoType::simple_type("bool")));
        }

        let mut lhs = lhs.clone();

        // Class types: try a user-defined `[operator <op>]` overload.
        if self.get_class_by_type(&lhs_value_type).is_some() {
            let overload_name = format!("[operator {}]", operator.to_string());

            let call_overload =
                self.call_object_method(&lhs, overload_name.clone(), vec![rhs.clone()], None);

            if let Ok(value) = call_overload {
                return Some(value);
            }

            // Overload didn't work — try to cast the lhs to the rhs's
            // built-in type via a user-defined `[operator to_<type>]`
            // method, then continue as if the lhs had been that type
            // all along.
            if rhs_value_type.is_datatype() || rhs_value_type.to_string() == "bool" {
                let overload_name = format!("[operator to_{}]", rhs_value_type);
                let cast_to_datatype =
                    self.call_object_method(&lhs, overload_name, Vec::new(), None);

                lhs = match cast_to_datatype {
                    Ok(value) => value,
                    Err(_) => return None,
                };
            }
        }

        // Built-in operators on integers, floats, chars, and bools
        // (covered by is_float | is_integer | is_datatype).
        if lhs_value_type.is_float() || lhs_value_type.is_integer() || lhs_value_type.is_datatype()
        {
            let rhs = self.box_value_to_type(&lhs.get_type(), rhs);

            if rhs.is_none() {
                return None;
            }

            let operation_type = match operator.to_string().as_str() {
                // Arithmetic operators yield the lhs type.
                "+" | "-" | "*" | "/" | "%" | "^" => lhs_value_type.clone(),

                // Everything else (comparison, boolean) yields bool.
                _ => types::PekoType::simple_type("bool"),
            };

            return Some(SimulatorValue::Value(operation_type));
        }

        // String-to-string equality / inequality.
        if lhs_value_type.is_string_type() && rhs_value_type.is_string_type() {
            if matches!(operator.to_string().as_str(), "==" | "!=") {
                return Some(SimulatorValue::Value(types::PekoType::simple_type("bool")));
            }
        }

        // Reference-like equality / inequality.
        //
        // Classes and every pointer form (`opaque`, `Pointer<T>`, `&T`,
        // `T[]`, `string`, `cstr`) are all represented as pointers, so they
        // can be compared with `==` / `!=`. A type counts as reference-like
        // when it is a pointer, a managed `Pointer<T>`, or resolves to a
        // class. The two operands must still be similar (which already
        // permits class vs `opaque` / `Pointer<void>` and connected
        // classes), so unrelated reference types do not compare.
        if matches!(operator.to_string().as_str(), "==" | "!=") {
            let lhs_is_reference_like = lhs_value_type.is_pointer()
                || lhs_value_type.type_name == "Pointer"
                || self.get_class_by_type(&lhs_value_type).is_some();
            let rhs_is_reference_like = rhs_value_type.is_pointer()
                || rhs_value_type.type_name == "Pointer"
                || self.get_class_by_type(&rhs_value_type).is_some();

            if lhs_is_reference_like
                && rhs_is_reference_like
                && self.types_similar(&lhs_value_type, &rhs_value_type)
            {
                return Some(SimulatorValue::Value(types::PekoType::simple_type("bool")));
            }
        }

        None
    }

    /// Reads an attribute from an object, returning either a loaded
    /// value of the attribute's type or a reference to it (depending on
    /// `load_value`).
    ///
    /// Returns descriptive `Err` strings on every failure mode.
    fn get_object_attribute(
        &mut self,
        object: &SimulatorValue,
        attribute_name: impl ToString,
        load_value: bool,
    ) -> Result<SimulatorValue, String> {
        let object_value_type = object.get_type();
        let attribute_name_str = attribute_name.to_string();

        let Some(class) = self.get_class_by_type(&object_value_type) else {
            return Err(format!(
                "attribute `{attribute_name_str}` cannot be read: type `{object_value_type}` has no associated class, so it has no attributes",
            ));
        };

        if !class.attributes.contains_key(&attribute_name_str) {
            return Err(format!(
                "no attribute `{attribute_name_str}` on type `{object_value_type}`. Check the attribute name and that it is declared on this class (or a parent)",
            ));
        }

        // Private attributes can only be read from within a method on
        // the same class.
        if class.attributes[&attribute_name_str].visibility.private
            && self.get_current_this().is_none()
        {
            return Err(format!(
                "cannot read private attribute `{attribute_name_str}` on type `{object_value_type}` from outside the class. Private attributes are only accessible from methods of the same class",
            ));
        }

        if load_value {
            Ok(SimulatorValue::Value(
                class.attributes[&attribute_name_str].attribute_type.clone(),
            ))
        } else {
            // Caller wants an lvalue — bump the reference depth so the
            // result represents `&attribute` rather than the loaded
            // value.
            let mut reference_type = class.attributes[&attribute_name_str].attribute_type.clone();
            reference_type.reference_depth += 1;

            Ok(SimulatorValue::Value(reference_type))
        }
    }

    /// Writes `value` into `attribute_name` on `object`. Returns `true`
    /// on success.
    ///
    /// Reuses [`Self::get_object_attribute`] for lookup / visibility /
    /// existence checking, then verifies the assigned value's type
    /// against the attribute's storage type.
    fn set_object_attribute(
        &mut self,
        object: &SimulatorValue,
        attribute_name: impl ToString,
        value: &SimulatorValue,
    ) -> bool {
        // Get the attribute as a reference (so its type carries the
        // extra reference-depth that we'll strip back off below for the
        // similarity check).
        let element_value = match self.get_object_attribute(object, attribute_name, false) {
            Ok(v) => v,
            Err(_) => return false,
        };

        let value_value_type = value.get_type();
        let element_value_type = element_value.get_type();

        // The element value is a reference; check assignment compat
        // against the underlying storage type.
        let mut storage_type = element_value_type.clone();
        storage_type.reference_depth -= 1;

        self.types_similar(&storage_type, &value_value_type)
    }
}
