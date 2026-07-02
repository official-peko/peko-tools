//! Parsing `.peko.h` FFI headers.
//!
//! A `.peko.h` is a C header that doubles as a Peko FFI surface. It includes
//! `peko.h` for the `p_*` type aliases, then marks the declarations that cross
//! into Peko with the `p_fn` and `p_var` markers:
//!
//! ```c
//! #include <peko.h>
//!
//! p_fn p_gc_opaque mem_alloc(p_i32 bytes);
//! p_var p_i32 count;
//! ```
//!
//! [`parse_header`] reads the file, finds the marked declarations, maps the
//! `p_*` vocabulary to Peko FFI types, and returns the declarations plus any
//! parse errors. Unmarked declarations, preprocessor lines, and ordinary
//! comments are ignored. This is a standalone parser with its own tokenizer; it
//! does not reuse the Peko source parser.

mod parser;

use std::fmt::Write;
use std::path::Path;

pub use parser::parse_header;

/// Reports whether a resolved module path is a `.peko.h` FFI header rather
/// than an ordinary `.peko` source file. The import path branches on this to
/// parse the file as an FFI surface.
pub fn is_ffi_header(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".peko.h")
}

/// Lowers the marked functions of a parsed header to equivalent external
/// Peko declaration source.
///
/// Each function becomes a bodyless `[external]` declaration. The `external`
/// attribute keeps the symbol's raw C name through codegen, so a call routes
/// to the C function by its own name. A variadic function adds the `variadic`
/// attribute. A function whose return type is `void` omits the `=> Type`
/// clause, matching the Peko surface for a function with no return type.
///
/// The generated source is handed to the Peko parser by the import path, so
/// the FFI declarations flow through the same module machinery as ordinary
/// external declarations. Variables are not lowered here; the import path
/// reports them separately.
pub fn header_to_peko_source(header: &ParsedHeader) -> String {
    let mut source = String::new();
    for function in &header.module.functions {
        let mut modifiers = vec!["external"];
        if function.variadic {
            modifiers.push("variadic");
        }
        if function.gc_safe {
            modifiers.push("gcsafe");
        }
        let attributes = format!("[{}]", modifiers.join(" "));

        let params: Vec<String> = function
            .params
            .iter()
            .map(|param| format!("{}: {}", param.name, param.ty.peko))
            .collect();

        if function.return_type.peko == "void" {
            let _ = writeln!(
                source,
                "{attributes} fn {}({});",
                function.name,
                params.join(", ")
            );
        } else {
            let _ = writeln!(
                source,
                "{attributes} fn {}({}) => {};",
                function.name,
                params.join(", "),
                function.return_type.peko
            );
        }
    }

    // A variable becomes an external global with no initializer: the C side
    // owns the storage, and codegen emits a declaration-only reference to it.
    for variable in &header.module.variables {
        let _ = writeln!(
            source,
            "[external] let {}: {};",
            variable.name, variable.ty.peko
        );
    }

    source
}

/// A parsed FFI header: the declarations that crossed into Peko, plus any
/// errors found while parsing.
#[derive(Debug, Clone, Default)]
pub struct ParsedHeader {
    /// The FFI declarations.
    pub module: FfiModule,
    /// Errors found while parsing. A declaration that errors is skipped and
    /// parsing resumes at the next marker.
    pub errors: Vec<FfiError>,
}

/// The FFI declarations of one `.peko.h`.
#[derive(Debug, Clone, Default)]
pub struct FfiModule {
    /// The marked functions.
    pub functions: Vec<FfiFunction>,
    /// The marked variables.
    pub variables: Vec<FfiVariable>,
}

/// A marked FFI function.
#[derive(Debug, Clone)]
pub struct FfiFunction {
    /// The function name.
    pub name: String,
    /// The return type.
    pub return_type: FfiType,
    /// The parameters, in order.
    pub params: Vec<FfiParam>,
    /// Whether the function ends with a C variadic (`...`).
    pub variadic: bool,
    /// Whether the declaration carries the `p_gcsafe` attribute, marking the
    /// function as a GC safepoint. A safepoint call can collect, so codegen
    /// does not treat it as a leaf.
    pub gc_safe: bool,
}

