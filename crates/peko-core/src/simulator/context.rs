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
//! 2. [`SimulatorContextSnapshot`] - a named bundle of the seven fields
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
    SimulatorClass, SimulatorClassAttribute, SimulatorClassVirtualTable, SimulatorFunction,
    SimulatorModule, SimulatorVariable,
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
/// * **Diagnostics / target** - what we're compiling for and what's gone
///   wrong so far.
/// * **Module / scope state** - which module and scope is currently
///   active, plus the lookup tables for symbols visible to tooling.
/// * **Type-inference state** - running expected types, return types,
///   and generic substitutions in scope.
/// * **Object-access state** - `this` binding, attributes pending
///   initialization, and "previous expression was `this`" used to
///   resolve method calls.
/// * **Trace bookkeeping** - function-call traces and defined-object
///   spans surfaced to IDE clients.
#[derive(Clone)]
pub struct PekoSimulatorContext {
    /// When `true`, errors raised inside imported modules are collapsed
    /// to a single error at the import site in the primary file rather
    /// than reported in their original location.
    pub minified_import_errors: bool,

    /// When `true`, a function declaration registers its signature but skips
    /// simulating its body. The header pass sets this so every function in a
    /// module is registered before any body is checked, which lets a function
    /// call another declared later in the same module.
    pub declaring_signatures_only: bool,

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

    /// In-scope generic-type substitutions (e.g. `T` -> `String`).
    pub generic_types: HashMap<String, types::PekoType>,

    /// Declared return type of the function we're currently inside.
    /// Used for type-inference when the type-checker has to consult what
    /// the surrounding function expects.
    pub current_return_type: Option<types::PekoType>,

    /// Expected type(s) at the current expression position - typically
    /// set by variable declarations to inform inference of the RHS.
    pub current_expected_type_options: Option<Vec<types::PekoType>>,

    /// Counter for fresh backward-inference variables (`?0`, `?1`, ...). A
    /// `new Class()` whose type arguments cannot be inferred forward binds one
    /// of these per missing argument; a later constraining call resolves it.
    pub inference_counter: usize,

    /// Bindings (`?N` -> concrete type) discovered by the most recent method
    /// call, for the caller to back-patch into the receiver variable's type.
    /// Cleared at the start of each `call_object_method`.
    pub last_call_inference: HashMap<String, types::PekoType>,

    /// `true` when the value at the current position is consumed (a variable
    /// initializer, a call argument, a return value, ...). A construct that
    /// can be either a statement or an expression, like `if`, reads this to
    /// decide which it is.
    pub expecting_value: bool,

    /// Set true while simulating a method body when the body reassigns an
    /// attribute of `this` or calls a `[mutates]` method on one. Read after
    /// the body to auto-mark the method `[mutates]`.
    pub current_method_mutates: bool,

    /// The `[mutates]` flag of the most recently dispatched method call. Lets
    /// a caller see whether the callee mutates (24.2 rule 2).
    pub last_called_method_mutates: bool,

    /// Name of the method whose body is currently being simulated, if any.
    /// Pairs with `current_this`'s class to identify the calling method when
    /// recording mutation-call edges.
    pub current_method_name: Option<String>,

    /// Recorded `(caller_class, caller_method, callee_class, callee_method)`
    /// edges for every method call made on `this` or an attribute of `this`.
    /// A fixpoint over these after simulation propagates `[mutates]` through
    /// forward references that a single pass cannot see (24.2 rule 2).
    pub mutates_call_edges: Vec<(String, String, String, String)>,

    /// Names of local bindings declared without an initializer that have not
    /// yet been definitely assigned. Reading one is a use-before-init error
    /// (the binding must be assigned on every path that reaches the read).
    pub uninitialized_variables: std::collections::HashSet<String>,

