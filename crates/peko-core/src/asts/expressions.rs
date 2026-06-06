//! Expression AST nodes.
//!
//! An expression produces a value at runtime: literals (in [`super::values`]),
//! arithmetic and boolean operations, function calls, object construction and
//! access, array literals, ranges, casts, optional unwraps, and PekoX tag
//! expressions are all here.

use std::collections::HashMap;

use derive_new::new;

use super::data_structures::{PositionData, PositionedValue, Spanned, StringChunk};
use super::PekoAST;
use crate::types;

/// An array literal: `#[item1, item2, ...]`.
#[derive(Clone, new)]
pub struct ArrayAST {
    pub start: PositionData,
    pub end: PositionData,
    pub values: Vec<PekoAST>,
}

impl Spanned for ArrayAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A map literal: `#{key1: value1, key2: value2, ...}`.
#[derive(Clone, new)]
pub struct MapAST {
    pub start: PositionData,
    pub end: PositionData,
    pub key_values: Vec<(PekoAST, PekoAST)>,
}

impl Spanned for MapAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A PekoX tag expression: `<tag attr="x" event=closure>children</tag>` or
/// the self-closing form `<tag />`.
///
/// `attributes_start` / `attributes_end` cover the attribute list specifically
/// (inside the opening tag); `start` / `end` cover the entire tag span
/// including children.
#[derive(Clone, new)]
pub struct PekoXTagAST {
    pub start: PositionData,
    pub end: PositionData,
    pub attributes_start: PositionData,
    pub attributes_end: PositionData,
    pub tag: String,
    pub attributes: HashMap<String, PekoAST>,
    pub events: HashMap<String, PositionedValue<Vec<PekoAST>>>,
    pub children: Vec<PekoAST>,
    pub inner_text: Vec<StringChunk>,
}

impl Spanned for PekoXTagAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// Object instantiation via function-call syntax: `Object()` or
/// `Object<T1, T2>()`.
#[derive(Clone, new)]
pub struct ObjectConstructionAST {
    pub start: PositionData,
    pub end: PositionData,
    pub class_name: PositionedValue<String>,
    pub object_generics: Vec<types::PekoType>,
    pub arguments: Vec<(Option<PositionedValue<String>>, PekoAST)>,
}

impl Spanned for ObjectConstructionAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// Object access: `value.member`.
///
/// `ObjectAccessAST` carries no explicit span fields, instead its span is derived
/// from the spans of its `object` and `access` children.
#[derive(Clone, new)]
pub struct ObjectAccessAST {
    pub object: Box<PekoAST>,
    pub access: Box<PekoAST>,
}

impl Spanned for ObjectAccessAST {
    fn get_start(&self) -> &PositionData {
        self.object.get_start()
    }

    fn get_end(&self) -> &PositionData {
        self.access.get_end()
    }
}

/// Array indexing: `arrayvalue[idx]`.
///
/// The `end` field is present for symmetry with `start` but the [`Spanned`]
/// impl derives the actual end from the `access` child (i.e. the closing
/// `]` of the index expression). Preserved from the original implementation.
#[derive(Clone, new)]
pub struct ArrayAccessAST {
    pub start: PositionData,
    pub end: PositionData,
    pub array: Box<PekoAST>,
    pub access: Box<PekoAST>,
}

impl Spanned for ArrayAccessAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        // Note: deliberately uses the access child's end rather than the
        // struct's `end` field. Matches the original behavior of
        // `PekoAST::get_end` for this variant.
        self.access.get_end()
    }
}

/// An optional unwrap: `optional?`.
#[derive(Clone, new)]
pub struct UnwrapAST {
    pub start: PositionData,
    pub end: PositionData,
    pub optional: Box<PekoAST>,
}

impl Spanned for UnwrapAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A type cast expression.
///
/// `CastAST` derives both ends of its span from its children: the start
/// from `value`, and the end from the cast target's type-end position.
#[derive(Clone, new)]
pub struct CastAST {
    pub value: Box<PekoAST>,
    pub cast_to: types::PekoType,
}

impl Spanned for CastAST {
    fn get_start(&self) -> &PositionData {
        self.value.get_start()
    }

    fn get_end(&self) -> &PositionData {
        &self.cast_to.end_position
    }
}

/// Access to a module's symbols: `module::nested::symbol`.
#[derive(Clone, new)]
pub struct ModuleAccessAST {
    pub start: PositionData,
    pub end: PositionData,
    pub module_names: Vec<PositionedValue<String>>,
    pub accessor: Box<PekoAST>,
}

impl Spanned for ModuleAccessAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A reference to a previously-declared variable by name.
#[derive(Clone, new)]
pub struct VariableReferenceAST {
    pub variable_name: PositionedValue<String>,
}

impl Spanned for VariableReferenceAST {
    fn get_start(&self) -> &PositionData {
        &self.variable_name.start
    }

    fn get_end(&self) -> &PositionData {
        &self.variable_name.end
    }
}

/// A range expression: `from..to`.
#[derive(Clone, new)]
pub struct RangeAST {
    pub range_from: Box<PekoAST>,
    pub range_to: Box<PekoAST>,
}

impl Spanned for RangeAST {
    fn get_start(&self) -> &PositionData {
        self.range_from.get_start()
    }

    fn get_end(&self) -> &PositionData {
        self.range_to.get_end()
    }
}

/// A function call: `f(arg1, arg2)` or `f<T1, T2>(arg1, arg2)`.
///
/// `function_reference` is the callee expression (an identifier, an object
/// access, etc.); `function_generics` carries any explicit type arguments;
/// `arguments` carries the actual argument list, where each entry pairs an
/// optional argument name (for keyword-style calls) with the argument value.
#[derive(Clone, new)]
pub struct FunctionCallAST {
    pub start: PositionData,
    pub end: PositionData,
    pub function_reference: Box<PekoAST>,
    pub function_generics: Vec<types::PekoType>,
    pub arguments: Vec<(Option<PositionedValue<String>>, PekoAST)>,
}

impl Spanned for FunctionCallAST {
    fn get_start(&self) -> &PositionData {
        &self.start
    }

    fn get_end(&self) -> &PositionData {
        &self.end
    }
}

/// A binary expression: `lhs op rhs`.
#[derive(Clone, new)]
pub struct BinaryExpressionAST {
    pub lhs: Box<PekoAST>,
    pub rhs: Box<PekoAST>,
    pub operator: String,
}

impl BinaryExpressionAST {
    /// Returns a reference to the left-hand operand.
    #[must_use]
    pub fn get_lhs(&self) -> &PekoAST {
        &self.lhs
    }

    /// Returns a reference to the right-hand operand.
    #[must_use]
    pub fn get_rhs(&self) -> &PekoAST {
        &self.rhs
    }
}

impl Spanned for BinaryExpressionAST {
    fn get_start(&self) -> &PositionData {
        self.lhs.get_start()
    }

    fn get_end(&self) -> &PositionData {
        self.rhs.get_end()
    }
}

/// A unary expression: `op operand`.
#[derive(Clone, new)]
pub struct UnaryExpressionAST {
    pub operand: Box<PekoAST>,
    pub operator: String,
}

impl UnaryExpressionAST {
    /// Returns a reference to the operand.
    #[must_use]
    pub fn get_operand(&self) -> &PekoAST {
        &self.operand
    }
}

impl Spanned for UnaryExpressionAST {
    fn get_start(&self) -> &PositionData {
        self.operand.get_start()
    }

    fn get_end(&self) -> &PositionData {
        self.operand.get_end()
    }
}
