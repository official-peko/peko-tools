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
    ExecutionArgument, ExecutionClass, ExecutionClassAttribute, ExecutionClassGeneric,
    ExecutionClassVirtualTable, ExecutionFunction, ExecutionFunctionGeneric, ExecutionModule,
    ExecutionValue, ExecutionVariable,
};
use indexmap::IndexMap;

use crate::{ExternalModuleInfo, ExternalModuleVersion};
use crate::asts::PekoAST;
use crate::asts::data_structures::{PositionData, PositionedValue, StringChunk};
use crate::asts::expressions::ObjectConstructionAST;
use crate::asts::values::StringAST;
use crate::types::PekoType;

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
            FunctionGenericType,
            ArgumentType,
            ClassType,
            ClassGenericType,
            ClassVirtualTableType,
            ClassAttributeType,
        > + Clone,
    ValueType: ExecutionValue,
    VariableType: ExecutionVariable<ValueType, ModuleType> + Clone,
    FunctionType: ExecutionFunction<ArgumentType, ModuleType> + Clone,
    ArgumentType: ExecutionArgument,
    FunctionGenericType: ExecutionFunctionGeneric<ModuleType> + Clone,
    ClassType: ExecutionClass<ClassType, ClassVirtualTableType, ClassAttributeType, ModuleType> + Clone,
    ClassVirtualTableType: ExecutionClassVirtualTable<FunctionType>,
    ClassAttributeType: ExecutionClassAttribute,
    ClassGenericType: ExecutionClassGeneric<ModuleType> + Clone,
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
            if let Some(variants) = current.read().unwrap().get_enums().get(enum_name) {
                return Some(variants.clone());
            }

            let parent = current.read().unwrap().get_parent()?.clone();
            current = parent;
        }
    }

    /// Registers an enum and its variant names in the current module so the
    /// name resolves as a type and its variants resolve through
    /// `Enum::Variant`.
    fn register_enum(&mut self, enum_name: String, variants: Vec<String>) {
        self.get_module_context()
            .current_module()
            .write()
            .unwrap()
            .get_enums_mut()
            .insert(enum_name, variants);
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
    fn get_external_modules(&self) -> &HashMap<String, ExternalModuleInfo>;

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
        generic: &FunctionGenericType,
        type_parameters: Vec<PekoType>,
    ) -> Option<FunctionType>;

    /// Backend hook: instantiates a generic class with concrete type
    /// arguments.
    fn create_generic_class(
        &mut self,
        generic: &ClassGenericType,
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
            Self::build_module_id(&root_folder, &current_file, "project")
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
                vec![ExternalModuleVersion::new(String::new(), entry_dir, entry_name)],
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

    /// Walks up the module tree looking for a generic-class declaration
    /// named `generic_name`.
    fn find_class_generic_in_current(
        &self,
        generic_name: impl ToString,
    ) -> Option<ClassGenericType> {
        let mut current = self.get_module_context().current_module();

        loop {
            for (genname, generic) in current.read().unwrap().get_class_generics() {
                if &generic_name.to_string() == genname {
                    return Some(generic.read().unwrap().clone());
                }
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

    /// Walks up the module tree looking for a generic-function
    /// declaration named `generic_name`.
    fn find_function_generic_in_current(
        &self,
        generic_name: impl ToString,
    ) -> Option<FunctionGenericType> {
        let mut current = self.get_module_context().current_module();

        loop {
            for (genname, generic) in current.read().unwrap().get_function_generics() {
                if &generic_name.to_string() == genname {
                    return Some(generic.read().unwrap().clone());
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
            type1 = self.get_generic_types()[type1.name()].clone();

            // Add all the non-definition type info back in.
            type1.array_depth += array_depth;
            type1.reference_depth += reference_depth;

            array_depth = type1.array_depth;
            reference_depth = type1.reference_depth;
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
        // class body or modules to expand, so they expand to themselves.
        if self.get_enum_variants(type1.name()).is_some() {
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
                    argument_expansion_failed = true;
                    break;
                }
            }
        }
        self.get_module_context_mut()
            .step_forward(argument_post_stack);
        if argument_expansion_failed {
            return None;
        }

        if type1.is_base_type() || type1.name() == "Pointer" {
            return Some(type1);
        }

        // Convert the type to a string that can be interpreted as a class name.
        let mut module_names = type1.module_names().to_vec();
        let class_name = type1.declutter().to_string();
        let mut class_name_base = type1.clone().declutter();
        class_name_base.generics_mut().clear();
        let class_name_base = class_name_base.to_string();

        let mut class_parent = if !module_names.is_empty() {
            let mut contained_in = if self.find_module_in_current(&module_names[0]).is_some() {
                self.find_module_in_current(&module_names[0]).unwrap()
            } else if self
                .get_module_context()
                .top_level_modules
                .contains_key(&module_names[0])
            {
                self.get_module_context().top_level_modules[&module_names[0]].clone()
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

            if contained_in
                .read()
                .unwrap()
                .get_classes()
                .contains_key(&class_name)
                || contained_in
                    .read()
                    .unwrap()
                    .get_class_generics()
                    .contains_key(&class_name)
                || contained_in
                    .read()
                    .unwrap()
                    .get_class_generics()
                    .contains_key(&class_name_base)
            {
                if contained_in
                    .read()
                    .unwrap()
                    .get_classes()
                    .contains_key(&class_name)
                {
                    contained_in.read().unwrap().get_classes()[&class_name]
                        .read()
                        .unwrap()
                        .get_parent_module()
                } else if contained_in
                    .read()
                    .unwrap()
                    .get_class_generics()
                    .contains_key(&class_name_base)
                {
                    contained_in.read().unwrap().get_class_generics()[&class_name_base]
                        .read()
                        .unwrap()
                        .get_parent_module()
                } else {
                    contained_in.read().unwrap().get_class_generics()[&class_name]
                        .read()
                        .unwrap()
                        .get_parent_module()
                }
            } else {
                return None;
            }
        } else if self.find_class_in_current(type1.name()).is_some() {
            self.find_class_in_current(type1.name())
                .as_ref()
                .unwrap()
                .get_parent_module()
        } else if self.find_class_generic_in_current(&class_name).is_some() {
            self.find_class_generic_in_current(&class_name)
                .as_ref()
                .unwrap()
                .get_parent_module()
        } else if self
            .find_class_generic_in_current(&class_name_base)
            .is_some()
        {
            self.find_class_generic_in_current(&class_name_base)
                .as_ref()
                .unwrap()
                .get_parent_module()
        } else {
            return None;
        };

        if !class_parent
            .read()
            .unwrap()
            .get_classes()
            .contains_key(&class_name)
            && class_parent
                .read()
                .unwrap()
                .get_class_generics()
                .contains_key(&class_name_base)
        {
            let generic = class_parent.read().unwrap().get_class_generics()[&class_name_base]
                .read()
                .unwrap()
                .clone();
            return self
                .create_generic_class(&generic, type1.generics().to_vec())
                .map(|class| class.get_class_type().clone());
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

        Some(type1)
    }

    /// Returns `true` if `peko_type` resolves to a valid type in scope.
    fn type_exists(&mut self, peko_type: &PekoType) -> bool {
        // Error types are treated as poison (they technically exist).
        if peko_type.is_error_type() {
            return true;
        }

        if peko_type.name() == "Pointer" && !peko_type.generics().is_empty() {
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

        // Expand the type to its full official definition.
        let type_expanded = self.expand_type(peko_type);
        let Some(peko_type) = type_expanded else {
            return false;
        };

        match peko_type.name() {
            // Built-in types obviously exist.
            "string" | "opaque" | "int" | "int16" | "int128" | "int64" | "float" | "double"
            | "f16" | "char" | "bool" | "void" | "cstr" => true,
            _ => {
                // Check generic types again -- perhaps the expanded type name
                // exists as a generic type.
                if self.get_generic_types().contains_key(peko_type.name()) {
                    self.type_exists(&self.get_generic_types()[peko_type.name()].clone())
                } else if self.get_enum_variants(peko_type.name()).is_some() {
                    // Enums are user-defined named types backed by an integer.
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

        // Convert the type to a class name.
        let class_name = type_expanded.declutter().to_string();

        // Find the module the class is defined in, based on the expanded
        // type's module path.
        let (mut next_module, starting_point) = if !type_expanded.module_names().is_empty()
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
                && next_module
                    .read()
                    .unwrap()
                    .get_class_generics()
                    .contains_key(type_expanded.name())
            {
                // Instantiate the generic using the provided generic
                // types as the type parameters.
                let next_generic = next_module.clone().read().unwrap().get_class_generics()
                    [type_expanded.name()]
                    .read()
                    .unwrap()
                    .clone();

                return self
                    .create_generic_class(&next_generic, type_expanded.generics().to_vec());
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
    fn types_similar(&mut self, type1: &PekoType, type2: &PekoType) -> bool {
        // Error types "work" with any other type.
        if type1.is_error_type() || type2.is_error_type() {
            return true;
        }

        // Equal types are trivially similar.
        if self.types_equal(type1, type2) {
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

        // Non-optional types can be casted to options.
        if type2_expanded.name() == "Option"
                && type2_expanded.generics().len() == 1
                // Check if the inner type of the optional is similar to other type.
                && self.types_similar(
                    &type2_expanded.generics()[0],
                    &type1_expanded,
                )
        {
            return true;
        }

        let type1_is_managed = type1.name() == "Pointer";
        let type2_is_managed = type2.name() == "Pointer";

        // Pointers can be blindly cast to each other.
        if (type1_expanded.is_pointer() || type1_is_managed)
            && (type2_expanded.is_pointer() || type2_is_managed)
        {
            return true;
        }

        // Class instances are represented as pointers, so an untyped
        // pointer (`opaque` or `Pointer<void>`) casts cleanly to and from
        // a class instance. Depth must match: a bare untyped pointer pairs
        // with a bare class value, `opaque[]` with a class array, and so
        // on. The element type on the class side is what must be a class.
        let type1_is_untyped_pointer = type1_expanded.no_depth().to_string() == "opaque"
            || (type1_expanded.name() == "Pointer"
                && type1_expanded.generics().len() == 1
                && type1_expanded.generics()[0].name() == "void");
        let type2_is_untyped_pointer = type2_expanded.no_depth().to_string() == "opaque"
            || (type2_expanded.name() == "Pointer"
                && type2_expanded.generics().len() == 1
                && type2_expanded.generics()[0].name() == "void");

        // Compare pointer/reference depth so the untyped pointer and the
        // class are at the same level of indirection.
        let depths_match = type1_expanded.array_depth + type1_expanded.reference_depth
            == type2_expanded.array_depth + type2_expanded.reference_depth;

        if depths_match && (type1_is_untyped_pointer || type2_is_untyped_pointer) {
            if type1_is_untyped_pointer {
                let mut class_side = type2_expanded.clone();
                class_side.array_depth = 0;
                class_side.reference_depth = 0;
                if self.get_class_by_type(&class_side).is_some() {
                    return true;
                }
            }

            if type2_is_untyped_pointer {
                let mut class_side = type1_expanded.clone();
                class_side.array_depth = 0;
                class_side.reference_depth = 0;
                if self.get_class_by_type(&class_side).is_some() {
                    return true;
                }
            }
        }

        let type1_is_managed_string = type1.name() == "string";
        let type2_is_managed_string = type2.name() == "string";

        if (type1_is_managed_string || type1.is_string_type())
            && (type2_is_managed_string || type2.is_string_type())
        {
            return true;
        }

        // All built-in types can be cast to each other.
        if type1_expanded.is_builtin_type() && type2_expanded.is_builtin_type() {
            return true;
        }

        // Connected classes can be cast to each other.
        let type1_class = self.get_class_by_type(&type1_expanded);
        let type2_class = self.get_class_by_type(&type2_expanded);

        if let (Some(class1), Some(class2)) = (&type1_class, &type2_class)
            && (self.classes_connect(class1, class2) || self.classes_connect(class2, class1))
        {
            return true;
        }

        // Classes implementing `operator to_<type>` casts are similar to
        // <type>.
        if let Some(class1) = type1_class
            && type2_expanded.is_builtin_type()
            && class1
                .get_main_virtual_table()
                .get_methods()
                .contains_key(&format!("[operator to_{}]", type2_expanded))
        {
            return true;
        }

        if type2_class.is_some()
            && type1_expanded.is_builtin_type()
            && type2_class
                .unwrap()
                .get_main_virtual_table()
                .get_methods()
                .contains_key(&format!("[operator to_{}]", type1_expanded))
        {
            return true;
        }

        // Strings and opaques are technically pointers; cast freely.
        if ((type1.is_pointer() || type1.to_string() == "string") && type2.to_string() == "opaque")
            || (type1.to_string() == "opaque"
                && (type2.is_pointer() || type2.to_string() == "string"))
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

    fn types_equal(&mut self, type1: &PekoType, type2: &PekoType) -> bool {
        // Error types "work" with any type.
        if type1.is_error_type() || type2.is_error_type() {
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

        // Compare via stringified form.
        type1_expanded.to_string() == type2_expanded.to_string()
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
            if provided_arguments.is_some() {
                for (argument_name, argument_type) in provided_arguments.clone().unwrap().iter() {
                    if function.get_arguments().contains_key(argument_name)
                        && function.get_arguments()[argument_name].has_default_value()
                    {
                        // Only an exactly-equal argument type matches. Implicit
                        // casts are gone, so a similar-but-unequal type is not a
                        // candidate, and const cannot be dropped.
                        if self.types_equal(
                            function.get_arguments()[argument_name].get_argument_type(),
                            argument_type,
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

                    // Only an exactly-equal argument type matches, and const
                    // cannot be dropped to satisfy the parameter.
                    if !self.types_equal(arg.get_argument_type(), &argument_types[index])
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
                        // Only an exactly-equal argument type matches, and const
                        // cannot be dropped.
                        if !self.types_equal(argument_type, function.get_var_args_type().unwrap())
                            || !self
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
