//! Statement AST nodes.
//!
//! Statements perform an action but do not (directly) produce a value:
//! variable reassignment, control flow (`if`, `while`, `for`, `break`,
//! `return`), platform-specific blocks, and the four top-level module
//! statements (`import`, `link`, `style`, `asset`).

use derive_new::new;

use super::PekoAST;
use super::data_structures::{ConditionBody, PositionData, PositionedValue, Spanned, UnpackItem};

/// A platform-specific block.
///
/// `architecture_test` distinguishes `arch { ... }` (architecture filter)
/// from `platform { ... }` (operating-system filter). `targets` lists the
/// platform or architecture names the block applies to; `body` is the
/// statements executed when at least one target matches.
#[derive(Clone, new)]
pub struct PlatformStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub architecture_test: bool,
    pub targets: Vec<PositionedValue<String>>,
    pub body: PositionedValue<Vec<PekoAST>>,
}

impl Spanned for PlatformStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `demo { ... }` conditional-compilation block. Its body is compiled and run
/// only in demo mode (`peko demo` / `peko build --demo`); a normal or release
/// build omits it entirely — the body is never simulated, codegen'd, or linked,
/// so any `import` it holds (and that import's transitive dependencies, e.g.
/// pekoshots) never reaches the binary. Modeled on [`PlatformStatementAST`].
#[derive(Clone, new)]
pub struct DemoStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub body: PositionedValue<Vec<PekoAST>>,
}

impl Spanned for DemoStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A variable reassignment: `x = value`, `x += value`, etc.
///
/// `assignment_operator` is `None` for plain `=`, and `Some("+")`,
/// `Some("-")`, etc. for compound assignments (`+=`, `-=`, ...).
///
/// `VariableReassignmentAST` derives its span from its children: start from
/// the variable reference, end from the assigned value.
#[derive(Clone, new)]
pub struct VariableReassignmentAST {
    pub variable_reference: Box<PekoAST>,
    pub variable_value: Box<PekoAST>,
    pub assignment_operator: Option<String>,
}

impl Spanned for VariableReassignmentAST {
    fn get_start(&self) -> &PositionData {
        self.variable_reference.get_start()
    }

    fn get_end(&self) -> &PositionData {
        self.variable_value.get_end()
    }
}

/// A `return` statement, with an optional return value.
#[derive(Clone, new)]
pub struct ReturnAST {
    pub start: PositionData,
    pub end: PositionData,
    pub return_value: Option<Box<PekoAST>>,
}

impl Spanned for ReturnAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// An `if` / `else if` / `else` statement.
///
/// `conditional_bodies` carries the `if` and any `else if` arms in source
/// order; `else_body` carries the final `else` block when present.
#[derive(Clone, new)]
pub struct IfStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub conditional_bodies: Vec<ConditionBody>,
    pub else_body: Option<PositionedValue<Vec<PekoAST>>>,
}

impl Spanned for IfStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// One arm of a `switch`: a pattern and the body run when it matches.
///
/// `pattern` is the `Enum::Variant` expression to match, or `None` for the
/// `_ =>` default arm that catches every remaining variant.
#[derive(Clone, new)]
pub struct SwitchArm {
    pub start: PositionData,
    pub end: PositionData,
    pub pattern: Option<Box<PekoAST>>,
    pub body: PositionedValue<Vec<PekoAST>>,
}

/// A `switch` over an enum: matches the subject against variant arms.
///
/// `arms` carries every `Enum::Variant => { ... }` arm and the optional
/// `_ => { ... }` default in source order. A switch must cover every variant
/// or include the default arm.
#[derive(Clone, new)]
pub struct SwitchStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub subject: Box<PekoAST>,
    pub arms: Vec<SwitchArm>,
}

impl Spanned for SwitchStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `while` loop with a single conditional body.
#[derive(Clone, new)]
pub struct WhileLoopAST {
    pub start: PositionData,
    pub end: PositionData,
    pub conditional_body: ConditionBody,
}

impl Spanned for WhileLoopAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `for` loop: `for item_id in iterator { body }`.
#[derive(Clone, new)]
pub struct ForLoopAST {
    pub start: PositionData,
    pub end: PositionData,
    pub item_id: PositionedValue<String>,
    pub iterator: Box<PekoAST>,
    pub body: PositionedValue<Vec<PekoAST>>,
}

impl Spanned for ForLoopAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `break` statement.
#[derive(Clone, new)]
pub struct BreakAST {
    pub start: PositionData,
    pub end: PositionData,
}

impl Spanned for BreakAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `continue` statement.
#[derive(Clone, new)]
pub struct ContinueAST {
    pub start: PositionData,
    pub end: PositionData,
}

impl Spanned for ContinueAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A module `import` statement.
///
/// `module_path` holds the import path split into its identifier segments.
/// The import `pkg::file::leaf` is stored as the segments `pkg`, `file`,
/// `leaf` in order. A bare `import name` is a single-segment path.
///
/// `import_as` carries the optional `as` alias. `symbols_to_unpack` carries
/// the optional `from` unpack list (which may include nested module unpacks
/// and glob imports). `module_version` carries the optional version pin and
/// is `None` when no `@"version"` was written.
#[derive(Clone, new)]
pub struct ImportStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub module_path: Vec<PositionedValue<String>>,
    pub import_as: Option<PositionedValue<String>>,
    pub symbols_to_unpack: Vec<UnpackItem>,
    pub module_version: Option<PositionedValue<String>>,
    /// An `export` re-exports the module as a submodule of the current one so a
    /// package entry (lib.peko) exposes it as `package::submodule`. A plain
    /// `import` leaves this false.
    pub is_export: bool,
}

impl Spanned for ImportStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `link` statement that references an external object file or library.
#[derive(Clone, new)]
pub struct LinkStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub object: PositionedValue<String>,
    pub link_as: PositionedValue<String>,
}

impl Spanned for LinkStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A `style` statement that imports an SCSS stylesheet.
#[derive(Clone, new)]
pub struct StyleStatementAST {
    pub start: PositionData,
    pub end: PositionData,
    pub stylesheet: PositionedValue<String>,
}

impl Spanned for StyleStatementAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}
