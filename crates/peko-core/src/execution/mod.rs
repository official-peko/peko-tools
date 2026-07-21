//! # Peko Core Execution
//!
//! Backend-agnostic algorithms over the [`data_structures`] trait
//! interfaces.
//!
//! This module provides the heavy logic shared between Pekoscript backends:
//!
//! * **Module stack management** ([`ExecutionModuleContext`]): a
//!   push/pop stack of `Arc<RwLock<ModuleType>>` references with flags for
//!   whether the current frame is mid-access of another module and whether
//!   it represents a generic instantiation in progress.
//! * **Symbol resolution** ([`ExecutionContextAlgorithms`]): finding
//!   modules, classes, functions, and variables in the current scope and
//!   its ancestors.
//! * **Type expansion**: resolving generic types and fully-qualifying
//!   class names against the module tree.
//! * **Overload resolution**: picking the best function from an overload
//!   set given concrete argument types.
//!
//! Both the simulator and the LLVM codegen build their own concrete
//! `ModuleType`, `ValueType`, `VariableType`, etc., and then implement
//! [`ExecutionContextAlgorithms`] over those types to inherit every
//! algorithm here.

pub mod data_structures;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use data_structures::{
    EnumDefinition, ExecutionArgument, ExecutionClass, ExecutionClassAttribute,
    ExecutionClassVirtualTable, ExecutionFunction, ExecutionModule, ExecutionValue,
    ExecutionVariable, TraitDefinition,
};
use indexmap::IndexMap;

use crate::asts::PekoAST;
use crate::asts::data_structures::{PositionData, PositionedValue, StringChunk};
use crate::asts::expressions::ObjectConstructionAST;
use crate::asts::values::StringAST;
use crate::diagnostics;
use crate::types::PekoType;
use crate::types::TypeRestraint;
use crate::{ExternalModuleInfo, ExternalModuleVersion};

/// One frame of the module-resolution stack.
///
/// The execution context maintains a stack of these so that nested module
/// accesses (`outer::inner::Symbol`) and generic-class instantiations can
/// be unwound back to whatever scope was active before they began.
#[derive(Clone)]
pub struct ModuleStackEntry<ModuleType> {
    /// The module this frame represents.
    pub module: Arc<RwLock<ModuleType>>,

    /// `true` if this frame was pushed for the duration of accessing a
    /// member of `module` from outside it (e.g. evaluating an
    /// `other_module::symbol` reference).
    pub accessing: bool,

    /// `true` if this frame was pushed for generic-class or
    /// generic-function instantiation rather than ordinary execution.
    pub for_generic: bool,
}

impl<ModuleType> ModuleStackEntry<ModuleType> {
    /// Constructs a new module stack entry.
    pub fn new(module: Arc<RwLock<ModuleType>>, accessing: bool, for_generic: bool) -> Self {
        Self {
            module,
            accessing,
            for_generic,
        }
    }
}

/// Module-resolution state shared across the execution algorithms.
///
/// The context tracks the active module stack and the registry of
/// top-level (importable) modules, including the special `main` module
/// where execution begins and the `extern` module where externally-linked
/// declarations live.
#[derive(Clone)]
pub struct ExecutionModuleContext<ModuleType> {
    /// Active module stack. The last entry is the *current* module (what
    /// most callers care about). Earlier entries describe outer scopes that
    /// will be returned to when the current one is popped.
    pub module_stack: Vec<ModuleStackEntry<ModuleType>>,

    /// Top-level modules accessible by name. Always contains `"main"`
    /// (initial entry point) and `"extern"` (linked-symbol home).
    pub top_level_modules: IndexMap<String, Arc<RwLock<ModuleType>>>,

    /// Convenience handle on the `extern` module since it's frequently
    /// queried during external-symbol resolution.
    pub extern_module: Arc<RwLock<ModuleType>>,
}

impl<ModuleType> ExecutionModuleContext<ModuleType> {
    /// Constructs a fresh context with `main_module` as the starting frame
    /// and `extern_module` registered for external symbols.
    pub fn new(
        main_module: Arc<RwLock<ModuleType>>,
        extern_module: Arc<RwLock<ModuleType>>,
    ) -> Self {
        let mut top_level_modules = IndexMap::new();
        top_level_modules.insert("main".to_string(), main_module.clone());
        top_level_modules.insert("extern".to_string(), extern_module.clone());

        Self {
            module_stack: vec![ModuleStackEntry::new(main_module, false, false)],
            top_level_modules,
            extern_module,
        }
    }

    /// Merges additional top-level modules into the registry, overwriting
    /// existing entries with the same name. If `top_levels` contains an
    /// `"extern"` entry, the convenience handle is updated to match.
    pub fn load_modules(&mut self, top_levels: IndexMap<String, Arc<RwLock<ModuleType>>>) {
        if top_levels.contains_key("extern") {
            self.extern_module = top_levels["extern"].clone();
        }

        self.top_level_modules.extend(top_levels);
    }

    /// Returns the module on top of the stack.
    pub fn current_module(&self) -> Arc<RwLock<ModuleType>> {
        self.module_stack.last().unwrap().module.clone()
    }

    /// Returns `true` if the top of the stack was pushed for an
    /// inter-module access.
    pub fn accessing_current_module(&self) -> bool {
        self.module_stack.last().unwrap().accessing
    }

    /// Returns `true` if the top of the stack was pushed for a generic
    /// instantiation.
    pub fn current_module_for_generic(&self) -> bool {
        self.module_stack.last().unwrap().for_generic
    }

    /// Pushes `module` onto the stack with the given access/generic flags.
    pub fn move_to_module(
        &mut self,
        module: Arc<RwLock<ModuleType>>,
        accessing: bool,
        generic_generation: bool,
    ) {
        self.module_stack
            .push(ModuleStackEntry::new(module, accessing, generic_generation));
    }

    /// Pops the top frame and returns its module.
    pub fn move_out_of_module(&mut self) -> Arc<RwLock<ModuleType>> {
        self.module_stack.pop().unwrap().module
    }

    /// Pops every frame currently flagged as an inter-module access,
    /// returning them in stack order so they can be restored with
    /// [`Self::step_forward`].
    pub fn step_back(&mut self) -> Vec<ModuleStackEntry<ModuleType>> {
        let mut moved_out = Vec::new();

        while self.accessing_current_module() {
            moved_out.insert(0, self.module_stack.pop().unwrap());
        }

        moved_out
    }

    /// Variant of [`Self::step_back`] that also pops generic-instantiation
    /// frames.
    pub fn step_back_generics(&mut self) -> Vec<ModuleStackEntry<ModuleType>> {
        let mut moved_out = Vec::new();

        while self.accessing_current_module() || self.current_module_for_generic() {
            moved_out.insert(0, self.module_stack.pop().unwrap());
        }

        moved_out
    }

    /// Restores frames previously removed by [`Self::step_back`] or
    /// [`Self::step_back_generics`].
    pub fn step_forward(&mut self, post_stack: Vec<ModuleStackEntry<ModuleType>>) {
        self.module_stack.extend(post_stack);
    }
}

/// The outcome of resolving an `import` path to a module on disk.
///
/// Produced by [`ExecutionContextAlgorithms::resolve_module`] and consumed
/// by both backends so they share one resolution path. `info` is a real
/// [`ExternalModuleInfo`] for registry packages and a synthesized one for
/// local modules. `entry_file` is the concrete `.peko` file to load.
/// `module_id` is the canonical identifier used to discriminate unpacked
/// symbols. `is_registry` is `true` when the path resolved to an on-system
/// registry package. `new_root_folder` is the root folder to use while the
/// resolved module loads.
#[derive(Clone)]
pub struct ResolvedModule {
    pub info: ExternalModuleInfo,
    pub entry_file: PathBuf,
    pub module_id: String,
    pub is_registry: bool,
    pub new_root_folder: PathBuf,
}

/// The umbrella trait that ties together every execution algorithm.
///
/// Implementations supply the concrete types for modules, values,
/// variables, functions, classes, and their generic counterparts. The
/// trait then provides default implementations for every cross-cutting
/// algorithm (symbol lookup, type expansion, overload resolution, etc.)
/// and a small set of required methods for backend-specific bits
/// (constructing concrete values, instantiating generics, applying
/// operators).
///
/// The bound on `ModuleType` matches the F-bounded shape from
/// [`ExecutionModule`] so that all the companion types are consistent
/// throughout the trait.
pub trait ExecutionContextAlgorithms<
    ModuleType: ExecutionModule<
            ModuleType,
            ValueType,
            VariableType,
            FunctionType,
            ArgumentType,
            ClassType,
            ClassVirtualTableType,
            ClassAttributeType,
        > + Clone,
    ValueType: ExecutionValue,
    VariableType: ExecutionVariable<ValueType, ModuleType> + Clone,
    FunctionType: ExecutionFunction<ArgumentType, ModuleType> + Clone,
    ArgumentType: ExecutionArgument,
    ClassType: ExecutionClass<ClassType, ClassVirtualTableType, ClassAttributeType, ModuleType> + Clone,
    ClassVirtualTableType: ExecutionClassVirtualTable<FunctionType>,
    ClassAttributeType: ExecutionClassAttribute,