    /// `true` while simulating the target of a direct assignment (`x = ...`),
    /// so resolving the target does not count as a read for the
    /// use-before-init check.
    pub simulating_assignment_target: bool,

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
            Arc::clone(&main_scope),
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
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
            Arc::clone(&extern_scope),
            Vec::new(),
            IndexMap::new(),
            IndexMap::new(),
        )));

        PekoSimulatorContext {
            minified_import_errors: false,
            declaring_signatures_only: false,
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
            inference_counter: 0,
            last_call_inference: HashMap::new(),
            expecting_value: false,
            current_method_mutates: false,
            last_called_method_mutates: false,
            current_method_name: None,
            mutates_call_edges: Vec::new(),
            uninitialized_variables: std::collections::HashSet::new(),
            simulating_assignment_target: false,
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

        // Import traits. Traits are stored by value (like enums) and resolved
        // through the module's trait map, so only the map entry is copied; they
        // carry no scope symbol.
        for (trait_name, trait_definition) in from.read().unwrap().traits.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(trait_name.clone()))
            {
                continue;
            }
            if !current_symbols.is_empty()
                && let Some((index, _)) = current_symbols.iter().find_position(|symbol_name| {
                    symbol_name == &&PositionedValue::create_no_position(trait_name.clone())
                })
            {
                current_symbols.remove(index);
            }

            to.write()
                .unwrap()
                .traits
                .insert(trait_name.clone(), trait_definition);
        }

        // Import enums. Like traits, an enum is stored by value and resolved by
        // name; copy each entry so an enum an imported module declares resolves
        // and compares here.
        for (enum_name, variants) in from.read().unwrap().enums.clone() {
            if !unpacked_symbols.is_empty()
                && !unpack_all
                && !current_symbols
                    .contains(&PositionedValue::create_no_position(enum_name.clone()))
            {
                continue;
            }
            if !current_symbols.is_empty()
                && let Some((index, _)) = current_symbols.iter().find_position(|symbol_name| {
                    symbol_name == &&PositionedValue::create_no_position(enum_name.clone())
                })
            {
                current_symbols.remove(index);
            }

            to.write()
                .unwrap()
                .enums
                .insert(enum_name.clone(), variants);
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
                    submodule.read().unwrap().scope.clone(),
                    Vec::new(),
                    IndexMap::new(),
                    IndexMap::new(),
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
    /// methods and attributes as a list of [`ScopeSymbol`]s - filtered
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

        let mut object_symbols = Vec::new();

        // A generic-parameter-typed object has no class. Its accessible surface
        // is whatever its bounds grant: the methods of every `impl Trait` bound.
        // A `from Class` bound erases to the class itself, so it resolves as a
        // class through the branch below instead.
        let Some(object_class) = self.get_class_by_type(&object.object_type) else {
            if object.object_type.is_generic_param() {
                object_symbols.extend(self.symbols_from_bounds(&object.object_type));
            }
            return object_symbols;
        };

        // Surface every accessible method.
        for (method_name, method_functions) in &object_class.main_virtual_table.methods {
            // Only take the first overload - completion just needs the
            // name and a representative signature.
            let first_function = method_functions[0].read().unwrap().clone();

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

    /// The methods reachable through a generic parameter's bounds, as scope
    /// symbols for completion. Each `impl Trait` bound contributes that trait's
    /// method slots. Slot arguments carry no source names, so they are surfaced
    /// as positional `arg0`, `arg1`, and so on. Static slots are included so a
    /// type-level bound method appears alongside the instance methods.
    fn symbols_from_bounds(&self, generic_type: &types::PekoType) -> Vec<ScopeSymbol> {
        let mut symbols = Vec::new();

        for restraint in generic_type.restraints() {
            let types::TypeRestraint::Impl(trait_type) = restraint else {
                continue;
            };
            let Some(trait_definition) = self.get_trait(trait_type.name()) else {
                continue;
            };

            for slot in &trait_definition.methods {
                let mut arguments = IndexMap::new();
                for (index, argument_type) in slot.argument_types.iter().enumerate() {
                    arguments.insert(
                        format!("arg{index}"),
                        (VisibilityData::default(), argument_type.clone()),
                    );
                }

                let visibility = VisibilityData {
                    is_static: slot.is_static,
                    ..VisibilityData::default()
                };

                symbols.push(ScopeSymbol::Function(
                    ScopeFunction::new(
                        None,
                        slot.name.clone(),
                        slot.return_type.clone(),
                        PositionData::default(),
                        PositionData::default(),
                        false,
                        arguments,
                        Vec::new(),
                    ),
                    visibility,
                ));
            }
        }

        symbols
    }

    /// Records a value's type as a completion-source object at `position`.
    ///
    /// The type is expanded first so a generic parameter resolves to its bound
    /// carrier, whose restraints drive member completion. Only a class type or
    /// a generic-parameter type is recorded; any other type (a scalar, an enum,
    /// a closure) records nothing, so non-object values are ignored. This is
    /// the single place the object-access completion path is fed from, so every
    /// value-producing site records consistently.
    pub fn record_defined_object(
        &mut self,
        object_type: &types::PekoType,
        is_this: bool,
        position: PositionData,
    ) {
        let recorded = self
            .expand_type(object_type)
            .unwrap_or_else(|| object_type.clone());
        if self.get_class_by_type(&recorded).is_some() || recorded.is_generic_param() {
            self.defined_objects
                .push(DefinedObject::new(is_this, recorded, position));
        }
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

            // Collect symbols visible at the resolved scope, but only those
            // declared IN this module. A module that unpacks another
            // (`import { * } from collections`) copies those symbols into its
            // own scope; `mod::` access should list only the module's own
            // declarations, not what it re-imported. The module's own source is
            // its scope's end position file (the import binds the scope's start
            // to the import site but its end to the module's own file).
            if found {
                let module_own_file = current_scope.as_ref().read().unwrap().end.file.clone();
                for (_, symbol) in current_scope.as_ref().read().unwrap().symbols.iter() {
                    // List only the module's own declarations. A symbol brought
                    // in by an unpack keeps its origin file, so a symbol whose
                    // file differs from this module's is not a member. A module
                    // brought in by `import name` is an alias, not a member of
                    // this module, so module-kind symbols are dropped as well.
                    if symbol.get_kind() == "module" || symbol.get_start().file != module_own_file {
                        continue;
                    }
                    if symbol.get_start().file != position.file
                        || symbol.get_start().positioned_before(position.clone())
                    {
                        available_symbols.push(symbol.clone());
                    }
                }
            }

            return available_symbols;
        }

        // Otherwise, resolve the first segment as a module: an alias bound in
        // the current module (`import pekoui as ui`), or a top-level module by
        // name (`import pekoui`). With package memoization a reused package
        // stays in top_level_modules under its own name while the alias lives
        // only in the importing module's module_aliases, so both are checked.
        let module_start = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .module_aliases
            .get(first_name)
            .cloned()
            .or_else(|| {
                self.module_context
                    .top_level_modules
                    .get(first_name)
                    .cloned()
            });

        if let Some(mut module_referenced) = module_start {
            // Descend into submodules.
            let mut found = true;
            while !module_path.is_empty() {
                let current_module_to_find = module_path.remove(0);
                let child = module_referenced
                    .read()
                    .unwrap()
                    .modules
                    .get(current_module_to_find)
                    .cloned();
                match child {
                    Some(child) => module_referenced = child,
                    None => {
                        found = false;
                        break;
                    }
                }
            }

            // Surface the resolved module's own declarations plus its exported
            // submodules. A module-kind symbol is a member only when it is an
            // actual submodule (registered by `export`); a plain `import` alias
            // is not, so it is dropped. A non-module symbol whose file differs
            // from this module's was pulled in by an unpack and is not a member.
            if found {
                let module = module_referenced.read().unwrap();
                let module_scope = module.scope.clone();
                let submodules: std::collections::HashSet<String> =
                    module.modules.keys().cloned().collect();
                drop(module);

                let scope = module_scope.read().unwrap();
                let module_own_file = scope.end.file.clone();
                for (_, symbol) in &scope.symbols {
                    if symbol.get_kind() == "module" {
                        if !submodules.contains(&symbol.get_name()) {
                            continue;
                        }
                    } else if symbol.get_start().file != module_own_file {
                        continue;
                    }
                    available_symbols.push(symbol.clone());
                }
            }
        }

        available_symbols
    }

    /// Returns every symbol visible at `position` - globals, plus every
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

    /// Propagates `[mutates]` across the recorded call edges to a fixpoint
    /// (24.2 rule 2). The single pass during simulation cannot see a method
    /// that is marked `[mutates]` only after the caller was already
    /// simulated; this corrects every such method's flag after the fact.
    ///
    /// This updates the stored flags (so codegen and later analysis read the
    /// right value). It does not re-issue the const-mutation diagnostics that
    /// already ran during simulation.
    pub fn propagate_mutates_fixpoint(&mut self) {
        // Collect every class by name across the module tree.
        let mut classes: HashMap<String, Arc<RwLock<SimulatorClass>>> = HashMap::new();
        let mut module_queue: Vec<Arc<RwLock<SimulatorModule>>> = self
            .module_context
            .top_level_modules
            .values()
            .cloned()
            .collect();
        while let Some(module) = module_queue.pop() {
            let module_read = module.read().unwrap();
            for (name, class) in &module_read.classes {
                classes.entry(name.clone()).or_insert_with(|| class.clone());
            }
            for submodule in module_read.modules.values() {
                module_queue.push(submodule.clone());
            }
        }

        let method_mutates = |class: &str, method: &str| -> bool {
            classes.get(class).is_some_and(|class_ref| {
                class_ref
                    .read()
                    .unwrap()
                    .main_virtual_table
                    .methods
                    .get(method)
                    .is_some_and(|overloads| {
                        overloads
                            .iter()
                            .any(|overload| overload.read().unwrap().visibility.mutates)
                    })
            })
        };

        let edges = self.mutates_call_edges.clone();

        loop {
            let mut changed = false;

            for (caller_class, caller_method, callee_class, callee_method) in &edges {
                if !method_mutates(callee_class, callee_method) {
                    continue;
                }

                let overloads = classes.get(caller_class).and_then(|class_ref| {
                    class_ref
                        .read()
                        .unwrap()
                        .main_virtual_table
                        .methods
                        .get(caller_method)
                        .cloned()
                });

                if let Some(overloads) = overloads {
                    for overload in &overloads {
                        let mut overload_write = overload.write().unwrap();
                        if !overload_write.visibility.mutates {
                            overload_write.visibility.mutates = true;
                            changed = true;
                        }
                    }
                }
            }

            if !changed {
                break;
            }
        }
    }

    /// Records that a symbol named `name` was referenced, marking it used in
    /// the current scope. Usage propagates up to the declaring scope during
    /// the unused-symbol pass (24.1).
    pub fn mark_symbol_used(&mut self, name: &str) {
        if let Some(scope) = self.current_scope.as_ref() {
            scope.write().unwrap().used_symbols.insert(name.to_string());
        }
    }

    /// Walks the main module's scope tree and warns about declared symbols
    /// that are never referenced (24.1). A symbol is exempt when it is
    /// explicitly `[public]` (an intentional external surface) or named
    /// `main` (the entry point). Class members are not warned in this pass.
    pub fn report_unused_symbols(&mut self) {
        let root = self
            .module_context
            .top_level_modules
            .get("main")
            .map(|module| module.read().unwrap().scope.clone());

        if let Some(root) = root {
            self.report_unused_in_scope(&root);
        }
    }

    /// Recursive helper for [`Self::report_unused_symbols`]. Returns the set
    /// of symbol names used anywhere in the subtree rooted at `scope`, and
    /// warns about this scope's own unused symbols along the way.
    fn report_unused_in_scope(
        &mut self,
        scope: &Arc<RwLock<Scope>>,
    ) -> std::collections::HashSet<String> {
        let (top_level, mut subtree_used, child_scopes, own_symbols) = {
            let scope_read = scope.read().unwrap();
            (
                scope_read.top_level,
                scope_read.used_symbols.clone(),
                scope_read.scopes.clone(),
                scope_read
                    .symbols
                    .iter()
                    .map(|(name, symbol)| (name.clone(), symbol.clone()))
                    .collect::<Vec<_>>(),
            )
        };

        for child in &child_scopes {
            subtree_used.extend(self.report_unused_in_scope(child));
        }

        for (name, symbol) in own_symbols {
            if subtree_used.contains(&name) {
                continue;
            }

            // Only top-level free declarations and local variables are warned
            // in this pass. Class members live in non-top-level class scopes
            // and are skipped.
            let (visibility, definition_start, kind) = match &symbol {
                // `this` is injected into every method scope, not declared, so
                // it is never reported.
                ScopeSymbol::Variable(variable, visibility)
                    if !variable.attribute && name != "this" =>
                {
                    (visibility, variable.definition_start.clone(), "variable")
                }
                ScopeSymbol::Function(function, visibility)
                    if top_level && !function.generic && name != "main" =>
                {
                    (visibility, function.definition_start.clone(), "function")
                }
                _ => continue,
            };

            if visibility.public {
                continue;
            }

            let file = self.get_current_file();
            self.diagnostics.report_diagnostic(diagnostics::PekoDiagnostic::new(
                definition_start.clone(),
                definition_start,
                format!(
                    "{kind} `{name}` is never used. Remove it, or mark it `[public]` if it is an intentional external surface",
                ),
                diagnostics::DiagnosticType::Warning,
                file,
            ));
        }

        subtree_used
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
    /// The simulator doesn't actually allocate anything - it just
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
    ///   *partial* match - or, failing that, the first choice.
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

            for (_, arg) in function.arguments.iter().skip(usize::from(class_function)) {
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
                        // score-reset happens on a non-similar type -
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
                if let Some(var_args_type) = &function.var_args_type
                    && argument_types.len() > function.arguments.len()
                {
                    for argument_type in
                        &argument_types[function.arguments.len()..argument_types.len()]
                    {
                        if self.types_equal(argument_type, var_args_type) {
                            current_type_match_score += 2;
                        } else if self.types_similar(argument_type, var_args_type) {
                            current_type_match_score += 1;
                        }
                    }
                }
            }

            // Pick this candidate if it beat the running best - but only
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

        // Always return *something* - fall back to the first candidate
        // if no partial match scored above zero. This is what makes the
        // method useful for diagnostic recovery.
        if max_type_match_score == 0 {
            Some(function_choices[0].clone())
        } else {
            Some(closest_function)
        }
    }
}

