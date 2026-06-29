//! `PekoValueBuilder` implementations for the value-producing AST nodes:
//! booleans, numbers, characters, strings (plain and encrypted), and the
//! null literal.

use peko_core::asts::values::{
    BooleanAST, CharAST, EncryptedStringAST, NullAST, NumberAST, StringAST,
};
use peko_core::diagnostics;
use peko_core::execution::ExecutionContextAlgorithms;
use peko_core::types::PekoType;

use crate::codegen::PekoValueBuilder;
use crate::codegen::builders::prelude::*;
use crate::codegen::context::PekoCodegenContext;
use crate::codegen::data_structures::{CodegenValue, managed_pointer_type};

impl PekoValueBuilder for BooleanAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // A bool literal is the boxed `bool` value type wrapping a raw i1.
        let raw = codegen_context.create_constant_boolean(self.value.value);
        codegen_context
            .box_value_to_type(&PekoType::simple_type("bool"), &raw)
            .unwrap_or_else(|| codegen_context.create_error_value())
    }
}

impl PekoValueBuilder for NumberAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // A number literal is the boxed `number` value type wrapping a raw
        // f64. Build the raw double and box it; raw machine integers come from
        // FFI and `constant<T>(...)`, not bare literals.
        let raw = codegen_context.create_constant_double(self.value.value);
        codegen_context
            .box_value_to_type(&PekoType::simple_type("number"), &raw)
            .unwrap_or_else(|| codegen_context.create_error_value())
    }
}

impl PekoValueBuilder for CharAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // A char literal is the boxed `char` value type wrapping a raw i8.
        let raw = codegen_context.create_constant_char(self.value.value);
        codegen_context
            .box_value_to_type(&PekoType::simple_type("char"), &raw)
            .unwrap_or_else(|| codegen_context.create_error_value())
    }
}

impl PekoValueBuilder for EncryptedStringAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Allocate the encrypted string's backing buffer: one byte per
        // character plus the NUL terminator. The buffer is a managed
        // `Pointer<char>` (char is one byte, so the byte count is the
        // character count plus one).
        let buffer_type = managed_pointer_type(PekoType::simple_type("char"));
        let mut allocate_string =
            match codegen_context.allocate_raw(self.encrypt.value.len() + 1, &buffer_type) {
                Some(value) => value,
                None => {
                    codegen_context.diagnostics.report_diagnostic(
                        diagnostics::PekoDiagnostic::new(
                            self.encrypt.start.clone(),
                            self.encrypt.end.clone(),
                            "a bug has occurred in the linkage of the standard library".to_string(),
                            diagnostics::DiagnosticType::Error,
                            codegen_context.get_current_file().to_path_buf(),
                        ),
                    );
                    return codegen_context.create_error_value();
                }
            };
        allocate_string.value_type = buffer_type;

        // Write each character of the source string into the buffer.
        for (index, character) in self.encrypt.value.chars().enumerate() {
            let index = codegen_context.create_constant_int64(index as i32);
            let current_string_element =
                codegen_context.get_array_element(&allocate_string, &index);
            let char_value = codegen_context.create_constant_char(character);
            codegen_context.build_store(&current_string_element, &char_value);
        }

        // Append the NUL terminator.
        let last_index = codegen_context.create_constant_int64(self.encrypt.value.len() as i32);
        let last_string_element = codegen_context.get_array_element(&allocate_string, &last_index);
        let null_char = codegen_context.create_constant_char('\0');
        codegen_context.build_store(&last_string_element, &null_char);

        allocate_string
    }
}

impl PekoValueBuilder for NullAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        codegen_context.create_null_pointer()
    }
}

impl PekoValueBuilder for StringAST {
    fn build_value(&self, codegen_context: &mut PekoCodegenContext) -> CodegenValue {
        // Plain (non-interpolated) string: emit a single C string constant.
        if !self.interpolated {
            let text = if self.chunks.is_empty() {
                String::new()
            } else {
                self.chunks[0].get_text()
            };
            return codegen_context.create_string(text);
        }

        // Interpolated path: build a `%`-delimited format string and a
        // parallel list of values, then hand both to the runtime's
        // `unsafe_format` function.
        let mut format_string = String::new();
        let mut interpolated_values = Vec::new();

        for chunk in &self.chunks {
            if chunk.is_text() {
                format_string.push_str(chunk.get_text().as_str());
                continue;
            }

            format_string.push('%');

            // Codegen every AST in the interpolation body. Only the last
            // value is used; the earlier ones are for side effects.
            let mut built_values = Vec::new();
            for ast in &chunk.get_interpolation() {
                built_values.push(ast.build_value(codegen_context));
            }

            if built_values.is_empty() {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        chunk.start.clone(),
                        chunk.end.clone(),
                        "expected a value to interpolate but got nothing".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
                continue;
            }

            let last_value = built_values.last().unwrap();
            if !codegen_context
                .types_similar(&last_value.value_type, &PekoType::simple_type("string"))
            {
                codegen_context
                    .diagnostics
                    .report_diagnostic(diagnostics::PekoDiagnostic::new(
                        chunk.start.clone(),
                        chunk.end.clone(),
                        "expected a string for interpolated value".to_string(),
                        diagnostics::DiagnosticType::Error,
                        codegen_context.get_current_file().to_path_buf(),
                    ));
            } else {
                interpolated_values.push(last_value.clone());
            }
        }

        let format_string = codegen_context.create_string(format_string);

        let interpolation_list = codegen_context
            .create_standard_array(&PekoType::simple_type("string"), interpolated_values)
            .unwrap_or_else(|| codegen_context.create_error_value());

        let (previous_line, previous_file) = codegen_context.track_call_position(
            self.start.file.to_string_lossy().into_owned(),
            self.start.line,
        );

        let format_call = codegen_context.call_named_function(
            "standard::unsafe_format",
            vec![format_string, interpolation_list],
        );

        codegen_context.reset_call_position(&previous_line, &previous_file);

        format_call.unwrap_or_else(|| codegen_context.create_error_value())
    }
}
