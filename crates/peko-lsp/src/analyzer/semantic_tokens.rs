//! Semantic-token classification.
//!
//! Walks a file's own AST (parsed without the import prelude) and emits a
//! classified span for every identifier: declarations, references, parameters,
//! fields, enum variants, module paths, and type annotations. The result is a
//! flat list of engine-neutral [`SemanticToken`]s sorted by position; the wire
//! boundary transcodes and delta-encodes them.
//!
//! Classification is structural, from the shape of the AST, not from full
//! name resolution. A first pass records the file's declared class, enum,
//! trait, function, and module names so a bare reference to one is colored by
//! its declared kind; anything else falls back to `variable`.

use std::collections::HashMap;
use std::path::Path;

use peko_core::asts::PekoAST;
use peko_core::asts::data_structures::{ClassMethod, PositionedValue};
use peko_core::asts::declarations::FunctionDeclarationAST;
use peko_core::lexer::TokenList;
use peko_core::parser::PekoParser;
use peko_core::types::{PekoType, PekoTypeKind};

use crate::server::analysis::{Position, Range, SemanticToken, sem};

/// Classify the file's identifiers into semantic tokens.
pub fn collect(file: &Path, source: &str) -> Vec<SemanticToken> {
    let asts = parse_file(file, source);

    let mut decl_kinds = HashMap::new();
    collect_decl_kinds(&asts, &mut decl_kinds);

    let mut ctx = Ctx {
        out: Vec::new(),
        decl_kinds,
    };
    ctx.walk_body(&asts);

    let mut tokens = ctx.out;
    tokens.sort_by(|a, b| {
        (a.range.start.line, a.range.start.character)
            .cmp(&(b.range.start.line, b.range.start.character))
    });
    // Drop exact-duplicate spans so the delta encoding never emits a zero-width
    // repeat at the same position.
    tokens.dedup_by(|a, b| a.range == b.range);
    tokens
}

/// Parse the file into its own top-level ASTs, without the analyzer's default
/// import prelude, so no synthetic nodes leak into the token stream.
fn parse_file(file: &Path, source: &str) -> Vec<PekoAST> {
    let mut parser = PekoParser::new(TokenList::from_source(source, file), file);
    let mut asts = Vec::new();
    let max_iterations = source.len() + 1;
    let mut iterations = 0;
    while !parser.tokens.finished() || parser.has_pending() {
        iterations += 1;
        if iterations > max_iterations {
            break;
        }
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
        asts.push(parser.parse());
        if parser.tokens.current_token().equals(";") || parser.tokens.current_token().equals("}") {
            parser.tokens.increase_index();
        }
    }
    asts
}

/// Record the kind of every named declaration so bare references resolve to it.
fn collect_decl_kinds(asts: &[PekoAST], map: &mut HashMap<String, u32>) {
    for ast in asts {
        match ast {
            PekoAST::Class(c) => {
                map.insert(c.class_name.value.clone(), sem::CLASS);
            }
            PekoAST::Enum(e) => {
                map.insert(e.enum_name.value.clone(), sem::ENUM);
            }
            PekoAST::Trait(t) => {
                map.insert(t.trait_name.value.clone(), sem::INTERFACE);
            }
            PekoAST::FunctionDeclaration(f) => {
                map.insert(f.function_name.value.clone(), sem::FUNCTION);
            }
            PekoAST::ModuleCreation(m) => {
                map.insert(m.module_name.value.clone(), sem::NAMESPACE);
                collect_decl_kinds(&m.module_body.value, map);
            }
            _ => {}
        }
    }
}

struct Ctx {
    out: Vec<SemanticToken>,
    decl_kinds: HashMap<String, u32>,
}

impl Ctx {
    fn push_name(&mut self, name: &PositionedValue<String>, token_type: u32, modifiers: u32) {
        let len = name.value.chars().count() as u32;
        let line = name.start.line.saturating_sub(1) as u32;
        let ch = name.start.column as u32;
        self.push_span(line, ch, len, token_type, modifiers);
    }

    /// Emit a token for a name that may be qualified (`a::b::Name`): each
    /// leading segment is a namespace and the final segment gets `final_kind`.
    fn push_qualified_name(
        &mut self,
        name: &PositionedValue<String>,
        final_kind: u32,
        modifiers: u32,
    ) {
        let line = name.start.line.saturating_sub(1) as u32;
        let mut col = name.start.column as u32;
        let parts: Vec<&str> = name.value.split("::").collect();
        let last = parts.len().saturating_sub(1);
        for (index, part) in parts.iter().enumerate() {
            let len = part.chars().count() as u32;
            if index == last {
                self.push_span(line, col, len, final_kind, modifiers);
            } else {
                self.push_span(line, col, len, sem::NAMESPACE, 0);
            }
            col += len + 2;
        }
    }