// ----- Generic instantiation helpers ---------------------------------------

impl PekoSimulatorContext {
    /// The type a generic parameter erases to for body type-checking. An
    /// unbounded parameter erases to the root `Object`; a `from Class` bound
    /// carries the class (granting its fields and methods); an `impl Trait`
    /// bound carries the trait (granting its methods). A `from` bound is
    /// preferred when both are present, since it grants the most access.
    fn erasure_carrier(
        &self,
        typename: &str,
        bounds: &IndexMap<String, Vec<types::TypeRestraint>>,
    ) -> types::PekoType {
        if let Some(restraints) = bounds.get(typename) {
            // A `from Class` bound grants the class's full surface, so the
            // carrier is that class.
            for restraint in restraints {
                if let types::TypeRestraint::From(class_type) = restraint {
                    return class_type.clone();
                }
            }
            // Otherwise carry every `impl Trait` bound on a generic carrier, so
            // each bound trait's methods and operators resolve. Carrying all
            // bounds (rather than only the first) lets a parameter bound by
            // several traits, such as `KT: impl Hash, impl Equals`, use every
            // one.
            if restraints
                .iter()
                .any(|restraint| matches!(restraint, types::TypeRestraint::Impl(_)))
            {
                return types::PekoType::generic_type(typename.to_string(), restraints.clone());
            }
        }

        types::PekoType::simple_type("Object")
    }

