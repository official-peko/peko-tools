//! Concrete types implementing the `peko_core::execution` traits for
//! codegen. These mirror the simulator's data structures
//! one-for-one but carry the extra LLVM-specific state (LLVM values
//! and types, per-module `LLVMValueRef` maps for cross-module
//! references, etc.).

use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::{Arc, RwLock};

use derive_new::new;
use indexmap::IndexMap;
use itertools::Itertools;
use llvm_sys_180::core;
use llvm_sys_180::prelude::{LLVMContextRef, LLVMModuleRef, LLVMTypeRef, LLVMValueRef};
use llvm_sys_180::target_machine::LLVMTargetRef;

use peko_core::asts::PekoAST;
use peko_core::asts::data_structures::{PositionData, PositionedValue, VisibilityData};
use peko_core::asts::declarations::{ClassAST, FunctionDeclarationAST};
use peko_core::execution::data_structures::{
    ExecutionArgument, ExecutionClass, ExecutionClassAttribute, ExecutionClassGeneric,
    ExecutionClassVirtualTable, ExecutionFunction, ExecutionFunctionGeneric, ExecutionModule,
    ExecutionValue, ExecutionVariable,
};
use peko_core::target::{OperatingSystem, PekoTarget};
use peko_core::types::PekoType;

use crate::codegen::cstr;
use crate::codegen::symbol::SymbolName;

/// Build a `Pointer<inner>` type: the managed (address space 1) pointer
/// to a value of `inner`. Used wherever codegen needs a GC-managed
/// pointer to a known value type, such as a character buffer or a boxed
/// captured variable.
pub fn managed_pointer_type(inner: PekoType) -> PekoType {
    PekoType::new(
        Vec::new(),
        "Pointer".to_string(),
        vec![inner],
        0,
        0,
        0,
        None,
        false,
        PositionData::default(),
        PositionData::default(),
    )
}

/// Whether a type is a managed pointer `Pointer<T>`: the address space 1
/// pointer wrapper, as opposed to a raw `T*` (which uses pointer depth).
pub fn is_managed_pointer(ty: &PekoType) -> bool {
    // `string` is a managed char buffer (address space 1), morally a
    // `Pointer<char>`: it is indexable and participates in the managed-pointer
    // coercion/indexing paths exactly like `Pointer<T>`. `cstr` is the raw
    // (address space 0) counterpart and is deliberately NOT managed here.
    (ty.type_name == "Pointer" || ty.type_name == "string")
        && ty.pointer_depth == 0
        && ty.reference_depth == 0
}

/// The type produced by loading through or dereferencing a pointer.
///
/// For a managed pointer `Pointer<T>` the result is `T`. For `string` (a
/// managed `char` buffer) the result is `char`. For a raw pointer `T*` the
/// result is `T` with one less pointer depth. This is the single place that
/// knows how the managed and raw pointer forms each "decrease" by one level.
pub fn pointee_type(ty: &PekoType) -> PekoType {
    if ty.type_name == "string" && ty.pointer_depth == 0 && ty.reference_depth == 0 {
        return PekoType::simple_type("char");
    }
    if is_managed_pointer(ty) {
        ty.generic_types
            .first()
            .cloned()
            .unwrap_or_else(|| PekoType::simple_type("void"))
    } else {
        let mut inner = ty.clone();
        inner.decrease_pointer_depth();
        inner
    }
}

/// Build the interior-pointer type for an element of `item_type` reached
/// through a base pointer.
///
/// An interior pointer keeps the address space of its base: if the base
/// is managed (a `Pointer<T>` or a managed object reference), the result
/// is a managed `Pointer<item_type>`; otherwise it is a raw `item_type*`.
/// This matches LLVM, where a GEP into an address space 1 pointer yields
/// an address space 1 pointer.
pub fn reference_into(item_type: &PekoType, base_managed: bool) -> PekoType {
    if base_managed {
        managed_pointer_type(item_type.clone())
    } else {
        let mut raw = item_type.clone();
        raw.pointer_depth += 1;
        raw
    }
}

// ----------------------
// --- CODEGEN VALUES ---
// ----------------------

/// A value produced during codegen: an LLVM SSA value paired with the
/// Peko type the IR-level value represents.
#[derive(Debug, Clone, new)]
pub struct CodegenValue {
    pub llvm_value: LLVMValueRef,
    pub value_type: PekoType,
}

impl ExecutionValue for CodegenValue {
    fn get_type(&self) -> PekoType {
        self.value_type.clone()
    }
}

// -------------------------
// --- CODEGEN FUNCTIONS ---
// -------------------------

/// Information for a single function argument.
#[derive(Clone, new)]
pub struct CodegenArg {
    pub visibility: VisibilityData,
    pub argument_type: PekoType,
    pub default_value: Option<PekoAST>,
}

impl ExecutionArgument for CodegenArg {
    fn get_argument_type(&self) -> &PekoType {
        &self.argument_type
    }

    fn get_argument_type_mut(&mut self) -> &mut PekoType {
        &mut self.argument_type
    }

