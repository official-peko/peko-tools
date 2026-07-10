//! Formatting for declaration AST nodes.
//!
//! Declarations carry a visibility prefix, optional generic parameters with
//! bounds, and a brace-delimited body. Class bodies are printed as attributes
//! followed by methods; the source order between the two groups is not tracked
//! by the AST, so this grouping is the canonical form.

use indexmap::IndexMap;

use crate::asts::CommentAST;
use crate::asts::data_structures::{
    ClassAttributeData, ClassMethod, ClassMethodInfo, DeclarationArgumentData, PositionData,
    PositionedValue, VisibilityData,
};
use crate::asts::declarations::{
    ClassAST, ClosureAST, DestructureAST, EnumDeclarationAST, FunctionDeclarationAST,
    ModuleCreationAST, NewVariableAST, TraitDeclarationAST,
};
use crate::types::{PekoType, TypeRestraint};

use super::context::FormatContext;
use super::{Format, format_block, format_delimited_list, format_sequence};

/// Emit a `[modifiers] ` visibility prefix, or nothing when no modifier is set.
fn format_visibility(visibility: &VisibilityData, ctx: &mut FormatContext) {
    let rendered = visibility.to_string();
    if rendered != "[]" {
        ctx.write(&rendered);
        ctx.write(" ");
    }
}

/// Emit a `<T: impl A, from B, U>` generic parameter list, or nothing when the
/// declaration has no parameters.
fn format_generic_parameters(
    names: &[PositionedValue<String>],
    bounds: &IndexMap<String, Vec<TypeRestraint>>,
    ctx: &mut FormatContext,
) {
    if names.is_empty() {
        return;
    }
    ctx.write("<");
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            ctx.write(", ");
        }
        ctx.write(&name.value);
        if let Some(restraints) = bounds.get(&name.value)
            && !restraints.is_empty()
        {
            ctx.write(": ");
            for (bound_index, restraint) in restraints.iter().enumerate() {
                if bound_index > 0 {
                    ctx.write(", ");
                }
                match restraint {
                    TypeRestraint::Impl(bound) => ctx.write(&format!("impl {bound}")),
                    TypeRestraint::From(bound) => ctx.write(&format!("from {bound}")),
                }
            }
        }
    }
    ctx.write(">");
}

/// Emit a `(arg: T, arg: U = default, Args<V> => rest)` parameter list.
fn format_declaration_arguments(
    arguments: &IndexMap<PositionedValue<String>, DeclarationArgumentData>,
    varargs_type: &Option<PekoType>,
    varargs_name: &PositionedValue<String>,
    ctx: &mut FormatContext,
) {
    // A parameter list is either the named arguments, plus an optional trailing
    // `Args<T> => rest` variadic. Both are gathered so the list wraps as a unit.
    enum ParamItem<'a> {
        Named(&'a PositionedValue<String>, &'a DeclarationArgumentData),
        VarArgs(&'a PekoType, &'a PositionedValue<String>),
    }

    let mut items: Vec<ParamItem> = arguments
        .iter()
        .map(|(name, data)| ParamItem::Named(name, data))
        .collect();
    if let Some(varargs_type) = varargs_type {
        items.push(ParamItem::VarArgs(varargs_type, varargs_name));
    }

    format_delimited_list(
        &items,
        "(",
        ")",
        |item, ctx| match item {
            ParamItem::Named(name, data) => {
                format_visibility(&data.visibility, ctx);
                ctx.write(&format!("{}: {}", name.value, data.argument_type));
                if let Some(default) = &data.default_value {
                    ctx.write(" = ");
                    default.format(ctx);
                }
            }
            ParamItem::VarArgs(varargs_type, varargs_name) => {
                ctx.write(&format!("Args<{varargs_type}> => {}", varargs_name.value));
            }
        },
        ctx,
    );
}

impl Format for NewVariableAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_visibility(&self.visibility, ctx);
        ctx.write(if self.constant { "const " } else { "let " });
        ctx.write(&self.name.value);
        if let Some(variable_type) = &self.variable_type {
            ctx.write(&format!(": {variable_type}"));
        }
        if let Some(value) = &self.variable_value {
            ctx.write(" = ");
            value.format(ctx);
        }
    }
}

impl Format for DestructureAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("let (");
        for (index, name) in self.names.iter().enumerate() {
            if index > 0 {
                ctx.write(", ");
            }
            ctx.write(&name.value);
        }
        ctx.write(") = ");
        self.value.format(ctx);
    }
}