    /// Type-check every generic in the current module ONCE with each parameter
    /// erased to its bound carrier. This enforces the erasure invariant that a
    /// generic body uses only the capabilities its bounds grant, independent of
    /// any concrete instantiation. It reuses the instantiation path with the
    /// carrier types as arguments, so misuse of an erased parameter (a method
    /// or field its bounds do not provide) is reported against the body.
    pub fn check_generics_erased(&mut self) {
        let module = self.module_context.current_module();
        // Function templates: overloads that still hold a source AST.
        let function_generics: Vec<_> = module
            .read()
            .unwrap()
            .functions
            .values()
            .flatten()
            .filter(|overload| overload.read().unwrap().source_function.is_some())
            .cloned()
            .collect();
        // Class templates: classes that still hold a source AST.
        let class_generics: Vec<_> = module
            .read()
            .unwrap()
            .classes
            .values()
            .filter(|class| class.read().unwrap().source_class.is_some())
            .cloned()
            .collect();

        for generic in function_generics {
            let generic = generic.read().unwrap().clone();
            let bounds = generic
                .source_function
                .as_ref()
                .map(|ast| ast.generic_bounds.clone())
                .unwrap_or_default();
            let carriers: Vec<types::PekoType> = generic
                .generic_typenames
                .iter()
                .map(|name| self.erasure_carrier(&name.value, &bounds))
                .collect();
            self.create_generic_function(&generic, carriers);
        }

        for generic in class_generics {
            let generic = generic.read().unwrap().clone();
            let bounds = generic
                .source_class
                .as_ref()
                .map(|ast| ast.generic_bounds.clone())
                .unwrap_or_default();
            let carriers: Vec<types::PekoType> = generic
                .generic_typenames
                .iter()
                .map(|name| self.erasure_carrier(&name.value, &bounds))
                .collect();
            self.create_generic_class(&generic, carriers);
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
        SimulatorClass,
        SimulatorClassVirtualTable,
        SimulatorClassAttribute,
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

    fn get_diagnostics_mut(&mut self) -> &mut crate::diagnostics::DiagnosticList {
        &mut self.diagnostics
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
        generic: &SimulatorFunction,
        type_parameters: Vec<types::PekoType>,
    ) -> Option<SimulatorFunction> {
        // The template carries its source AST; a non-template function cannot be
        // instantiated.
        let source = generic.source_function.clone()?;

        let mut type_parameters_expanded = Vec::new();
        let mut name_parts: Vec<String> = Vec::new();

        let post_stack = self.module_context.step_back();

        for parameter in type_parameters {
            let type_expanded = self.expand_type(&parameter)?;
            name_parts.push(type_expanded.to_string());
            type_parameters_expanded.push(type_expanded);
        }

        self.module_context.step_forward(post_stack);

        // Build the qualified generic function name: `Foo<T, U>`.
        let mut generic_function_name = source.function_name.clone();
        generic_function_name.value.push('<');
        generic_function_name.value.push_str(&name_parts.join(", "));
        generic_function_name.value.push('>');

        // Arity mismatch - caller asked for the wrong number of type
        // parameters.
        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        // Each type argument must satisfy its parameter's `impl`/`from` bounds.
        self.check_generic_bounds(
            &generic.generic_typenames,
            &source.generic_bounds,
            &type_parameters_expanded,
        );

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

        let mut generic_function = source.clone();
        generic_function.function_name = generic_function_name.clone();

        // Snapshot the context before re-simulating, switch to the
        // declaration module, then simulate.
        let context = self.snapshot_context();
        self.module_context
            .move_to_module(generic.parent.clone(), false, true);

        let module = self.module_context.current_module();
        generic_function.generic_types.clear();
        generic_function.simulate(self);

        self.module_context.move_out_of_module();

        // Capture a reference to the newly-generated function before
        // restoring the context.
        let function_reference = Some(
            module.read().unwrap().functions[&generic_function_name.value][0]
                .read()
                .unwrap()
                .clone(),
        );

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
        generic: &SimulatorClass,
        type_parameters: Vec<types::PekoType>,
    ) -> Option<SimulatorClass> {
        // The template carries its source AST; a non-template class cannot be
        // instantiated.
        let source = generic.source_class.as_deref().cloned()?;

        let mut type_parameters_expanded = Vec::new();
        let mut name_parts: Vec<String> = Vec::new();

        let post_stack = self.module_context.step_back();

        for parameter in type_parameters {
            let mut type_expanded = self.expand_type(&parameter)?;

            // Collapse a deeply nested erased instantiation. A method that
            // returns a generic of its own parameters (zip returning
            // Array<Pair<T, U>>) would otherwise instantiate an unbounded tower
            // (Array<Pair<Pair<..>, U>> ...). Once a carrier-bearing argument
            // nests two levels deep, canonicalize it to its carrier so the tower
            // is bounded. Shallow structures such as Pair<T, U> are kept intact
            // so their shape (for example for destructuring) survives. Concrete
            // arguments contain no carrier and monomorphize in full.
            if !type_expanded.is_generic_param()
                && types::generic_nesting_depth(&type_expanded) >= 2
                && let Some(carrier) = types::first_generic_param(&type_expanded)
            {
                type_expanded = carrier;
            }

            name_parts.push(type_expanded.to_string());
            type_parameters_expanded.push(type_expanded);
        }

        self.module_context.step_forward(post_stack);

        let mut generic_class_name = source.class_name.clone();
        generic_class_name.value.push('<');
        generic_class_name.value.push_str(&name_parts.join(", "));
        generic_class_name.value.push('>');

        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        // Already instantiated. The class is registered under its (possibly
        // collapsed) name once simulated, and pre-registered as a shell before
        // its bodies run. Returning it serves two purposes: a cache hit that
        // avoids re-simulating a built class, and a cycle break when a generic
        // method body constructs the same generic while it is still simulating.
        // The caller looks up the un-collapsed name, so this in-function check is
        // what catches the collapsed erased instantiations.
        if let Some(existing) = generic
            .parent
            .read()
            .unwrap()
            .classes
            .get(&generic_class_name.value)
        {
            return Some(existing.read().unwrap().clone());
        }

        // Each type argument must satisfy its parameter's `impl`/`from` bounds.
        self.check_generic_bounds(
            &generic.generic_typenames,
            &source.generic_bounds,
            &type_parameters_expanded,
        );

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

        // Method-level generics stay erased even under monomorphization: bind
        // every method-declared parameter to a bare carrier so a method
        // signature that references it type-checks. The concrete type is
        // recovered per call.
        for class_method in &source.methods {
            for type_name in class_method.get_generic_types() {
                new_generic_types
                    .entry(type_name.value.clone())
                    .or_insert_with(|| {
                        types::PekoType::generic_type(
                            type_name.value.clone(),
                            class_method
                                .get_generic_bounds()
                                .get(&type_name.value)
                                .cloned()
                                .unwrap_or_default(),
                        )
                    });
            }
        }

        let previous_context_generic_types = self.generic_types.clone();
        self.generic_types.clear();
        self.generic_types.extend(new_generic_types);

        let mut generic_class = source.clone();
        generic_class.class_name = generic_class_name.clone();

        let context = self.snapshot_context();
        self.module_context
            .move_to_module(generic.parent.clone(), false, true);

        if let Some(scope) = generic.template_scope.clone() {
            self.current_scope = Some(scope);
        }
        let module = self.module_context.current_module().clone();

        generic_class.simulate(self);

        self.module_context.move_out_of_module();

        let class_reference = Some(
            module.read().unwrap().classes[&generic_class_name.value]
                .read()
                .unwrap()
                .clone(),
        );

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
        function_name_type = self.expand_type(&function_name_type)?;

        // Walk the module path to find the function's defining module. The
        // first segment may be a per-file alias (`import pekoui as ui`) bound in
        // the current module, or a top-level module by name.
        let module_names = function_name_type.module_names();
        let mut next_module = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .module_aliases
            .get(&module_names[0])
            .cloned()
            .or_else(|| {
                self.module_context
                    .top_level_modules
                    .get(&module_names[0])
                    .cloned()
            })?;
        for module_name in &module_names[1..] {
            let child = next_module
                .read()
                .unwrap()
                .modules
                .get(module_name)
                .cloned()?;
            next_module = child;
        }

        let argument_types: Vec<PekoType> = function_arguments
            .iter()
            .map(SimulatorValue::get_type)
            .collect();

        let function_to_call = self.choose_function(
            next_module.read().unwrap().functions[function_name_type.name()]
                .iter()
                .map(|f| f.read().unwrap().clone())
                .collect(),
            argument_types,
            None,
            false,
        )?;

        // Verify argument types one more time at the chosen overload -
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

        // Reset backward-inference bindings; this call may discover some.
        self.last_call_inference.clear();

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
            class.main_virtual_table.methods[&method_name_str]
                .iter()
                .map(|f| f.read().unwrap().clone())
                .collect()
        } else {
            return Err(format!(
                "no method `{method_name_str}` on type `{object_value_type}`. Check the method name, that the method is declared on this class (or a parent), and that you are accessing it via the correct object",
            ));
        };

        // Argument types - both positional and keyword.
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

        // Backward inference: a parameter that is an inference variable (`?N`,
        // from a `new Class()` whose type arguments were not inferable) is
        // constrained to the supplied argument's type. The caller back-patches
        // these into the receiver variable. `this` is the first parameter, so
        // it is skipped to align with the explicit argument types.
        for (param, argument_type) in method.arguments.values().skip(1).zip(argument_types.iter()) {
            let param_name = param.argument_type.name();
            if param_name.starts_with('?') {
                self.last_call_inference
                    .entry(param_name.to_string())
                    .or_insert_with(|| argument_type.clone());
            }
        }

        // Visibility check: private methods can only be called from
        // within a method on the same class (i.e. when we have a `this`).
        if method.visibility.private && self.current_this.is_none() {
            return Err(format!(
                "cannot call private method `{method_name_str}` on type `{object_value_type}` from outside the class. Private methods are only accessible from other methods of the same class",
            ));
        }

        // Record the callee's mutation status so a caller can propagate it
        // (24.2 rule 2).
        self.last_called_method_mutates = method.visibility.mutates;

        // A `[mutates]` method cannot be called on a `const` value (21.2).
        if object_value_type.is_const() && method.visibility.mutates {
            return Err(format!(
                "cannot call `[mutates]` method `{method_name_str}` on a `const` value of type `{object_value_type}`. A const value is immutable; cast it to a mutable type with `as` first",
            ));
        }

        // Detect "every argument has a default" - relevant to the
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
            if let Some(var_args_type) = &method.var_args_type {
                let mut var_arguments = Vec::new();
                for arg in arguments.iter().skip(break_index) {
                    var_arguments.push(arg.clone());
                }

                if self
                    .create_standard_array(var_args_type, var_arguments)
                    .is_none()
                {
                    return Err(format!(
                        "variadic arguments to method `{method_name_str}` could not be packed into an array of type `{}`. At least one variadic argument has a type that does not match the declared variadic type",
                        var_args_type,
                    ));
                }
            };
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

        // Method-level generics: infer each parameter the method declares itself
        // by unifying its declared argument types against the supplied ones, then
        // substitute into the result, mirroring the codegen so both observe the
        // same concrete element type.
        let mut result_type = method.return_type.clone();
        if !method.method_generic_typenames.is_empty() {
            let method_names: std::collections::HashSet<String> = method
                .method_generic_typenames
                .iter()
                .map(|name| name.value.clone())
                .collect();
            let mut method_substitution = HashMap::new();
            for (declared, actual) in method.arguments.values().skip(1).zip(argument_types.iter()) {
                types::infer_generic_bindings(
                    &declared.argument_type,
                    actual,
                    &method_names,
                    &mut method_substitution,
                );
            }
            if !method_substitution.is_empty() {
                result_type = types::substitute_generic_params(&result_type, &method_substitution);
            }
        }

        Ok(SimulatorValue::Value(result_type))
    }

    /// Constructs an `Array<T>` simulator value.
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

        let mut array_object_type = types::PekoType::from_string("Array", String::new());
        array_object_type.generics_mut().push(array_type.clone());

        Some(SimulatorValue::Value(array_object_type))
    }