    fn push_span(&mut self, line: u32, ch: u32, len: u32, token_type: u32, modifiers: u32) {
        if len == 0 {
            return;
        }
        self.out.push(SemanticToken {
            range: Range {
                start: Position {
                    line,
                    character: ch,
                },
                end: Position {
                    line,
                    character: ch + len,
                },
            },
            token_type,
            modifiers,
        });
    }

    fn walk_body(&mut self, body: &[PekoAST]) {
        for ast in body {
            self.walk(ast);
        }
    }

    fn walk_opt(&mut self, ast: &Option<Box<PekoAST>>) {
        if let Some(inner) = ast {
            self.walk(inner);
        }
    }

    fn walk_type(&mut self, ty: &PekoType) {
        match &ty.kind {
            PekoTypeKind::Basic { info, .. } => {
                // A trailing `?` desugars to a synthetic `Option` wrapper whose
                // name is not written in the source; it shares its start with
                // its single generic. Skip the wrapper name and color the inner
                // type instead.
                let synthetic = info.generics.len() == 1
                    && info.generics[0].start_position.index == ty.start_position.index;
                if !synthetic {
                    // The type starts after any leading `&` reference markers,
                    // then runs `module::module::Name`. Each module segment is a
                    // namespace and the trailing name is the type; the `::`
                    // separators are two chars each.
                    let line = ty.start_position.line.saturating_sub(1) as u32;
                    let mut col = ty.start_position.column as u32 + ty.reference_depth as u32;
                    for module in &info.module_names {
                        let len = module.chars().count() as u32;
                        self.push_span(line, col, len, sem::NAMESPACE, 0);
                        col += len + 2;
                    }
                    self.push_span(line, col, info.name.chars().count() as u32, sem::TYPE, 0);
                }
                for generic in &info.generics {
                    self.walk_type(generic);
                }
            }
            PekoTypeKind::Generic { name, .. } => {
                let line = ty.start_position.line.saturating_sub(1) as u32;
                let ch = ty.start_position.column as u32 + ty.reference_depth as u32;
                let len = name.chars().count() as u32;
                self.push_span(line, ch, len, sem::TYPE_PARAMETER, 0);
            }
            PekoTypeKind::Function {
                arguments,
                return_type,
                ..
            } => {
                for arg in arguments {
                    self.walk_type(arg);
                }
                if let Some(rt) = return_type {
                    self.walk_type(rt);
                }
            }
        }
    }

    /// A named-argument list: keyword names are parameters, values recurse.
    fn walk_call_args(&mut self, args: &[(Option<PositionedValue<String>>, PekoAST)]) {
        for (keyword, value) in args {
            if let Some(name) = keyword {
                self.push_name(name, sem::PARAMETER, 0);
            }
            self.walk(value);
        }
    }

    fn walk_function_decl(&mut self, decl: &FunctionDeclarationAST, name_kind: u32) {
        self.push_name(&decl.function_name, name_kind, sem::MOD_DECLARATION);
        for generic in &decl.generic_types {
            self.push_name(generic, sem::TYPE_PARAMETER, sem::MOD_DECLARATION);
        }
        for (param, data) in &decl.arguments {
            self.push_name(param, sem::PARAMETER, sem::MOD_DECLARATION);
            self.walk_type(&data.argument_type);
            if let Some(default) = &data.default_value {
                self.walk(default);
            }
        }
        if let Some(rt) = &decl.return_type {
            self.walk_type(rt);
        }
        if let Some(body) = &decl.function_body {
            self.walk_body(&body.value);
        }
    }