/// One parameter of an FFI function.
#[derive(Debug, Clone)]
pub struct FfiParam {
    /// The parameter name.
    pub name: String,
    /// The parameter type.
    pub ty: FfiType,
}

/// A marked FFI variable.
#[derive(Debug, Clone)]
pub struct FfiVariable {
    /// The variable name.
    pub name: String,
    /// The variable type.
    pub ty: FfiType,
}

/// An FFI type, as a Peko type expression.
///
/// The `p_*` aliases map to Peko FFI types: scalars (`i32`, `f32`, `bool`,
/// `char`), `cstr`, `opaque`, and the managed `pointer<...>` forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfiType {
    /// The Peko type expression, for example `i32`, `cstr`, `pointer<void>`,
    /// or `pointer<MyType>`.
    pub peko: String,
}

/// One failure while parsing a `.peko.h`.
#[derive(Debug, Clone)]
pub struct FfiError {
    /// The 1-based line of the offending token.
    pub line: usize,
    /// The 1-based column of the offending token.
    pub column: usize,
    /// A description of the problem.
    pub message: String,
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_ffi_header_matches_only_peko_h() {
        assert!(is_ffi_header(Path::new("clib/file.peko.h")));
        assert!(!is_ffi_header(Path::new("clib/file.peko")));
        assert!(!is_ffi_header(Path::new("clib/file.h")));
    }

    #[test]
    fn lowers_functions_to_external_declarations() {
        let header = "\
            p_fn p_gc_opaque mem_alloc(p_i32 bytes);\n\
            p_fn p_i32 puts(p_cstr text);\n\
            p_fn void log_init();\n";
        let parsed = parse_header(header);
        assert!(parsed.errors.is_empty());

        let source = header_to_peko_source(&parsed);
        assert_eq!(
            source,
            "[external] fn mem_alloc(bytes: i32) => pointer<void>;\n\
             [external] fn puts(text: cstr) => i32;\n\
             [external] fn log_init();\n"
        );
    }

    #[test]
    fn lowers_variadic_with_the_variadic_attribute() {
        let header = "p_fn p_i32 printf(p_cstr format, ...);\n";
        let parsed = parse_header(header);
        assert!(parsed.errors.is_empty());

        let source = header_to_peko_source(&parsed);
        assert_eq!(
            source,
            "[external variadic] fn printf(format: cstr) => i32;\n"
        );
    }

    #[test]
    fn gcsafe_attribute_parses_and_lowers() {
        let header = "p_fn p_gcsafe p_gc_opaque run(p_gc(Buffer) b);\n";
        let parsed = parse_header(header);
        assert!(parsed.errors.is_empty(), "errors: {:?}", parsed.errors);
        assert!(parsed.module.functions[0].gc_safe);

        let source = header_to_peko_source(&parsed);
        assert_eq!(
            source,
            "[external gcsafe] fn run(b: pointer<Buffer>) => pointer<void>;\n"
        );
    }

    #[test]
    fn variadic_and_gcsafe_compose() {
        let header = "p_fn p_gcsafe p_i32 logf(p_cstr fmt, ...);\n";
        let parsed = parse_header(header);
        assert!(parsed.errors.is_empty());

        let source = header_to_peko_source(&parsed);
        assert_eq!(
            source,
            "[external variadic gcsafe] fn logf(fmt: cstr) => i32;\n"
        );
    }

    #[test]
    fn functions_without_the_attribute_are_not_gcsafe() {
        let parsed = parse_header("p_fn void plain(void);\n");
        assert!(!parsed.module.functions[0].gc_safe);
    }

    #[test]
    fn lowers_variables_to_external_globals() {
        let header = "\
            p_var p_i64 frame_count;\n\
            p_var p_gc_opaque shared_buffer;\n";
        let parsed = parse_header(header);
        assert!(parsed.errors.is_empty());

        let source = header_to_peko_source(&parsed);
        assert_eq!(
            source,
            "[external] let frame_count: i64;\n\
             [external] let shared_buffer: pointer<void>;\n"
        );
    }
}