impl Format for FunctionDeclarationAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_visibility(&self.visibility, ctx);
        ctx.write("fn ");
        ctx.write(&self.function_name.value);
        format_generic_parameters(&self.generic_types, &self.generic_bounds, ctx);
        format_declaration_arguments(&self.arguments, &self.varargs_type, &self.varargs_name, ctx);
        if let Some(return_type) = &self.return_type {
            ctx.write(&format!(" => {return_type}"));
        }
        // A bodyless function is an external declaration; its `;` terminator is
        // added by the sequence formatter.
        if let Some(body) = &self.function_body {
            ctx.write(" ");
            format_block(&body.value, ctx);
        }
    }
}

impl Format for ClosureAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("closure");
        if !self.captures.is_empty() {
            ctx.write("[");
            for (index, capture) in self.captures.iter().enumerate() {
                if index > 0 {
                    ctx.write(", ");
                }
                ctx.write(&capture.value);
            }
            ctx.write("]");
        }
        // A closure declares no varargs, so an empty varargs name is passed.
        let no_varargs_name = PositionedValue::create_no_position(String::new());
        format_declaration_arguments(&self.arguments, &None, &no_varargs_name, ctx);
        if let Some(return_type) = &self.return_type {
            ctx.write(&format!(" => {return_type}"));
        }
        ctx.write(" ");
        format_block(&self.closure_body.value, ctx);
    }
}

/// Render one class method: a constructor or a regular method.
fn format_class_method(method: &ClassMethod, ctx: &mut FormatContext) {
    let info: &ClassMethodInfo = method.get_info();
    format_visibility(&info.visibility, ctx);
    match method {
        ClassMethod::Constructor(_, parent_call) => {
            ctx.write("constructor");
            format_generic_parameters(&info.generic_types, &info.generic_bounds, ctx);
            format_declaration_arguments(
                &info.arguments,
                &info.varargs_type,
                &info.varargs_name,
                ctx,
            );
            if let Some(parent_call) = parent_call {
                ctx.write(" : ");
                // The parent constructor call reuses the function-call form.
                crate::asts::PekoAST::FunctionCall(parent_call.clone()).format(ctx);
            }
        }
        ClassMethod::Method(_, return_type) => {
            ctx.write(&format!("fn {}", info.name.value));
            format_generic_parameters(&info.generic_types, &info.generic_bounds, ctx);
            format_declaration_arguments(
                &info.arguments,
                &info.varargs_type,
                &info.varargs_name,
                ctx,
            );
            if let Some(return_type) = return_type {
                ctx.write(&format!(" => {return_type}"));
            }
        }
    }
    ctx.write(" ");
    format_block(&info.body.value, ctx);
}