    fn walk(&mut self, ast: &PekoAST) {
        match ast {
            // ----- values -------------------------------------------------
            PekoAST::Char(_)
            | PekoAST::Number(_)
            | PekoAST::Boolean(_)
            | PekoAST::String(_)
            | PekoAST::EncryptedString(_)
            | PekoAST::Null(_)
            | PekoAST::Break(_)
            | PekoAST::Continue(_)
            | PekoAST::Placeholder(_)
            | PekoAST::Comment(_)
            | PekoAST::LinkStatement(_)
            | PekoAST::StyleStatement(_) => {}

            // ----- expressions --------------------------------------------
            PekoAST::Array(a) => self.walk_body(&a.values),
            PekoAST::Map(m) => {
                for (key, value) in &m.key_values {
                    self.walk(key);
                    self.walk(value);
                }
            }
            PekoAST::VariableReference(v) => {
                let kind = self
                    .decl_kinds
                    .get(&v.variable_name.value)
                    .copied()
                    .unwrap_or(sem::VARIABLE);
                self.push_name(&v.variable_name, kind, 0);
            }
            PekoAST::FunctionCall(call) => {
                match call.function_reference.as_ref() {
                    PekoAST::VariableReference(v) => {
                        self.push_name(&v.variable_name, sem::FUNCTION, 0);
                    }
                    PekoAST::ObjectAccess(oa) => {
                        self.walk(&oa.object);
                        if let PekoAST::VariableReference(v) = oa.access.as_ref() {
                            self.push_name(&v.variable_name, sem::METHOD, 0);
                        } else {
                            self.walk(&oa.access);
                        }
                    }
                    PekoAST::ModuleAccess(ma) => {
                        for module in &ma.module_names {
                            self.push_name(module, sem::NAMESPACE, 0);
                        }
                        if let PekoAST::VariableReference(v) = ma.accessor.as_ref() {
                            self.push_name(&v.variable_name, sem::FUNCTION, 0);
                        } else {
                            self.walk(&ma.accessor);
                        }
                    }
                    other => self.walk(other),
                }
                for generic in &call.function_generics {
                    self.walk_type(generic);
                }
                self.walk_call_args(&call.arguments);
            }
            PekoAST::ObjectConstruction(oc) => {
                self.push_qualified_name(&oc.class_name, sem::CLASS, 0);
                for generic in &oc.object_generics {
                    self.walk_type(generic);
                }
                self.walk_call_args(&oc.arguments);
            }
            PekoAST::ObjectAccess(oa) => {
                self.walk(&oa.object);
                match oa.access.as_ref() {
                    // `obj.field`
                    PekoAST::VariableReference(v) => {
                        self.push_name(&v.variable_name, sem::PROPERTY, 0);
                    }
                    // `obj.method(...)` parses with the call as the access child.
                    PekoAST::FunctionCall(call) => {
                        if let PekoAST::VariableReference(v) = call.function_reference.as_ref() {
                            self.push_name(&v.variable_name, sem::METHOD, 0);
                        } else {
                            self.walk(&call.function_reference);
                        }
                        for generic in &call.function_generics {
                            self.walk_type(generic);
                        }
                        self.walk_call_args(&call.arguments);
                    }
                    other => self.walk(other),
                }
            }
            PekoAST::ArrayAccess(aa) => {
                self.walk(&aa.array);
                self.walk(&aa.access);
            }
            PekoAST::BinaryExpression(b) => {
                self.walk(&b.lhs);
                self.walk(&b.rhs);
            }
            PekoAST::UnaryExpression(u) => self.walk(&u.operand),
            PekoAST::ModuleAccess(ma) => {
                for module in &ma.module_names {
                    self.push_name(module, sem::NAMESPACE, 0);
                }
                self.walk(&ma.accessor);
            }
            PekoAST::Unwrap(u) => {
                self.walk(&u.optional);
                if let Some(else_body) = &u.else_body {
                    self.walk_body(&else_body.value);
                }
            }
            PekoAST::Cast(c) => {
                self.walk(&c.value);
                self.walk_type(&c.cast_to);
            }
            PekoAST::Range(r) => {
                self.walk(&r.range_from);
                self.walk(&r.range_to);
            }
            PekoAST::PekoXTag(tag) => {
                // The tag name follows the opening `<`. Coloring it here (from
                // the AST) avoids the grammar ambiguity between `<Tag>` and a
                // generic like `Array<T>`.
                let line = tag.start.line.saturating_sub(1) as u32;
                let name_col = tag.start.column as u32 + 1;
                self.push_span(line, name_col, tag.tag.chars().count() as u32, sem::TYPE, 0);
                for value in tag.attributes.values() {
                    self.walk(value);
                }
                for body in tag.events.values() {
                    self.walk_body(&body.value);
                }
                self.walk_body(&tag.children);
            }

            // ----- statements ---------------------------------------------
            PekoAST::VariableReassignment(r) => {
                self.walk(&r.variable_reference);
                self.walk(&r.variable_value);
            }
            PekoAST::Return(r) => self.walk_opt(&r.return_value),
            PekoAST::IfStatement(i) => {
                for arm in &i.conditional_bodies {
                    self.walk(&arm.condition);
                    self.walk_body(&arm.body.value);
                }
                if let Some(else_body) = &i.else_body {
                    self.walk_body(&else_body.value);
                }
            }
            PekoAST::Switch(s) => {
                self.walk(&s.subject);
                for arm in &s.arms {
                    if let Some(pattern) = &arm.pattern {
                        self.walk(pattern);
                    }
                    self.walk_body(&arm.body.value);
                }
            }
            PekoAST::WhileLoop(w) => {
                self.walk(&w.conditional_body.condition);
                self.walk_body(&w.conditional_body.body.value);
            }
            PekoAST::ForLoop(f) => {
                self.push_name(&f.item_id, sem::VARIABLE, sem::MOD_DECLARATION);
                self.walk(&f.iterator);
                self.walk_body(&f.body.value);
            }
            PekoAST::PlatformStatement(p) => self.walk_body(&p.body.value),
            PekoAST::ImportStatement(imp) => {
                for segment in &imp.module_path {
                    self.push_name(segment, sem::NAMESPACE, 0);
                }
                if let Some(alias) = &imp.import_as {
                    self.push_name(alias, sem::NAMESPACE, sem::MOD_DECLARATION);
                }
            }

            // ----- declarations -------------------------------------------
            PekoAST::NewVariable(v) => {
                self.push_name(&v.name, sem::VARIABLE, sem::MOD_DECLARATION);
                if let Some(ty) = &v.variable_type {
                    self.walk_type(ty);
                }
                if let Some(value) = &v.variable_value {
                    self.walk(value);
                }
            }
            PekoAST::Destructure(d) => {
                for name in &d.names {
                    self.push_name(name, sem::VARIABLE, sem::MOD_DECLARATION);
                }
            }
            PekoAST::FunctionDeclaration(f) => self.walk_function_decl(f, sem::FUNCTION),
            PekoAST::Closure(c) => {
                for (param, data) in &c.arguments {
                    self.push_name(param, sem::PARAMETER, sem::MOD_DECLARATION);
                    self.walk_type(&data.argument_type);
                }
                for capture in &c.captures {
                    self.push_name(capture, sem::VARIABLE, 0);
                }
                if let Some(rt) = &c.return_type {
                    self.walk_type(rt);
                }
                self.walk_body(&c.closure_body.value);
            }
            PekoAST::Class(c) => {
                self.push_name(&c.class_name, sem::CLASS, sem::MOD_DECLARATION);
                for generic in &c.generics {
                    self.push_name(generic, sem::TYPE_PARAMETER, sem::MOD_DECLARATION);
                }
                for parent in &c.derives_from {
                    self.walk_type(parent);
                }
                for implemented in &c.implements {
                    self.walk_type(implemented);
                }
                for (attr, data) in &c.attributes {
                    self.push_name(attr, sem::PROPERTY, sem::MOD_DECLARATION);
                    self.walk_type(&data.attribute_type);
                }
                for method in &c.methods {
                    let info = method.get_info();
                    self.push_name(&info.name, sem::METHOD, sem::MOD_DECLARATION);
                    for generic in &info.generic_types {
                        self.push_name(generic, sem::TYPE_PARAMETER, sem::MOD_DECLARATION);
                    }
                    for (param, data) in &info.arguments {
                        self.push_name(param, sem::PARAMETER, sem::MOD_DECLARATION);
                        self.walk_type(&data.argument_type);
                    }
                    match method {
                        ClassMethod::Method(_, Some(rt)) => self.walk_type(rt),
                        ClassMethod::Constructor(_, Some(super_call)) => {
                            self.walk_call_args(&super_call.arguments);
                        }
                        _ => {}
                    }
                    self.walk_body(&info.body.value);
                }
            }
            PekoAST::Trait(t) => {
                self.push_name(&t.trait_name, sem::INTERFACE, sem::MOD_DECLARATION);
                for generic in &t.generics {
                    self.push_name(generic, sem::TYPE_PARAMETER, sem::MOD_DECLARATION);
                }
                for method in &t.methods {
                    self.walk_function_decl(method, sem::METHOD);
                }
            }
            PekoAST::Enum(e) => {
                self.push_name(&e.enum_name, sem::ENUM, sem::MOD_DECLARATION);
                for variant in &e.variants {
                    self.push_name(variant, sem::ENUM_MEMBER, sem::MOD_DECLARATION);
                }
            }
            PekoAST::ModuleCreation(m) => {
                self.push_name(&m.module_name, sem::NAMESPACE, sem::MOD_DECLARATION);
                self.walk_body(&m.module_body.value);
            }
        }
    }
}