    /// Constructs a `Map<K, V>` simulator value.
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

        let mut map_object_type = types::PekoType::from_string("Map", String::new());
        map_object_type.generics_mut().push(key_type.clone());
        map_object_type.generics_mut().push(value_type.clone());

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
        let class_to_create = self.get_class_by_type(class_type)?;

        // The simulator's "allocation" is just a typed value - see
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
        // Enum-to-enum `==` / `!=`. Checked before expansion because a bare
        // cross-module enum value does not expand in the importing module. A
        // qualified enum type and a bare reference to the same enum compare
        // equal, so the operands match one qualified and one bare.
        if matches!(operator.to_string().as_str(), "==" | "!=")
            && self.enum_types_match(&lhs.get_type(), &rhs.get_type())
        {
            return Some(SimulatorValue::Value(types::PekoType::simple_type("i1")));
        }

        let lhs_value_type = self.expand_type(&lhs.get_type());
        let rhs_value_type = self.expand_type(&rhs.get_type());

        if lhs_value_type.is_none() || rhs_value_type.is_none() {
            return None;
        }

        let lhs_value_type = lhs_value_type.unwrap();
        let rhs_value_type = rhs_value_type.unwrap();

        // Closure-to-closure: any operator returns the raw boolean (closures
        // are first-class and the language permits comparisons).
        if lhs_value_type.is_closure()
            && rhs_value_type.is_closure()
            && self.types_equal(&lhs_value_type, &rhs_value_type)
        {
            return Some(SimulatorValue::Value(types::PekoType::simple_type("i1")));
        }

