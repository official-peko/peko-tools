//! Formatting for expression AST nodes.
//!
//! Binary and unary expressions are printed with canonical parenthesization:
//! the parser builds a faithful precedence tree, so the printer re-inserts only
//! the parentheses needed to preserve that tree and drops the rest. Everything
//! else (calls, accesses, literal collections, ranges, casts) is atomic or
//! postfix and never needs wrapping.

use crate::asts::PekoAST;
use crate::asts::data_structures::PositionedValue;
use crate::asts::expressions::{
    ArrayAST, ArrayAccessAST, BinaryExpressionAST, CastAST, CastKind, FunctionCallAST, MapAST,
    ModuleAccessAST, ObjectAccessAST, ObjectConstructionAST, PekoXTagAST, RangeAST,
    UnaryExpressionAST, UnwrapAST, VariableReferenceAST,
};
use crate::types::PekoType;

use super::context::FormatContext;
use super::{Format, format_block, format_delimited_list};

/// Binding precedence of a binary operator. Higher binds tighter. Mirrors the
/// parser's `get_operator_precedence` so the printed grouping reparses to the
/// same tree.
fn binary_precedence(operator: &str) -> i32 {
    match operator {
        ".." => 1,
        "&&" | "||" => 2,
        "==" | ">=" | ">" | "<=" | "<" | "!=" => 3,
        "+" | "-" => 4,
        "*" | "/" | "%" => 5,
        "^" => 6,
        "&" | "!" => 7,
        _ => 3,
    }
}

/// Precedence of any expression node, used to decide whether an operand needs
/// parentheses. Atomic and postfix forms return a value above every operator so
/// they are never wrapped.
fn expression_precedence(expression: &PekoAST) -> i32 {
    match expression {
        PekoAST::BinaryExpression(binary) => binary_precedence(&binary.operator),
        PekoAST::UnaryExpression(_) => 8,
        PekoAST::Range(_) => 1,
        _ => i32::MAX,
    }
}

/// Format an operand of a binary operator, wrapping it in parentheses only when
/// dropping them would change the parse. Operators are left-associative, so a
/// right operand of equal precedence must be parenthesized while a left operand
/// of equal precedence need not be.
fn format_operand(
    operand: &PekoAST,
    parent_precedence: i32,
    is_right: bool,
    ctx: &mut FormatContext,
) {
    let operand_precedence = expression_precedence(operand);
    let needs_parentheses = if is_right {
        operand_precedence <= parent_precedence
    } else {
        operand_precedence < parent_precedence
    };
    if needs_parentheses {
        ctx.write("(");
        operand.format(ctx);
        ctx.write(")");
    } else {
        operand.format(ctx);
    }
}

/// Format a `<T, U>` type-argument list. Emits nothing for an empty list.
fn format_type_arguments(types: &[PekoType], ctx: &mut FormatContext) {
    if types.is_empty() {
        return;
    }
    ctx.write("<");
    for (index, argument) in types.iter().enumerate() {
        if index > 0 {
            ctx.write(", ");
        }
        ctx.write(&argument.to_string());
    }
    ctx.write(">");
}

/// Format a parenthesized argument list, including keyword arguments written as
/// `name = value`.
fn format_arguments(
    arguments: &[(Option<PositionedValue<String>>, PekoAST)],
    ctx: &mut FormatContext,
) {
    format_delimited_list(
        arguments,
        "(",
        ")",
        |(name, value), ctx| {
            if let Some(name) = name {
                ctx.write(&name.value);
                ctx.write(" = ");
            }
            value.format(ctx);
        },
        ctx,
    );
}

impl Format for BinaryExpressionAST {
    fn format(&self, ctx: &mut FormatContext) {
        let precedence = binary_precedence(&self.operator);
        format_operand(&self.lhs, precedence, false, ctx);
        // The range operator reads as a single token with no surrounding
        // spaces (`1..10`); every other binary operator is spaced.
        if self.operator == ".." {
            ctx.write("..");
        } else {
            ctx.write(&format!(" {} ", self.operator));
        }
        format_operand(&self.rhs, precedence, true, ctx);
    }
}