>
{
    /// Generic type substitutions currently in scope (e.g. `T` -> `String`
    /// inside a generic function body).
    fn get_generic_types(&self) -> &HashMap<String, PekoType>;

    /// Mutable view of the generic-type substitutions.
    fn get_generic_types_mut(&mut self) -> &mut HashMap<String, PekoType>;

    /// The variant names of a registered enum, in declaration order, or
    /// `None` when no enum by that name is reachable from the current module.
    ///
    /// Walks up the module tree from the current module, mirroring how class
    /// names resolve.
    fn get_enum_variants(&self, enum_name: &str) -> Option<Vec<String>> {
        let mut current = self.get_module_context().current_module();

        loop {
            if let Some(definition) = current.read().unwrap().get_enums().get(enum_name) {
                return Some(definition.variants.clone());
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Registers an enum, its variant names, and its visibility in the current
    /// module so the name resolves as a type, its variants resolve through
    /// `Enum::Variant`, and a `[private]` enum is rejected across modules.
    fn register_enum(&mut self, enum_name: String, variants: Vec<String>, private: bool) {
        self.get_module_context()
            .current_module()
            .write()
            .unwrap()
            .get_enums_mut()
            .insert(enum_name, EnumDefinition { variants, private });
    }

    /// The trait definition registered under `trait_name`, or `None` when no
    /// trait by that name is reachable from the current module. Walks up the
    /// module tree, mirroring how class and enum names resolve.
    fn get_trait(&self, trait_name: &str) -> Option<TraitDefinition> {
        let mut current = self.get_module_context().current_module();

        loop {
            if let Some(definition) = current.read().unwrap().get_traits().get(trait_name) {
                return Some(definition.clone());
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Registers a trait definition in the current module so its name resolves
    /// as a type and classes can declare conformance to it.
    fn register_trait(&mut self, definition: TraitDefinition) {
        self.get_module_context()
            .current_module()
            .write()
            .unwrap()
            .get_traits_mut()
            .insert(definition.name.clone(), definition);
    }

    /// The `this` binding active in the current scope (set when inside a
    /// method body), or `None` outside method scope.
    fn get_current_this(&self) -> &Option<VariableType>;

    /// Mutable view of the current `this` binding.
    fn get_current_this_mut(&mut self) -> &mut Option<VariableType>;

    /// Shared module-resolution state.
    fn get_module_context(&self) -> &ExecutionModuleContext<ModuleType>;

    /// Mutable view of the module-resolution state.
    fn get_module_context_mut(&mut self) -> &mut ExecutionModuleContext<ModuleType>;

    /// The package registry: every module discoverable in the local and
    /// global Peko packages directories, keyed by module name.
    fn get_external_modules(&self) -> &IndexMap<String, ExternalModuleInfo>;

    /// The root folder that `project::` paths resolve against and that
    /// canonical module ids are rooted at.
    fn get_root_folder(&self) -> &PathBuf;

    /// Mutable view of the root folder. The creator of the context sets
    /// the initial value, and the import logic reassigns it while a
    /// registry package loads.
    fn get_root_folder_mut(&mut self) -> &mut PathBuf;

    /// Backend hook: instantiates a generic function with concrete type
    /// arguments. Returns `None` if instantiation fails (e.g. mismatched
    /// parameter count).
    fn create_generic_function(
        &mut self,
        generic: &FunctionType,
        type_parameters: Vec<PekoType>,
    ) -> Option<FunctionType>;

    /// Backend hook: instantiates a generic class with concrete type
    /// arguments.
    fn create_generic_class(
        &mut self,
        generic: &ClassType,
        type_parameters: Vec<PekoType>,
    ) -> Option<ClassType>;

    /// Returns the source-file path the currently-executing code lives in.
    ///
    /// Steps back through any inter-module access frames so that the
    /// reported file reflects the *original* source location rather than
    /// whichever module is currently being inspected.
    fn get_current_file(&self) -> std::path::PathBuf {
        let mut module_stack = self.get_module_context().clone();
        module_stack.step_back();
        module_stack
            .current_module()
            .read()
            .unwrap()
            .get_file()
            .to_path_buf()
    }

    /// Backend hook: the diagnostic sink the current pass writes errors and
    /// warnings into. The simulator and the code generator each own a list, so
    /// the shared algorithms can report against whichever pass is running.
    fn get_diagnostics_mut(&mut self) -> &mut diagnostics::DiagnosticList;

    /// Verify each concrete type argument satisfies its generic parameter's
    /// declared bounds. `impl Trait` requires the argument's class (or an
    /// ancestor) to implement the trait; `from Class` requires it to be that
    /// class or a descendant. A violated bound is reported as an error. Bound
    /// checking is compile-time only and is identical under the simulator and
    /// the code generator, so it lives on the shared trait and both call it
    /// from their instantiation path.
    fn check_generic_bounds(
        &mut self,
        generic_typenames: &[PositionedValue<String>],
        generic_bounds: &IndexMap<String, Vec<TypeRestraint>>,
        concrete_types: &[PekoType],
    ) {
        for (typename, concrete) in generic_typenames.iter().zip(concrete_types.iter()) {
            let Some(bounds) = generic_bounds.get(&typename.value) else {
                continue;
            };

            for bound in bounds {
                let message = match bound {
                    TypeRestraint::Impl(trait_type) => {
                        if self.concrete_satisfies_impl(concrete, trait_type.name()) {
                            continue;
                        }
                        format!(
                            "type `{concrete}` does not satisfy the bound `{}: impl {trait_type}`. The type must implement trait `{trait_type}`",
                            typename.value,
                        )
                    }
                    TypeRestraint::From(parent_type) => {
                        if self.concrete_satisfies_from(concrete, parent_type.name()) {
                            continue;
                        }
                        format!(
                            "type `{concrete}` does not satisfy the bound `{}: from {parent_type}`. The type must be `{parent_type}` or inherit from it",
                            typename.value,
                        )
                    }
                };

                let file = self.get_current_file();
                let diagnostic = diagnostics::PekoDiagnostic::new(
                    concrete.start_position.clone(),
                    concrete.end_position.clone(),
                    message,
                    diagnostics::DiagnosticType::Error,
                    file,
                );

                // The erased backend re-resolves a generic type at every use, so
                // the same violation can reach this point many times. Report it
                // once per source site: skip it when an identical diagnostic
                // (same span, file, and message) is already recorded.
                let already_reported = self.get_diagnostics_mut().iter().any(|existing| {
                    existing.start.index == diagnostic.start.index
                        && existing.start.file == diagnostic.start.file
                        && existing.message == diagnostic.message
                });
                if !already_reported {
                    self.get_diagnostics_mut().report_diagnostic(diagnostic);
                }
            }
        }
    }

    /// Walk the concrete type's class hierarchy; satisfied when any level lists
    /// the trait in its `impl` clause.
    fn concrete_satisfies_impl(&mut self, concrete: &PekoType, trait_name: &str) -> bool {
        // A value already typed as the trait itself (a trait object, or the
        // erased carrier used to type-check a generic body) satisfies the bound.
        if concrete.name() == trait_name {
            return true;
        }

        // An erased carrier satisfies a bound when the trait is among the
        // restraints it carries (a `KT: impl Hash, impl Equals` carrier
        // satisfies both Hash and Equals).
        if concrete.is_generic_param()
            && concrete.restraints().iter().any(|restraint| {
                matches!(restraint, TypeRestraint::Impl(trait_type) if trait_type.name() == trait_name)
            })
        {
            return true;
        }

        let mut class = self.get_class_by_type(concrete);
        while let Some(current) = class {
            if current
                .get_implemented_trait_names()
                .iter()
                .any(|name| name == trait_name)
            {
                return true;
            }
            class = current.get_parent_class().cloned();
        }
        false
    }

    /// Walk the concrete type's class hierarchy; satisfied when any level's
    /// class name matches the bound class.
    fn concrete_satisfies_from(&mut self, concrete: &PekoType, parent_name: &str) -> bool {
        let mut class = self.get_class_by_type(concrete);
        while let Some(current) = class {
            if current.get_class_type().name() == parent_name {
                return true;
            }
            class = current.get_parent_class().cloned();
        }
        false
    }

    /// Parses a version string of the form `v1.2.3` into its numeric
    /// components. The leading `v` is dropped and each dot-separated piece
    /// is parsed as an integer. A piece that does not parse counts as 0.
    fn parse_version_components(version: &str) -> Vec<u64> {
        let trimmed = version.strip_prefix('v').unwrap_or(version);
        trimmed
            .split('.')
            .map(|piece| piece.parse::<u64>().unwrap_or(0))
            .collect()
    }

    /// Compares two version strings by numeric component. Returns the
    /// ordering of `left` relative to `right`. Missing trailing components
    /// count as 0, so `v1.2` and `v1.2.0` compare equal.
    fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
        let left_parts = Self::parse_version_components(left);
        let right_parts = Self::parse_version_components(right);
        let length = left_parts.len().max(right_parts.len());

        for index in 0..length {
            let left_value = left_parts.get(index).copied().unwrap_or(0);
            let right_value = right_parts.get(index).copied().unwrap_or(0);
            let ordering = left_value.cmp(&right_value);
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }

        std::cmp::Ordering::Equal
    }

    /// Selects an installed version of an external module.
    ///
    /// A pin that names an installed version wins. Otherwise the greatest
    /// version by numeric component comparison is chosen. Returns `None` when
    /// no versions are installed.
    fn select_external_version<'v>(
        versions: &'v [ExternalModuleVersion],
        pin: Option<&str>,
    ) -> Option<&'v ExternalModuleVersion> {
        if let Some(pin) = pin
            && let Some(found) = versions.iter().find(|entry| entry.version == pin)
        {
            return Some(found);
        }
        versions
            .iter()
            .max_by(|left, right| Self::compare_versions(&left.version, &right.version))
    }

    /// Resolves a name inside a local directory using the local precedence
    /// order. The sibling file `<base>/<name>.peko` wins, then the folder
    /// entry `<base>/<name>/main.peko`, then `<base>/<name>/page.peko`, then
    /// the FFI header `<base>/<name>.peko.h`. Returns the first candidate that
    /// exists.
    fn resolve_local_name(base: &Path, name: &str) -> Option<PathBuf> {
        let sibling = base.join(format!("{name}.peko"));
        if sibling.exists() {
            return Some(sibling);
        }

        let folder_main = base.join(name).join("main.peko");
        if folder_main.exists() {
            return Some(folder_main);
        }

        let folder_page = base.join(name).join("page.peko");
        if folder_page.exists() {
            return Some(folder_page);
        }

        let ffi_header = base.join(format!("{name}.peko.h"));
        if ffi_header.exists() {
            return Some(ffi_header);
        }

        None
    }

    /// Builds the canonical module id for a resolved local file.
    ///
    /// The resolved path is made relative to the root folder. Each
    /// directory of the resolved file that sits above the root contributes
    /// a `parent` token. The remaining in-root path components contribute
    /// their own names, the `.peko` extension is dropped, and a trailing
    /// `main` component collapses into its containing directory. The
    /// `root_token` (the project name or a package name) leads the id, and
    /// all parts join with `__`.
    fn build_module_id(root_folder: &Path, resolved_file: &Path, root_token: &str) -> String {
        let canonical_file = resolved_file
            .canonicalize()
            .unwrap_or_else(|_| resolved_file.to_path_buf());
        let canonical_root = root_folder
            .canonicalize()
            .unwrap_or_else(|_| root_folder.to_path_buf());

        let mut parent_tokens = Vec::new();
        let mut walk_root = canonical_root.clone();

        // Climb up from the root until the resolved file sits underneath
        // it. Each climb above the root adds one parent token.
        let relative = loop {
            if let Ok(stripped) = canonical_file.strip_prefix(&walk_root) {
                break stripped.to_path_buf();
            }

            match walk_root.parent() {
                Some(parent) => {
                    parent_tokens.push(String::from("parent"));
                    walk_root = parent.to_path_buf();
                }
                None => break canonical_file.clone(),
            }
        };

        let mut segments = vec![String::from(root_token)];
        segments.extend(parent_tokens);

        let mut components: Vec<String> = relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(piece) => Some(piece.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();

        // Drop the file extension on the final component.
        if let Some(last) = components.last_mut()
            && let Some(stripped) = last.strip_suffix(".peko")
        {
            *last = String::from(stripped);
        }

        // A trailing `main` collapses into its containing directory.
        if components.last().map(String::as_str) == Some("main") {
            components.pop();
        }

        segments.extend(components);
        segments.join("__")
    }

    /// Resolves an `import` path to a module on disk.
    ///
    /// `path_ids` is the import path split into identifier segments.
    /// `version` is the optional version pin for a registry package.
    /// `importing_file` is the file the `import` statement appears in, used
    /// for `parent::` resolution and plain local resolution.
    ///
    /// The first segment selects a starting directory. `project` uses the
    /// root folder. `parent` uses the directory one level above the
    /// importing file's directory. A name found in the package registry
    /// uses that package's versioned directory and reassigns the root
    /// folder. A plain name uses the importing file's directory. `extern`
    /// is reserved and cannot be imported.
    ///
    /// From the starting directory the path segments are walked. Every
    /// segment except the last descends into a subdirectory of the same
    /// name. The last segment resolves to a file using the local
    /// precedence: `<dir>/<name>.peko`, then `<dir>/<name>/main.peko`,
    /// then `<dir>/<name>/page.peko`. Registry walks use only the
    /// `<name>.peko` and `<name>/main.peko` forms. Returns `None` when no
    /// candidate file exists.
    fn resolve_module(
        &self,
        path_ids: &[String],
        version: Option<&str>,
        importing_file: &Path,
    ) -> Option<ResolvedModule> {
        if path_ids.is_empty() {
            return None;
        }

        let first = path_ids[0].as_str();

        // extern is reserved and auto-imported everywhere.
        if first == "extern" {
            return None;
        }

        let importing_dir = importing_file.parent().unwrap_or_else(|| Path::new("."));
        let root_folder = self.get_root_folder().clone();

        let mut is_registry = false;
        let mut new_root_folder = root_folder.clone();
        let mut root_token = String::from("project");
        let mut registry_entry_file: Option<String> = None;

        // base_dir is the directory the next segment resolves against.
        // walk_segments are the segments still to resolve from base_dir.
        let base_dir: PathBuf;
        let walk_segments: &[String];

        if first == "project" {
            base_dir = root_folder.clone();
            walk_segments = &path_ids[1..];
        } else if first == "parent" {
            base_dir = importing_dir
                .parent()
                .unwrap_or(importing_dir)
                .to_path_buf();
            walk_segments = &path_ids[1..];
        } else if let Some(module_info) = self.get_external_modules().get(first) {
            // Registry package. The version pin selects the installed version,
            // or the latest version when no pin was given or the pin is missing
            // from the installed set. Submodules and the entry resolve within
            // the version's source root.
            is_registry = true;
            root_token = String::from(first);

            let chosen = Self::select_external_version(&module_info.versions, version)?;
            new_root_folder = chosen.source_root.clone();
            base_dir = chosen.source_root.clone();
            registry_entry_file = Some(chosen.entry_file.clone());
            walk_segments = &path_ids[1..];
        } else {
            // Plain local name relative to the importing file's directory.
            base_dir = importing_dir.to_path_buf();
            walk_segments = path_ids;
        }

        // A registry import with no further segments uses the package
        // entry directly.
        if is_registry && walk_segments.is_empty() {
            let entry = registry_entry_file.as_deref().unwrap_or("main.peko");
            let current_file = base_dir.join(entry);
            return self.finish_resolution(
                first,
                path_ids,
                current_file,
                is_registry,
                root_token,
                root_folder,
                new_root_folder,
            );
        }

        if walk_segments.is_empty() {
            return None;
        }

        // Walk every segment except the last as a directory descent, then
        // resolve the last segment to a file.
        let mut current_dir = base_dir;
        let last_index = walk_segments.len() - 1;

        for segment in &walk_segments[..last_index] {
            current_dir = current_dir.join(segment);
        }

        let last_segment = &walk_segments[last_index];

        let current_file = if is_registry {
            let sibling = current_dir.join(format!("{last_segment}.peko"));
            if sibling.exists() {
                sibling
            } else {
                let folder_main = current_dir.join(last_segment).join("main.peko");
                if folder_main.exists() {
                    folder_main
                } else {
                    return None;
                }
            }
        } else {
            Self::resolve_local_name(&current_dir, last_segment)?
        };

        self.finish_resolution(
            first,
            path_ids,
            current_file,
            is_registry,
            root_token,
            root_folder,
            new_root_folder,
        )
    }

    /// The root token a locally-resolved import leads its module id with.
    ///
    /// When `root_folder` matches an external package's source root, the
    /// package name is the token, so a package's own sibling imports build the
    /// same module id as the same file imported by package path. Otherwise the
    /// token is `project`, for the top-level project's own files.
    fn local_root_token(&self, root_folder: &Path) -> String {
        let canonical_root = root_folder
            .canonicalize()
            .unwrap_or_else(|_| root_folder.to_path_buf());

        for (name, info) in self.get_external_modules() {
            for version in &info.versions {
                let canonical_source = version
                    .source_root
                    .canonicalize()
                    .unwrap_or_else(|_| version.source_root.clone());
                if canonical_source == canonical_root {
                    return name.clone();
                }
            }
        }

        String::from("project")
    }

    /// Builds the final [`ResolvedModule`] from a resolved entry file.
    ///
    /// Computes the canonical module id, synthesizes a local
    /// [`ExternalModuleInfo`] when the import is not a registry package,
    /// and packages the entry file together with the root folder to use
    /// while the module loads.
    fn finish_resolution(
        &self,
        first: &str,
        path_ids: &[String],
        current_file: PathBuf,
        is_registry: bool,
        root_token: String,
        root_folder: PathBuf,
        new_root_folder: PathBuf,
    ) -> Option<ResolvedModule> {
        let module_id = if is_registry {
            Self::build_module_id(&new_root_folder, &current_file, &root_token)
        } else {
            // A local import resolves relative to the root folder. While a
            // package loads, the root folder is the package source root, so a
            // package's own sibling import (a bare `core` from inside std)
            // roots under the package token and shares identity with the same
            // file imported by its package path (`std::core`). Project files
            // root under the project token.
            let local_token = self.local_root_token(&root_folder);
            Self::build_module_id(&root_folder, &current_file, &local_token)
        };

        let entry_name = current_file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("main.peko"));

        let entry_dir = current_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let info = if is_registry {
            self.get_external_modules().get(first).cloned().unwrap()
        } else {
            ExternalModuleInfo::new(
                String::from(path_ids.last().map(String::as_str).unwrap_or(first)),
                String::from("local module"),
                vec![ExternalModuleVersion::new(
                    String::new(),
                    entry_dir,
                    entry_name,
                )],
            )
        };

        Some(ResolvedModule {
            info,
            entry_file: current_file,
            module_id,
            is_registry,
            new_root_folder,
        })
    }

    /// Walks up the module tree from the current frame looking for a
    /// sub-module named `module_name`. Returns the matching module
    /// reference, or `None` if no ancestor contains one.
    fn find_module_in_current(
        &self,
        module_name: impl ToString,
    ) -> Option<Arc<RwLock<ModuleType>>> {
        let mut current = self.get_module_context().current_module();
        if current.read().unwrap().get_name() == module_name.to_string() {
            return Some(current);
        }

        loop {
            for (modname, module) in current.read().unwrap().get_modules() {
                if &module_name.to_string() == modname {
                    return Some(module.clone());
                }
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Resolves a module path such as `["webview"]` or `["a", "b"]` to the
    /// module it names, walking the same way qualified class types resolve: the
    /// first segment is found in the current module tree or the top-level
    /// modules, and each further segment steps into a submodule. Returns `None`
    /// when any segment does not resolve.
    fn resolve_qualified_module(&self, module_names: &[String]) -> Option<Arc<RwLock<ModuleType>>> {
        if module_names.is_empty() {
            return None;
        }
        let mut names = module_names.to_vec();
        let aliased = self
            .get_module_context()
            .current_module()
            .read()
            .unwrap()
            .get_module_aliases()
            .get(&names[0])
            .cloned();
        let mut contained_in = if let Some(aliased) = aliased {
            aliased
        } else if let Some(module) = self.find_module_in_current(&names[0]) {
            module
        } else if self
            .get_module_context()
            .top_level_modules
            .contains_key(&names[0])
        {
            self.get_module_context().top_level_modules[&names[0]].clone()
        } else {
            return None;
        };
        names.remove(0);

        for module_name in &names {
            if !contained_in
                .read()
                .unwrap()
                .get_modules()
                .contains_key(module_name)
            {
                if contained_in.read().unwrap().get_name() == module_name {
                    break;
                }
                return None;
            }
            let parent = contained_in.read().unwrap().get_modules()[module_name].clone();
            contained_in = parent;
        }

        Some(contained_in)
    }

    /// Whether `type1` names an enum, whether by a bare name reachable up the
    /// current module tree or by a module-qualified path. Generic, array, and
    /// function types are never enums.
    fn is_reachable_enum(&self, type1: &PekoType) -> bool {
        if !type1.generics().is_empty() || type1.is_function() {
            return false;
        }
        if self.get_enum_variants(type1.name()).is_some() {
            return true;
        }
        !type1.module_names().is_empty()
            && self
                .resolve_qualified_module(type1.module_names())
                .map(|module| {
                    module
                        .read()
                        .unwrap()
                        .get_enums()
                        .contains_key(type1.name())
                })
                .unwrap_or(false)
    }

    /// Walks up the module tree looking for a class named `class_name`.
    fn find_class_in_current(&self, class_name: impl ToString) -> Option<ClassType> {
        let mut current = self.get_module_context().current_module();

        loop {
            for (classname, class) in current.read().unwrap().get_classes() {
                if &class_name.to_string() == classname {
                    return Some(class.read().unwrap().clone());
                }
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Whether `module` holds a generic-class template under `name`: a class
    /// stored under its bare name that still holds its source AST.
    fn module_has_class_template(&self, module: &Arc<RwLock<ModuleType>>, name: &str) -> bool {
        module
            .read()
            .unwrap()
            .get_classes()
            .get(name)
            .map(|class| class.read().unwrap().get_source_class().is_some())
            .unwrap_or(false)
    }

    /// Whether `module` holds a generic-function template under `name`: an
    /// overload stored under the bare name that still holds its source AST.
    fn module_has_function_template(&self, module: &Arc<RwLock<ModuleType>>, name: &str) -> bool {
        module
            .read()
            .unwrap()
            .get_functions()
            .get(name)
            .map(|overloads| {
                overloads
                    .iter()
                    .any(|overload| overload.read().unwrap().get_source_function().is_some())
            })
            .unwrap_or(false)
    }

    /// The generic-function template overload under `name` in `module`, if any.
    fn module_function_template(
        &self,
        module: &Arc<RwLock<ModuleType>>,
        name: &str,
    ) -> Option<FunctionType> {
        module
            .read()
            .unwrap()
            .get_functions()
            .get(name)?
            .iter()
            .find(|overload| overload.read().unwrap().get_source_function().is_some())
            .map(|overload| overload.read().unwrap().clone())
    }

    /// Walks up the module tree looking for a generic-class template named
    /// `generic_name`. A template is a class stored under its bare name that
    /// still holds its source AST (it has type parameters awaiting
    /// instantiation).
    fn find_class_generic_in_current(&self, generic_name: impl ToString) -> Option<ClassType> {
        let mut current = self.get_module_context().current_module();
        let target = generic_name.to_string();

        loop {
            if let Some(class) = current.read().unwrap().get_classes().get(&target)
                && class.read().unwrap().get_source_class().is_some()
            {
                return Some(class.read().unwrap().clone());
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// The module a class named `class_name` is declared in, found by walking up
    /// the current module tree. Returns only the parent-module handle, reading
    /// it through the class without cloning the class. Used on the hot type
    /// resolution path, where the full class value is not needed.
    fn find_class_parent_module_in_current(
        &self,
        class_name: impl ToString,
    ) -> Option<Arc<RwLock<ModuleType>>> {
        let mut current = self.get_module_context().current_module();
        let target = class_name.to_string();

        loop {
            if let Some(class) = current.read().unwrap().get_classes().get(&target) {
                return Some(class.read().unwrap().get_parent_module());
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// The module a generic-class template named `generic_name` is declared in.
    /// Mirrors `find_class_generic_in_current` but returns only the parent
    /// module handle, avoiding a full class clone on the type resolution path.
    fn find_class_generic_parent_module_in_current(
        &self,
        generic_name: impl ToString,
    ) -> Option<Arc<RwLock<ModuleType>>> {
        let mut current = self.get_module_context().current_module();
        let target = generic_name.to_string();

        loop {
            if let Some(class) = current.read().unwrap().get_classes().get(&target)
                && class.read().unwrap().get_source_class().is_some()
            {
                return Some(class.read().unwrap().get_parent_module());
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Walks up the module tree looking for a function overload set named
    /// `function_name`.
    fn find_function_in_current(&self, function_name: impl ToString) -> Option<Vec<FunctionType>> {
        let mut current = self.get_module_context().current_module();

        loop {
            for (funcname, func) in current.read().unwrap().get_functions() {
                if &function_name.to_string() == funcname {
                    return Some(func.iter().map(|f| f.read().unwrap().clone()).collect());
                }
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Walks up the module tree looking for a generic-function template named
    /// `generic_name`: an overload stored under the bare name that still holds
    /// its source AST.
    fn find_function_generic_in_current(
        &self,
        generic_name: impl ToString,
    ) -> Option<FunctionType> {
        let mut current = self.get_module_context().current_module();
        let target = generic_name.to_string();

        loop {
            if let Some(overloads) = current.read().unwrap().get_functions().get(&target) {
                for overload in overloads {
                    if overload.read().unwrap().get_source_function().is_some() {
                        return Some(overload.read().unwrap().clone());
                    }
                }
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Walks up the module tree looking for a top-level (global) variable
    /// named `variable_name`.
    fn find_global_variable_in_current(
        &self,
        variable_name: impl ToString,
    ) -> Option<VariableType> {
        let mut current = self.get_module_context().current_module();

        loop {
            for (varname, var) in current.read().unwrap().get_variables() {
                if &variable_name.to_string() == varname {
                    return Some(var.read().unwrap().clone());
                }
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    // ----- Simulator/codegen-shared algorithms -----------------------------

    /// Expands a type expression to its fully-qualified form.
    ///
    /// * Substitutes any in-scope generic types.
    /// * Converts trailing `?` modifiers to `standard::Option<T>` wrappers.
    /// * Recursively expands argument types and return types of function
    ///   and closure types.
    /// * Walks the module tree to fully qualify class names with their
    ///   declaring module path.
    ///
    /// Returns `None` if any inner type can't be resolved.
    fn expand_type(&mut self, type1: &PekoType) -> Option<PekoType> {
        let mut type1 = type1.clone();

        // A type that already carries a full expansion needs no re-resolution.
        // Expansion qualifies every module path to its absolute form and
        // recursively expands generic arguments, so the result is
        // context-independent. The flag is set only by a completed expansion,
        // so a set flag means every module walk and generic instantiation for
        // this type already ran. Reusing it skips the whole resolution below,
        // which is the hottest path in both the simulator and codegen.
        if type1.fully_expanded {
            return Some(type1);
        }

        // Error types cannot be expanded.
        if type1.is_error_type() {
            type1.fully_expanded = true;
            return Some(type1);
        }

        // Save the non-definition type info.
        let mut array_depth = type1.array_depth;
        let mut reference_depth = type1.reference_depth;

        // Expand a generic type. e.g. T = String, T -> String.
        while self.get_generic_types().contains_key(type1.name()) {
            let mut replacement = self.get_generic_types()[type1.name()].clone();

            // Add all the non-definition type info back in.
            replacement.array_depth += array_depth;
            replacement.reference_depth += reference_depth;

            // An erased generic parameter is mapped to a bounded `Generic`
            // carrier of its own name. It is terminal: adopt the carrier
            // (keeping its bounds) and stop, so the parameter is not chased
            // into an infinite self-substitution.
            let self_mapped = replacement.is_generic_param() && replacement.name() == type1.name();

            array_depth = replacement.array_depth;
            reference_depth = replacement.reference_depth;
            type1 = replacement;

            if self_mapped {
                break;
            }
        }

        // A generic parameter is a terminal named type during erased
        // compilation. It lowers to a thin managed object pointer and its
        // bounds drive dispatch, so it expands to itself.
        if type1.is_generic_param() {
            type1.fully_expanded = true;
            return Some(type1);
        }

        // Expand the argument and return types of functions/closures.
        if type1.is_function() {
            // Expand the argument types.
            for argument_type in type1.generics_mut().iter_mut() {
                let type_expanded = self.expand_type(argument_type);
                *argument_type = type_expanded?;
            }

            // If the function has a return type, then expand that too.
            if let Some(return_type) = type1.function_return().cloned() {
                let return_type_expanded = self.expand_type(&return_type)?;
                type1.set_function_return(Some(return_type_expanded));
            }

            type1.fully_expanded = true;
            return Some(type1);
        }

        // Get the type on its own, no other clutter.
        let type1_no_pointers = PekoType::from_string(type1.name(), "");

        // Built-in types cannot be expanded (they have no class or modules).
        if type1_no_pointers.is_datatype() {
            type1.fully_expanded = true;
            return Some(type1);
        }

        // Enums are terminal named types backed by an integer. They have no
        // class body or modules to expand, so they expand to themselves. A
        // module-qualified enum (`fs::OpenMode`) is terminal too; it keeps its
        // qualifier so a later comparison can match a bare reference to the same
        // enum, and this stops it from falling through to class resolution,
        // which would report it as undefined.
        if self.get_enum_variants(type1.name()).is_some() || self.is_reachable_enum(&type1) {
            type1.fully_expanded = true;
            return Some(type1);
        }

        // Traits are terminal named types. A value of trait type is a fat
        // pointer (self plus witness). A trait has no class body or modules to
        // expand, so it expands to itself.
        if self.get_trait(type1.name()).is_some() {
            type1.fully_expanded = true;
            return Some(type1);
        }

        // For any generic type within this type, expand that.
        // e.g. standard::Map<String, File> -> standard::Map<standard::String, fs::File>.
        //
        // Generic type arguments are written at the use site, so they are
        // resolved against the call module, not the module currently being
        // accessed. step_back pops any inter-module access frames so an
        // unqualified argument name (for example Subscriber in
        // Mutex<Map<int, Subscriber>>) is found in the module that wrote it.
        // When there are no access frames on top, step_back pops nothing and
        // this is a plain expansion. step_forward always runs, including on
        // the failure path, so the stack is never left short.
        let argument_post_stack = self.get_module_context_mut().step_back();
        let mut argument_expansion_failed = false;
        for generic_type in type1.generics_mut().iter_mut() {
            match self.expand_type(generic_type) {
                Some(expanded) => *generic_type = expanded,
                None => {
                    // An unresolved bare identifier in a generic-argument
                    // position is an erased carrier: a generic parameter with no
                    // binding in the current context (for example `Option<U>`
                    // where the method parameter U is erased). Keep it as a
                    // terminal generic parameter so the enclosing class type
                    // still resolves, matching erasure. A name with a module path
                    // or nested generics is a genuine lookup failure.
                    if generic_type.module_names().is_empty()
                        && generic_type.generics().is_empty()
                        && !generic_type.is_function()
                        && !generic_type.name().is_empty()
                    {
                        let mut carrier =
                            PekoType::generic_type(generic_type.name().to_string(), Vec::new());
                        carrier.array_depth = generic_type.array_depth;
                        carrier.reference_depth = generic_type.reference_depth;
                        *generic_type = carrier;
                    } else {
                        argument_expansion_failed = true;
                        break;
                    }
                }
            }
        }
        self.get_module_context_mut()
            .step_forward(argument_post_stack);
        if argument_expansion_failed {
            return None;
        }

        if type1.is_base_type() || type1.name() == "pointer" {
            type1.fully_expanded = true;
            return Some(type1);
        }

        // Convert the type to a string that can be interpreted as a class name.
        let mut module_names = type1.module_names().to_vec();
        let class_name = type1.declutter().to_string();
        let mut class_name_base = type1.clone().declutter();
        class_name_base.generics_mut().clear();
        let class_name_base = class_name_base.to_string();

        let mut class_parent = if !module_names.is_empty() {
            // The first segment may be a per-file alias (`import pekoui as ui`)
            // bound in the current module, a sub-module reachable up the current
            // tree, or a top-level module by name. A memoized/reused package
            // stays in top_level_modules under its own name while the alias
            // lives only in module_aliases, so the alias is resolved first.
            let aliased = self
                .get_module_context()
                .current_module()
                .read()
                .unwrap()
                .get_module_aliases()
                .get(&module_names[0])
                .cloned();
            let mut contained_in = if let Some(aliased) = aliased {
                aliased
            } else if let Some(found) = self.find_module_in_current(&module_names[0]) {
                found
            } else if let Some(top) = self
                .get_module_context()
                .top_level_modules
                .get(&module_names[0])
            {
                top.clone()
            } else {
                return None;
            };
            module_names.remove(0);

            for module_name in &module_names {
                if !contained_in
                    .read()
                    .unwrap()
                    .get_modules()
                    .contains_key(module_name)
                {
                    if contained_in.read().unwrap().get_name() == module_name {
                        break;
                    }

                    return None;
                }

                let parent = contained_in.read().unwrap().get_modules()[module_name].clone();
                contained_in = parent;
            }

            let has_concrete = contained_in
                .read()
                .unwrap()
                .get_classes()
                .contains_key(&class_name);
            let has_base_template = self.module_has_class_template(&contained_in, &class_name_base);
            if has_concrete || has_base_template {
                let lookup_name = if has_concrete {
                    &class_name
                } else {
                    &class_name_base
                };
                contained_in.read().unwrap().get_classes()[lookup_name]
                    .read()
                    .unwrap()
                    .get_parent_module()
            } else {
                return None;
            }
        } else if let Some(parent) = self.find_class_parent_module_in_current(type1.name()) {
            parent
        } else if let Some(parent) = self.find_class_generic_parent_module_in_current(&class_name) {
            parent
        } else {
            self.find_class_generic_parent_module_in_current(&class_name_base)?
        };

        if !class_parent
            .read()
            .unwrap()
            .get_classes()
            .contains_key(&class_name)
            && self.module_has_class_template(&class_parent, &class_name_base)
        {
            let generic = class_parent.read().unwrap().get_classes()[&class_name_base]
                .read()
                .unwrap()
                .clone();

            // Instantiate the generic class. Under erasure the compiled class is
            // shared across instantiations, so the expanded type keeps its own
            // concrete generic arguments (used for use-site member
            // substitution) and is qualified below, rather than adopting the
            // compiled class's parameter-named type.
            self.create_generic_class(&generic, type1.generics().to_vec())?;
        }

        type1.module_names_mut().clear();
        loop {
            type1
                .module_names_mut()
                .insert(0, class_parent.read().unwrap().get_name().to_owned());
            if class_parent.read().unwrap().get_parent().is_none() {
                break;
            }
            let parent = class_parent.read().unwrap().get_parent().unwrap().clone();
            class_parent = parent;
        }

        type1.fully_expanded = true;
        Some(type1)
    }

    /// Returns `true` if `peko_type` resolves to a valid type in scope.
    fn type_exists(&mut self, peko_type: &PekoType) -> bool {
        // Error types are treated as poison (they technically exist).
        if peko_type.is_error_type() {
            return true;
        }

        // An erased generic parameter is a valid bounded type. It maps to a
        // carrier of its own name, so resolving it through the generic-type
        // substitution below would recurse forever; treat it as terminal.
        if peko_type.is_generic_param() {
            return true;
        }

        if peko_type.name() == "pointer" && !peko_type.generics().is_empty() {
            return self.type_exists(&peko_type.generics()[0]);
        }

        // If it's a closure, convert it into its equivalent function type
        // and type-check that. More efficient than separate code for
        // closure types.
        if peko_type.is_closure() {
            let mut function_type = peko_type.clone();
            function_type
                .generics_mut()
                .insert(0, PekoType::simple_type("opaque"));
            function_type.set_closure(false);

            if !function_type.is_function() {
                function_type.set_function_return(Some(PekoType::simple_type("void")));
            }

            return self.type_exists(&function_type);
        }

        // Check function types.
        if let Some(return_type) = peko_type.function_return() {
            // First, check all the argument types.
            for argument_type in peko_type.generics() {
                if !self.type_exists(argument_type) {
                    return false;
                }
            }
            // Then check the return type.
            return self.type_exists(return_type);
        }

        // If the current type is a generic type, substitute it for its
        // actual type and check that.
        // e.g. T -> string | context.type_exists(T) -> context.type_exists(string).
        if self.get_generic_types().contains_key(peko_type.name()) {
            return self.type_exists(&self.get_generic_types()[peko_type.name()].clone());
        }

        // Enums are user-defined named types backed by an integer. They do not
        // expand to a class definition, so recognize them before expansion,
        // which would otherwise fail and report the enum as undefined. A
        // module-qualified enum (`fs::OpenMode`) is recognized here too, since a
        // qualified enum name resolves to no class.
        if self.is_reachable_enum(peko_type) {
            return true;
        }

        // Expand the type to its full official definition.
        let type_expanded = self.expand_type(peko_type);
        let Some(peko_type) = type_expanded else {
            return false;
        };

        match peko_type.name() {
            // Built-in FFI scalars obviously exist. bool, char, and string are
            // object wrappers and resolve through the class fallthrough below.
            "opaque" | "i32" | "i16" | "i128" | "i64" | "f32" | "f64" | "f16" | "void" | "cstr"
            | "i1" | "i8" => true,
            _ => {
                // Check generic types again -- perhaps the expanded type name
                // exists as a generic type.
                if self.get_generic_types().contains_key(peko_type.name()) {
                    self.type_exists(&self.get_generic_types()[peko_type.name()].clone())
                } else if self.get_enum_variants(peko_type.name()).is_some() {
                    // Enums are user-defined named types backed by an integer.
                    true
                } else if self.get_trait(peko_type.name()).is_some() {
                    // Traits are usable as types: a value of trait type is a
                    // fat pointer (self plus witness).
                    true
                } else {
                    // Otherwise, the type must be user-defined. `get_class_by_type`
                    // returns None if there's no user-defined (class) type
                    // for that type -- handy shortcut.
                    let class = self.get_class_by_type(&peko_type);
                    class.is_some()
                }
            }
        }
    }

    /// Resolves the class declaration for a given type, walking the module
    /// tree and instantiating generic-class declarations as needed.
    fn get_class_by_type(&mut self, type1: &PekoType) -> Option<ClassType> {
        // Error types cannot have a class type.
        if type1.is_error_type() {
            return None;
        }

        // Expand the type all the way.
        let type_expanded = self.expand_type(type1)?;

        // A generic parameter is not a class. An erased parameter is a thin
        // managed object; dispatch on it is bound-driven (the from-class or
        // impl-trait carrier), resolved at the call site, not here.
        if type_expanded.is_generic_param() {
            return None;
        }

        // Convert the type to a class name.
        let class_name = type_expanded.declutter().to_string();

        // Find the module the class is defined in, based on the expanded
        // type's module path. The first segment may be a per-file alias
        // (`import pekoui as ui`), a top-level module by name, or the current
        // module. A memoized/reused package stays in top_level_modules under
        // its own name while the alias lives only in the importing module's
        // module_aliases, so the alias is resolved first.
        let aliased_first = if !type_expanded.module_names().is_empty() {
            self.get_module_context()
                .current_module()
                .read()
                .unwrap()
                .get_module_aliases()
                .get(&type_expanded.module_names()[0])
                .cloned()
        } else {
            None
        };

        let (mut next_module, starting_point) = if let Some(aliased) = aliased_first {
            (aliased, 1)
        } else if !type_expanded.module_names().is_empty()
            && self
                .get_module_context()
                .top_level_modules
                .contains_key(&type_expanded.module_names()[0])
        {
            (
                self.get_module_context().top_level_modules[&type_expanded.module_names()[0]]
                    .clone(),
                1,
            )
        } else if !type_expanded.module_names().is_empty() && {
            let current = self.get_module_context().current_module();
            current.read().unwrap().get_name() == type_expanded.module_names()[0]
        } {
            (self.get_module_context().current_module(), 1)
        } else {
            (self.get_module_context().current_module(), 0)
        };

        for i in starting_point..type_expanded.module_names().len() {
            if !next_module
                .read()
                .unwrap()
                .get_modules()
                .contains_key(&type_expanded.module_names()[i])
            {
                if next_module.read().unwrap().get_name() == type_expanded.module_names()[i] {
                    if next_module.read().unwrap().get_name() == type_expanded.module_names()[i] {
                        break;
                    }

                    break;
                }

                return None;
            }

            let parent = next_module.as_ref().read().unwrap().get_modules()
                [&type_expanded.module_names()[i]]
                .clone();
            next_module = parent;
        }

        // The current module might not contain the class directly, it
        // might be a generic awaiting instantiation.
        if !next_module
            .read()
            .unwrap()
            .get_classes()
            .contains_key(&class_name)
        {
            if !type_expanded.generics().is_empty()
                && self.module_has_class_template(&next_module, type_expanded.name())
            {
                // Instantiate the template using the provided generic types as
                // the type parameters.
                let next_generic = next_module.clone().read().unwrap().get_classes()
                    [type_expanded.name()]
                .read()
                .unwrap()
                .clone();

                return self.create_generic_class(&next_generic, type_expanded.generics().to_vec());
            }

            return None;
        }

        Some(
            next_module.clone().read().unwrap().get_classes()[&class_name]
                .read()
                .unwrap()
                .clone(),
        )
    }

    /// Returns `true` if `class1` derives from `class2` through any chain
    /// of inheritance.
    ///
    /// `class1` should be at or below `class2` in the inheritance
    /// hierarchy. The check is reflexive (a class connects to itself).
    fn classes_connect(&self, class1: &ClassType, class2: &ClassType) -> bool {
        // Same class type means they connect (reflexive).
        if class1.get_class_type().to_string() == class2.get_class_type().to_string() {
            return true;
        }

        // Walk class1's parent chain looking for class2.
        if class1.get_parent_class().is_none() {
            return false;
        }

        let mut class1_parent = class1.get_parent_class().unwrap();

        while class1_parent.get_class_type().to_string() != class2.get_class_type().to_string() {
            if class1_parent.get_parent_class().is_none() {
                break;
            }

            class1_parent = class1_parent.get_parent_class().unwrap();
        }

        class1_parent.get_class_type().to_string() == class2.get_class_type().to_string()
    }

    /// Whether a value of `source` type can flow into a `target` slot with
    /// respect to const-ness. Const is added automatically (`T` to `const T`)
    /// but is never dropped (`const T` to `T`) without an explicit `as`.
    fn const_compatible(&self, source: &PekoType, target: &PekoType) -> bool {
        target.is_const() || !source.is_const()
    }

    /// Returns `true` if two types are similar enough to be cast between.
    ///
    /// Looser than [`Self::types_equal`]: pointer-to-pointer casts, casts
    /// between built-in types, casts up or down a class hierarchy, and
    /// casts via user-defined `operator to_X` conversions all count.
    /// If `type1` is a value-type wrapper -- a class whose only user attribute
    /// is a single non-array, non-reference scalar named `raw` -- returns that
    /// raw scalar type. An FFI scalar value auto-boxes into such a wrapper (for
    /// example an `f64` into `number`). `string` is not one of these (it has
    /// two fields).
    fn value_wrapper_raw(&mut self, type1: &PekoType) -> Option<PekoType> {
        let class = self.get_class_by_type(type1)?;
        let mut user_attributes = class
            .get_attributes()
            .iter()
            .filter(|(name, _)| name.as_str() != "<main_virtual_table>");
        let (name, attribute) = user_attributes.next()?;
        if user_attributes.next().is_some() || name != "raw" {
            return None;
        }
        let raw_type = attribute.get_attribute_type();
        if raw_type.array_depth > 0 || raw_type.reference_depth > 0 {
            return None;
        }
        Some(raw_type.clone())
    }

    fn types_similar(&mut self, type1: &PekoType, type2: &PekoType) -> bool {
        // Error types "work" with any other type.
        if type1.is_error_type() || type2.is_error_type() {
            return true;
        }

        // Equal types are trivially similar.
        if self.types_equal(type1, type2) {
            return true;
        }

        // Auto-box: a raw FFI scalar value (type1) coerces up into its
        // value-type wrapper (type2) when the wrapper's raw field accepts it.
        // One-directional: only ffi-value to wrapper, never the reverse, so a
        // wrapper is never silently treated as a raw scalar.
        if self.get_class_by_type(type1).is_none()
            && let Some(raw_type) = self.value_wrapper_raw(type2)
            && self.types_similar(type1, &raw_type)
        {
            return true;
        }

        // Expand both types for proper checking.
        let type1_expanded = self.expand_type(type1);
        let type2_expanded = self.expand_type(type2);

        if type1_expanded.is_none() || type2_expanded.is_none() {
            return false;
        }

        let type1_expanded = type1_expanded.unwrap();
        let type2_expanded = type2_expanded.unwrap();

        // A value coerces into an Option of a compatible inner type. The value
        // is the source (type1) and the Option's inner type is the target, so
        // the similarity is checked value-to-inner -- the same direction as the
        // surrounding coercion, which matters for the one-directional auto-box.
        if type2_expanded.name() == "Option"
            && type2_expanded.generics().len() == 1
            && self.types_similar(&type1_expanded, &type2_expanded.generics()[0])
        {
            return true;
        }

        // Managed string and the raw string type name the same logical type.
        let type1_is_managed_string = type1.name() == "string";
        let type2_is_managed_string = type2.name() == "string";

        if (type1_is_managed_string || type1.is_string_type())
            && (type2_is_managed_string || type2.is_string_type())
        {
            return true;
        }

        // A subclass coerces up to a superclass. Every other conversion -- a
        // downcast, an unrelated class, a different builtin width, a pointer
        // retype, an untyped pointer to a class -- is not implicit and needs
        // an explicit `as` or `danger_cast`.
        let type1_class = self.get_class_by_type(&type1_expanded);
        let type2_class = self.get_class_by_type(&type2_expanded);

        if let (Some(class1), Some(class2)) = (&type1_class, &type2_class)
            && self.classes_connect(class1, class2)
        {
            return true;
        }

        false
    }

    /// Returns `true` only if two types are exactly equal after full
    /// expansion.
    /// The common value type of an `if` used as an expression, or `None` when
    /// the `if` is a plain statement.
    ///
    /// An expression `if` requires an else and every branch reaching the merge
    /// with a tail value (`Some`) of one common, non-void type. The simulator
    /// and codegen call this with the same branch tails so they agree on
    /// whether the `if` produces a value.
    fn if_expression_value_type(
        &mut self,
        else_present: bool,
        branch_tails: &[Option<PekoType>],
    ) -> Option<PekoType> {
        if !else_present || branch_tails.is_empty() || branch_tails.iter().any(Option::is_none) {
            return None;
        }

        let first = branch_tails[0].clone().unwrap();
        let first_string = first.to_string();
        if first_string == "default" || first_string == "void" {
            return None;
        }

        for tail in branch_tails {
            let tail = tail.as_ref().unwrap();
            if !self.types_equal(tail, &first) {
                return None;
            }
        }

        Some(first)
    }

    /// Whether two types name the same enum reached one qualified and one bare.
    ///
    /// A `module::Enum::Variant` access yields a value typed by the bare enum
    /// name, while an annotation or parameter keeps the module qualifier, so
    /// `webview::WebViewHint` and `WebViewHint` name the same enum. Expansion
    /// cannot reconcile them, since a bare cross-module enum name does not
    /// resolve in the importing module. The bare names must match and at least
    /// one side must resolve to an enum; neither unqualified side may name a
    /// distinct concrete type (a class or trait of that name), which would
    /// otherwise be wrongly unified with the enum.
    fn enum_types_match(&mut self, type1: &PekoType, type2: &PekoType) -> bool {
        if type1.name() != type2.name()
            || type1.array_depth != type2.array_depth
            || type1.reference_depth != type2.reference_depth
            || !type1.generics().is_empty()
            || !type2.generics().is_empty()
            || type1.is_function()
            || type2.is_function()
        {
            return false;
        }

        if !self.is_reachable_enum(type1) && !self.is_reachable_enum(type2) {
            return false;
        }

        for side in [type1, type2] {
            if side.module_names().is_empty()
                && self.get_enum_variants(side.name()).is_none()
                && (self.get_class_by_type(side).is_some() || self.get_trait(side.name()).is_some())
            {
                return false;
            }
        }

        true
    }

    fn types_equal(&mut self, type1: &PekoType, type2: &PekoType) -> bool {
        // Error types "work" with any type.
        if type1.is_error_type() || type2.is_error_type() {
            return true;
        }

        // A module-qualified enum and a bare reference to the same enum are the
        // same type, reconciled before expansion since a bare cross-module enum
        // name does not resolve in the importer.
        if self.enum_types_match(type1, type2) {
            return true;
        }

        // Expand the types for proper checking.
        let type1_expanded = self.expand_type(type1);
        let type2_expanded = self.expand_type(type2);

        if type1_expanded.is_none() || type2_expanded.is_none() {
            return false;
        }

        let type1_expanded = type1_expanded.unwrap();
        let type2_expanded = type2_expanded.unwrap();

        self.types_equal_expanded(&type1_expanded, &type2_expanded)
    }

    /// Structural equality over two already-expanded types. An erased generic
    /// parameter matches any type (every instantiation shares one thin-pointer
    /// slot and the parameter's bounds were enforced at the instantiation site),
    /// so a parameter compares equal wherever it appears, including nested in a
    /// generic argument: `Map<KT, VT>` matches `Map<string, number>`.
    fn types_equal_expanded(&self, type1: &PekoType, type2: &PekoType) -> bool {
        // A generic parameter is a wildcard.
        if type1.is_generic_param() || type2.is_generic_param() {
            return true;
        }

        if type1.array_depth != type2.array_depth
            || type1.reference_depth != type2.reference_depth
            || type1.is_function() != type2.is_function()
            // A function and a `closure(...)` are distinct types: a bare function
            // reference cannot stand in for a closure (it has no environment, and
            // coercing one produces a broken thunk that crashes at the call).
            || type1.is_closure() != type2.is_closure()
            || type1.name() != type2.name()
            || type1.module_names() != type2.module_names()
            || type1.generics().len() != type2.generics().len()
        {
            return false;
        }

        for (generic1, generic2) in type1.generics().iter().zip(type2.generics().iter()) {
            if !self.types_equal_expanded(generic1, generic2) {
                return false;
            }
        }

        match (type1.function_return(), type2.function_return()) {
            (Some(return1), Some(return2)) => self.types_equal_expanded(return1, return2),
            (None, None) => true,
            _ => false,
        }
    }

    /// Whether a value of `argument_type` may be passed to a parameter of
    /// `parameter_type`. An exact type match always works; a subclass argument
    /// is also accepted for a superclass parameter (an implicit upcast, since a
    /// subclass is everywhere usable as its base). Other conversions are not
    /// implicit and need an explicit `as` / `danger_cast`.
    fn argument_matches_parameter(
        &mut self,
        argument_type: &PekoType,
        parameter_type: &PekoType,
    ) -> bool {
        if self.types_equal(argument_type, parameter_type) {
            return true;
        }

        // Depth must agree: an upcast applies to a bare instance, not an array
        // or reference of one.
        if argument_type.array_depth + argument_type.reference_depth
            != parameter_type.array_depth + parameter_type.reference_depth
        {
            return false;
        }

        // A class argument coerces to a trait it implements (implicit trait
        // object), so `f(x)` stands in for `f(x as Trait)`.
        if self.get_trait(parameter_type.name()).is_some()
            && let Some(argument_class) = self.get_class_by_type(argument_type)
            && argument_class
                .get_implemented_trait_names()
                .iter()
                .any(|name| name == parameter_type.name())
        {
            return true;
        }

        match (
            self.get_class_by_type(argument_type),
            self.get_class_by_type(parameter_type),
        ) {
            (Some(argument_class), Some(parameter_class)) => {
                self.classes_connect(&argument_class, &parameter_class)
            }
            _ => false,
        }
    }

    /// Picks the matching function from `function_choices` for the supplied
    /// positional and keyword argument types.
    ///
    /// A choice matches only when every argument type is exactly equal to the
    /// corresponding parameter type. Implicit casts are gone, so a
    /// similar-but-unequal argument is not a candidate and the caller must
    /// cast explicitly. Argument-count, keyword, default-value, and variadic
    /// rules still gate which choices are eligible:
    ///
    /// 1. A choice taking more arguments than were provided is rejected
    ///    (unless every argument has a default and none were provided).
    /// 2. A keyword arg that does not exist on the choice rejects it.
    /// 3. A single unequal-type argument rejects the whole choice.
    ///
    /// Returns `Some((index, choice))` for the first fully-equal choice, or
    /// `None` if nothing matched.
    fn choose_function_and_index(
        &mut self,
        function_choices: Vec<FunctionType>,
        argument_types: Vec<PekoType>,
        provided_arguments: Option<HashMap<String, PekoType>>,
        class_function: bool,
    ) -> Option<(usize, FunctionType)> {
        if function_choices.is_empty() {
            return None;
        }

        // Tracks the closest match across all candidates.
        let mut max_type_match_score = 0;
        let mut closest_function = function_choices.first().unwrap().clone();
        let mut closest_function_index = 0;

        for (current_function_idx, function) in function_choices.iter().enumerate() {
            let mut current_type_match_score = 0;

            // Check if every argument has a default value (keyword-only call works).
            let mut all_arguments_keywords = !function.get_arguments().is_empty()
                || (class_function && function.get_arguments().len() > 1);

            for (_, arg) in function
                .get_arguments()
                .iter()
                .skip(usize::from(class_function))
            {
                if !arg.has_default_value() {
                    all_arguments_keywords = false;
                    break;
                }
            }

            // Score keyword-supplied arguments first.
            if let Some(provided_argument_map) = provided_arguments.as_ref() {
                for (argument_name, argument_type) in provided_argument_map.iter() {
                    if function.get_arguments().contains_key(argument_name)
                        && function.get_arguments()[argument_name].has_default_value()
                    {
                        // An exactly-equal type or an implicit upcast matches.
                        // Other conversions are not implicit, and const cannot be
                        // dropped.
                        if self.argument_matches_parameter(
                            argument_type,
                            function.get_arguments()[argument_name].get_argument_type(),
                        ) && self.const_compatible(
                            argument_type,
                            function.get_arguments()[argument_name].get_argument_type(),
                        ) {
                            current_type_match_score += 2;
                        } else {
                            current_type_match_score = 0;
                            break;
                        }

                    // Rule 3: provided keyword doesn't exist on the choice.
                    } else {
                        current_type_match_score = 0;
                        break;
                    }
                }

            // Rule 4: every arg has a default and none were provided.
            } else if all_arguments_keywords
                && (argument_types.is_empty() || (class_function && argument_types.len() == 1))
            {
                current_type_match_score += 1;

            // Check positional arguments.
            } else if function.get_arguments().len() <= argument_types.len() {
                // Rule 1.
                if argument_types.len() == function.get_arguments().len() {
                    current_type_match_score = 1;
                }

                for (index, (_, arg)) in function.get_arguments().iter().enumerate() {
                    if function.get_var_args_type().is_some()
                        && index == function.get_arguments().len() - 1
                    {
                        break;
                    }

                    // Rule 2.
                    if index >= argument_types.len() {
                        current_type_match_score = 0;
                        break;
                    }

                    // An exactly-equal type or an implicit upcast matches, and
                    // const cannot be dropped to satisfy the parameter.
                    if !self
                        .argument_matches_parameter(&argument_types[index], arg.get_argument_type())
                        || !self.const_compatible(&argument_types[index], arg.get_argument_type())
                    {
                        current_type_match_score = 0;
                        break;
                    }

                    current_type_match_score += 2;
                }

                // Score any provided variadic arguments.
                if function.get_var_args_type().is_some()
                    && argument_types.len() > function.get_arguments().len()
                {
                    for argument_type in
                        &argument_types[(function.get_arguments().len())..argument_types.len()]
                    {
                        // An exactly-equal type or an implicit upcast matches,
                        // and const cannot be dropped.
                        if !self.argument_matches_parameter(
                            argument_type,
                            function.get_var_args_type().unwrap(),
                        ) || !self
                            .const_compatible(argument_type, function.get_var_args_type().unwrap())
                        {
                            current_type_match_score = 0;
                            break;
                        }

                        current_type_match_score += 2;
                    }
                }
            }

            // Does this choice beat the current best?
            if current_type_match_score > max_type_match_score
                // Make sure the choice actually passed all checks -- it's
                // possible for a failed choice to score above zero.
                && (provided_arguments.is_some()
                    || (argument_types.len() == function.get_arguments().len()
                        || (argument_types.len() >= function.get_arguments().len()
                            && (function.get_visibility().variadic
                                || function.get_var_args_type().is_some()))
                        || (all_arguments_keywords
                            && (argument_types.is_empty()
                                || (class_function && argument_types.len() == 1)))))
            {
                max_type_match_score = current_type_match_score;
                closest_function = function.clone();
                closest_function_index = current_function_idx;
            }
        }

        if max_type_match_score == 0 {
            None
        } else {
            Some((closest_function_index, closest_function))
        }
    }

    /// Wrapper around [`Self::choose_function_and_index`] that drops the
    /// index for callers that only need the chosen function.
    fn choose_function(
        &mut self,
        function_choices: Vec<FunctionType>,
        argument_types: Vec<PekoType>,
        provided_arguments: Option<HashMap<String, PekoType>>,
        class_function: bool,
    ) -> Option<FunctionType> {
        self.choose_function_and_index(
            function_choices,
            argument_types,
            provided_arguments,
            class_function,
        )
        .map(|(_, choice)| choice)
    }

    /// Returns `true` if the named attribute on `object`'s class is
    /// declared `state` (i.e. visible to subscribers tracking state
    /// changes).
    fn is_attribute_state(&mut self, object: &ValueType, attribute_name: impl ToString) -> bool {
        let object_class = self.get_class_by_type(&object.get_type());
        let object_class = match object_class {
            Some(c) => c,
            None => return false,
        };

        object_class
            .get_attributes()
            .contains_key(&attribute_name.to_string())
            && object_class.get_attributes()[&attribute_name.to_string()]
                .get_visibility()
                .state
    }

    /// Backend hook: applies a binary operator between two values,
    /// returning the resulting value or `None` if the operator can't be
    /// applied.
    fn apply_operator(
        &mut self,
        operator: impl ToString,
        lhs: &ValueType,
        rhs: &ValueType,
    ) -> Option<ValueType>;

    /// Backend hook: reads `attribute_name` off `object`. Returns `Err`
    /// with a human-readable explanation if the attribute can't be found
    /// or isn't visible at this call site.
    fn get_object_attribute(
        &mut self,
        object: &ValueType,
        attribute_name: impl ToString,
        load_value: bool,
    ) -> Result<ValueType, String>;

    /// Backend hook: writes `value` to `attribute_name` on `object`.
    /// Returns `true` on success.
    fn set_object_attribute(
        &mut self,
        object: &ValueType,
        attribute_name: impl ToString,
        value: &ValueType,
    ) -> bool;

    /// Backend hook: invokes `method_name` on `object` with `arguments`
    /// (and optional keyword arguments).
    fn call_object_method(
        &mut self,
        object: &ValueType,
        method_name: impl ToString,
        arguments: Vec<ValueType>,
        provided_arguments: Option<HashMap<String, ValueType>>,
    ) -> Result<ValueType, String>;

    /// Backend hook: invokes a named function. The name may include the
    /// module path if needed for resolution.
    fn call_named_function(
        &mut self,
        function_name: impl ToString,
        function_arguments: Vec<ValueType>,
    ) -> Option<ValueType>;

    /// Backend hook: constructs a `standard::Array` value where every
    /// element is equal to or similar to `array_type`.
    fn create_standard_array(
        &mut self,
        array_type: &PekoType,
        values: Vec<ValueType>,
    ) -> Option<ValueType>;

    /// Backend hook: constructs a `standard::Map` value with explicit
    /// key and value types.
    fn create_standard_map(
        &mut self,
        key_type: &PekoType,
        value_type: &PekoType,
        key_value_pairs: Vec<(ValueType, ValueType)>,
    ) -> Option<ValueType>;

    /// Builds an AST node that constructs a `standard::String` from a
    /// literal `value`. Used by codegen when synthesizing string objects
    /// at internal boundaries.
    fn create_standard_string_ast(&mut self, value: impl ToString) -> PekoAST {
        PekoAST::ObjectConstruction(ObjectConstructionAST::new(
            PositionData::default(),
            PositionData::default(),
            PositionedValue::create_no_position(String::from("String")),
            Vec::new(),
            vec![(
                None,
                PekoAST::String(StringAST::new(
                    PositionData::default(),
                    PositionData::default(),
                    false,
                    vec![StringChunk::new_text(
                        PositionData::default(),
                        PositionData::default(),
                        value.to_string(),
                    )],
                )),
            )],
        ))
    }

    /// Backend hook: constructs an object value of `class_type` by
    /// invoking its constructor with `constructor_arguments`.
    fn create_object(
        &mut self,
        class_type: &PekoType,
        constructor_arguments: Vec<ValueType>,
    ) -> Option<ValueType>;
}