        let mut lhs = lhs.clone();

        // `&&` / `||` between bool and i1 (in any mix) short-circuit on the raw
        // i1 rather than routing through the And/Or trait, yielding i1.
        let operator_is_logical = matches!(operator.to_string().as_str(), "&&" | "||");
        let is_bool_like = |t: &PekoType| t.name() == "bool" || t.name() == "i1";
        if operator_is_logical && is_bool_like(&lhs_value_type) && is_bool_like(&rhs_value_type) {
            return Some(SimulatorValue::Value(types::PekoType::simple_type("i1")));
        }

        // Enum-to-enum: an enum lowers to its i32 variant index, so `==` / `!=`
        // compares those indices and yields a raw i1.
        if matches!(operator.to_string().as_str(), "==" | "!=")
            && self.get_enum_variants(lhs_value_type.name()).is_some()
            && self.get_enum_variants(rhs_value_type.name()).is_some()
        {
            return Some(SimulatorValue::Value(types::PekoType::simple_type("i1")));
        }

        // Erased carrier operand: route the operator to its core trait method,
        // resolved through the carrier's bounds. A `KT: impl Equals` carrier
        // supports `==` through the Equals trait's `equals`, mirroring the
        // codegen's bound-driven dispatch.
        if lhs_value_type.is_generic_param() {
            let operator_str = operator.to_string();
            if let Some(method_name) = types::operator_trait_method(&operator_str) {
                for restraint in lhs_value_type.restraints() {
                    let types::TypeRestraint::Impl(trait_type) = restraint else {
                        continue;
                    };
                    let Some(trait_definition) = self.get_trait(trait_type.name()) else {
                        continue;
                    };
                    if let Some(slot) = trait_definition
                        .methods
                        .iter()
                        .find(|method| method.name.as_str() == method_name)
                    {
                        return Some(SimulatorValue::Value(slot.return_type.clone()));
                    }
                }
            }
        }

