//! Formatting for statement AST nodes.
//!
//! Control-flow statements print their condition or subject as an expression
//! and their bodies through the shared block formatter, so nested statements
//! indent uniformly.

use crate::asts::data_structures::UnpackItem;
use crate::asts::statements::{
    BreakAST, ContinueAST, DemoStatementAST, ForLoopAST, IfStatementAST, ImportStatementAST,
    LinkStatementAST, PlatformStatementAST, ReturnAST, StyleStatementAST, SwitchStatementAST,
    VariableReassignmentAST, WhileLoopAST,
};

use super::context::FormatContext;
use super::{Format, format_block};

impl Format for VariableReassignmentAST {
    fn format(&self, ctx: &mut FormatContext) {
        self.variable_reference.format(ctx);
        match &self.assignment_operator {
            Some(operator) => ctx.write(&format!(" {operator}= ")),
            None => ctx.write(" = "),
        }
        self.variable_value.format(ctx);
    }
}

impl Format for ReturnAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("return");
        if let Some(value) = &self.return_value {
            ctx.write(" ");
            value.format(ctx);
        }
    }
}

impl Format for IfStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        for (index, conditional) in self.conditional_bodies.iter().enumerate() {
            if index == 0 {
                ctx.write("if ");
            } else {
                ctx.write(" else if ");
            }
            conditional.condition.format(ctx);
            ctx.write(" ");
            format_block(&conditional.body.value, ctx);
        }
        if let Some(else_body) = &self.else_body {
            ctx.write(" else ");
            format_block(&else_body.value, ctx);
        }
    }
}

impl Format for SwitchStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("switch ");
        self.subject.format(ctx);
        if self.arms.is_empty() {
            ctx.write(" {}");
            return;
        }
        ctx.write(" {");
        ctx.newline();
        ctx.indent();
        for arm in &self.arms {
            match &arm.pattern {
                Some(pattern) => pattern.format(ctx),
                None => ctx.write("_"),
            }
            ctx.write(" => ");
            format_block(&arm.body.value, ctx);
            ctx.newline();
        }
        ctx.dedent();
        ctx.write("}");
    }
}

impl Format for WhileLoopAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("while ");
        self.conditional_body.condition.format(ctx);
        ctx.write(" ");
        format_block(&self.conditional_body.body.value, ctx);
    }
}

impl Format for ForLoopAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(&format!("for {} in ", self.item_id.value));
        self.iterator.format(ctx);
        ctx.write(" ");
        format_block(&self.body.value, ctx);
    }
}

impl Format for BreakAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("break");
    }
}

impl Format for ContinueAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("continue");
    }
}

/// Render one item of an `import { ... } from` unpack list.
fn format_unpack_item(item: &UnpackItem, ctx: &mut FormatContext) {
    match item {
        UnpackItem::All => ctx.write("*"),
        UnpackItem::Symbol(name) => ctx.write(&name.value),
        UnpackItem::ModuleSymbols(module) => {
            // Nested unpack: `module::{ a, b }`. Rendered best-effort from the
            // module name and its inner items.
            ctx.write(&module.module_name.value);
            ctx.write("::{ ");
            for (index, inner) in module.unpacked_items.iter().enumerate() {
                if index > 0 {
                    ctx.write(", ");
                }
                format_unpack_item(inner, ctx);
            }
            ctx.write(" }");
        }
    }
}

impl Format for ImportStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        // An `export` re-exports the module as a submodule of the current one
        // (a package entry exposing `package::submodule`); a plain `import`
        // does not. Preserve which one it was.
        ctx.write(if self.is_export { "export " } else { "import " });

        if !self.symbols_to_unpack.is_empty() {
            ctx.write("{ ");
            for (index, item) in self.symbols_to_unpack.iter().enumerate() {
                if index > 0 {
                    ctx.write(", ");
                }
                format_unpack_item(item, ctx);
            }
            ctx.write(" } from ");
        }

        let path: Vec<&str> = self
            .module_path
            .iter()
            .map(|segment| segment.value.as_str())
            .collect();
        ctx.write(&path.join("::"));

        if let Some(version) = &self.module_version {
            ctx.write(&format!(" @\"{}\"", version.value));
        }
        if let Some(alias) = &self.import_as {
            ctx.write(&format!(" as {}", alias.value));
        }
    }
}

impl Format for LinkStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        // The parser stores the linked path with `/` separators; restore the
        // `::` source form.
        ctx.write(&format!(
            "link {} as {}",
            self.object.value.replace('/', "::"),
            self.link_as.value
        ));
    }
}

impl Format for StyleStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(&format!("style \"{}\"", self.stylesheet.value));
    }
}

impl Format for PlatformStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write(if self.architecture_test {
            "arch "
        } else {
            "platform "
        });
        let targets: Vec<&str> = self
            .targets
            .iter()
            .map(|target| target.value.as_str())
            .collect();
        ctx.write(&targets.join(", "));
        ctx.write(" ");
        format_block(&self.body.value, ctx);
    }
}

impl Format for DemoStatementAST {
    fn format(&self, ctx: &mut FormatContext) {
        ctx.write("demo ");
        format_block(&self.body.value, ctx);
    }
}
