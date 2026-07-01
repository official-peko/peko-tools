//! The `PekoCodegenContext` struct, its constructor, and the
//! `ExecutionContextAlgorithms` impl.
//!
//! All LLVM-building methods that previously lived on the inherent impl
//! of `PekoCodegenContext` have been moved into layered traits under
//! [`crate::codegen::builders`]. See [`crate::codegen::builders`] for
//! the layer table and per-trait documentation.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use llvm_sys_180::core;
use llvm_sys_180::prelude::{
    LLVMBasicBlockRef, LLVMBuilderRef, LLVMContextRef, LLVMModuleRef, LLVMValueRef,
};
use peko_core::ExternalModuleInfo;
use peko_core::execution::data_structures::ExecutionModule;
use peko_core::execution::{ExecutionContextAlgorithms, ExecutionModuleContext};
use peko_core::types::{PekoType, TypeRestraint};

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::prelude::*;
use crate::codegen::cstr;
use crate::codegen::data_structures::{
    BooleanOperation, CodegenArg, CodegenClass, CodegenClassAttribute, CodegenFunction,
    CodegenModule, CodegenValue, CodegenVariable, CodegenVirtualTable, NumericalOperation,
};

/// The symbol an erased generic compiles under: the bare name followed by the
/// generic typenames in angle brackets. The typenames are fixed for a given
/// declaration, so every concrete instantiation resolves to this one name and
/// the body is compiled once.
pub fn erased_generic_symbol(base: &str, typenames: &[String]) -> String {
    format!("{base}<{}>", typenames.join(", "))
}

/// A substitution mapping each generic typename to a bare `Generic` carrier.
/// Used to lower an erased class's member types outside the class's own generic
/// context, for example when declaring its methods into an importing module:
/// each parameter lowers to a thin managed pointer regardless of its bounds.
pub fn class_carrier_substitution(
    typenames: &[peko_core::asts::data_structures::PositionedValue<String>],
) -> HashMap<String, PekoType> {
    typenames
        .iter()
        .map(|name| {
            (
                name.value.clone(),
                PekoType::generic_type(name.value.clone(), Vec::new()),
            )
        })
        .collect()
}

// Generic substitution and inference live in peko_core so the codegen and the
// simulator resolve erased generics identically. Re-exported here so existing
// `crate::codegen::context::substitute_generic_params` references keep working.
pub use peko_core::types::{infer_generic_bindings, substitute_generic_params, substitute_self};

#[derive(Clone)]
pub struct PekoCodegenContext {
    pub unnamed_offset: usize,

    pub state: Vec<String>,
    pub generated_args: Vec<CodegenValue>,
    pub generated_kw_args: Option<HashMap<String, CodegenValue>>,

    pub diagnostics: peko_core::diagnostics::DiagnosticList,
    pub errored: bool,

    pub external_modules: HashMap<String, ExternalModuleInfo>,
    pub module_context: ExecutionModuleContext<CodegenModule>,
    pub outside_primary_module: bool,
    pub outside_declarations_only: bool,
    pub target: peko_core::target::PekoTarget,
    pub creating_required: bool,

    /// Per-class method function declarations created by the header pass
    /// (`ClassAST::declare`), keyed by the class's fully qualified type string,
    /// in the class's source method order. The body pass reuses these instead
    /// of re-creating the LLVM functions, so a class dispatched from another
    /// class's body before its own body is built still resolves.
    pub class_function_values: HashMap<String, Vec<CodegenFunction>>,

    /// True while the header pass is materializing a class's layout, method
    /// signatures, and static data. Method bodies are skipped in this pass and
    /// emitted later by the body pass.
    pub building_class_signatures: bool,

    /// Generic class instantiations whose signatures were built during the
    /// signature pass and whose method bodies are still pending. A signature
    /// pass that instantiates a generic (a method returning `Option<bool>`)
    /// only lays out the instantiation, since the classes it dispatches on may
    /// not be laid out yet. The bodies are emitted once every class is laid
    /// out, by draining this queue after the body pass.
    ///
    /// Each entry records the module that owned the instantiation's method
    /// declarations (the module being signature-built when the instantiation
    /// fired). The body pass keys method functions by that owning module, so
    /// the drain restores it before emitting, even when an unrelated nested
    /// import is the one that triggers the drain.
    pub pending_generic_class_bodies:
        Vec<(CodegenClass, Vec<PekoType>, Arc<RwLock<CodegenModule>>)>,

    /// Build state of each erased generic class by its compiled name. An erased
    /// class is compiled once and shared across every instantiation; `false`
    /// marks signatures emitted (bodies deferred), `true` marks bodies emitted.
    /// A use site that resolves an already-built erased class returns the cached
    /// class rather than re-emitting its method bodies.
    pub erased_generic_classes: HashMap<String, bool>,

    /// When set, a non-generic function declaration registers its signature and
    /// returns before building its body. The signature pass sets this so every
    /// top-level function is declared before any body is built, which lets a
    /// body (or a closure in it) call a function defined later in the file.
    pub declaring_signatures_only: bool,

    /// Erased generic class names whose instantiation is currently in progress.
    /// A self-referential field (a generic whose member is the same erased
    /// generic) re-enters create_generic_class while it is still laying out;
    /// this set breaks the cycle by returning the in-progress shell.
    pub generic_instantiations_in_progress: HashSet<String>,

    pub files_to_link: Vec<PathBuf>,

    pub imported_styles: HashMap<PathBuf, String>,
    pub compiled_styles_folder: Option<PathBuf>,
    pub application_id: Option<String>,
    pub asset_debug_folder: Option<PathBuf>,

    pub previous_scoped_variables: Vec<HashMap<String, CodegenVariable>>,
    pub scoped_variables: HashMap<String, CodegenVariable>,
    pub local_scope: bool,
    pub generic_types: HashMap<String, PekoType>,

    pub return_references: bool,
    pub current_return_type: Option<PekoType>,
    pub current_expected_type_options: Option<Vec<PekoType>>,

    /// Counter for fresh backward-inference variables (`?N`) bound when a
    /// `new Class()` cannot infer its type arguments forward. Mirrors the
    /// simulator; the erased body lowers them like any generic carrier.
    pub inference_counter: usize,

    /// `true` when the value at the current position is consumed (a variable
    /// initializer, a call argument, a return value, ...). `if` reads this to
    /// decide whether it is a value-producing expression or a statement.
    pub expecting_value: bool,

    pub current_this: Option<CodegenVariable>,
    pub previous_was_this: bool,
    pub attributes_to_set: Vec<String>,

    pub primary_object: Option<CodegenValue>,
    pub accessed_state: Option<String>,

    pub in_constructor: bool,
    pub windowsgui: bool,

    pub llvm_builder: LLVMBuilderRef,
    pub llvm_context: LLVMContextRef,

    pub current_loop_finish_block: Option<LLVMBasicBlockRef>,
    pub current_loop_block: Option<LLVMBasicBlockRef>,
    pub current_basic_block: Option<LLVMBasicBlockRef>,
    pub current_entry_block: Option<LLVMBasicBlockRef>,
    pub current_llvm_function: Option<LLVMValueRef>,

    pub final_linked_module: Option<LLVMModuleRef>,

    /// The folder that `project::` imports resolve against and that
    /// canonical module ids are rooted at. The import logic reassigns
    /// this while a registry package loads and restores it afterward.
    pub root_folder: PathBuf,
}