impl Format for UnaryExpressionAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(&self.operator);
        // A unary operator binds tighter than every binary one, so a
        // lower-precedence operand needs parentheses.
        if expression_precedence(&self.operand) < 8 {
            ctx.write("(");
            self.operand.format(ctx);
            ctx.write(")");
        } else {
            self.operand.format(ctx);
        }
    }
}

impl Format for RangeAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_operand(&self.range_from, 1, false, ctx);
        ctx.write("..");
        format_operand(&self.range_to, 1, true, ctx);
    }
}

impl Format for VariableReferenceAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(&self.variable_name.value);
    }
}

impl Format for ArrayAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_delimited_list(
            &self.values,
            "#[",
            "]",
            |value, ctx| value.format(ctx),
            ctx,
        );
    }
}

impl Format for MapAST {
    fn format(&self, ctx: &mut FormatContext) {
        format_delimited_list(
            &self.key_values,
            "#{",
            "}",
            |(key, value), ctx| {
                key.format(ctx);
                ctx.write(": ");
                value.format(ctx);
            },
            ctx,
        );
    }
}

impl Format for FunctionCallAST {
    fn format(&self, ctx: &mut FormatContext) {
        self.function_reference.format(ctx);
        format_type_arguments(&self.function_generics, ctx);
        format_arguments(&self.arguments, ctx);
    }
}

impl Format for ObjectConstructionAST {
    fn format(&self, ctx: &mut FormatContext) {
        // Object construction is always a `new` expression; the keyword is
        // implied by this node.
        ctx.write("new ");
        ctx.write(&self.class_name.value);
        format_type_arguments(&self.object_generics, ctx);
        format_arguments(&self.arguments, ctx);
    }
}

impl Format for ObjectAccessAST {
    fn format(&self, ctx: &mut FormatContext) {
        self.object.format(ctx);
        ctx.write(".");
        self.access.format(ctx);
    }
}

impl Format for ArrayAccessAST {
    fn format(&self, ctx: &mut FormatContext) {
        self.array.format(ctx);
        ctx.write("[");
        self.access.format(ctx);
        ctx.write("]");
    }
}

impl Format for ModuleAccessAST {
    fn format(&self, ctx: &mut FormatContext) {
        for module_name in &self.module_names {
            ctx.write(&module_name.value);
            ctx.write("::");
        }
        self.accessor.format(ctx);
    }
}

impl Format for UnwrapAST {
    fn format(&self, ctx: &mut FormatContext) {
        self.optional.format(ctx);
        ctx.write("?");
        if let Some(else_body) = &self.else_body {
            ctx.write(" else ");
            format_block(&else_body.value, ctx);
        }
    }
}

impl Format for CastAST {
    fn format(&self, ctx: &mut FormatContext) {
        match self.kind {
            CastKind::Checked => {
                self.value.format(ctx);
                ctx.write(&format!(" as {}", self.cast_to));
            }
            CastKind::Forced => {
                ctx.write(&format!("danger_cast<{}>(", self.cast_to));
                self.value.format(ctx);
                ctx.write(")");
            }
            CastKind::Constant => {
                ctx.write(&format!("constant<{}>(", self.cast_to));
                self.value.format(ctx);
                ctx.write(")");
            }
        }
    }
}

impl Format for PekoXTagAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(&format!("<{}", self.tag));

        // Attributes and events live in hash maps, so their names are sorted
        // for a deterministic, canonical ordering.
        let mut attribute_names: Vec<&String> = self.attributes.keys().collect();
        attribute_names.sort();
        for name in attribute_names {
            ctx.write(&format!(" {name}="));
            self.attributes[name].format(ctx);
        }

        let mut event_names: Vec<&String> = self.events.keys().collect();
        event_names.sort();
        for name in event_names {
            ctx.write(&format!(" {name}=closure {{", ));
            let body = &self.events[name];
            if !body.value.is_empty() {
                ctx.write(" ");
                for (index, statement) in body.value.iter().enumerate() {
                    if index > 0 {
                        ctx.write("; ");
                    }
                    statement.format(ctx);
                }
                ctx.write(" ");
            }
            ctx.write("}");
        }

        let has_children = !self.children.is_empty() || !self.inner_text.is_empty();
        if !has_children {
            ctx.write(" />");
            return;
        }

        ctx.write(">");
        for child in &self.children {
            child.format(ctx);
        }
        ctx.write(&format!("</{}>", self.tag));
    }
}