    fn has_default_value(&self) -> bool {
        self.default_value.is_some()
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

/// A generated function.
///
/// `function_value` is indexed by the importing module's UUID: the
/// same function can have different LLVM-level declarations in
/// different modules (each module sees its own `declare` or `define`
/// for the function).
#[derive(Clone, new)]
pub struct CodegenFunction {
    pub visibility: VisibilityData,

    pub return_type: PekoType,
    pub arguments: IndexMap<String, CodegenArg>,
    pub var_args_type: Option<PekoType>,

    pub function_value: HashMap<String, CodegenValue>,
    pub virtual_table_index: usize,

    pub qualified_name: SymbolName,
    pub parent: Arc<RwLock<CodegenModule>>,

    pub imported_from: Option<Arc<RwLock<CodegenModule>>>,
    pub parent_class_info: Option<(PekoType, Arc<RwLock<CodegenModule>>)>,
}

impl CodegenFunction {
    /// Look up this function's LLVM value as seen from `module_ref`.
    ///
    /// Returns `None` if either `module_ref` has no top-level UUID
    /// (a not-yet-registered submodule) or the function has not been
    /// imported into that module.
    pub fn get_value(&self, module_ref: Arc<RwLock<CodegenModule>>) -> Option<CodegenValue> {
        let module_uuid = module_ref.read().unwrap().get_uuid()?;
        self.function_value.get(&module_uuid).cloned()
    }

    /// Return a synthetic `PekoType` describing this function's type:
    /// generic types hold the argument types, `function_type` holds
    /// the return type.
    pub fn get_type(&self) -> PekoType {
        PekoType::new(
            Vec::new(),
            String::new(),
            self.arguments
                .iter()
                .map(|arg| arg.1.argument_type.clone())
                .collect_vec(),
            0,
            0,
            0,
            Some(self.return_type.clone()),
            false,
            PositionData::default(),
            PositionData::default(),
        )
    }
}

impl ExecutionFunction<CodegenArg, CodegenModule> for CodegenFunction {
    fn get_parent_module(&self) -> Arc<RwLock<CodegenModule>> {
        self.parent.clone()
    }

    fn get_arguments(&self) -> &IndexMap<String, CodegenArg> {
        &self.arguments
    }

    fn get_arguments_mut(&mut self) -> &mut IndexMap<String, CodegenArg> {
        &mut self.arguments
    }

    fn get_return_type(&self) -> &PekoType {
        &self.return_type
    }

    fn get_return_type_mut(&mut self) -> &mut PekoType {
        &mut self.return_type
    }

    fn get_var_args_type(&self) -> Option<&PekoType> {
        self.var_args_type.as_ref()
    }

    fn get_var_args_type_mut(&mut self) -> &mut Option<PekoType> {
        &mut self.var_args_type
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

// -------------------------
// --- CODEGEN VARIABLES ---
// -------------------------

/// A global variable that has been declared but whose initial value
/// has not yet been emitted. The initializer is held alongside the
/// declaration and run inside the module's globals-setter function.
#[derive(Clone, new)]
pub struct GlobalVariable {
    pub value: CodegenValue,
    pub variable_type: PekoType,
    pub variable_name: String,
    pub file: PathBuf,
}

/// A stored variable: either a local stack allocation or a global.
///
/// `variable_value` is indexed by importing-module UUID, just like
/// `CodegenFunction::function_value`.
#[derive(Clone, new)]
pub struct CodegenVariable {
    pub variable_visibility: VisibilityData,

    pub variable_type: PekoType,
    pub variable_value: HashMap<String, CodegenValue>,

    pub qualified_name: Option<SymbolName>,
    pub parent: Arc<RwLock<CodegenModule>>,

    pub imported_from: Option<Arc<RwLock<CodegenModule>>>,
}

impl CodegenVariable {
    /// Look up this variable's LLVM value as seen from `module_ref`.
    pub fn get_value(&self, module_ref: Arc<RwLock<CodegenModule>>) -> Option<CodegenValue> {
        let module_uuid = module_ref.read().unwrap().get_uuid()?;
        self.variable_value.get(&module_uuid).cloned()
    }
}

impl ExecutionVariable<CodegenValue, CodegenModule> for CodegenVariable {
    fn get_parent_module(&self) -> Arc<RwLock<CodegenModule>> {
        self.parent.clone()
    }

    fn get_variable_type(&self) -> &PekoType {
        &self.variable_type
    }

    fn get_variable_type_mut(&mut self) -> &mut PekoType {
        &mut self.variable_type
    }

    fn get_variable_visibility(&self) -> &VisibilityData {
        &self.variable_visibility
    }

    fn get_variable_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.variable_visibility
    }
}

// -----------------------
// --- CODEGEN CLASSES ---
// -----------------------

/// Information for a single class attribute.
#[derive(Clone, new)]
pub struct CodegenClassAttribute {
    pub visibility: VisibilityData,
    pub attribute_type: PekoType,

    pub struct_index: usize,
    pub llvm_type: LLVMTypeRef,
}

impl ExecutionClassAttribute for CodegenClassAttribute {
    fn get_attribute_type(&self) -> &PekoType {
        &self.attribute_type
    }

    fn get_attribute_type_mut(&mut self) -> &mut PekoType {
        &mut self.attribute_type
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

/// A class's main virtual table: method name to list of overloads.
#[derive(Clone, new)]
pub struct CodegenVirtualTable {
    pub methods: IndexMap<String, Vec<Arc<RwLock<CodegenFunction>>>>,

    pub struct_index: usize,
    pub llvm_type: LLVMTypeRef,
}

impl CodegenVirtualTable {
    /// Total number of methods, summed across all overload sets.
    pub fn get_method_count(&self) -> usize {
        self.methods.values().map(|overloads| overloads.len()).sum()
    }
}

impl ExecutionClassVirtualTable<CodegenFunction> for CodegenVirtualTable {
    fn get_methods(&self) -> &IndexMap<String, Vec<Arc<RwLock<CodegenFunction>>>> {
        &self.methods
    }

    fn get_methods_mut(&mut self) -> &mut IndexMap<String, Vec<Arc<RwLock<CodegenFunction>>>> {
        &mut self.methods
    }
}

/// A generated class.
#[derive(Clone, new)]
pub struct CodegenClass {
    pub class_type: PekoType,

    pub parent_class: Option<Box<CodegenClass>>,

    pub attributes: IndexMap<String, CodegenClassAttribute>,
    pub main_virtual_table: CodegenVirtualTable,

    pub struct_type: LLVMTypeRef,
    pub parent: Arc<RwLock<CodegenModule>>,

    pub imported_from: Option<Arc<RwLock<CodegenModule>>>,

    pub type_descriptor: HashMap<String, CodegenValue>,
}

impl CodegenClass {
    /// Look up this class's GC type descriptor as seen from the module
    /// with `module_uuid`. Returns `None` when the descriptor has not yet
    /// been emitted or declared in that module.
    pub fn get_descriptor(&self, module_uuid: &str) -> Option<CodegenValue> {
        self.type_descriptor.get(module_uuid).cloned()
    }
}

impl ExecutionClass<CodegenClass, CodegenVirtualTable, CodegenClassAttribute, CodegenModule>
    for CodegenClass
{
    fn get_parent_module(&self) -> Arc<RwLock<CodegenModule>> {
        self.parent.clone()
    }

    fn get_attributes(&self) -> &IndexMap<String, CodegenClassAttribute> {
        &self.attributes
    }

    fn get_attributes_mut(&mut self) -> &mut IndexMap<String, CodegenClassAttribute> {
        &mut self.attributes
    }

    fn get_class_type(&self) -> &PekoType {
        &self.class_type
    }

    fn get_class_type_mut(&mut self) -> &mut PekoType {
        &mut self.class_type
    }

    fn get_main_virtual_table(&self) -> &CodegenVirtualTable {
        &self.main_virtual_table
    }

    fn get_main_virtual_table_mut(&mut self) -> &mut CodegenVirtualTable {
        &mut self.main_virtual_table
    }

    fn get_parent_class(&self) -> Option<&CodegenClass> {
        self.parent_class.as_deref()
    }

    fn get_parent_class_mut(&mut self) -> &mut Option<Box<CodegenClass>> {
        &mut self.parent_class
    }
}

// ------------------------------
// --- CODEGEN CLASS GENERICS ---
// ------------------------------

/// A generic class declaration that hasn't been instantiated yet.
#[derive(Clone, new)]
pub struct CodegenClassGeneric {
    pub visibility: VisibilityData,

    pub generic_typenames: Vec<PositionedValue<String>>,
    pub class: ClassAST,
    pub module: Arc<RwLock<CodegenModule>>,

    pub filename: PathBuf,

    pub imported_from: Option<Arc<RwLock<CodegenModule>>>,
}

impl ExecutionClassGeneric<CodegenModule> for CodegenClassGeneric {
    fn get_parent_module(&self) -> Arc<RwLock<CodegenModule>> {
        self.module.clone()
    }

    fn get_class(&self) -> &ClassAST {
        &self.class
    }

    fn get_class_mut(&mut self) -> &mut ClassAST {
        &mut self.class
    }

    fn get_filename(&self) -> &Path {
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

    fn get_module(&self) -> &Arc<RwLock<CodegenModule>> {
        &self.module
    }

    fn get_module_mut(&mut self) -> &mut Arc<RwLock<CodegenModule>> {
        &mut self.module
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

// ---------------------------------
// --- CODEGEN FUNCTION GENERICS ---
// ---------------------------------

/// A generic function declaration that hasn't been instantiated yet.
#[derive(Clone, new)]
pub struct CodegenFunctionGeneric {
    pub visibility: VisibilityData,

    pub generic_typenames: Vec<PositionedValue<String>>,
    pub function: FunctionDeclarationAST,
    pub module: Arc<RwLock<CodegenModule>>,

    pub filename: PathBuf,

    pub imported_from: Option<Arc<RwLock<CodegenModule>>>,
}

impl ExecutionFunctionGeneric<CodegenModule> for CodegenFunctionGeneric {
    fn get_parent_module(&self) -> Arc<RwLock<CodegenModule>> {
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

    fn get_module(&self) -> &Arc<RwLock<CodegenModule>> {
        &self.module
    }

    fn get_module_mut(&mut self) -> &mut Arc<RwLock<CodegenModule>> {
        &mut self.module
    }

    fn get_visibility(&self) -> &VisibilityData {
        &self.visibility
    }

    fn get_visibility_mut(&mut self) -> &mut VisibilityData {
        &mut self.visibility
    }
}

// -----------------------
// --- CODEGEN MODULES ---
// -----------------------

/// State for the deferred initializer function that sets all module-level globals.
#[derive(Clone, new)]
pub struct ModuleGlobalsInfo {
    pub globals_function: CodegenValue,
    pub globals_set_name: SymbolName,
    pub globals_to_set: Vec<(GlobalVariable, PekoAST)>,
}

/// State carried only on top-level (root) modules.
#[derive(Clone)]
pub struct TopLevelModuleInfo {
    pub llvm_module: LLVMModuleRef,
    pub globals_info: ModuleGlobalsInfo,
    pub modules_imported: HashMap<String, Arc<RwLock<CodegenModule>>>,
    pub imported_by: Vec<Arc<RwLock<CodegenModule>>>,
    pub uuid: String,
    pub imported_styles: Vec<PathBuf>,
}

/// Patch an ELF64 little-endian object file in-place, setting SHF_WRITE
/// on the .llvm_stackmaps section so that ld.lld accepts R_AARCH64_ABS64
/// relocations in it under PIC/PIE. LLVM's AsmPrinter emits stack map
/// records with absolute 64-bit function addresses regardless of the
/// relocation model. On aarch64 Android, ld.lld rejects ABS64 in read-only
/// sections; making the section writable moves it into a writable PT_LOAD
/// segment where the dynamic linker can apply the relocations at load time.
/// This is a no-op on non-ELF or non-aarch64 targets (detected by checking
/// the ELF magic and e_machine field before doing anything).
pub fn patch_stackmaps_section_writable(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    // Validate ELF64 little-endian magic.
    if data.len() < 64
        || &data[0..4] != b"\x7fELF"
        || data[4] != 2  // EI_CLASS = ELFCLASS64
        || data[5] != 1
    // EI_DATA = ELFDATA2LSB
    {
        return Ok(()); // Not an ELF64 LE object; skip.
    }

    // e_machine at offset 0x12 (2 bytes LE).
    // AArch64 = 183 (0xB7), x86_64 = 62 (0x3E). Only patch these two
    // since they are the only targets where ld.lld rejects ABS64/ABS32
    // relocations in .llvm_stackmaps under PIC.
    let e_machine = u16::from_le_bytes([data[0x12], data[0x13]]);
    if e_machine != 183 && e_machine != 62 {
        return Ok(()); // Not a target that needs patching.
    }

    // ELF64 header fields (all little-endian):
    //   e_shoff  at 0x28 (8 bytes): offset of section header table
    //   e_shentsize at 0x3A (2 bytes): size of one section header entry
    //   e_shnum  at 0x3C (2 bytes): number of section header entries
    //   e_shstrndx at 0x3E (2 bytes): index of .shstrtab section
    let e_shoff = u64::from_le_bytes(data[0x28..0x30].try_into().unwrap()) as usize;
    let e_shentsize = u16::from_le_bytes([data[0x3A], data[0x3B]]) as usize;
    let e_shnum = u16::from_le_bytes([data[0x3C], data[0x3D]]) as usize;
    let e_shstrndx = u16::from_le_bytes([data[0x3E], data[0x3F]]) as usize;

    if e_shoff == 0 || e_shstrndx == 0 || e_shstrndx >= e_shnum {
        return Ok(());
    }

    // Locate .shstrtab: section header at index e_shstrndx.
    // ELF64 Shdr layout: sh_name(4) sh_type(4) sh_flags(8) sh_addr(8)
    //   sh_offset(8) sh_size(8) ...
    let shstrtab_hdr = e_shoff + e_shstrndx * e_shentsize;
    let shstrtab_offset = u64::from_le_bytes(
        data[shstrtab_hdr + 24..shstrtab_hdr + 32]
            .try_into()
            .unwrap(),
    ) as usize;
    let shstrtab_size = u64::from_le_bytes(
        data[shstrtab_hdr + 32..shstrtab_hdr + 40]
            .try_into()
            .unwrap(),
    ) as usize;

    // Walk section headers to find .llvm_stackmaps by name.
    for i in 0..e_shnum {
        let shdr = e_shoff + i * e_shentsize;
        let sh_name_idx = u32::from_le_bytes(data[shdr..shdr + 4].try_into().unwrap()) as usize;
        // Read the null-terminated section name from .shstrtab.
        if sh_name_idx >= shstrtab_size {
            continue;
        }
        let name_start = shstrtab_offset + sh_name_idx;
        let name_end = data[name_start..]
            .iter()
            .position(|&b| b == 0)
            .map(|p| name_start + p)
            .unwrap_or(name_start);
        if &data[name_start..name_end] != b".llvm_stackmaps" {
            continue;
        }
        // sh_flags is at shdr+8, 8 bytes LE.
        // SHF_WRITE = 0x1, SHF_ALLOC = 0x2. Set SHF_WRITE.
        let flags_offset = shdr + 8;
        let mut sh_flags =
            u64::from_le_bytes(data[flags_offset..flags_offset + 8].try_into().unwrap());
        sh_flags |= 0x1; // SHF_WRITE
        let patched = sh_flags.to_le_bytes();
        file.seek(SeekFrom::Start(flags_offset as u64))?;
        file.write_all(&patched)?;
        break;
    }

    Ok(())
}

impl TopLevelModuleInfo {
    /// Whether this module has been imported by a module whose top-level
    /// UUID is `other_uuid`.
    pub fn is_imported_by(&self, other_uuid: String) -> bool {
        self.imported_by
            .iter()
            .any(|module| module.read().unwrap().get_uuid().unwrap() == other_uuid)
    }

    /// Synthesize the `gc.safepoint_poll` function that the
    /// PlaceSafepoints pass inserts at loop back-edges and call sites.
    /// The pass looks the function up by name and inlines it at each poll
    /// site, so it must be defined (not merely declared) in the module
    /// before the pass runs.
    fn synthesize_safepoint_poll(&self) {
        synthesize_safepoint_poll_into(self.llvm_module);
    }
}

/// Synthesize `gc.safepoint_poll` (and the `pgc_collection_requested`
/// global and `pgc_enter_safepoint` declaration it references) into the
/// given module. The place-safepoints pass finds this function by name
/// and inlines a call to it at loop back-edges and call sites. Important:
/// if the poll already exists in the module, nothing is added. A free
/// function so both the per-module emit path (`TopLevelModuleInfo`) and the
/// linked-module emit path (`PekoCodegenContext`) can prepare a module
/// before the GC pass pipeline runs.
pub fn synthesize_safepoint_poll_into(module: llvm_sys_180::prelude::LLVMModuleRef) {
    // Skip if the poll already exists
    let poll_name = cstr("gc.safepoint_poll");
    let existing = unsafe { core::LLVMGetNamedFunction(module, poll_name.as_ptr()) };
    if !existing.is_null() {
        return;
    }

    let i32_type = unsafe { core::LLVMInt32Type() };
    let void_type = unsafe { core::LLVMVoidType() };

    // external global i32 @pgc_collection_requested
    let flag_name = cstr("pgc_collection_requested");
    let flag_global = unsafe {
        let existing_flag = core::LLVMGetNamedGlobal(module, flag_name.as_ptr());
        if existing_flag.is_null() {
            let global = core::LLVMAddGlobal(module, i32_type, flag_name.as_ptr());
            core::LLVMSetLinkage(global, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
            global
        } else {
            existing_flag
        }
    };

    // declare void @pgc_enter_safepoint()
    let enter_name = cstr("pgc_enter_safepoint");
    let enter_fn_type = unsafe { core::LLVMFunctionType(void_type, std::ptr::null_mut(), 0, 0) };
    let enter_fn = unsafe {
        let existing_enter = core::LLVMGetNamedFunction(module, enter_name.as_ptr());
        if existing_enter.is_null() {
            let function = core::LLVMAddFunction(module, enter_name.as_ptr(), enter_fn_type);
            core::LLVMSetLinkage(function, llvm_sys_180::LLVMLinkage::LLVMExternalLinkage);
            function
        } else {
            existing_enter
        }
    };

    // define internal void @gc.safepoint_poll()
    //
    // Internal (private) linkage is essential for per-module compilation:
    // this function is synthesized into every module that has GC functions
    // so place-safepoints can find and inline it. With external linkage,
    // linking the per-module objects together produces duplicate-symbol
    // errors for gc.safepoint_poll. Internal linkage makes each object's
    // copy private; after place-safepoints inlines it at the poll sites,
    // the standalone definition is unused and is dropped by the optimizer.
    let poll_fn_type = unsafe { core::LLVMFunctionType(void_type, std::ptr::null_mut(), 0, 0) };
    let poll_fn = unsafe { core::LLVMAddFunction(module, poll_name.as_ptr(), poll_fn_type) };
    unsafe {
        core::LLVMSetLinkage(poll_fn, llvm_sys_180::LLVMLinkage::LLVMInternalLinkage);
    }

    let builder = unsafe { core::LLVMCreateBuilder() };

    let entry_name = cstr("entry");
    let poll_block_name = cstr("poll");
    let done_name = cstr("done");
    let entry_block = unsafe { core::LLVMAppendBasicBlock(poll_fn, entry_name.as_ptr()) };
    let poll_block = unsafe { core::LLVMAppendBasicBlock(poll_fn, poll_block_name.as_ptr()) };
    let done_block = unsafe { core::LLVMAppendBasicBlock(poll_fn, done_name.as_ptr()) };

    // entry: load the flag, branch to poll if non-zero else done.
    unsafe {
        core::LLVMPositionBuilderAtEnd(builder, entry_block);
        let empty = cstr("");
        let flag_value = core::LLVMBuildLoad2(builder, i32_type, flag_global, empty.as_ptr());
        let zero = core::LLVMConstInt(i32_type, 0, 0);
        let is_set = core::LLVMBuildICmp(
            builder,
            llvm_sys_180::LLVMIntPredicate::LLVMIntNE,
            flag_value,
            zero,
            empty.as_ptr(),
        );
        core::LLVMBuildCondBr(builder, is_set, poll_block, done_block);

        // poll: call pgc_enter_safepoint, then fall through to done.
        core::LLVMPositionBuilderAtEnd(builder, poll_block);
        core::LLVMBuildCall2(
            builder,
            enter_fn_type,
            enter_fn,
            std::ptr::null_mut(),
            0,
            empty.as_ptr(),
        );
        core::LLVMBuildBr(builder, done_block);

        // done: return.
        core::LLVMPositionBuilderAtEnd(builder, done_block);
        core::LLVMBuildRetVoid(builder);

        core::LLVMDisposeBuilder(builder);
    }
}

/// Set `module`'s datalayout from the target machine's canonical layout,
/// ensuring address space 1 (managed pointers) is marked non-integral via the
/// `ni:` specifier. RewriteStatepointsForGC requires this; see the call site
/// in `run_gc_statepoint_passes` for the full rationale. The base layout comes
/// from the target machine so the result is correct on every platform; we only
/// append the non-integral marker for address space 1 if the layout does not
/// already declare it.
fn set_module_datalayout_with_managed_addrspace(
    module: llvm_sys_180::prelude::LLVMModuleRef,
    target_machine: llvm_sys_180::target_machine::LLVMTargetMachineRef,
) {
    unsafe {
        let target_data = llvm_sys_180::target_machine::LLVMCreateTargetDataLayout(target_machine);
        let layout_ptr = llvm_sys_180::target::LLVMCopyStringRepOfTargetData(target_data);

        let base_layout = if layout_ptr.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(layout_ptr)
                .to_string_lossy()
                .into_owned()
        };

        // Only add the non-integral marker for address space 1 if it is not
        // already present. A datalayout may already carry an `ni:` clause
        // (e.g. "...-ni:1" or "...-ni:1:2"); appending a second one would be
        // invalid, so detect any existing `ni:` and leave it untouched.
        let final_layout = if base_layout.contains("ni:") {
            base_layout.clone()
        } else if base_layout.is_empty() {
            // No base layout (should not happen with a real target machine, but
            // be defensive): at minimum declare address space 1 non-integral.
            String::from("ni:1")
        } else {
            format!("{base_layout}-ni:1")
        };

        let final_layout_c = cstr(&final_layout);
        core::LLVMSetDataLayout(module, final_layout_c.as_ptr());

        if !layout_ptr.is_null() {
            core::LLVMDisposeMessage(layout_ptr);
        }
        llvm_sys_180::target::LLVMDisposeTargetData(target_data);
    }
}

/// Run the GC lowering pipeline on `module`: place-safepoints inserts
/// safepoint polls (calls to the synthesized gc.safepoint_poll) at loop
/// back-edges and call sites; rewrite-statepoints-for-gc then rewrites every
/// live managed (address space 1) pointer across a safepoint to flow through
/// the gc.statepoint / gc.relocate intrinsics and arranges for the
/// .llvm_stackmaps section to be emitted during code generation. Functions
/// opt in via their statepoint GC strategy (set in create_function_raw);
/// untagged external C functions are left untouched. Returns Err with the
/// LLVM error text if the pipeline fails. A free function so both emit paths
/// (per-module and linked-module) run identical lowering before code
/// generation; without it, statepoint intrinsics survive to the object and
/// the linker reports undefined references to llvm.experimental.gc.statepoint
/// and __LLVM_StackMaps.
pub fn run_gc_statepoint_passes(
    module: llvm_sys_180::prelude::LLVMModuleRef,
    target_machine: llvm_sys_180::target_machine::LLVMTargetMachineRef,
) -> Result<(), String> {
    // RewriteStatepointsForGC requires address space 1 (our managed-pointer
    // address space) to be NON-INTEGRAL. The LLVM Statepoints documentation
    // states this as a hard assumption: "The pass assumes that all addrspace(1)
    // pointers are non-integral pointer types." Non-integral pointers cannot be
    // round-tripped through integers, which is what lets the pass soundly infer
    // base pointers and emit correct gc.relocate operands. If the module has no
    // datalayout (the default), address space 1 is integral, the base-pointer
    // inference is unsound, and the pass can emit a malformed gc.relocate whose
    // base operand points at the call target / statepoint intrinsic instead of
    // a real GC pointer, leaving llvm.experimental.gc.statepoint un-lowered and
    // undefined at link time. Set the datalayout from the target machine and
    // ensure it marks address space 1 non-integral before running the passes.
    set_module_datalayout_with_managed_addrspace(module, target_machine);

    let pass_options =
        unsafe { llvm_sys_180::transforms::pass_builder::LLVMCreatePassBuilderOptions() };
    // place-safepoints runs first so the synthesized internal
    // gc.safepoint_poll function exists when the pass looks it up by name. The
    // pass inserts a poll at each loop back-edge and call site and inlines the
    // poll body there. The poll has no callers of its own, so an optimization
    // pass ahead of it would remove it and leave place-safepoints with a null
    // poll to call. place-safepoints inserts only poll checks and does not do
    // the base-pointer inference that needs clean IR, so unoptimized IR is a
    // valid input.
    //
    // A targeted set of canonicalization passes runs between place-safepoints
    // and rewrite-statepoints-for-gc. sroa, early-cse, gvn, and simplifycfg
    // collapse redundant addrspacecast, store, and load patterns into the
    // clean SSA form the base-pointer inference expects, and simplifycfg
    // removes the now-inlined standalone poll definition. The full default<O2>
    // pipeline is not used here: some of its transforms fold the addrspacecast
    // chains around managed pointers into shapes whose base pointer
    // rewrite-statepoints-for-gc cannot infer, which produces a malformed
    // gc.relocate and leaves llvm.experimental.gc.statepoint un-lowered and
    // undefined at link time. The targeted set gives clean input without those
    // folds. Each poll reads the volatile pgc_collection_requested flag and
    // calls the external pgc_enter_safepoint, so these passes keep the poll
    // checks in place.
    //
    // rewrite-statepoints-for-gc runs last on the canonicalized IR. It routes
    // every live managed (address space 1) pointer across a safepoint through
    // the gc.statepoint and gc.relocate intrinsics and arranges for the
    // .llvm_stackmaps section to be emitted during code generation.
    let passes_c =
        c"function(place-safepoints),function(sroa,early-cse,gvn,simplifycfg),rewrite-statepoints-for-gc";
    let pass_error = unsafe {
        llvm_sys_180::transforms::pass_builder::LLVMRunPasses(
            module,
            passes_c.as_ptr(),
            target_machine,
            pass_options,
        )
    };
    unsafe {
        llvm_sys_180::transforms::pass_builder::LLVMDisposePassBuilderOptions(pass_options);
    }
    if !pass_error.is_null() {
        let mut text = String::from("GC statepoint pass failed");
        let message = unsafe { llvm_sys_180::error::LLVMGetErrorMessage(pass_error) };
        if !message.is_null() {
            text = unsafe { std::ffi::CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned();
            unsafe { llvm_sys_180::error::LLVMDisposeErrorMessage(message) };
        }
        return Err(text);
    }
    Ok(())
}

impl TopLevelModuleInfo {
    /// Emit this module's LLVM IR as a target-specific object file at
    /// `output_file`. Returns `true` on success, `false` if any of the
    /// LLVM steps failed.
    pub fn output_binary(&mut self, target: PekoTarget, output_file: impl AsRef<Path>) -> bool {
        unsafe {
            llvm_sys_180::target::LLVM_InitializeAllTargetInfos();
            llvm_sys_180::target::LLVM_InitializeAllTargets();
            llvm_sys_180::target::LLVM_InitializeAllTargetMCs();
            llvm_sys_180::target::LLVM_InitializeAllAsmParsers();
            llvm_sys_180::target::LLVM_InitializeAllAsmPrinters();
            llvm_sys_180::target::LLVM_InitializeNativeTarget();
        }

        let mut llvm_target: LLVMTargetRef =
            unsafe { llvm_sys_180::target_machine::LLVMGetFirstTarget() };

        // LLVM writes its error message into the slot pointed to by
        // `&mut error_llvm` on failure; the slot starts null and is
        // checked + drained after each call.
        let mut error_llvm: *mut std::ffi::c_char = std::ptr::null_mut();

        // Build the target triple as a NUL-terminated C string; used
        // both here and again by `LLVMCreateTargetMachine` below.
        let triple_c = cstr(target.to_triple());

        if unsafe {
            llvm_sys_180::target_machine::LLVMGetTargetFromTriple(
                triple_c.as_ptr(),
                &mut llvm_target,
                &mut error_llvm,
            )
        } == 1
        {
            drain_llvm_error(&mut error_llvm);
            return false;
        }

        let cpu_c = c"generic";
        let features_c = c"";

        let target_machine = unsafe {
            llvm_sys_180::target_machine::LLVMCreateTargetMachine(
                llvm_target,
                triple_c.as_ptr(),
                cpu_c.as_ptr(),
                features_c.as_ptr(),
                llvm_sys_180::target_machine::LLVMCodeGenOptLevel::LLVMCodeGenLevelNone,
                match target.operating_system {
                    OperatingSystem::Android | OperatingSystem::Linux => {
                        llvm_sys_180::target_machine::LLVMRelocMode::LLVMRelocPIC
                    }
                    _ => llvm_sys_180::target_machine::LLVMRelocMode::LLVMRelocDefault,
                },
                llvm_sys_180::target_machine::LLVMCodeModel::LLVMCodeModelDefault,
            )
        };

        let output_path = output_file.as_ref();
        let output_c = cstr(output_path.to_str().unwrap());

        // Synthesize the gc.safepoint_poll function so PlaceSafepoints can
        // find and inline it. Must happen before the pass pipeline runs.
        self.synthesize_safepoint_poll();

        // Lower GC safepoints and statepoints before code generation.
        if let Err(text) = run_gc_statepoint_passes(self.llvm_module, target_machine) {
            eprintln!("GC statepoint pass error: {text}");
            return false;
        }

        // Optional debug dump of the module after the GC statepoint passes
        // have run, so the rewritten IR (gc.statepoint / gc.relocate, the
        // unwrapped intrinsics) can be inspected. Gated on the
        // PEKO_DUMP_GC_IR environment variable so normal builds pay no
        // cost. Writes to "<output>.post-gc.ll".
        if std::env::var_os("PEKO_DUMP_GC_IR").is_some() {
            let dump_path = format!("{}.post-gc.ll", output_path.to_string_lossy());
            let dump_c = cstr(dump_path);
            let mut dump_error: *mut std::ffi::c_char = std::ptr::null_mut();
            unsafe {
                core::LLVMPrintModuleToFile(self.llvm_module, dump_c.as_ptr(), &mut dump_error);
            }
            drain_llvm_error(&mut dump_error);
        }

        let success = unsafe {
            llvm_sys_180::target_machine::LLVMTargetMachineEmitToFile(
                target_machine,
                self.llvm_module,
                output_c.as_ptr() as *mut c_char,
                llvm_sys_180::target_machine::LLVMCodeGenFileType::LLVMObjectFile,
                &mut error_llvm,
            )
        } == 0;

        drain_llvm_error(&mut error_llvm);

        // On aarch64 Android and x86_64 Linux, ld.lld rejects ABS64
        // relocations in .llvm_stackmaps under PIC. Patch the section
        // header to SHF_WRITE so the dynamic linker applies them instead.
        if matches!(
            target.operating_system,
            peko_core::target::OperatingSystem::Android | peko_core::target::OperatingSystem::Linux
        ) && let Err(e) = patch_stackmaps_section_writable(output_path)
        {
            eprintln!("warning: failed to patch .llvm_stackmaps: {e}");
        }

        success
    }

    /// Write this module's LLVM IR (as `.ll` text) to `path`.
    pub fn emit_ir(&mut self, path: impl AsRef<Path>) -> bool {
        let path_c = cstr(path.as_ref().to_str().unwrap());
        let result =
            unsafe { core::LLVMPrintModuleToFile(self.llvm_module, path_c.as_ptr(), null_mut()) };
        result == 1
    }

    /// Run LLVM's module verifier and print any diagnostics it produces.
    pub fn check_module(&mut self) {
        let mut out_message: *mut std::ffi::c_char = std::ptr::null_mut();

        unsafe {
            llvm_sys_180::analysis::LLVMVerifyModule(
                self.llvm_module,
                llvm_sys_180::analysis::LLVMVerifierFailureAction::LLVMPrintMessageAction,
                &mut out_message,
            );
        }

        drain_llvm_error(&mut out_message);
    }
}

/// Read, print, and dispose of an LLVM-allocated out-message.
///
/// LLVM's C API returns error / verifier messages by writing a newly
/// allocated C string into a `*mut c_char` slot supplied by the caller.
/// On a successful call the slot remains null. After reading, the
/// caller must release the allocation via `LLVMDisposeMessage` or leak
/// memory.
fn drain_llvm_error(slot: &mut *mut std::ffi::c_char) {
    if slot.is_null() {
        return;
    }

    let msg = unsafe { CStr::from_ptr(*slot) }
        .to_string_lossy()
        .into_owned();

    unsafe {
        core::LLVMDisposeMessage(*slot);
    }
    *slot = std::ptr::null_mut();

    if !msg.is_empty() {
        eprintln!("{msg}");
    }
}

/// A module in the codegen module tree. Either a top-level module
/// (which owns an `LLVMModuleRef` and tracks imports / globals state
/// via `top_level_info`) or a nested submodule (which doesn't).
#[derive(Clone)]
pub struct CodegenModule {
    pub name: String,
    pub file: PathBuf,
    pub visibility: VisibilityData,
    pub parent: Option<Arc<RwLock<CodegenModule>>>,
    pub top_level_info: Option<TopLevelModuleInfo>,

    pub modules: IndexMap<String, Arc<RwLock<CodegenModule>>>,
    pub functions: IndexMap<String, Vec<Arc<RwLock<CodegenFunction>>>>,
    pub variables: IndexMap<String, Arc<RwLock<CodegenVariable>>>,
    pub classes: IndexMap<String, Arc<RwLock<CodegenClass>>>,
    pub enums: IndexMap<String, Vec<String>>,
    pub class_generics: IndexMap<String, Arc<RwLock<CodegenClassGeneric>>>,
    pub function_generics: IndexMap<String, Arc<RwLock<CodegenFunctionGeneric>>>,
}

impl CodegenModule {
    /// Record that `imported_from` imports this module. The tracking
    /// info lives on the nearest top-level ancestor (walking up the
    /// parent chain).
    pub fn add_imported_by(&mut self, imported_from: Arc<RwLock<CodegenModule>>) {
        if let Some(top_level) = self.top_level_info.as_mut() {
            top_level.imported_by.push(imported_from);
            return;
        }

        if self.parent.is_none() {
            return;
        }

        let mut next_parent = self.parent.as_ref().unwrap().clone();
        loop {
            let has_top_level = next_parent.read().unwrap().top_level_info.is_some();
            if has_top_level {
                next_parent
                    .write()
                    .unwrap()
                    .top_level_info
                    .as_mut()
                    .unwrap()
                    .imported_by
                    .push(imported_from.clone());
            }

            if has_top_level || next_parent.read().unwrap().parent.is_none() {
                return;
            }

            let parent = next_parent.read().unwrap().parent.as_ref().unwrap().clone();
            next_parent = parent;
        }
    }

    /// Walk parent pointers until the topmost module is reached.
    pub fn get_top_parent(mut submodule: Arc<RwLock<CodegenModule>>) -> Arc<RwLock<CodegenModule>> {
        loop {
            if submodule.read().unwrap().parent.is_none() {
                return submodule;
            }

            let parent = submodule.read().unwrap().parent.as_ref().unwrap().clone();
            submodule = parent;
        }
    }

    /// The top-level UUID this module belongs to, or `None` if no
    /// ancestor in the chain has been registered as a top-level module
    /// yet.
    pub fn get_uuid(&self) -> Option<String> {
        Some(self.get_top_level()?.uuid)
    }

    /// The top-level info for this module's root, walking parent
    /// pointers until one is found.
    pub fn get_top_level(&self) -> Option<TopLevelModuleInfo> {
        if self.top_level_info.is_some() {
            return self.top_level_info.clone();
        }
        self.parent.as_ref()?;

        let mut next_parent = self.parent.as_ref().unwrap().clone();
        loop {
            let has_top_level = next_parent.read().unwrap().top_level_info.is_some();
            if has_top_level {
                return next_parent.read().unwrap().top_level_info.clone();
            }

            let parent = next_parent.read().unwrap().parent.as_ref()?.clone();
            next_parent = parent;
        }
    }

    /// Build a new top-level (root) module. Either consumes a
    /// caller-supplied `LLVMModuleRef` or creates a fresh one named
    /// after the module.
    pub fn new_top_level(
        name: impl ToString,
        file: impl AsRef<Path>,
        llvm_module: Option<LLVMModuleRef>,
        llvm_context: LLVMContextRef,
    ) -> Self {
        let name = name.to_string();

        let module = match llvm_module {
            Some(module) => module,
            None => {
                let module_name_c = cstr(&name);
                unsafe {
                    core::LLVMModuleCreateWithNameInContext(module_name_c.as_ptr(), llvm_context)
                }
            }
        };

        let global_set_name = format!("{}::set_globals", name);
        let global_set_name_c = cstr(&global_set_name);

        let globals_function_value = unsafe {
            core::LLVMAddFunction(
                module,
                global_set_name_c.as_ptr(),
                core::LLVMFunctionType(core::LLVMVoidType(), std::ptr::null_mut(), 0, 0),
            )
        };

        unsafe {
            let gc_strategy = cstr("statepoint-example");
            core::LLVMSetGC(globals_function_value, gc_strategy.as_ptr());
            // The globals init function runs initializers that allocate and
            // hold managed values, so it participates in GC and the collector
            // may walk its frame. Force the frame pointer so its frame carries
            // a standard frame record.
            crate::codegen::builders::functions::set_frame_pointer_all(globals_function_value);
        }

        Self {
            name: name.clone(),
            file: file.as_ref().to_path_buf(),
            visibility: VisibilityData::open_visibility(),
            parent: None,
            top_level_info: Some(TopLevelModuleInfo {
                llvm_module: module,
                globals_info: ModuleGlobalsInfo {
                    globals_function: CodegenValue::new(
                        globals_function_value,
                        PekoType::new(
                            Vec::new(),
                            String::new(),
                            Vec::new(),
                            0,
                            0,
                            0,
                            Some(PekoType::simple_type("void")),
                            false,
                            PositionData::default(),
                            PositionData::default(),
                        ),
                    ),
                    globals_set_name: SymbolName::from(None, None, global_set_name, None),
                    globals_to_set: Vec::new(),
                },
                modules_imported: HashMap::new(),
                imported_by: Vec::new(),
                uuid: format!("{}{}", name, uuid::Uuid::new_v4()),
                imported_styles: Vec::new(),
            }),
            modules: IndexMap::new(),
            functions: IndexMap::new(),
            variables: IndexMap::new(),
            classes: IndexMap::new(),
            enums: IndexMap::new(),
            class_generics: IndexMap::new(),
            function_generics: IndexMap::new(),
        }
    }

    /// Build a new nested (non-top-level) module.
    pub fn new(name: impl ToString, file: impl AsRef<Path>) -> Self {
        Self {
            name: name.to_string(),
            file: file.as_ref().to_path_buf(),
            visibility: VisibilityData::open_visibility(),
            parent: None,
            top_level_info: None,
            modules: IndexMap::new(),
            functions: IndexMap::new(),
            variables: IndexMap::new(),
            classes: IndexMap::new(),
            enums: IndexMap::new(),
            class_generics: IndexMap::new(),
            function_generics: IndexMap::new(),
        }
    }
}

impl
    ExecutionModule<
        CodegenModule,
        CodegenValue,
        CodegenVariable,
        CodegenFunction,
        CodegenFunctionGeneric,
        CodegenArg,
        CodegenClass,
        CodegenClassGeneric,
        CodegenVirtualTable,
        CodegenClassAttribute,
    > for CodegenModule
{
    fn get_file(&self) -> &Path {
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

    fn get_modules(&self) -> &IndexMap<String, Arc<RwLock<CodegenModule>>> {
        &self.modules
    }

    fn get_variables(&self) -> &IndexMap<String, Arc<RwLock<CodegenVariable>>> {
        &self.variables
    }

    fn get_functions(&self) -> &IndexMap<String, Vec<Arc<RwLock<CodegenFunction>>>> {
        &self.functions
    }

    fn get_function_generics(&self) -> &IndexMap<String, Arc<RwLock<CodegenFunctionGeneric>>> {
        &self.function_generics
    }

    fn get_classes(&self) -> &IndexMap<String, Arc<RwLock<CodegenClass>>> {
        &self.classes
    }

    fn get_class_generics(&self) -> &IndexMap<String, Arc<RwLock<CodegenClassGeneric>>> {
        &self.class_generics
    }

    fn get_parent(&self) -> Option<&Arc<RwLock<CodegenModule>>> {
        self.parent.as_ref()
    }

    fn get_name(&self) -> &str {
        &self.name
    }

    fn get_modules_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<CodegenModule>>> {
        &mut self.modules
    }

    fn get_variables_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<CodegenVariable>>> {
        &mut self.variables
    }

    fn get_functions_mut(&mut self) -> &mut IndexMap<String, Vec<Arc<RwLock<CodegenFunction>>>> {
        &mut self.functions
    }

    fn get_function_generics_mut(
        &mut self,
    ) -> &mut IndexMap<String, Arc<RwLock<CodegenFunctionGeneric>>> {
        &mut self.function_generics
    }

    fn get_classes_mut(&mut self) -> &mut IndexMap<String, Arc<RwLock<CodegenClass>>> {
        &mut self.classes
    }

    fn get_enums(&self) -> &IndexMap<String, Vec<String>> {
        &self.enums
    }

    fn get_enums_mut(&mut self) -> &mut IndexMap<String, Vec<String>> {
        &mut self.enums
    }

    fn get_class_generics_mut(
        &mut self,
    ) -> &mut IndexMap<String, Arc<RwLock<CodegenClassGeneric>>> {
        &mut self.class_generics
    }

    fn get_parent_mut(&mut self) -> &mut Option<Arc<RwLock<CodegenModule>>> {
        &mut self.parent
    }

    fn get_name_mut(&mut self) -> &mut String {
        &mut self.name
    }
}

// ----------------------------
// --- OPERATOR DISCRIMINANTS ---
// ----------------------------

/// Discriminant for the numerical operators recognized by codegen.
#[derive(Clone)]
pub enum NumericalOperation {
    Addition,
    Subtraction,
    Multiplication,
    Division,
    Modulus,
    Exponentiation,
    Equals,
    NotEquals,
    GreaterThan,
    GreaterThanEqual,
    LessThan,
    LessThanEqual,
}

impl NumericalOperation {
    /// Parse a string operator into a `NumericalOperation`. Returns
    /// `None` for any unrecognized operator.
    pub fn from(operator: &impl ToString) -> Option<NumericalOperation> {
        Some(match operator.to_string().as_str() {
            "+" => Self::Addition,
            "-" => Self::Subtraction,
            "*" => Self::Multiplication,
            "/" => Self::Division,
            "%" => Self::Modulus,
            "^" => Self::Exponentiation,
            "==" => Self::Equals,
            "!=" => Self::NotEquals,
            ">" => Self::GreaterThan,
            ">=" => Self::GreaterThanEqual,
            "<" => Self::LessThan,
            "<=" => Self::LessThanEqual,
            _ => return None,
        })
    }
}

/// Discriminant for the short-circuit boolean operators.
#[derive(Clone)]
pub enum BooleanOperation {
    And,
    Or,
}

impl BooleanOperation {
    /// Parse a string operator into a `BooleanOperation`. Returns
    /// `None` for any unrecognized operator.
    pub fn from(operator: &impl ToString) -> Option<BooleanOperation> {
        Some(match operator.to_string().as_str() {
            "&&" => Self::And,
            "||" => Self::Or,
            _ => return None,
        })
    }
}