impl PekoCodegenContext {
    /// Construct a fresh codegen context for the given target and source file.
    ///
    /// Creates the two default top-level LLVM modules (`main` and `extern`),
    /// initializes an empty diagnostic list, and binds a fresh LLVM IR
    /// builder. The returned context is positioned at the top level of the
    /// `main` module with no active function or basic block.
    pub fn new(
        target: peko_core::target::PekoTarget,
        current_file: PathBuf,
        outside_declarations_only: bool,
        root_folder: PathBuf,
    ) -> PekoCodegenContext {
        let llvm_context = unsafe { core::LLVMGetGlobalContext() };

        let main_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
            "main",
            current_file.clone(),
            None,
            llvm_context,
        )));

        let extern_module = Arc::new(RwLock::new(CodegenModule::new_top_level(
            "extern",
            current_file,
            None,
            llvm_context,
        )));

        PekoCodegenContext {
            unnamed_offset: 0,

            state: Vec::new(),
            generated_args: Vec::new(),
            generated_kw_args: None,
            creating_required: false,
            class_function_values: HashMap::new(),
            building_class_signatures: false,
            pending_generic_class_bodies: Vec::new(),
            erased_generic_classes: HashMap::new(),
            declaring_signatures_only: false,
            generic_instantiations_in_progress: HashSet::new(),

            diagnostics: peko_core::diagnostics::DiagnosticList::new(),
            errored: false,

            external_modules: HashMap::new(),
            outside_declarations_only,
            outside_primary_module: false,
            target,

            files_to_link: Vec::new(),

            imported_styles: HashMap::new(),
            compiled_styles_folder: None,
            asset_debug_folder: None,
            application_id: None,

            module_context: ExecutionModuleContext::new(main_module, extern_module),

            previous_scoped_variables: Vec::new(),
            scoped_variables: HashMap::new(),
            local_scope: false,
            expecting_value: false,
            generic_types: HashMap::new(),

            return_references: false,
            current_return_type: None,
            current_expected_type_options: None,
            inference_counter: 0,

            current_this: None,
            previous_was_this: false,
            attributes_to_set: Vec::new(),

            primary_object: None,
            accessed_state: None,

            in_constructor: false,
            windowsgui: false,

            llvm_builder: unsafe { core::LLVMCreateBuilder() },
            llvm_context,

            current_loop_finish_block: None,
            current_loop_block: None,
            current_basic_block: None,
            current_entry_block: None,
            current_llvm_function: None,
            final_linked_module: None,
            root_folder,
        }
    }

    /// Emit the method bodies of every generic instantiation that was laid out
    /// during the signature pass. Call this after the body pass, when every
    /// class is fully laid out, so a deferred body can box and dispatch on any
    /// class. Re-instantiating with the signature flag clear takes the
    /// signatures-done path and builds only the bodies. A body may itself
    /// instantiate a new generic; with the flag clear that instantiation builds
    /// fully in place rather than queueing, so the queue drains to empty.
    /// Declare `function` in the current module's LLVM module and return the
    /// external declaration value. Mirrors the per-module declaration the
    /// import path creates, including gc-leaf marking, so a call emitted in a
    /// module that never imported the function still resolves. `owner` is the
    /// module that owns the function definition, used to lower its type.
    pub fn declare_function_in_current(
        &mut self,
        function: &CodegenFunction,
        owner: &Arc<RwLock<CodegenModule>>,
    ) -> CodegenValue {
        let function_type = function.get_type();
        let variadic = function.visibility.variadic;
        let external = function.visibility.external;
        let gc_safepoint = function.visibility.gc_safepoint;
        let qualified_name = function.qualified_name.clone();

        // Lower the function type in its owner's module, as the import path
        // does, so the signature matches the definition.
        self.module_context
            .move_to_module(owner.clone(), false, false);
        let function_llvm_type = self
            .get_llvm_type_full(&function_type, true, variadic)
            .unwrap();
        self.module_context.move_out_of_module();

        let current_module = {
            let post_stack = self.module_context.step_back_generics();
            let module = self.module_context.current_module();
            self.module_context.step_forward(post_stack);
            module
        };

        let function_qualified_name = cstr(qualified_name.to_string(!external));
        let llvm_module = current_module
            .read()
            .unwrap()
            .get_top_level()
            .unwrap()
            .llvm_module;
        // Reuse an existing declaration of this symbol in the module rather than
        // adding a second one (LLVM would rename the duplicate), so declaring on
        // demand is idempotent.
        let existing = unsafe {
            core::LLVMGetNamedFunction(llvm_module, function_qualified_name.as_ptr())
        };
        if !existing.is_null() {
            return CodegenValue::new(existing, function_type);
        }
        let new_function_value = CodegenValue::new(
            unsafe {
                core::LLVMAddFunction(
                    llvm_module,
                    function_qualified_name.as_ptr(),
                    function_llvm_type,
                )
            },
            function_type,
        );
        unsafe {
            core::LLVMSetLinkage(
                new_function_value.llvm_value,
                llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
            );
        }

        // The allocation entrypoints always collect and so are never gc-leaf;
        // every other external function is leaf unless declared gcsafe. This
        // mirrors the marking applied where the function is first declared.
        let unmangled_name = qualified_name.to_string(false);
        let is_gc_alloc_entrypoint =
            unmangled_name == "peko_gc_alloc_object" || unmangled_name == "peko_gc_alloc";
        if external && !gc_safepoint && !is_gc_alloc_entrypoint {
            crate::codegen::builders::functions::set_gc_leaf_attribute(
                new_function_value.llvm_value,
            );
        }

        new_function_value
    }

    pub fn drain_pending_generic_class_bodies(&mut self) {
        let previously_building_signatures = self.building_class_signatures;
        self.building_class_signatures = false;
        while let Some((generic, type_parameters, owning_module)) =
            self.pending_generic_class_bodies.pop()
        {
            // Restore the owning module so the body pass finds the method
            // functions keyed under it, then unwind it afterward.
            self.module_context
                .move_to_module(owning_module, false, false);
            self.create_generic_class(&generic, type_parameters);
            self.module_context.move_out_of_module();
        }
        self.building_class_signatures = previously_building_signatures;
    }

    /// The restraints on a generic-parameter value. Prefers the bounds the
    /// value's own type carries; when those are empty (a value derived through a
    /// field load or index can drop them), it falls back to the authoritative
    /// carrier installed in the current generic context.
    pub fn generic_param_restraints(&self, value_type: &PekoType) -> Vec<TypeRestraint> {
        if !value_type.restraints().is_empty() {
            return value_type.restraints().to_vec();
        }
        self.get_generic_types()
            .get(value_type.name())
            .map(|carrier| carrier.restraints().to_vec())
            .unwrap_or_default()
    }

    /// Map each of an erased generic class's typenames to the concrete generic
    /// argument at the same position in `object_type`. Used to substitute a
    /// class's member types at a use site (for example `KT -> string`,
    /// `VT -> number` for an instance of `Map<string, number>`).
    fn class_generic_substitution(
        &mut self,
        class: &CodegenClass,
        object_type: &PekoType,
    ) -> HashMap<String, PekoType> {
        let mut substitution = HashMap::new();
        for (typename, concrete) in class
            .generic_typenames
            .iter()
            .zip(object_type.generics().iter())
        {
            if let Some(expanded) = self.expand_type(concrete) {
                substitution.insert(typename.value.clone(), expanded);
            }
        }
        substitution
    }

    /// Calls a `static` trait method at the type level: `Type::method(args)`.
    ///
    /// A static method has no receiver, so the concrete type is known here. The
    /// method is dispatched directly through its own symbol (like a
    /// constructor), not through a vtable, and its arguments carry no `this`.
    /// The method's return type was bound to the concrete class when it was
    /// registered, so `Self` already reads as that class.
    pub fn call_static_method(
        &mut self,
        class_type: &PekoType,
        method_name: impl ToString,
        arguments: Vec<CodegenValue>,
    ) -> Result<CodegenValue, String> {
        let method_name_str = method_name.to_string();

        let class = match self.get_class_by_type(class_type) {
            Some(class) => class,
            None => return Err(format!("could not find type '{class_type}'")),
        };

        let method_options: Vec<CodegenFunction> = if class
            .main_virtual_table
            .methods
            .contains_key(&method_name_str)
        {
            class.main_virtual_table.methods[&method_name_str]
                .iter()
                .map(|option| option.read().unwrap().clone())
                .filter(|function| function.is_static)
                .collect()
        } else {
            return Err(format!(
                "no static method '{method_name_str}' on type '{class_type}'"
            ));
        };

        if method_options.is_empty() {
            return Err(format!(
                "no static method '{method_name_str}' on type '{class_type}'"
            ));
        }

        let argument_types: Vec<PekoType> =
            arguments.iter().map(|arg| arg.value_type.clone()).collect();

        let method = match self.choose_function(method_options, argument_types, None, true) {
            Some(method) => method,
            None => {
                return Err(format!(
                    "incorrect argument types for static method '{method_name_str}'"
                ));
            }
        };

        // Resolve the method's own symbol, declaring it on demand when the
        // current module has not imported it, mirroring the method-body path.
        let owning_uuid = self.get_owning_module_uuid();
        let function_value = match method.function_value.get(&owning_uuid) {
            Some(value) => value.clone(),
            None => {
                let owner = method.parent.clone();
                self.declare_function_in_current(&method, &owner)
            }
        };

        // Box each argument to its declared parameter type. There is no `this`,
        // so the arguments align one-to-one with the method's parameters.
        let mut boxed_arguments = Vec::new();
        for (argument, (_, parameter)) in itertools::izip!(&arguments, &method.arguments) {
            let boxed = match self.box_value_to_type(&parameter.argument_type, argument) {
                Some(value) => value,
                None => {
                    return Err(format!(
                        "incorrect argument types for static method '{method_name_str}' (expected '{}' but got '{}')",
                        parameter.argument_type, argument.value_type
                    ));
                }
            };
            boxed_arguments.push(boxed);
        }

        Ok(self.call_function(
            &method.get_type(),
            false,
            function_value.llvm_value,
            boxed_arguments,
        ))
    }
}