/// One renderable item inside a class, trait, or enum body: a member or a
/// captured comment. Each carries its source position so a body prints in
/// source order with its comments interleaved in place.
enum BodyEntry<'a> {
    Attribute(&'a PositionedValue<String>, &'a ClassAttributeData),
    Method(&'a ClassMethod),
    TraitSlot(&'a FunctionDeclarationAST),
    Variant(&'a PositionedValue<String>),
    Comment(&'a CommentAST),
}

impl BodyEntry<'_> {
    /// The item's source start position, used to order entries and to detect
    /// same-line trailing comments.
    fn position(&self) -> &PositionData {
        match self {
            BodyEntry::Attribute(name, _) => &name.start,
            BodyEntry::Method(method) => &method.get_info().start,
            BodyEntry::TraitSlot(method) => &method.start,
            BodyEntry::Variant(variant) => &variant.start,
            BodyEntry::Comment(comment) => &comment.start,
        }
    }

    /// Render this item, including any member terminator (`;` or `,`). A comment
    /// prints only its own text.
    fn format_entry(&self, ctx: &mut FormatContext) {
        match self {
            BodyEntry::Attribute(name, data) => {
                format_class_attribute(name, data, ctx);
                ctx.write(";");
            }
            BodyEntry::Method(method) => format_class_method(method, ctx),
            BodyEntry::TraitSlot(method) => {
                method.format(ctx);
                // A slot with no default body is terminated with `;`.
                if method.function_body.is_none() {
                    ctx.write(";");
                }
            }
            BodyEntry::Variant(variant) => {
                ctx.write(&variant.value);
                ctx.write(",");
            }
            BodyEntry::Comment(comment) => comment.format(ctx),
        }
    }
}

/// Emit a class/trait/enum body from its entries: ` {`, each entry in source
/// order on its own line (terminated as its kind requires, with one preserved
/// blank line where the author left one and same-line trailing comments kept in
/// place), then `}`. An empty body collapses to ` {}`.
fn format_body_entries(mut entries: Vec<BodyEntry>, ctx: &mut FormatContext) {
    if entries.is_empty() {
        ctx.write(" {}");
        return;
    }

    // Members and comments interleave by source position. A member and its
    // same-line trailing comment order by column so the member prints first.
    entries.sort_by_key(|entry| (entry.position().line, entry.position().column));

    ctx.write(" {");
    ctx.newline();
    ctx.indent();

    let mut skip_next = false;
    for index in 0..entries.len() {
        // A trailing comment was already emitted on the previous line.
        if skip_next {
            skip_next = false;
            continue;
        }

        if index > 0 && ctx.blank_line_precedes(entries[index].position().line) {
            ctx.blank_line();
        }
        entries[index].format_entry(ctx);

        // A comment that follows code on its own source line is a trailing
        // comment on this entry; keep it on the same line.
        if let Some(BodyEntry::Comment(next)) = entries.get(index + 1)
            && ctx.has_code_before(next.start.line, next.start.column)
        {
            ctx.write(" ");
            next.format(ctx);
            skip_next = true;
        }
        ctx.newline();
    }

    ctx.dedent();
    ctx.write("}");
}

impl Format for ClassAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_visibility(&self.visibility, ctx);
        ctx.write(&format!("class {}", self.class_name.value));
        format_generic_parameters(&self.generics, &self.generic_bounds, ctx);

        // Every class implicitly derives from `Object`; the parser records it
        // even when the source omits it, so it is not rendered.
        let parents: Vec<String> = self
            .derives_from
            .iter()
            .map(PekoType::to_string)
            .filter(|parent| parent != "Object")
            .collect();
        if !parents.is_empty() {
            ctx.write(&format!(" from {}", parents.join(", ")));
        }
        if !self.implements.is_empty() {
            let traits: Vec<String> = self.implements.iter().map(PekoType::to_string).collect();
            ctx.write(&format!(" impl {}", traits.join(", ")));
        }

        let mut entries: Vec<BodyEntry> = Vec::new();
        for (name, attribute) in &self.attributes {
            entries.push(BodyEntry::Attribute(name, attribute));
        }
        for method in &self.methods {
            entries.push(BodyEntry::Method(method));
        }
        for comment in &self.comments {
            entries.push(BodyEntry::Comment(comment));
        }
        format_body_entries(entries, ctx);
    }
}

/// Render one class attribute: `[visibility] name: Type`.
fn format_class_attribute(
    name: &PositionedValue<String>,
    attribute: &ClassAttributeData,
    ctx: &mut FormatContext,
) {
    format_visibility(&attribute.visibility, ctx);
    ctx.write(&format!("{}: {}", name.value, attribute.attribute_type));
}

impl Format for TraitDeclarationAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_visibility(&self.visibility, ctx);
        ctx.write(&format!("trait {}", self.trait_name.value));
        format_generic_parameters(&self.generics, &IndexMap::new(), ctx);

        let mut entries: Vec<BodyEntry> = Vec::new();
        for method in &self.methods {
            entries.push(BodyEntry::TraitSlot(method));
        }
        for comment in &self.comments {
            entries.push(BodyEntry::Comment(comment));
        }
        format_body_entries(entries, ctx);
    }
}

impl Format for EnumDeclarationAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_visibility(&self.visibility, ctx);
        ctx.write(&format!("enum {}", self.enum_name.value));

        let mut entries: Vec<BodyEntry> = Vec::new();
        for variant in &self.variants {
            entries.push(BodyEntry::Variant(variant));
        }
        for comment in &self.comments {
            entries.push(BodyEntry::Comment(comment));
        }
        format_body_entries(entries, ctx);
    }
}

impl Format for ModuleCreationAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_visibility(&self.visibility, ctx);
        ctx.write(&format!("module {} ", self.module_name.value));
        if self.module_body.value.is_empty() {
            ctx.write("{}");
            return;
        }
        ctx.write("{");
        ctx.newline();
        ctx.indent();
        format_sequence(&self.module_body.value, ctx);
        ctx.dedent();
        ctx.write("}");
    }
}