        // Object types: route the operator to its core trait method (`+` ->
        // `plus`, `==` -> `equals`, and so on). An operator with no core trait
        // keeps the legacy `[operator <op>]` member name.
        if self.get_class_by_type(&lhs_value_type).is_some() {
            let operator_str = operator.to_string();
            let overload_name = types::operator_trait_method(&operator_str)
                .map_or_else(|| format!("[operator {operator_str}]"), str::to_string);

            let call_overload =
                self.call_object_method(&lhs, overload_name.clone(), vec![rhs.clone()], None);

            if let Ok(value) = call_overload {
                return Some(value);
            }

            // Overload didn't work - try to cast the lhs to the rhs's
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
            // here to ensure the rhs is actually valid
            let _rhs = self.box_value_to_type(&lhs.get_type(), rhs)?;

            let operation_type = match operator.to_string().as_str() {
                // Arithmetic operators yield the lhs type.
                "+" | "-" | "*" | "/" | "%" | "^" => lhs_value_type.clone(),

                // Comparison and boolean operators on raw scalars yield the raw
                // boolean i1. It auto-boxes to the bool object where one is
                // expected; conditions branch on it directly.
                _ => types::PekoType::simple_type("i1"),
            };

            return Some(SimulatorValue::Value(operation_type));
        }

        // String-to-string equality / inequality.
        if lhs_value_type.is_string_type()
            && rhs_value_type.is_string_type()
            && matches!(operator.to_string().as_str(), "==" | "!=")
        {
            return Some(SimulatorValue::Value(types::PekoType::simple_type("i1")));
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
                || lhs_value_type.name() == "pointer"
                || self.get_class_by_type(&lhs_value_type).is_some();
            let rhs_is_reference_like = rhs_value_type.is_pointer()
                || rhs_value_type.name() == "pointer"
                || self.get_class_by_type(&rhs_value_type).is_some();

            // A null / `opaque` value compares to any reference form without a
            // similarity check, matching the codegen's pointer comparison (a
            // null pointer is address-space-0 opaque and compares to any
            // pointer). Otherwise the two reference types must be similar.
            let either_opaque =
                lhs_value_type.name() == "opaque" || rhs_value_type.name() == "opaque";

            if lhs_is_reference_like
                && rhs_is_reference_like
                && (either_opaque || self.types_similar(&lhs_value_type, &rhs_value_type))
            {
                return Some(SimulatorValue::Value(types::PekoType::simple_type("i1")));
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
            // Caller wants an lvalue - bump the reference depth so the
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