impl
    ExecutionContextAlgorithms<
        CodegenModule,
        CodegenValue,
        CodegenVariable,
        CodegenFunction,
        CodegenArg,
        CodegenClass,
        CodegenVirtualTable,
        CodegenClassAttribute,
    > for PekoCodegenContext
{
    fn get_module_context(&self) -> &ExecutionModuleContext<CodegenModule> {
        &self.module_context
    }

    fn get_module_context_mut(&mut self) -> &mut ExecutionModuleContext<CodegenModule> {
        &mut self.module_context
    }

    fn get_generic_types(&self) -> &HashMap<String, PekoType> {
        &self.generic_types
    }

    fn get_current_this(&self) -> &Option<CodegenVariable> {
        &self.current_this
    }

    fn get_generic_types_mut(&mut self) -> &mut HashMap<String, PekoType> {
        &mut self.generic_types
    }

    fn get_current_this_mut(&mut self) -> &mut Option<CodegenVariable> {
        &mut self.current_this
    }

    fn get_external_modules(&self) -> &HashMap<String, ExternalModuleInfo> {
        &self.external_modules
    }

    fn get_root_folder(&self) -> &PathBuf {
        &self.root_folder
    }

    fn get_root_folder_mut(&mut self) -> &mut PathBuf {
        &mut self.root_folder
    }

    /// Generates a generic function with the provided type parameters.
    fn create_generic_function(
        &mut self,
        generic: &CodegenFunction,
        type_parameters: Vec<PekoType>,
    ) -> Option<CodegenFunction> {
        // The template carries its source AST; a non-template function cannot be
        // instantiated.
        let source = generic.source_function.clone()?;

        // An erased generic function compiles ONCE. Its compiled name carries
        // the generic typenames, not the concrete arguments, so every
        // instantiation resolves to the single body. The arguments only gate
        // arity here.
        if type_parameters.len() != generic.generic_typenames.len() {
            return None;
        }

        let typenames: Vec<String> = generic
            .generic_typenames
            .iter()
            .map(|name| name.value.clone())
            .collect();
        let mut generic_function_name = source.function_name.clone();
        generic_function_name.value =
            erased_generic_symbol(&generic_function_name.value, &typenames);

        // The body was already compiled under this erased name; reuse it rather
        // than emitting a second LLVM definition of the same symbol.
        if let Some(existing) = generic
            .parent
            .read()
            .unwrap()
            .get_functions()
            .get(&generic_function_name.value)
        {
            return Some(existing[0].read().unwrap().clone());
        }

        // Map each generic typename to its bounded carrier: a thin object whose
        // restraints (the function's `impl`/`from` bounds) drive dispatch. The
        // body is compiled against these carriers, not any concrete type.
        let mut new_generic_types = HashMap::new();
        for type_name in &generic.generic_typenames {
            new_generic_types.insert(
                type_name.value.clone(),
                PekoType::generic_type(
                    type_name.value.clone(),
                    source
                        .generic_bounds
                        .get(&type_name.value)
                        .cloned()
                        .unwrap_or_default(),
                ),
            );
        }

        let previous_context_generic_types = self.get_generic_types().clone();
        self.get_generic_types_mut().clear();
        self.get_generic_types_mut().extend(new_generic_types);

        let mut generic_function = source.clone();
        generic_function.function_name = generic_function_name.clone();

        let context = self.snapshot_context();

        self.module_context
            .move_to_module(generic.parent.clone(), false, true);
        let module = self.module_context.current_module().clone();

        generic_function.generic_types.clear();
        // An instantiation may be requested while the signature pass is active;
        // the erased body must still be fully emitted, not stopped short.
        let previous_signatures_only = self.declaring_signatures_only;
        self.declaring_signatures_only = false;
        generic_function.build_value(self);
        self.declaring_signatures_only = previous_signatures_only;

        self.module_context.move_out_of_module();

        let function_reference =
            module.read().unwrap().get_functions()[&generic_function_name.value].clone();
        let first_function = function_reference[0].read().unwrap().clone();

        let imported_by = module
            .read()
            .unwrap()
            .get_top_level()
            .as_ref()
            .unwrap()
            .imported_by
            .clone();
        for imported_to in imported_by {
            let imported_to_uuid = imported_to.read().unwrap().get_uuid().unwrap();
            for function in function_reference.iter() {
                if function
                    .read()
                    .unwrap()
                    .function_value
                    .contains_key(&imported_to_uuid)
                {
                    continue;
                }

                let (function_type, variadic, qualified_name, external) = {
                    let function = function.read().unwrap();
                    (
                        function.get_type(),
                        function.visibility.variadic,
                        function.qualified_name.clone(),
                        function.visibility.external,
                    )
                };

                self.module_context
                    .move_to_module(module.clone(), false, false);
                let function_llvm_type = self
                    .get_llvm_type_full(&function_type, true, variadic)
                    .unwrap();
                self.module_context.move_out_of_module();

                let owned_name = cstr(qualified_name.to_string(!external));

                let new_function_value = CodegenValue::new(
                    unsafe {
                        core::LLVMAddFunction(
                            imported_to
                                .read()
                                .unwrap()
                                .get_top_level()
                                .unwrap()
                                .llvm_module,
                            owned_name.as_ptr(),
                            function_llvm_type,
                        )
                    },
                    function_type,
                );

                unsafe {
                    core::LLVMSetLinkage(
                        new_function_value.llvm_value,
                        llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                    );
                }

                function
                    .write()
                    .unwrap()
                    .function_value
                    .insert(imported_to_uuid.clone(), new_function_value);
            }
        }

        module
            .write()
            .unwrap()
            .get_functions_mut()
            .insert(generic_function_name.value.clone(), function_reference);

        // After compiling the generic function body, declare the GC type
        // descriptors of every class the body allocated into each importing
        // module. The function body may allocate classes such as Option<T>
        // or Array<T> whose descriptors are defined in the generic's module.
        // Importing modules that call this function need external declarations
        // of those descriptors so the verifier does not see cross-module
        // references.
        {
            let module_uuid = module.read().unwrap().get_uuid().unwrap();
            let all_classes: Vec<_> = module
                .read()
                .unwrap()
                .get_classes()
                .iter()
                .map(|(name, class)| (name.clone(), class.clone()))
                .collect();

            let imported_by2 = module
                .read()
                .unwrap()
                .get_top_level()
                .as_ref()
                .unwrap()
                .imported_by
                .clone();

            for imported_to in imported_by2 {
                let imported_to_uuid = imported_to.read().unwrap().get_uuid().unwrap();
                for (_class_name, class) in &all_classes {
                    // Only act on classes whose descriptor is defined in
                    // the generic's module (has the module's own UUID) but
                    // not yet declared in the importing module.
                    let (
                        has_module_descriptor,
                        has_imported_descriptor,
                        attribute_types,
                        class_type,
                    ) = {
                        let class = class.read().unwrap();
                        (
                            class.type_descriptor.contains_key(&module_uuid),
                            class.type_descriptor.contains_key(&imported_to_uuid),
                            class
                                .attributes
                                .values()
                                .map(|attribute| attribute.attribute_type.clone())
                                .collect::<Vec<_>>(),
                            class.class_type.clone(),
                        )
                    };

                    if !has_module_descriptor {
                        continue;
                    }
                    if has_imported_descriptor {
                        continue;
                    }
                    let mut managed_offset_count = 0;
                    for attribute_type in &attribute_types {
                        if self.is_managed(attribute_type) {
                            managed_offset_count += 1;
                        }
                    }
                    self.module_context
                        .move_to_module(imported_to.clone(), false, false);
                    self.declare_class_descriptor(
                        &class_type.to_mangled_string(),
                        managed_offset_count,
                    );
                    self.module_context.move_out_of_module();
                }
            }
        }

        self.reset_context(context);
        self.generic_types.clear();
        self.generic_types.extend(previous_context_generic_types);

        Some(first_function)
    }

    /// Generates a generic class with the provided type parameters.
    fn create_generic_class(
        &mut self,
        generic: &CodegenClass,
        type_parameters: Vec<PekoType>,
    ) -> Option<CodegenClass> {
        // The template carries its source AST; a non-template class cannot be
        // instantiated.
        let source = generic.source_class.clone()?;

        // Box is the one generic the compiler monomorphizes: each Box<T> is a
        // distinct class laid out with T's real representation (so a raw FFI
        // value fits) and a descriptor that traces the held value only when T
        // is managed. Every other generic erases to one shared compiled class.
        let monomorphize = source.class_name.value == "Box";

        // Expand the concrete type parameters only to gate arity and to record
        // the deferred body instantiation; the erased class is shared, so the
        // arguments do not select a body.
        let mut type_parameters_expanded = Vec::new();
        let post_stack = self.module_context.step_back();
        for parameter in type_parameters {
            type_parameters_expanded.push(self.expand_type(&parameter)?);
        }
        self.module_context.step_forward(post_stack);

        if type_parameters_expanded.len() != generic.generic_typenames.len() {
            return None;
        }

        // An erased generic class compiles ONCE under a name carrying its
        // generic typenames. Every instantiation resolves to this one struct,
        // descriptor, vtable, and TypeInfo.
        let typenames: Vec<String> = generic
            .generic_typenames
            .iter()
            .map(|name| name.value.clone())
            .collect();
        let mut generic_class_name = source.class_name.clone();
        // A monomorphized class is named by its concrete type arguments so each
        // instantiation is distinct; an erased class is named by its parameter
        // names so every instantiation shares it.
        let name_components: Vec<String> = if monomorphize {
            type_parameters_expanded
                .iter()
                .map(|parameter| parameter.to_string())
                .collect()
        } else {
            typenames.clone()
        };
        generic_class_name.value =
            erased_generic_symbol(&generic_class_name.value, &name_components);

        // Cached classes: a fully-built erased class is reused as-is; a
        // signatures-only class is reused while still in the signature pass
        // (its bodies arrive through the deferred drain). Re-emitting a built
        // class would redefine its method bodies.
        match self.erased_generic_classes.get(&generic_class_name.value) {
            Some(true) => {
                return Some(
                    generic.parent.read().unwrap().get_classes()
                        [&generic_class_name.value]
                        .read()
                        .unwrap()
                        .clone(),
                );
            }
            Some(false) if self.building_class_signatures => {
                return Some(
                    generic.parent.read().unwrap().get_classes()
                        [&generic_class_name.value]
                        .read()
                        .unwrap()
                        .clone(),
                );
            }
            _ => {}
        }

        // Re-entrant self-referential instantiation: a generic whose own field
        // is the same erased generic (a linked list node, or Option carrying an
        // Option) re-enters here while still laying out. Its struct shell is
        // already declared in the module, so return it to break the cycle; the
        // field lays out as a pointer to the shell.
        if self
            .generic_instantiations_in_progress
            .contains(&generic_class_name.value)
            && let Some(existing) = generic
                .parent
                .read()
                .unwrap()
                .get_classes()
                .get(&generic_class_name.value)
        {
            return Some(existing.read().unwrap().clone());
        }

        // Map each generic typename to its bound type. A monomorphized class
        // binds the concrete type argument, so its fields lay out with the real
        // representation. An erased class binds a bounded carrier: a thin object
        // whose restraints (the class's `impl`/`from` bounds) drive dispatch.
        let mut new_generic_types = HashMap::new();
        for (index, type_name) in generic.generic_typenames.iter().enumerate() {
            let bound_type = if monomorphize {
                type_parameters_expanded[index].clone()
            } else {
                PekoType::generic_type(
                    type_name.value.clone(),
                    source
                        .generic_bounds
                        .get(&type_name.value)
                        .cloned()
                        .unwrap_or_default(),
                )
            };
            new_generic_types.insert(type_name.value.clone(), bound_type);
        }

        let previous_context_generic_types = self.generic_types.clone();
        self.generic_types.clear();
        self.generic_types.extend(new_generic_types);

        let mut generic_class = source.clone();
        generic_class.class_name = generic_class_name.clone();

        let context = self.snapshot_context();

        self.module_context
            .move_to_module(generic.parent.clone(), false, true);
        let module = self.module_context.current_module().clone();

        // Mark this erased name in progress so a self-referential field that
        // re-enters create_generic_class returns the in-progress shell rather
        // than recursing forever.
        self.generic_instantiations_in_progress
            .insert(generic_class_name.value.clone());

        // An instantiation may be requested while the signature pass is active;
        // the class body must still be fully emitted, not stopped short.
        let previous_signatures_only = self.declaring_signatures_only;
        self.declaring_signatures_only = false;
        generic_class.build_value(self);
        self.declaring_signatures_only = previous_signatures_only;

        self.generic_instantiations_in_progress
            .remove(&generic_class_name.value);

        self.module_context.move_out_of_module();

        let class_reference =
            module.read().unwrap().get_classes()[&generic_class_name.value].clone();

        // Record the generic typenames so a use site can substitute them
        // against an instance's concrete generic arguments.
        class_reference.write().unwrap().generic_typenames = generic.generic_typenames.clone();

        // Rewrite the class type's generic arguments. A monomorphized class
        // keeps its concrete arguments so it resolves and sizes by them. An
        // erased class rewrites them to bounded carriers so the one compiled
        // class resolves and sizes anywhere without its generic context
        // installed; the mangled name is unchanged, a carrier mangling to its
        // parameter name.
        {
            let mut class_type = class_reference.read().unwrap().class_type.clone();
            for (index, (generic, typename)) in class_type
                .generics_mut()
                .iter_mut()
                .zip(typenames.iter())
                .enumerate()
            {
                *generic = if monomorphize {
                    type_parameters_expanded[index].clone()
                } else {
                    PekoType::generic_type(typename.clone(), generic.restraints().to_vec())
                };
            }
            class_reference.write().unwrap().class_type = class_type;
        }

        // Make every stored member type self-describing. A monomorphized class
        // rewrites bare parameter names to the concrete arguments, so a field or
        // method is typed in T's real representation. An erased class rewrites
        // them to bounded carriers, so a parameter-typed member resolves outside
        // the class's generic context and keeps its bounds for dispatch.
        {
            let carriers: HashMap<String, PekoType> = if monomorphize {
                generic
                    .generic_typenames
                    .iter()
                    .enumerate()
                    .map(|(index, typename)| {
                        (typename.value.clone(), type_parameters_expanded[index].clone())
                    })
                    .collect()
            } else {
                typenames
                    .iter()
                    .map(|typename| {
                        (
                            typename.clone(),
                            PekoType::generic_type(
                                typename.clone(),
                                source
                                    .generic_bounds
                                    .get(typename)
                                    .cloned()
                                    .unwrap_or_default(),
                            ),
                        )
                    })
                    .collect()
            };

            let method_options: Vec<Arc<RwLock<CodegenFunction>>> = {
                let class_read = class_reference.read().unwrap();
                class_read
                    .main_virtual_table
                    .methods
                    .values()
                    .flatten()
                    .cloned()
                    .collect()
            };
            for option in method_options {
                let mut function = option.write().unwrap();
                function.return_type =
                    substitute_generic_params(&function.return_type, &carriers);
                for (_, argument) in function.arguments.iter_mut() {
                    argument.argument_type =
                        substitute_generic_params(&argument.argument_type, &carriers);
                }
                if let Some(var_args) = function.var_args_type.clone() {
                    function.var_args_type =
                        Some(substitute_generic_params(&var_args, &carriers));
                }
            }

            let mut class_write = class_reference.write().unwrap();
            for (_, attribute) in class_write.attributes.iter_mut() {
                attribute.attribute_type =
                    substitute_generic_params(&attribute.attribute_type, &carriers);
            }
        }

        let class_type = class_reference.read().unwrap().class_type.clone();
        let class_type_string = class_type.to_string();

        // Snapshot the instantiated overloads to declare into importers.
        let method_entries: Vec<(String, Arc<RwLock<CodegenFunction>>)> = {
            let class_read = class_reference.read().unwrap();
            let mut entries = Vec::new();
            for (method_name, method_options) in class_read.main_virtual_table.methods.iter() {
                for option in method_options.iter() {
                    entries.push((method_name.clone(), Arc::clone(option)));
                }
            }
            entries
        };

        let imported_by = module
            .read()
            .unwrap()
            .get_top_level()
            .as_ref()
            .unwrap()
            .imported_by
            .clone();
        for imported_to in imported_by {
            let imported_to_uuid = imported_to.read().unwrap().get_uuid().unwrap();

            // Register a descriptor DECLARATION for this importing module.
            // The instantiated generic class's descriptor is DEFINED only in
            // the generic's defining module (via build_value above); importing
            // modules that allocate this class need an external declaration of
            // the same descriptor symbol, keyed by their own UUID. Without it,
            // allocate_class fails its get_descriptor lookup for the importing
            // module. This mirrors the non-generic import path in
            // import_modules. The declaration's managed-offset count must match
            // the definition.
            if !class_reference
                .read()
                .unwrap()
                .type_descriptor
                .contains_key(&imported_to_uuid)
            {
                let attribute_types: Vec<PekoType> = class_reference
                    .read()
                    .unwrap()
                    .attributes
                    .values()
                    .map(|attribute| attribute.attribute_type.clone())
                    .collect();
                let mut managed_offset_count = 0;
                for attribute_type in &attribute_types {
                    if self.is_managed(attribute_type) {
                        managed_offset_count += 1;
                    }
                }

                let descriptor_declaration = {
                    self.module_context
                        .move_to_module(imported_to.clone(), false, false);
                    let declaration = self.declare_class_descriptor(
                        &class_type.to_mangled_string(),
                        managed_offset_count,
                    );
                    self.module_context.move_out_of_module();
                    declaration
                };

                class_reference
                    .write()
                    .unwrap()
                    .type_descriptor
                    .insert(imported_to_uuid.clone(), descriptor_declaration);
            }

            for (method_name, option) in method_entries.iter() {
                if option
                    .read()
                    .unwrap()
                    .function_value
                    .contains_key(&imported_to_uuid)
                {
                    continue;
                }

                let (
                    option_type,
                    option_variadic,
                    option_return_type,
                    option_arguments,
                    option_qualified,
                    option_parent_class_info,
                ) = {
                    let option = option.read().unwrap();
                    (
                        option.get_type(),
                        option.visibility.variadic,
                        option.return_type.clone(),
                        option.arguments.clone(),
                        option.qualified_name.clone(),
                        option.parent_class_info.clone(),
                    )
                };

                self.module_context
                    .move_to_module(module.clone(), false, false);

                // Substitute the erased class's generic parameters to bare
                // carriers so the method type lowers outside the class's own
                // generic context.
                let lowering_type = if typenames.is_empty() {
                    option_type.clone()
                } else {
                    substitute_generic_params(
                        &option_type,
                        &class_carrier_substitution(&generic.generic_typenames),
                    )
                };
                let function_llvm_type = self
                    .get_llvm_type_full(&lowering_type, true, option_variadic)
                    .unwrap();

                self.module_context.move_out_of_module();

                let mut parent_method: Option<CodegenValue> = None;
                let mut parent_slot: Option<Arc<RwLock<CodegenFunction>>> = None;
                if let Some((parent_type, parent_module)) = &option_parent_class_info
                    && parent_type.to_string() != class_type_string
                {
                    let parent_class_name = parent_type.declutter().to_string();
                    let parent_options: Vec<Arc<RwLock<CodegenFunction>>> = {
                        let parent_module_read = parent_module.read().unwrap();
                        parent_module_read.classes[&parent_class_name]
                            .read()
                            .unwrap()
                            .main_virtual_table
                            .methods[method_name]
                            .iter()
                            .map(Arc::clone)
                            .collect()
                    };

                    for parent_option in parent_options {
                        let (parent_return, parent_arguments, parent_value) = {
                            let parent_option = parent_option.read().unwrap();
                            (
                                parent_option.return_type.clone(),
                                parent_option.arguments.clone(),
                                parent_option.function_value.get(&imported_to_uuid).cloned(),
                            )
                        };

                        if !self.types_equal(&parent_return, &option_return_type)
                            || parent_arguments.len() != option_arguments.len()
                        {
                            continue;
                        }

                        let mut breakout = false;
                        for ((_, argument1), (_, argument2)) in
                            parent_arguments.iter().zip(option_arguments.iter()).skip(1)
                        {
                            if !self.types_equal(&argument1.argument_type, &argument2.argument_type)
                            {
                                breakout = true;
                                break;
                            }
                        }

                        if breakout {
                            continue;
                        }

                        if let Some(value) = parent_value {
                            parent_method = Some(value);
                        } else {
                            parent_slot = Some(parent_option);
                        }
                        break;
                    }
                }

                let new_function_value = match parent_method {
                    Some(value) => value,
                    None => {
                        let owned_name = cstr(option_qualified.to_string(true));
                        CodegenValue::new(
                            unsafe {
                                core::LLVMAddFunction(
                                    imported_to
                                        .read()
                                        .unwrap()
                                        .get_top_level()
                                        .unwrap()
                                        .llvm_module,
                                    owned_name.as_ptr(),
                                    function_llvm_type,
                                )
                            },
                            option_type,
                        )
                    }
                };

                unsafe {
                    core::LLVMSetLinkage(
                        new_function_value.llvm_value,
                        llvm_sys_180::LLVMLinkage::LLVMExternalLinkage,
                    );
                }

                if let Some(parent_slot) = parent_slot {
                    parent_slot
                        .write()
                        .unwrap()
                        .function_value
                        .insert(imported_to_uuid.clone(), new_function_value.clone());
                }

                option
                    .write()
                    .unwrap()
                    .function_value
                    .insert(imported_to_uuid.clone(), new_function_value);
            }
        }

        module.write().unwrap().get_classes_mut().insert(
            generic_class_name.value.clone(),
            Arc::clone(&class_reference),
        );

        self.reset_context(context);
        self.generic_types.clear();
        self.generic_types.extend(previous_context_generic_types);

        // A signature-pass instantiation laid out the class and declared its
        // methods, but `build_value` skipped the bodies. Defer the body build
        // until every class is laid out by recording the instantiation along
        // with the module that owns its method declarations (the module
        // current now that the instantiation's own frame is unwound).
        if self.building_class_signatures {
            let owning_module = self.module_context.current_module();
            self.pending_generic_class_bodies.push((
                generic.clone(),
                type_parameters_expanded.clone(),
                owning_module,
            ));
        }

        // Record the build state: signatures only during the signature pass,
        // bodies otherwise. A later use site reuses the cached class instead of
        // re-emitting its method bodies.
        self.erased_generic_classes.insert(
            generic_class_name.value.clone(),
            !self.building_class_signatures,
        );

        Some(class_reference.read().unwrap().clone())
    }

    fn apply_operator(
        &mut self,
        operator: impl ToString,
        lhs: &CodegenValue,
        rhs: &CodegenValue,
    ) -> Option<CodegenValue> {
        // Enum-to-enum `==` / `!=`, checked before expansion because a bare
        // cross-module enum value does not expand in the importing module. Both
        // lower to an i32 variant index, and a qualified enum type and a bare
        // reference to the same enum compare equal.
        let operator_early = operator.to_string();
        if matches!(operator_early.as_str(), "==" | "!=")
            && self.enum_types_match(&lhs.value_type, &rhs.value_type)
        {
            return Some(self.build_int_operation(
                NumericalOperation::from(&operator_early).unwrap(),
                lhs,
                rhs,
            ));
        }

        let lhs_type = self.expand_type(&lhs.value_type)?;
        let rhs_type = self.expand_type(&rhs.value_type)?;

        let mut lhs = lhs.clone();
        let mut rhs = rhs.clone();

        lhs.value_type = lhs_type;
        rhs.value_type = rhs_type;

        // For closures, any operation returns `true`. This is so closures
        // can be used in generic classes without creating type errors.
        if lhs.value_type.is_closure()
            && rhs.value_type.is_closure()
            && self.types_equal(&lhs.value_type, &rhs.value_type)
        {
            return Some(self.create_constant_boolean(true));
        }

        let operator_str = operator.to_string();

        // An operator on an erased generic-parameter operand routes to the
        // operand's bound trait method (`==` -> Equals.equals, `+` -> Plus.plus,
        // and so on), dispatched through the runtime itable scan on the thin
        // object. The bound must declare the operator's trait method.
        if lhs.value_type.is_generic_param()
            && let Some(method_name) = peko_core::types::operator_trait_method(&operator_str)
        {
            let restraints = self.generic_param_restraints(&lhs.value_type);
            for restraint in &restraints {
                if let TypeRestraint::Impl(trait_type) = restraint
                    && self
                        .get_trait(trait_type.name())
                        .map(|trait_definition| {
                            trait_definition
                                .methods
                                .iter()
                                .any(|method| method.name == method_name)
                        })
                        .unwrap_or(false)
                {
                    return Some(self.call_trait_method_erased(
                        &lhs,
                        trait_type,
                        method_name,
                        vec![rhs.clone()],
                    ));
                }
            }
        }

        // An enum value lowers to its i32 variant index, so `==` / `!=` between
        // two enum values compares those indices in machine code.
        if matches!(operator_str.as_str(), "==" | "!=")
            && self.get_enum_variants(lhs.value_type.name()).is_some()
            && self.get_enum_variants(rhs.value_type.name()).is_some()
        {
            return Some(self.build_int_operation(
                NumericalOperation::from(&operator_str).unwrap(),
                &lhs,
                &rhs,
            ));
        }

        // `&&` / `||` between bool and i1 (any mix) reduce both operands to raw
        // i1 and combine in machine code, bypassing the And/Or trait.
        if matches!(operator_str.as_str(), "&&" | "||")
            && matches!(lhs.value_type.name(), "bool" | "i1")
            && matches!(rhs.value_type.name(), "bool" | "i1")
        {
            let lhs_raw = self.to_raw_bool(&lhs);
            let rhs_raw = self.to_raw_bool(&rhs);
            let boolean_op = if operator_str == "&&" {
                crate::codegen::data_structures::BooleanOperation::And
            } else {
                crate::codegen::data_structures::BooleanOperation::Or
            };
            return Some(self.build_boolean_operation(boolean_op, &lhs_raw, &rhs_raw));
        }

        // If the LHS is an object, route the operator to its core trait method
        // (`+` -> `plus`, `==` -> `equals`, and so on). An operator with no core
        // trait keeps the legacy `[operator <op>]` member name.
        if self.get_class_by_type(&lhs.value_type).is_some() {
            let method_name = peko_core::types::operator_trait_method(&operator_str)
                .map_or_else(|| format!("[operator {operator_str}]"), str::to_string);
            let call_overload =
                self.call_object_method(&lhs, method_name, vec![rhs.clone()], None);

            if let Ok(value) = call_overload {
                return Some(value);
            }

            // Overload failed. If the RHS is a datatype, try to cast the
            // LHS to that datatype and continue as if the LHS were itself
            // a datatype.
            if rhs.value_type.is_datatype() {
                let cast_to_datatype = self.call_object_method(
                    &lhs,
                    format!("[operator to_{}]", rhs.value_type),
                    Vec::new(),
                    None,
                );

                match cast_to_datatype {
                    Ok(value) => lhs = value,
                    Err(_) => return None,
                }
            }
        }

        // Numeric / boolean operations.
        if lhs.value_type.is_float() || lhs.value_type.is_integer() || lhs.value_type.is_datatype()
        {
            let rhs = self.box_value_to_type(&lhs.value_type, &rhs)?;

            if lhs.value_type.to_string() == "bool"
                && (operator_str == "&&" || operator_str == "||")
            {
                return Some(self.build_boolean_operation(
                    BooleanOperation::from(&operator_str).unwrap(),
                    &lhs,
                    &rhs,
                ));
            }

            if lhs.value_type.is_integer() {
                return Some(self.build_int_operation(
                    NumericalOperation::from(&operator_str).unwrap(),
                    &lhs,
                    &rhs,
                ));
            } else {
                return Some(self.build_float_operation(
                    NumericalOperation::from(&operator_str).unwrap(),
                    &lhs,
                    &rhs,
                ));
            }
        }

        // String equality / inequality. Covers managed `string` and the raw
        // string forms (cstr / char[] / &char).
        let lhs_stringish =
            lhs.value_type.is_string_type() || lhs.value_type.to_string() == "string";
        let rhs_stringish = (rhs.value_type.is_string_type()
            || rhs.value_type.to_string() == "string")
            || self
                .get_class_by_type(&rhs.value_type)
                .is_some_and(|class| {
                    class
                        .main_virtual_table
                        .methods
                        .contains_key("[operator to_string]")
                });
        if lhs_stringish && rhs_stringish && (operator_str == "==" || operator_str == "!=") {
            return Some(self.build_string_comparison(&lhs, &rhs, operator_str == "=="));
        }

        // Pointer equality / inequality.
        if lhs.value_type.is_pointer()
            && rhs.value_type.is_pointer()
            && (operator_str == "==" || operator_str == "!=")
        {
            return Some(self.build_pointer_comparison(&lhs, &rhs, operator_str == "=="));
        }

        // Managed-reference identity (== / !=). Class instances, Pointer<T>,
        // and closures are pointers at the LLVM level, so reference equality
        // and null checks are pointer-identity comparisons. This also covers
        // comparing a managed reference against a Default / null value, which
        // lowers to an opaque (address-space-0) null pointer; the comparison
        // helper casts both sides through a pointer-sized integer, so the
        // differing address spaces do not matter. At least one operand must be
        // a managed reference, and the other must be managed or an opaque /
        // null pointer.
        if operator_str == "==" || operator_str == "!=" {
            let lhs_managed = self.is_managed(&lhs.value_type);
            let rhs_managed = self.is_managed(&rhs.value_type);
            let lhs_opaque = lhs.value_type.to_string() == "opaque";
            let rhs_opaque = rhs.value_type.to_string() == "opaque";

            if (lhs_managed && rhs_opaque) || (lhs_opaque && rhs_managed) {
                return Some(self.build_pointer_comparison(&lhs, &rhs, operator_str == "=="));
            }
        }

        None
    }

    /// Calls a named function. The function name should include the
    /// owning module path when it cannot be located by type expansion
    /// alone.
    fn call_named_function(
        &mut self,
        function_name: impl ToString,
        function_arguments: Vec<CodegenValue>,
    ) -> Option<CodegenValue> {
        let mut function_name_type = PekoType::from_string(&function_name.to_string(), "");
        for generic_type in function_name_type.generics_mut().iter_mut() {
            *generic_type = self.expand_type(generic_type)?;
        }

        // Walk to the module that owns the function.
        let mut next_module = if !function_name_type.module_names().is_empty() {
            self.module_context.top_level_modules[&function_name_type.module_names()[0]].clone()
        } else {
            CodegenModule::get_top_parent(self.module_context.current_module())
        };

        for i in 1..function_name_type.module_names().len() {
            let child = next_module.read().unwrap().get_modules()
                [&function_name_type.module_names()[i]]
                .clone();
            next_module = child;
        }

        let mut argument_types = Vec::new();
        for argument in &function_arguments {
            argument_types.push(argument.value_type.clone());
        }

        // Pick the best-matching overload from the function's option set.
        let function_options: Vec<Arc<RwLock<CodegenFunction>>> = next_module
            .read()
            .unwrap()
            .get_functions()[function_name_type.name()]
            .iter()
            .map(Arc::clone)
            .collect();
        let mut function_to_call = self.choose_function(
            function_options
                .iter()
                .map(|option| option.read().unwrap().clone())
                .collect(),
            argument_types,
            None,
            false,
        )?;

        // Box arguments to the chosen overload's parameter types.
        let mut arguments_boxed = Vec::new();
        for (argument, (_, arg)) in
            itertools::izip!(&function_arguments, &function_to_call.arguments)
        {
            let boxed_argument = self.box_value_to_type(&arg.argument_type, argument)?;
            arguments_boxed.push(boxed_argument);
        }

        // Pass through any extra arguments to a variadic.
        if function_arguments.len() > function_to_call.arguments.len()
            && function_to_call.visibility.variadic
        {
            for argument in function_arguments
                .iter()
                .skip(function_to_call.arguments.len())
            {
                arguments_boxed.push(argument.clone());
            }
        }

        let post_stack = self.module_context.step_back_generics();
        let uuid = self
            .module_context
            .current_module()
            .read()
            .unwrap()
            .get_uuid()
            .unwrap();
        self.module_context.step_forward(post_stack);

        // The current module may not yet hold a declaration of the chosen
        // function (it was declared in another module, and the import that
        // would mirror it here did not run before this call - a deferred
        // generic body reaching an extern entrypoint, for example). Create the
        // per-module declaration on demand, mirroring the import path, and
        // record it on the shared function so later calls reuse it.
        if !function_to_call.function_value.contains_key(&uuid) {
            let function_value = self.declare_function_in_current(&function_to_call, &next_module);
            function_to_call
                .function_value
                .insert(uuid.clone(), function_value.clone());
            let chosen_name = function_to_call.qualified_name.to_string(true);
            for option in &function_options {
                if option.read().unwrap().qualified_name.to_string(true) == chosen_name {
                    option
                        .write()
                        .unwrap()
                        .function_value
                        .insert(uuid.clone(), function_value.clone());
                }
            }
        }

        Some(self.call_function(
            &function_to_call.get_type(),
            function_to_call.visibility.variadic,
            function_to_call.function_value[&uuid].llvm_value,
            arguments_boxed,
        ))
    }

    fn call_object_method(
        &mut self,
        object: &CodegenValue,
        method_name: impl ToString,
        mut arguments: Vec<CodegenValue>,
        provided_arguments: Option<HashMap<String, CodegenValue>>,
    ) -> Result<CodegenValue, String> {
        // Objects are the first argument to a method.
        arguments.insert(0, object.clone());

        let object_class = match self.get_class_by_type(&object.value_type) {
            Some(class) => class,
            None => {
                return Err(format!(
                    "could not find object type '{}'",
                    object.value_type
                ));
            }
        };

        let method_name_str = method_name.to_string();

        let method_options = if object_class
            .main_virtual_table
            .methods
            .contains_key(&method_name_str)
        {
            object_class.main_virtual_table.methods[&method_name_str]
                .iter()
                .map(|option| option.read().unwrap().clone())
                .collect()
        } else {
            return Err(format!(
                "could not find method type '{}' on object of type '{}'",
                method_name_str, object.value_type
            ));
        };

        let mut argument_types = Vec::new();
        for argument in &arguments {
            argument_types.push(argument.value_type.clone());
        }

        let provided_function_argument_types = provided_arguments.as_ref().map(|provided| {
            let mut arguments = HashMap::new();
            for (argument_name, argument_value) in provided {
                arguments.insert(argument_name.clone(), argument_value.value_type.clone());
            }
            arguments
        });

        let method = match self.choose_function(
            method_options,
            argument_types.clone(),
            provided_function_argument_types,
            true,
        ) {
            Some(method) => method,
            None => {
                return Err(format!(
                    "incorrect argument types for method '{}'",
                    method_name_str
                ));
            }
        };

        // Constructors are never virtual: the exact class being constructed
        // is always statically known, so there is no subtype whose
        // constructor could be reached through a base reference. Dispatch
        // them directly through the method's own symbol rather than loading
        // a function pointer out of the vtable. This avoids an indirect
        // statepoint (a gc.statepoint whose call target is a loaded pointer)
        // carrying multiple managed-pointer arguments.
        let direct_constructor_value = if method_name_str == "constructor" {
            method
                .function_value
                .get(&self.get_owning_module_uuid())
                .cloned()
        } else {
            None
        };

        // An erased class's method is typed in the class's generic parameters;
        // substitute them to bare carriers so the function type lowers (each
        // parameter to a thin managed pointer, matching the compiled body).
        let method_lowering_type = if object_class.generic_typenames.is_empty() {
            method.get_type()
        } else {
            crate::codegen::context::substitute_generic_params(
                &method.get_type(),
                &crate::codegen::context::class_carrier_substitution(
                    &object_class.generic_typenames,
                ),
            )
        };

        let object_vtable_method = match direct_constructor_value {
            Some(direct_value) => direct_value,
            None => {
                let object_vtable = self.get_object_vtable(object, true);
                self.get_vtable_method(
                    &object_vtable,
                    object_class.main_virtual_table.llvm_type,
                    &method_lowering_type,
                    method.virtual_table_index,
                    true,
                )
            }
        };

        // Private methods can only be called from inside the owning class.
        if method.visibility.private && self.current_this.is_none() {
            return Err(format!(
                "cannot access private method '{}' on object of type '{}'",
                method_name_str, object.value_type
            ));
        }

        // Determine whether every non-self argument has a default value;
        // when true the method can be invoked with all-keyword arguments.
        let mut all_args_keywords = method.arguments.len() > 1;
        for (_, arg) in method.arguments.iter().skip(1) {
            if arg.default_value.is_none() {
                all_args_keywords = false;
                break;
            }
        }

        let mut boxed_arguments = Vec::new();

        if !all_args_keywords
            || (provided_arguments.is_none()
                && (arguments.len() == method.arguments.len() || argument_types.len() != 1))
        {
            // Positional call.
            for (argument, (_, arg)) in itertools::izip!(&arguments, &method.arguments) {
                let boxed_argument_value = match self
                    .box_value_to_type(&arg.argument_type, argument)
                {
                    Some(value) => value,
                    None => {
                        return Err(format!(
                            "incorrect argument types for method '{}' (expected '{}' but got '{}')",
                            method_name_str, arg.argument_type, argument.value_type
                        ));
                    }
                };
                boxed_arguments.push(boxed_argument_value);
            }

            // Variadic tail: collect remaining positional args into the
            // var-args array.
            if let Some(var_args_type) = &method.var_args_type {
                let mut var_arguments = Vec::new();
                for arg in arguments.iter().skip(method.arguments.len()) {
                    var_arguments.push(arg.clone());
                }

                let create_array = self.create_standard_array(var_args_type, var_arguments);

                let create_array = match create_array {
                    Some(value) => value,
                    None => {
                        return Err(format!(
                            "could not create variable arguments of type '{}'",
                            var_args_type
                        ));
                    }
                };

                boxed_arguments.push(create_array);
            }
        } else {
            // Keyword call: for each declared arg, use the provided value
            // if any, otherwise its default.
            let provided_arguments = provided_arguments.unwrap_or_default();

            for (argument_name, arg) in method.arguments.iter().skip(1) {
                let argument_value = if provided_arguments.contains_key(argument_name) {
                    provided_arguments[argument_name].clone()
                } else {
                    arg.default_value.as_ref().unwrap().build_value(self)
                };

                let boxed_argument =
                    match self.box_value_to_type(&arg.argument_type, &argument_value) {
                        Some(value) => value,
                        None => {
                            return Err(format!(
                                "incorrect argument types for method '{}'",
                                method_name_str
                            ));
                        }
                    };

                boxed_arguments.push(boxed_argument);
            }

            boxed_arguments.insert(0, arguments[0].clone());
        }

        let mut function_call = self.call_function(
            &method_lowering_type,
            false,
            object_vtable_method.llvm_value,
            boxed_arguments,
        );

        // Whether the method's declared return type is a bare generic parameter
        // (for example `Option<T>.unwrap() => T`). Such a result comes straight
        // out of an erased slot, so if it resolves to an enum below it is a
        // Box<i32> cell that must be unboxed.
        let return_was_carrier = method.return_type.is_generic_param();

        // Use-site substitution: an erased generic class's methods are typed in
        // its generic parameters. Retype the result with the object's concrete
        // generic arguments, so a caller of `Map<string, number>.get` observes
        // `Option<number>` rather than `Option<VT>`.
        if !object_class.generic_typenames.is_empty() {
            let substitution = self.class_generic_substitution(&object_class, &object.value_type);
            function_call.value_type = crate::codegen::context::substitute_generic_params(
                &function_call.value_type,
                &substitution,
            );
        }

        // Method-level generics: infer each parameter the method declares itself
        // by unifying its declared argument types against the supplied ones, then
        // substitute into the result. So a caller of
        // `Array<number>.map(closure(x) => ...)` observes the mapped element type.
        if !method.method_generic_typenames.is_empty() {
            let method_names: HashSet<String> = method
                .method_generic_typenames
                .iter()
                .map(|name| name.value.clone())
                .collect();
            let mut method_substitution = HashMap::new();
            for ((_, declared), actual) in method
                .arguments
                .iter()
                .skip(1)
                .zip(argument_types.iter().skip(1))
            {
                crate::codegen::context::infer_generic_bindings(
                    &declared.argument_type,
                    actual,
                    &method_names,
                    &mut method_substitution,
                );
            }
            if !method_substitution.is_empty() {
                function_call.value_type = crate::codegen::context::substitute_generic_params(
                    &function_call.value_type,
                    &method_substitution,
                );
            }
        }

        // Unbox an enum that came out of an erased slot: the method returned a
        // bare carrier that resolved to an enum, so the value is a Box<i32> cell
        // whose i32 must be recovered to match the enum's representation.
        if return_was_carrier
            && function_call.value_type.array_depth == 0
            && self
                .get_enum_variants(function_call.value_type.name())
                .is_some()
        {
            let enum_type = function_call.value_type.clone();
            if let Some(unboxed) = self.unbox_enum_value(&function_call, &enum_type) {
                function_call = unboxed;
            }
        }

        // State-change notification: if this method mutates state on a
        // primary object that has an accessed state, notify the primary
        // object's `onStateChanged`. Skip when we are inside a
        // constructor since the object is not fully initialized.
        if !self.in_constructor
            && method.visibility.mutates
            && self.primary_object.is_some()
            && self.accessed_state.is_some()
        {
            let method_name_value =
                self.create_string(self.accessed_state.as_ref().unwrap().clone());
            let _ = self.call_object_method(
                &self.primary_object.clone().unwrap(),
                "onStateChanged",
                vec![method_name_value],
                None,
            );
        }

        self.primary_object = None;
        self.accessed_state = None;

        Ok(function_call)
    }

    fn set_object_attribute(
        &mut self,
        object: &CodegenValue,
        attribute_name: impl ToString,
        value: &CodegenValue,
    ) -> bool {
        let attribute_name_str = attribute_name.to_string();
        let get_attribute_pointer =
            self.get_object_attribute(object, attribute_name_str.as_str(), false);

        // Saving these here because get_object_attribute sets them as a
        // side effect of the state-tracking machinery; we need to restore
        // them after we've used the result.
        let previous_accessed_state = self.accessed_state.clone();
        let previous_primary_object = self.primary_object.clone();
        self.accessed_state = None;
        self.primary_object = None;

        let get_attribute_pointer = match get_attribute_pointer {
            Ok(value) => value,
            Err(_) => return false,
        };

        let mut attribute_type = get_attribute_pointer.value_type.clone();
        attribute_type.decrease_pointer_depth();

        let box_value = match self.box_value_to_type(&attribute_type, value) {
            Some(value) => value,
            None => return false,
        };

        self.build_managed_store(&get_attribute_pointer, &box_value);

        if !self.in_constructor
            && let (Some(accessed_state), Some(primary_object)) =
                (&previous_accessed_state, previous_primary_object)
        {
            let attribute_name_value = self.create_string(accessed_state);
            let _ = self.call_object_method(
                &primary_object,
                "onStateChanged".to_owned(),
                vec![attribute_name_value],
                None,
            );
        }

        true
    }

    fn get_object_attribute(
        &mut self,
        object: &CodegenValue,
        attribute_name: impl ToString,
        load_value: bool,
    ) -> Result<CodegenValue, String> {
        let class = match self.get_class_by_type(&object.value_type) {
            Some(class) => class,
            None => {
                return Err(format!(
                    "could not find object type '{}'",
                    object.value_type
                ));
            }
        };

        let attribute_name_str = attribute_name.to_string();

        if !class.attributes.contains_key(&attribute_name_str) {
            return Err(format!(
                "object of type '{}' does not have attribute '{}'",
                attribute_name_str, object.value_type
            ));
        }

        if class.attributes[&attribute_name_str].visibility.private
            && self.get_current_this().is_none()
        {
            return Err(format!(
                "cannot access private attribute '{}' of object of type '{}'",
                attribute_name_str, object.value_type
            ));
        }

        self.accessed_state = if class.attributes[&attribute_name_str].visibility.state {
            Some(attribute_name_str.clone())
        } else {
            None
        };
        self.primary_object = Some(object.clone());

        // Use-site substitution: a field typed in an erased generic class's
        // parameters takes the object's concrete generic argument, so
        // `Box<number>.value` reads as `number` rather than the parameter.
        let mut attribute_type = class.attributes[&attribute_name_str].attribute_type.clone();
        if !class.generic_typenames.is_empty() {
            let substitution = self.class_generic_substitution(&class, &object.value_type);
            attribute_type =
                crate::codegen::context::substitute_generic_params(&attribute_type, &substitution);
        }

        let element_access = self.get_struct_element(
            object,
            &attribute_type,
            class.attributes[&attribute_name_str].struct_index,
        );

        if load_value {
            Ok(self.load_value(&element_access))
        } else {
            Ok(element_access)
        }
    }

    fn create_standard_map(
        &mut self,
        key_type: &PekoType,
        value_type: &PekoType,
        key_value_pairs: Vec<(CodegenValue, CodegenValue)>,
    ) -> Option<CodegenValue> {
        let mut map_object_type = PekoType::from_string("Map", "");
        map_object_type.generics_mut().push(key_type.clone());
        map_object_type.generics_mut().push(value_type.clone());

        let map_object = self.create_object(&map_object_type, Vec::new())?;

        for (key, value) in &key_value_pairs {
            if self
                .call_object_method(&map_object, "set", vec![key.clone(), value.clone()], None)
                .is_err()
            {
                return None;
            }
        }

        Some(map_object)
    }

    fn create_standard_array(
        &mut self,
        array_type: &PekoType,
        values: Vec<CodegenValue>,
    ) -> Option<CodegenValue> {
        let mut array_object_type = PekoType::from_string("Array", "");
        array_object_type.generics_mut().push(array_type.clone());

        let array_object = self.create_object(&array_object_type, Vec::new())?;

        for value in &values {
            if self
                .call_object_method(&array_object, "push", vec![value.clone()], None)
                .is_err()
            {
                return None;
            }
        }

        Some(array_object)
    }

    fn create_object(
        &mut self,
        class_type: &PekoType,
        constructor_arguments: Vec<CodegenValue>,
    ) -> Option<CodegenValue> {
        let object_class = self.get_class_by_type(class_type)?;
        let allocate_object = self.allocate_class(&object_class)?;

        // Initialize a normal (vtable-bearing) class.
        if object_class.main_virtual_table.get_method_count() != 0
            && self
                .call_object_method(
                    &allocate_object,
                    "constructor",
                    constructor_arguments.clone(),
                    None,
                )
                .is_err()
        {
            return None;
        }

        // Initialize a struct class (no methods, no constructor).
        if object_class.main_virtual_table.get_method_count() == 0 {
            if constructor_arguments.len() != object_class.attributes.len() {
                return None;
            }

            for ((attribute_name, _), attribute_value) in
                object_class.attributes.iter().zip(&constructor_arguments)
            {
                if !self.set_object_attribute(&allocate_object, attribute_name, attribute_value) {
                    return None;
                }
            }
        }

        Some(allocate_object)
    }
}
